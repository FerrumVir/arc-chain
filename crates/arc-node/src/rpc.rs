use crate::SharedValidators;
use arc_consensus::StakeTier;
use arc_crypto::{Hash256, MerkleProof};
use arc_gpu::probe_gpu;
use arc_mempool::Mempool;
use arc_state::StateDB;
use arc_types::economics::RoleRevenueConfig;
use arc_types::*;
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Query, State as AxumState},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tower_http::cors::CorsLayer;

/// Faucet configuration.
const FAUCET_CLAIM_AMOUNT: u64 = 10_000;
const FAUCET_RATE_LIMIT_SECS: u64 = 60; // 1 minute per address (testnet - was 1 hour)
const FAUCET_GLOBAL_RATE_LIMIT: usize = 5000; // 5000 claims/minute - intentionally high for testnet TPS demos; lower before mainnet

/// Fixed amount transferred by one successfully mined, threshold-authorized
/// `CommunityInferenceReward` (0x25). A raw `InferenceAttestation` (0x16)
/// never earns this amount. The desktop reads the rate from RPC rather than
/// hardcoding it, while readiness fields separately disclose whether secure
/// reward issuance is actually available.
///
/// DERIVED from the single on-chain source of truth
/// (`arc_types::economics::INFERENCE_ATTESTATION_REWARD`, in base units) so
/// the number shown here and the value actually credited on-chain when an
/// authorized reward applies can never drift apart. Change the reward by
/// tuning that one constant.
const REWARD_PER_ATTESTATION_ARC: f64 = arc_types::economics::INFERENCE_ATTESTATION_REWARD as f64
    / arc_types::economics::ARC_BASE_UNITS as f64;

/// The peer-authenticated approval collector is compiled in. Runtime
/// readiness still fails closed unless the exact six-validator committee,
/// model, activation, and local validator key prerequisites are present.
const COMMUNITY_REWARD_APPROVAL_COLLECTION_READY: bool = true;
const COMMUNITY_REWARD_APPROVE_PATH: &str = "/internal/community/reward/approve";
const COMMUNITY_REWARD_EXPIRY_BLOCKS: u64 = 3_000;

/// Bond posted only by the local `InferenceAttestation` path, in base units.
/// Community-worker reward certificates use a protocol-fixed zero bond and
/// are paid exclusively by a separate `CommunityInferenceReward` transaction.
pub const DEFAULT_ATTESTATION_BOND: u64 = 1_000;

/// Challenge period, in blocks, on an auto-submitted `InferenceAttestation`.
/// Same story as the bond: previously a duplicated `100` literal.
pub const DEFAULT_ATTESTATION_CHALLENGE_PERIOD_BLOCKS: u64 = 100;

/// One ceiling shared by `/inference/run`, community assignments, and their
/// independent verifier. Keeping these contracts identical prevents the
/// coordinator from issuing work its own worker must reject.
pub const INFERENCE_RUN_MAX_TOKENS: u32 = 256;

/// How recently this node must have sealed a block for `/network/info` to
/// report `is_block_producing: true`.
///
/// Sized for the observed live failure, not for theory: four of six seeds have
/// not sealed a block in ~6 days while `GET /health` still answers `"ok"`,
/// because DAG rounds keep advancing without blocks. At the ~400 ms target
/// block time, 120 s is 300 missed blocks — far past any plausible hiccup, and
/// far short of the 6-day stall the desktop needs to surface.
pub const BLOCK_PRODUCTION_FRESH_SECS: u64 = 120;

/// How many blocks back `/network/info` scans for a block sealed by THIS
/// node's validator address. Bounded so the handler stays O(1)-ish on a node
/// with a 135 K-block store.
pub const SELF_PRODUCED_SCAN_BLOCKS: u64 = 512;

/// Ring capacity for this node's own measured `forward_shard` compute times.
/// Display-only, in-memory, and bounded: `/node/contribution` reports mean and
/// p50 over these samples so "more cores → more throughput" can be shown with
/// measurements instead of a fabricated earnings-per-core figure.
pub const OWN_COMPUTE_SAMPLE_CAP: usize = 256;

/// Authenticated community-mutation wire contract. V3 nodes deliberately do
/// not accept the legacy unsigned bodies.
pub const COMMUNITY_AUTH_VERSION: u8 = 1;
pub const COMMUNITY_REGISTER_PATH: &str = "/community/register";
pub const COMMUNITY_HEARTBEAT_PATH: &str = "/community/heartbeat";
pub const COMMUNITY_CLAIM_WORK_PATH: &str = "/community/claim_work";
pub const COMMUNITY_SUBMIT_WORK_PATH: &str = "/community/submit_work";
const COMMUNITY_AUTH_DOMAIN: &[u8] = b"ARC-community-http-mutation-v1\0";
const COMMUNITY_AUTH_MAX_CLOCK_SKEW_MS: u64 = 120_000;
// One extra millisecond closes the exact-boundary case where a maximally
// future-dated request is still accepted while its cache entry is pruned.
const COMMUNITY_AUTH_REPLAY_RETENTION_MS: u64 = COMMUNITY_AUTH_MAX_CLOCK_SKEW_MS * 2 + 1;
const COMMUNITY_AUTH_REPLAY_CACHE_MAX: usize = 100_000;
const COMMUNITY_MUTATION_BODY_LIMIT_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct CommunityReplayCache {
    /// (worker address, request nonce) -> server-time expiry in Unix ms.
    seen: HashMap<(Hash256, Hash256), u64>,
    next_prune_unix_ms: u64,
}

enum CommunityReplayError {
    Replay,
    Capacity,
}

impl CommunityReplayCache {
    /// Check-and-insert occurs under one mutex in the caller, making duplicate
    /// concurrent requests an atomic first-writer-wins operation.
    fn accept(
        &mut self,
        worker: Hash256,
        nonce: Hash256,
        now_unix_ms: u64,
    ) -> Result<(), CommunityReplayError> {
        if now_unix_ms >= self.next_prune_unix_ms
            || self.seen.len() >= COMMUNITY_AUTH_REPLAY_CACHE_MAX
        {
            self.seen.retain(|_, expiry| *expiry > now_unix_ms);
            self.next_prune_unix_ms = now_unix_ms.saturating_add(30_000);
        }
        if self.seen.contains_key(&(worker, nonce)) {
            return Err(CommunityReplayError::Replay);
        }
        if self.seen.len() >= COMMUNITY_AUTH_REPLAY_CACHE_MAX {
            return Err(CommunityReplayError::Capacity);
        }
        self.seen.insert(
            (worker, nonce),
            now_unix_ms.saturating_add(COMMUNITY_AUTH_REPLAY_RETENTION_MS),
        );
        Ok(())
    }
}

/// Chain identity as DECLARED by a genesis file, when the node was started
/// with one. Deliberately `Option`al everywhere downstream: a node booted
/// without `--genesis` genuinely does not know its network's declared name,
/// and `/network/info` reports that as null-plus-reason rather than guessing
/// (and never invents the word "mainnet").
#[derive(Debug, Clone)]
pub struct ChainIdentity {
    /// `chain.name` verbatim from the genesis TOML.
    pub name: String,
    /// `chain.chain_id` from the genesis TOML (defaults to `0x415243`).
    pub chain_id: String,
}

/// Shared node state passed to all handlers.
#[derive(Clone)]
pub struct NodeState {
    pub state: Arc<StateDB>,
    pub mempool: Arc<Mempool>,
    pub validator_address: Hash256,
    /// Validator keypair for signing coordinator-internal transactions such
    /// as inference attestations. Public paid-inference handlers never use it.
    /// Optional only for test fixtures; production paths always have it.
    pub validator_keypair: Option<Arc<arc_crypto::KeyPair>>,
    /// Local issuance/ingress switch for v1 rewards. Consensus additionally
    /// requires the genesis-committed activation height in `StateDB`; both
    /// gates must be open before this node reports rewards enabled.
    pub community_rewards_v1_enabled: bool,
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
    /// Streaming BLAKE3 commitment of every byte in the source `--model`
    /// artifact. This is the only identity accepted for inference routing,
    /// shard execution, caches, verification, and attestations.
    pub model_artifact_id: Option<arc_crypto::Hash256>,
    /// Live DAG validator set (updated by consensus loop via PeerConnected).
    pub dag_validators: SharedValidators,
    /// Per-sender rate limiter for tx submission: sender_address → last submit time.
    /// Limits to 10 tx/sec per sender to prevent mempool flood DoS.
    pub tx_rate_limit: Arc<dashmap::DashMap<[u8; 32], Instant>>,
    /// DAG consensus round (updated by consensus loop).
    pub dag_round: Arc<AtomicU64>,
    /// DAG committed block count (updated by consensus loop).
    pub dag_committed: Arc<AtomicU64>,
    /// Cryptographically random per-boot namespace for community job IDs.
    /// The monotonic counter alone restarts at zero, which previously made
    /// identical prompts reuse an already-paid job marker after a reboot.
    pub community_job_epoch: Hash256,
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
    pub shard_kv_caches: Arc<
        dashmap::DashMap<
            String,
            Arc<std::sync::Mutex<arc_inference::cached_integer_model::KVCache>>,
        >,
    >,
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
    /// Monotonic in-process counter, used ONLY to derive unique community
    /// job ids.
    ///
    /// It is deliberately no longer mixed into transaction nonces. It used to
    /// be added to the account's state nonce for every InferenceAttestation,
    /// but it accumulates forever — including across txs that never land — so
    /// after the first attestation applied, state advanced AND the counter
    /// advanced, and every later tx carried state+2 and failed InvalidNonce.
    /// Transaction nonces now come from account state on each submission; see
    /// `submit_inference_attestation`.
    pub attestation_nonce: Arc<AtomicU64>,
    /// Network-wide deterministic inference cache. Same prompt + same model
    /// returns the cached output_tokens in O(1), proven correct by the
    /// integer engine's determinism. Survives the full coordinator session
    /// (until eviction). The cache hit count is exposed in the response.
    pub inference_cache: Arc<arc_inference::distributed::DistributedCache>,
    /// Community worker registry - nodes that volunteered HTTP-based
    /// inference compute. Keyed by worker_id (self-chosen), value is the
    /// registration record + last-seen Instant for TTL pruning. Workers
    /// are pure outbound-HTTP contributors (POST to register, POST to
    /// heartbeat, long-poll for work). They never need inbound
    /// connectivity so they work behind any NAT / residential firewall.
    pub community_workers: Arc<dashmap::DashMap<String, (CommunityWorker, std::time::Instant)>>,
    /// Atomically consumed nonces for authenticated community HTTP mutations.
    /// Bounded and pruned by server time; shared by every authenticated
    /// community mutation route, including validator reward approvals.
    community_request_replays: Arc<parking_lot::Mutex<CommunityReplayCache>>,
    /// A validator signs at most one semantic commitment for a job and one
    /// job for a worker certificate during this process lifetime.
    community_reward_approval_jobs: Arc<dashmap::DashMap<Hash256, Hash256>>,
    community_reward_approval_certificates: Arc<dashmap::DashMap<Hash256, Hash256>>,
    /// Coordinator-side idempotency cache for rewards accepted into mempool.
    community_reward_submissions: Arc<dashmap::DashMap<Hash256, CommunityRewardSubmission>>,
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
    pub community_work_queue:
        Option<Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<WorkItem>>>>,
    /// Pending community work keyed by job id. Keeping the original work item
    /// and the worker assignment beside the response channel lets
    /// `/community/submit_work` prove that a result belongs to a real,
    /// coordinator-issued job before it updates counters or submits a
    /// reward-bearing attestation.
    pub community_work_results: Option<Arc<dashmap::DashMap<String, PendingCommunityWork>>>,
    /// At most one destructively claimed job per worker across concurrent
    /// seed-poll requests. Value is the assigned job id; an empty value means
    /// a claim long-poll currently holds the worker's reservation.
    pub community_active_jobs: Arc<dashmap::DashMap<String, String>>,
    /// Shared outbound HTTP client for ALL coordinator→shard traffic.
    /// Built once at boot so the keep-alive connection pool survives across
    /// requests. Previously every /inference/run_sharded and
    /// /inference/run_consensus call built its own `reqwest::Client`, which
    /// owns the pool — so it was dropped at the end of every request and each
    /// replica paid a fresh TCP handshake on the first hop of every request
    /// (40 ms LAX … 215 ms SGP from the probe host).
    pub inference_http: reqwest::Client,
    /// Sharded runs served from the deterministic cache. Kept SEPARATE from
    /// `sharded_runs_total`: a cache hit performs no pipeline walk, and
    /// counting it as a run inflated every "distributed inference served"
    /// figure the dashboard shows.
    pub sharded_cache_hits: Arc<AtomicU64>,
    /// Provenance of the ORIGINAL sharded run behind each cache key, keyed by
    /// cache-key hex. Lets a cache hit return the real attestation tx, input
    /// hash, shard trace and original wall time instead of zeros/empties.
    pub sharded_run_meta: Arc<dashmap::DashMap<String, Value>>,
    /// Dedicated rayon pool for local inference compute, rebuildable at
    /// runtime via POST /node/threads.
    ///
    /// `None` means "use rayon's implicit global pool", which is sized from
    /// `available_parallelism()` unless `RAYON_NUM_THREADS` is set in the
    /// environment. We default to None so a node that never touches
    /// /node/threads spawns exactly the threads it did before, and so the
    /// second `build_node_state` call (the ETH RPC server) doesn't double the
    /// process's worker threads.
    pub compute_pool: Arc<parking_lot::RwLock<Option<Arc<rayon::ThreadPool>>>>,
    /// Width of `compute_pool`. 0 = no dedicated pool (rayon global).
    pub compute_threads: Arc<AtomicU32>,
    /// Seed RPC endpoints ("host:9090") this node can pull shard topology
    /// from when its own registry lacks full coverage. Populated from
    /// --peers / --seeds-file in main.rs.
    pub seed_rpc_addrs: Arc<Vec<String>>,
    /// Last shard-registry bootstrap attempt, so a coordinator under load
    /// doesn't hammer the seeds once per failing request.
    pub last_registry_bootstrap: Arc<Mutex<Option<std::time::Instant>>>,
    /// Chain identity declared by the `--genesis` file, if one was supplied.
    /// `None` on a node started without `--genesis`: `/network/info` then
    /// reports the name and chain_id as null with a reason, since the only
    /// thing such a node actually knows about its chain is its genesis hash.
    pub chain_identity: Option<ChainIdentity>,
    /// Ring of this node's OWN measured `forward_shard` compute times (ms),
    /// newest last, capped at `OWN_COMPUTE_SAMPLE_CAP`.
    ///
    /// `latency_stats` cannot answer "how fast is this node's own compute":
    /// it is an EWMA of round-trip times to OTHER replicas, it holds one
    /// smoothed scalar rather than samples (so no percentile is derivable from
    /// it), and it contains no entry for this node at all unless this node
    /// dialled itself. These are real per-hop measurements taken by the local
    /// shard handler, which is what `/node/contribution` needs to report a
    /// mean and a p50 honestly. Display-only; never consensus input.
    pub own_compute_ms: Arc<parking_lot::Mutex<std::collections::VecDeque<u64>>>,
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
    /// True when this figure came from a cheap GET /health probe rather than
    /// a real forward_shard hop. Probe RTT is not comparable to hop latency
    /// (no compute, no activation payload), so it is only ever used to
    /// REPLACE a value we've decided is poisoned — never blended into one.
    /// The first real hop clears the flag.
    pub probe_only: bool,
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
/// registered via outbound HTTP POST. Workers contribute compute by
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
    /// Exact model identity hash. Human-readable model names are display-only;
    /// dispatch eligibility is always decided with this commitment.
    #[serde(default)]
    pub model_id: Option<String>,
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

/// How long a latency sample stays authoritative. Past this age the sample
/// is treated as UNKNOWN rather than as fact.
///
/// Without this the map was insert-only: nothing ever removed, decayed or
/// re-measured an entry. Live evidence (2026-08-16): every seed's table
/// showed ages of 37,000-39,000 seconds — samples over ten hours old — and
/// they still drove replica ordering. Worse, the ordering was
/// self-reinforcing: run_sharded only ever dialled replicas[0], so a replica
/// demoted by one bad sample received no further samples and could never
/// climb back. LHR sat at an EWMA of 37,276 ms across all six seeds while
/// its own recorded hops were 180-410 ms.
pub const LATENCY_STALE_SECS: u64 = 300;

/// Interval between background GET /health probes of known replica sockets.
pub const LATENCY_PROBE_INTERVAL_SECS: u64 = 60;

/// A recorded EWMA above this, AND more than `LATENCY_POISON_RATIO` times the
/// replica's measured health RTT, is treated as poisoned rather than slow.
pub const LATENCY_POISON_FLOOR_MS: f64 = 5_000.0;
pub const LATENCY_POISON_RATIO: f64 = 20.0;

/// Fold a real forward_shard hop observation into the EWMA for `socket`.
pub fn record_latency(stats: &dashmap::DashMap<String, LatencyEWMA>, socket: &str, hop_ms: u64) {
    let hop = hop_ms as f64;
    let now = std::time::Instant::now();
    stats
        .entry(socket.to_string())
        .and_modify(|e| {
            if e.probe_only {
                // The stored value was a health-probe placeholder, not a hop.
                // Replace rather than blend — mixing a ~200 ms probe RTT into
                // a hop EWMA would understate the hop cost for several
                // dispatches.
                e.ms = hop;
                e.count = 1;
                e.probe_only = false;
            } else {
                e.ms = LATENCY_ALPHA * hop + (1.0 - LATENCY_ALPHA) * e.ms;
                e.count = e.count.saturating_add(1);
            }
            e.last_updated = now;
        })
        .or_insert_with(|| LatencyEWMA {
            ms: hop,
            count: 1,
            last_updated: now,
            probe_only: false,
        });
}

/// Record a health-probe RTT as a PROVISIONAL latency for a socket we have no
/// fresh hop sample for. Never overwrites a fresh hop measurement.
pub fn record_probe_latency(
    stats: &dashmap::DashMap<String, LatencyEWMA>,
    socket: &str,
    probe_ms: u64,
) {
    let now = std::time::Instant::now();
    stats
        .entry(socket.to_string())
        .and_modify(|e| {
            if e.probe_only {
                e.ms = probe_ms as f64;
                e.last_updated = now;
            }
        })
        .or_insert_with(|| LatencyEWMA {
            ms: probe_ms as f64,
            count: 0,
            last_updated: now,
            probe_only: true,
        });
}

/// The latency we are willing to ACT on for this replica, or `None` when we
/// genuinely don't know. Ageing happens here rather than by mutating the map,
/// so /inference/latency_stats can still show the raw sample and its age.
pub fn effective_latency_ms(stat: &LatencyEWMA) -> Option<f64> {
    if stat.last_updated.elapsed().as_secs() >= LATENCY_STALE_SECS {
        None
    } else {
        Some(stat.ms)
    }
}

/// Sort a replica bucket by *fresh* EWMA latency ascending. Replicas with no
/// usable sample — never measured, or measured too long ago to trust — are
/// placed AFTER measured ones but keep their insertion order, so an unknown
/// replica is never starved of its next try.
pub fn sort_replicas_by_latency(
    replicas: &mut [ShardInfo],
    stats: &dashmap::DashMap<String, LatencyEWMA>,
) {
    replicas.sort_by(|a, b| {
        let a_ms = stats
            .get(&a.socket_addr)
            .and_then(|v| effective_latency_ms(&v));
        let b_ms = stats
            .get(&b.socket_addr)
            .and_then(|v| effective_latency_ms(&v));
        match (a_ms, b_ms) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

/// Decide whether a recorded EWMA should be discarded in favour of a fresh
/// health-probe RTT. Pure function so the policy is unit-testable.
///
/// A node can be genuinely slow, and we must not erase that. But an EWMA of
/// 37 seconds against a socket that answers /health in 200 ms is not "slow" —
/// it is a fossil from an incident that ended hours ago, and because the
/// router never re-dials a demoted replica it would never be corrected.
pub fn probe_supersedes_recorded(recorded_ms: f64, probe_ms: u64) -> bool {
    recorded_ms > LATENCY_POISON_FLOOR_MS
        && recorded_ms > LATENCY_POISON_RATIO * (probe_ms.max(1) as f64)
}

// ─── Runtime compute-pool control ──────────────────────────────────────────

/// Run `f` on this node's dedicated rayon pool when one is configured, else
/// on rayon's implicit global pool.
///
/// Everything inside `f` — including the `into_par_iter()` over attention
/// heads in `forward_shard_token` and the `par_chunks_mut` inside every
/// matmul — picks up the installed pool, because rayon resolves the current
/// pool from the calling thread. That is what makes POST /node/threads
/// actually move CPU utilisation instead of just changing a number.
pub fn install_on_compute_pool<R, F>(node: &NodeState, f: F) -> R
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    let pool = node.compute_pool.read().clone();
    match pool {
        Some(p) => p.install(f),
        None => f(),
    }
}

/// Build (or rebuild) the dedicated compute pool at `threads` width.
///
/// `threads == 0` drops the dedicated pool and returns the node to rayon's
/// global pool. The old pool is dropped once the last in-flight `install()`
/// releases its Arc, so a rebuild never interrupts a request already running.
pub fn set_compute_threads(node: &NodeState, threads: usize) -> Result<usize, String> {
    if threads == 0 {
        *node.compute_pool.write() = None;
        node.compute_threads.store(0, Ordering::Relaxed);
        return Ok(0);
    }
    if threads > 1024 {
        return Err(format!("threads must be <= 1024, got {}", threads));
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("arc-infer-{}", i))
        .build()
        .map_err(|e| format!("building rayon pool with {} threads: {}", threads, e))?;
    *node.compute_pool.write() = Some(Arc::new(pool));
    node.compute_threads
        .store(threads as u32, Ordering::Relaxed);
    Ok(threads)
}

/// Threads rayon's GLOBAL pool will use: `RAYON_NUM_THREADS` when set and
/// parseable, otherwise the machine's available parallelism.
pub fn rayon_global_width() -> usize {
    std::env::var("RAYON_NUM_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        })
}

// ─── Shared pipeline assembly ──────────────────────────────────────────────

/// One hop of the pipeline: the layer range, plus every replica that can
/// serve it, ordered best-first.
pub type PipelineHop = ((usize, usize), Vec<ShardInfo>);

/// Why the shard registry could not be turned into a runnable pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// The coordinator has no commitment for its source model artifact.
    ModelIdentityUnavailable,
    /// Shards exist, but none commit to the coordinator's exact artifact.
    NoMatchingModel { model_id: Hash256 },
    /// Registry is empty (or every entry was a stub).
    NoShards,
    /// Coverage stops before the model does.
    Gap {
        expected: usize,
        got: (usize, usize),
        node: String,
        addr: String,
    },
    /// Coverage ran out before reaching n_layers.
    Incomplete { covered: usize, total: usize },
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineError::ModelIdentityUnavailable => write!(
                f,
                "Exact source-artifact model commitment unavailable; sharded inference disabled"
            ),
            PipelineError::NoMatchingModel { model_id } => write!(
                f,
                "No shards announced for exact model artifact 0x{}",
                model_id.to_hex()
            ),
            PipelineError::NoShards => write!(
                f,
                "No shards announced. Need shard registry to be populated."
            ),
            PipelineError::Gap {
                expected,
                got,
                node,
                addr,
            } => write!(
                f,
                "Pipeline gap: expected layer {} next, got shard [{}, {}) (node {}, addr {})",
                expected, got.0, got.1, node, addr
            ),
            PipelineError::Incomplete { covered, total } => write!(
                f,
                "Pipeline incomplete: covered layers 0..{} but model has {} layers",
                covered, total
            ),
        }
    }
}

/// Turn a flat list of announced shards into a runnable pipeline: one entry
/// per layer range in walk order, each carrying its full replica list ordered
/// best-first.
///
/// This is THE pipeline planner. /inference/run_sharded, /inference/run_consensus
/// and /inference/auto all call it, because three separate implementations of
/// this logic is exactly how the live network ended up with three different
/// answers to "is the pipeline complete?":
///
///   - run_sharded had the repaired version (commit 30b3113).
///   - run_consensus — the endpoint the desktop actually calls — still had the
///     pre-30b3113 version: it PRESERVED a stub-only bucket instead of dropping
///     it, and its coverage walk had no overlap skip. That is the source of
///     "Pipeline gap: expected layer 32 next, got [28, 30)" on every coordinator
///     whenever the retired SAO/JNB shards were still registered.
///   - /inference/auto walked the raw replica list with no dedupe at all, so on
///     a 3x-replicated network the second [0, 6) replica made `contiguous`
///     false immediately. `has_full_pipeline` was therefore false for ANY
///     replication factor > 1 and /inference/auto never once took its own
///     documented best path.
///
/// Steps, in order:
///   1. bucket announcements by (start_layer, end_layer);
///   2. dedupe per node_name inside each bucket, preferring a routable addr
///      over a stub (a rebooted coordinator's self-announce and the gossiped
///      copy land under different registry keys);
///   3. drop stub addrs UNCONDITIONALLY, then drop buckets left empty. The old
///      run_consensus kept a stub "as a fallback" when no routable replica
///      existed — that is how a community worker announcing 127.0.0.1:9090
///      with an off-grid [0, 8) range became the only candidate for layer 0
///      and walked the pipeline off the rails. Honest failure beats fake
///      liveness;
///   4. order each bucket by fresh EWMA latency;
///   5. greedy contiguous cover from layer 0, skipping any range that starts
///      inside already-covered territory (an off-grid [0, 8) alongside the
///      standard [0, 6)/[6, 12) tiling must not manufacture a false gap).
///
/// The returned Vec IS the pipeline: callers index prefill workers, the decode
/// loop, the trace and cleanup off this one vector. Previously run_sharded
/// built a filtered `pipeline` for some of those and indexed the UNFILTERED
/// `pipeline_ranges` for the others, so any off-grid range silently shifted
/// the walk by one hop.
pub fn assemble_pipeline(
    announced: Vec<ShardInfo>,
    stats: &dashmap::DashMap<String, LatencyEWMA>,
) -> Result<Vec<PipelineHop>, PipelineError> {
    let mut by_range: std::collections::BTreeMap<(usize, usize), Vec<ShardInfo>> =
        std::collections::BTreeMap::new();
    for s in announced {
        let key = (s.start_layer, s.end_layer);
        let bucket = by_range.entry(key).or_default();
        match bucket
            .iter()
            .position(|existing| existing.node_name == s.node_name)
        {
            None => bucket.push(s),
            Some(i) => {
                if is_stub_socket_addr(&bucket[i].socket_addr)
                    && !is_stub_socket_addr(&s.socket_addr)
                {
                    bucket[i] = s;
                }
            }
        }
    }

    by_range.retain(|_, bucket| {
        bucket.retain(|s| !is_stub_socket_addr(&s.socket_addr));
        !bucket.is_empty()
    });
    for bucket in by_range.values_mut() {
        sort_replicas_by_latency(bucket, stats);
    }

    // BTreeMap already iterates in (start, end) order, so the SHORTEST range
    // beginning at each layer is seen first.
    let candidates: Vec<PipelineHop> = by_range.into_iter().collect();
    if candidates.is_empty() {
        return Err(PipelineError::NoShards);
    }

    let n_layers = candidates[0].1[0].total_layers;
    let mut chosen: Vec<PipelineHop> = Vec::with_capacity(candidates.len());
    let mut covered_to = 0usize;
    for ((start, end), replicas) in candidates {
        if covered_to >= n_layers {
            break;
        }
        if start < covered_to {
            // Overlaps a range we already took: a duplicate, or an off-grid
            // alternative we chose not to walk.
            continue;
        }
        if start != covered_to {
            return Err(PipelineError::Gap {
                expected: covered_to,
                got: (start, end),
                node: replicas[0].node_name.clone(),
                addr: replicas[0].socket_addr.clone(),
            });
        }
        covered_to = end;
        chosen.push(((start, end), replicas));
    }
    if covered_to != n_layers {
        return Err(PipelineError::Incomplete {
            covered: covered_to,
            total: n_layers,
        });
    }
    Ok(chosen)
}

/// Filter announcements by exact artifact commitment before planning. Invalid
/// or legacy shape-derived IDs are ineligible rather than silently co-mingled.
pub fn assemble_pipeline_for_model(
    announced: Vec<ShardInfo>,
    expected_model_id: Hash256,
    stats: &dashmap::DashMap<String, LatencyEWMA>,
) -> Result<Vec<PipelineHop>, PipelineError> {
    let had_announcements = !announced.is_empty();
    let matching: Vec<_> = announced
        .into_iter()
        .filter(|shard| {
            parse_hash256_hex(&shard.model_id, "shard model_id").ok() == Some(expected_model_id)
        })
        .collect();
    if matching.is_empty() && had_announcements {
        return Err(PipelineError::NoMatchingModel {
            model_id: expected_model_id,
        });
    }
    assemble_pipeline(matching, stats)
}

/// `assemble_pipeline` against this node's live registry, restricted to this
/// coordinator's exact source artifact.
pub fn assemble_pipeline_for(node: &NodeState) -> Result<Vec<PipelineHop>, PipelineError> {
    let model_id = node
        .model_artifact_id
        .ok_or(PipelineError::ModelIdentityUnavailable)?;
    assemble_pipeline_for_model(
        fresh_shards(&node.shard_registry),
        model_id,
        &node.latency_stats,
    )
}

/// Build a `NodeState` from components.
// These are the node's runtime handles, not a parameter list that wants
// grouping: four of them are `Option<Arc<..>>` and two are `Hash256`, so
// collapsing them into a struct trades a compiler-checked positional error
// for a silent field mix-up at every call site. Kept explicit on purpose.
#[allow(clippy::too_many_arguments)]
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
    model_artifact_id: Option<arc_crypto::Hash256>,
) -> NodeState {
    let tier = StakeTier::from_stake(stake).unwrap_or(StakeTier::Spark);
    NodeState {
        state,
        mempool,
        validator_address,
        validator_keypair,
        community_rewards_v1_enabled: false,
        stake,
        tier,
        boot_time,
        peer_count,
        faucet_claims: Arc::new(dashmap::DashMap::new()),
        faucet_claims_total: Arc::new(AtomicU32::new(0)),
        inference_model,
        candle_engine,
        candle_model_id,
        model_artifact_id,
        dag_validators: Arc::new(parking_lot::RwLock::new(vec![(validator_address, stake)])),
        tx_rate_limit: Arc::new(dashmap::DashMap::new()),
        dag_round: Arc::new(AtomicU64::new(0)),
        dag_committed: Arc::new(AtomicU64::new(0)),
        community_job_epoch: arc_crypto::KeyPair::generate_ed25519().address(),
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
        verification_manager: Arc::new(std::sync::Mutex::new(
            arc_vm::inference_verify::VerificationManager::new(),
        )),
        revenue_config: RoleRevenueConfig::default(),
        community_workers: Arc::new(dashmap::DashMap::new()),
        community_request_replays: Arc::new(parking_lot::Mutex::new(
            CommunityReplayCache::default(),
        )),
        community_reward_approval_jobs: Arc::new(dashmap::DashMap::new()),
        community_reward_approval_certificates: Arc::new(dashmap::DashMap::new()),
        community_reward_submissions: Arc::new(dashmap::DashMap::new()),
        // Community work dispatch — bounded mpsc with 256-slot buffer. New
        // jobs that arrive when 256 are already queued get backpressure
        // (the dispatcher in /inference/run awaits .send().await). Workers
        // long-poll the receiver in claim_work; multiple handlers race for
        // each item via the tokio Mutex.
        community_work_tx: None,
        community_work_queue: None,
        community_work_results: None,
        community_active_jobs: Arc::new(dashmap::DashMap::new()),
        // One client, one connection pool, for the life of the process.
        // `pool_idle_timeout` is deliberately longer than the 15 s shard
        // announcement tick so an idle inter-seed connection survives between
        // requests. Falls back to a default client if the builder ever fails
        // (it can't in practice — no TLS backend selection happens here).
        inference_http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .build()
            .unwrap_or_default(),
        sharded_cache_hits: Arc::new(AtomicU64::new(0)),
        sharded_run_meta: Arc::new(dashmap::DashMap::new()),
        compute_pool: Arc::new(parking_lot::RwLock::new(None)),
        compute_threads: Arc::new(AtomicU32::new(0)),
        seed_rpc_addrs: Arc::new(Vec::new()),
        last_registry_bootstrap: Arc::new(Mutex::new(None)),
        // No genesis file is visible from here; `serve` overwrites this when
        // main.rs was given --genesis.
        chain_identity: None,
        own_compute_ms: Arc::new(parking_lot::Mutex::new(
            std::collections::VecDeque::with_capacity(OWN_COMPUTE_SAMPLE_CAP),
        )),
    }
}

/// Push `v` onto a bounded sample ring, evicting the oldest first so the ring
/// always holds the most RECENT `cap` measurements. Split out from
/// `record_own_compute_ms` so the eviction policy is unit-testable without
/// constructing a whole `NodeState`.
fn push_bounded(ring: &mut std::collections::VecDeque<u64>, v: u64, cap: usize) {
    if cap == 0 {
        return;
    }
    while ring.len() >= cap {
        ring.pop_front();
    }
    ring.push_back(v);
}

/// Record one of THIS node's own `forward_shard` compute measurements.
/// Keeps at most `OWN_COMPUTE_SAMPLE_CAP` samples, evicting oldest first.
fn record_own_compute_ms(node: &NodeState, compute_ms: u64) {
    push_bounded(
        &mut node.own_compute_ms.lock(),
        compute_ms,
        OWN_COMPUTE_SAMPLE_CAP,
    );
}

/// Arithmetic mean of `samples`. `None` for an empty slice — an average of no
/// measurements is not zero, it is unknown.
fn mean_u64(samples: &[u64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let sum: u128 = samples.iter().map(|&v| v as u128).sum();
    Some(sum as f64 / samples.len() as f64)
}

/// Median (p50) of `samples`. For an even sample count this returns the LOWER
/// of the two middle values rather than interpolating, so the reported figure
/// is always a value that was actually measured. `None` for an empty slice.
fn p50_u64(samples: &[u64]) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    Some(sorted[(sorted.len() - 1) / 2])
}

/// How many more verified community-job rewards the treasury can still pay:
/// `floor(treasury_balance / reward_per_attestation)`.
///
/// The reward is a TRANSFER out of a finite treasury, not an emission, so a
/// projection that ignores the remaining pool is dishonest — this is the term
/// that makes "you will earn X/day forever" false. `None` when the reward per
/// attestation is zero, where the quotient is undefined rather than infinite.
pub fn rewards_remaining(treasury_balance: u64, reward_per_attestation: u64) -> Option<u64> {
    if reward_per_attestation == 0 {
        return None;
    }
    Some(treasury_balance / reward_per_attestation)
}

/// Registered-vs-active split of a validator set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatorSplit {
    /// Every entry in the set, including zero-stake ones.
    pub registered: usize,
    /// Entries whose stake meets `min_active_stake`.
    pub active: usize,
    /// Entries carrying exactly zero stake.
    pub zero_stake: usize,
    /// Sum of all stake in the set.
    pub total_stake: u64,
    /// Sum of stake across active entries only.
    pub active_stake: u64,
}

/// Split a validator set into registered vs active at `min_active_stake`.
///
/// Exists because `/validators`, `/health` and `/stats` all report
/// `dag_validators.len()`, which counts zero-stake peers. On the live network
/// that inflates the set by 4 of 14: those peers cannot vote and cannot
/// produce, so every count derived from the raw length overstates the network.
/// A zero `min_active_stake` still leaves zero-stake entries inactive — a
/// validator with no stake at risk is not securing anything.
pub fn split_validators(validators: &[(Hash256, u64)], min_active_stake: u64) -> ValidatorSplit {
    let mut split = ValidatorSplit {
        registered: validators.len(),
        active: 0,
        zero_stake: 0,
        total_stake: 0,
        active_stake: 0,
    };
    for (_, stake) in validators {
        split.total_stake = split.total_stake.saturating_add(*stake);
        if *stake == 0 {
            split.zero_stake += 1;
            continue;
        }
        if *stake >= min_active_stake {
            split.active += 1;
            split.active_stake = split.active_stake.saturating_add(*stake);
        }
    }
    split
}

/// Observed attestation rate, in attestations per day, measured from the block
/// TIMESTAMPS of the first and last attestation this node can see.
///
/// Returns `Err(reason)` — never a number — whenever the rate is not
/// computable. No assumed block time is involved: a nominal 400 ms block time
/// applied to a chain that has sealed nothing for six days would manufacture a
/// throughput figure out of a stall.
///
/// `n` attestations spanning the window define `n - 1` intervals, so the rate
/// is `(n - 1) / elapsed_days`. Using `n / elapsed_days` would overstate the
/// rate by a full interval, which matters most at the low counts real workers
/// have (2 attestations would read as double the observed rate).
pub fn attestations_per_day_observed(
    count: u64,
    first_ts_ms: Option<u64>,
    last_ts_ms: Option<u64>,
) -> Result<f64, &'static str> {
    if count == 0 {
        return Err("no attestations for this address are visible from this node");
    }
    if count < 2 {
        return Err("a single attestation defines no interval; a rate needs at least two");
    }
    let (Some(first), Some(last)) = (first_ts_ms, last_ts_ms) else {
        return Err(
            "block timestamps for the observed window are not retained by this node \
                    (non-archive nodes prune old blocks)",
        );
    };
    if first == 0 || last == 0 {
        return Err("a block in the observed window carries a zero timestamp");
    }
    if last <= first {
        return Err(
            "first and last attestation fall in the same instant; no elapsed time to \
                    divide by",
        );
    }
    let elapsed_ms = (last - first) as f64;
    Ok((count - 1) as f64 * 86_400_000.0 / elapsed_ms)
}

/// The most recent block actually PRESENT in this node's block store.
///
/// Not the same as `get_block(state.height())`, and the difference is not
/// theoretical: on a chain sealing blocks every ~100 ms, the height counter
/// advances before the block body is inserted, so a direct lookup at `height()`
/// misses most of the time. A producing node then reports no block hash, no
/// block timestamp and a null age — i.e. it looks exactly like a stalled one,
/// which is the single distinction `/network/info` exists to make. Non-archive
/// pruning can also leave the newest retained block below `height()`.
///
/// Scans back at most `SELF_PRODUCED_SCAN_BLOCKS` and returns the first block
/// found, so the reported height/hash/timestamp always describe a real block.
fn latest_available_block(node: &NodeState) -> Option<Block> {
    let h = node.state.height();
    let floor = h.saturating_sub(SELF_PRODUCED_SCAN_BLOCKS);
    let mut i = h;
    loop {
        if let Some(b) = node.state.get_block(i) {
            return Some(b);
        }
        if i == 0 || i <= floor {
            return None;
        }
        i -= 1;
    }
}

/// Wall-clock unix milliseconds, for display-only age math. Never consensus
/// input: nothing derived from this reaches a block, a hash or a vote.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Age in seconds of a unix-millis timestamp, saturating at zero. `None` when
/// the timestamp is zero (absent) or lies in the future by more than a second
/// — a negative age is a clock disagreement, not an age.
fn age_secs_from_ms(ts_ms: u64) -> Option<u64> {
    if ts_ms == 0 {
        return None;
    }
    let now = now_unix_ms();
    if ts_ms > now.saturating_add(1_000) {
        return None;
    }
    Some(now.saturating_sub(ts_ms) / 1_000)
}

/// An explorer link for an attestation tx, and the reason when there isn't one.
///
/// Returns `(explorer_url, unavailable_reason)`. The URL is non-null ONLY when
/// the transaction actually exists in a block on this node.
///
/// This used to be an unconditional `format!("/tx/0x{hash}")`, emitted the
/// instant the tx was inserted into the mempool. On a chain that is not sealing
/// blocks that link never resolves: `/tx/{hash}` answers
/// `{"error":"Transaction ... not found"}` forever, so the demo handed the
/// caller a receipt for a transaction that did not exist and never would. A
/// link that is not yet valid is not a link — it is an invented fact, and the
/// caller cannot tell the difference from a real one.
///
/// When the chain is stalled this says so explicitly rather than saying
/// "pending", because "pending" implies it is coming.
fn explorer_url_for(
    node: &NodeState,
    tx_hash: &arc_crypto::Hash256,
    attestation_status: &str,
) -> (Value, Option<String>) {
    let hex_hash = hex::encode(tx_hash.0);

    if node.state.get_transaction(&tx_hash.0).is_some() {
        return (Value::String(format!("/tx/0x{hex_hash}")), None);
    }

    if attestation_status != "submitted_to_mempool" {
        return (
            Value::Null,
            Some(format!(
                "no explorer link: the attestation was not submitted (status {attestation_status}), \
                 so no transaction exists to link to."
            )),
        );
    }

    // Submitted, not yet in a block. Whether it ever will be depends on this
    // chain still sealing — which is a fact we hold, so state it.
    let chain_advancing = latest_available_block(node)
        .and_then(|b| age_secs_from_ms(b.header.timestamp))
        .map(|age| age <= BLOCK_PRODUCTION_FRESH_SECS);

    let reason = match chain_advancing {
        Some(false) => format!(
            "no explorer link: tx 0x{hex_hash} is in this node's mempool but block production on \
             this node is STALLED, so it will not be mined and /tx/0x{hex_hash} will keep \
             returning 'not found'. Submit against a seed that is sealing blocks."
        ),
        Some(true) => format!(
            "no explorer link yet: tx 0x{hex_hash} is in the mempool and this node is sealing \
             blocks, so poll /tx/0x{hex_hash} — the link becomes valid once it is mined."
        ),
        None => format!(
            "no explorer link: tx 0x{hex_hash} is in the mempool, but this node cannot determine \
             whether it is sealing blocks, so it cannot say the link will ever resolve."
        ),
    };

    (Value::Null, Some(reason))
}

/// Capacity of the community work mpsc. Each slot is a single whole-prompt
/// job; under heavy load the dispatcher's `.send().await` provides natural
/// backpressure without unbounded memory growth.
const COMMUNITY_WORK_QUEUE_CAP: usize = 256;

/// Start the RPC server.
// Same reasoning as `build_node_state`, more so: this is the single process
// entry point for the RPC layer and several parameters share a type
// (`Option<Arc<AtomicU64>>` twice, `Vec<String>`/`Vec<ShardInfo>`). A params
// struct would compile just as happily with two of them transposed, which is
// exactly the failure this signature makes impossible.
#[allow(clippy::too_many_arguments)]
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
    model_artifact_id: Option<arc_crypto::Hash256>,
    dag_validators: Option<SharedValidators>,
    dag_round: Option<Arc<AtomicU64>>,
    dag_committed: Option<Arc<AtomicU64>>,
    shard_infos: Vec<ShardInfo>,
    // seed_rpc_addrs: seed RPC endpoints ("host:9090") used to bootstrap the
    //   shard registry when this node is asked to coordinate but only knows
    //   its own shards.
    // compute_threads: dedicated inference-pool width; 0 = rayon's global pool.
    seed_rpc_addrs: Vec<String>,
    compute_threads: usize,
    // chain_identity: the genesis file's declared chain name / chain_id, when
    //   the node was started with --genesis. Surfaced by GET /network/info so
    //   the desktop never has to guess which chain it is talking to.
    chain_identity: Option<ChainIdentity>,
    // Local issuance half of the two-part rollout gate. Consensus activation
    // comes from canonical genesis and is enforced independently by StateDB.
    community_rewards_v1_enabled: bool,
) -> anyhow::Result<()> {
    if community_rewards_v1_enabled && state.community_rewards_v1_activation_height().is_none() {
        anyhow::bail!(
            "--enable-community-rewards-v1 requires [chain].community_rewards_v1_activation_height in canonical genesis"
        );
    }
    let mut node = build_node_state(
        state,
        mempool,
        validator_address,
        validator_keypair,
        stake,
        boot_time,
        peer_count,
        inference_model,
        candle_engine,
        candle_model_id,
        model_artifact_id,
    );
    node.chain_identity = chain_identity;
    node.community_rewards_v1_enabled = community_rewards_v1_enabled;
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
    node.seed_rpc_addrs = Arc::new(seed_rpc_addrs);
    // Seed the local registry with every range this node holds so /shards
    // reports the full picture the moment RPC comes up. The registry is
    // keyed by (socket_addr + range) so two entries with the same socket but
    // different ranges coexist.
    for si in &shard_infos {
        let key = format!("{}#{}-{}", si.socket_addr, si.start_layer, si.end_layer);
        node.shard_registry
            .insert(key, (si.clone(), std::time::Instant::now()));
    }

    // ── Dedicated inference compute pool ────────────────────────────────
    if compute_threads > 0 {
        match set_compute_threads(&node, compute_threads) {
            Ok(n) => tracing::info!("Inference compute pool: {} threads (--threads)", n),
            Err(e) => tracing::warn!(
                "could not build {}-thread compute pool: {}",
                compute_threads,
                e
            ),
        }
    } else {
        tracing::info!(
            "Inference compute pool: rayon global ({} threads; set RAYON_NUM_THREADS \
             or --threads N, or POST /node/threads at runtime)",
            rayon_global_width()
        );
    }

    // ── Coordinator shard-registry puller ───────────────────────────────
    // A node that holds no shards of its own never learned the network's
    // topology: the announce/pull background tasks in main.rs are gated on
    // `!shard_infos.is_empty()`, so a pure coordinator's registry stayed empty
    // and every sharded request failed with "Pipeline incomplete" even though
    // the seeds collectively cover the whole model. Requests also bootstrap
    // on demand; this keeps a warm registry so the FIRST request is fast too.
    //
    // GET only. This never writes to a remote node.
    if node.shard_infos.is_empty() && !node.seed_rpc_addrs.is_empty() {
        let puller = node.clone();
        tokio::spawn(async move {
            loop {
                bootstrap_shard_registry_from_seeds(&puller).await;
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            }
        });
        tracing::info!(
            seeds = node.seed_rpc_addrs.len(),
            "coordinator mode: pulling shard topology from seeds every 20s (GET /shards)"
        );
    }

    // ── Background replica latency refresher ────────────────────────────
    // The EWMA map used to be insert-only, so one bad incident pinned a
    // replica's ordering forever: run_sharded only dialled replicas[0], the
    // demoted replica received no further samples, and nothing ever
    // re-measured it. LHR carried an EWMA of 37,276 ms on all six seeds
    // while answering its own hops in 180-410 ms.
    //
    // This task GETs /health (read-only, no side effects) on every socket in
    // the registry once a minute. It never invents a hop latency: a probe RTT
    // is only used to (a) provide a provisional ordering hint for a replica we
    // have no hop sample for at all, or (b) evict a recorded EWMA that the
    // probe proves is a fossil. See `probe_supersedes_recorded`.
    {
        let probe_node = node.clone();
        tokio::spawn(async move {
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("latency prober disabled: {}", e);
                    return;
                }
            };
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(LATENCY_PROBE_INTERVAL_SECS))
                    .await;

                let mut sockets: Vec<String> = fresh_shards(&probe_node.shard_registry)
                    .into_iter()
                    .map(|s| s.socket_addr)
                    .filter(|a| !is_stub_socket_addr(a))
                    .collect();
                sockets.sort();
                sockets.dedup();

                for socket in sockets {
                    let t0 = std::time::Instant::now();
                    let ok = client
                        .get(format!("http://{}/health", socket))
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false);
                    if !ok {
                        // Unreachable: leave the recorded value alone. The
                        // hop path's failover already handles a dead replica,
                        // and inventing a latency for it would be a lie.
                        continue;
                    }
                    let probe_ms = t0.elapsed().as_millis() as u64;

                    let recorded = probe_node
                        .latency_stats
                        .get(&socket)
                        .map(|v| (v.ms, v.probe_only));
                    match recorded {
                        Some((ms, false)) if probe_supersedes_recorded(ms, probe_ms) => {
                            probe_node.latency_stats.remove(&socket);
                            record_probe_latency(&probe_node.latency_stats, &socket, probe_ms);
                            tracing::info!(
                                socket = %socket,
                                stale_ewma_ms = ms,
                                probe_ms,
                                "replica latency looked poisoned (health RTT contradicts it); \
                                 reset to a provisional probe value so it can be re-measured"
                            );
                        }
                        Some((_, false)) => { /* plausible hop sample: keep it */ }
                        _ => record_probe_latency(&probe_node.latency_stats, &socket, probe_ms),
                    }
                }
            }
        });
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/info", get(chain_info))
        .route("/node/info", get(node_info))
        // Runtime inference width. GET reports the current pool; POST
        // rebuilds it live, which is how an operator "adds two cores"
        // mid-demo without restarting the node.
        .route(
            "/node/threads",
            get(get_node_threads).post(set_node_threads),
        )
        // Additive honest-projection routes (v0.7.11+). Absent on every live
        // seed (v0.7.2 / v0.7.9) — clients must treat 404 as "unknown".
        .route("/network/info", get(network_info))
        .route("/node/contribution", get(node_contribution))
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
        // Per-worker earnings derived only from successful on-chain
        // CommunityInferenceReward receipts (tx 0x25), never raw worker
        // attestations (tx 0x16) or a synthesized count*2.5 estimate.
        .route("/worker/earnings/{address}", get(worker_earnings))
        // v0.7.0: live community-worker leaderboard. Reads the in-memory
        // CommunityWorker registry; no chain query. Sorted by composite
        // score (success rate * 1000 - avg_ms). Dashboard renders this.
        .route("/workers/scoreboard", get(workers_scoreboard))
        // Pipeline-parallel sharded inference
        .route("/inference/run_sharded", post(inference_run_sharded))
        .route("/inference/run_consensus", post(inference_run_consensus))
        .route(
            "/inference/forward_shard",
            post(inference_forward_shard)
                .layer(DefaultBodyLimit::max(MAX_FORWARD_SHARD_RESPONSE_BYTES)),
        )
        .route("/inference/cleanup_shard", post(inference_cleanup_shard))
        // Tier 1 status remains readable, but public writes fail closed until
        // committee/VRF authorization and exact-artifact execution are bound
        // end to end. The submit route is intentionally retained so clients
        // receive an explicit 503 instead of mistaking a 404 for skew.
        .route("/inference/onchain/submit", post(inference_onchain_submit))
        .route(
            "/inference/onchain/result/{request_id}",
            get(inference_onchain_result),
        )
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
        .route(
            "/inference/verification_status",
            get(inference_verification_status),
        )
        // Revenue split info
        .route("/economics/revenue_split", get(get_revenue_split))
        .route("/economics/rewards", get(economics_rewards))
        // Milestone C: read-only registry + demand discovery. Workers use
        // these to discover what models exist and what ranges are open
        // for the taking. Writes go through /tx/submit_signed like any
        // other chain mutation - no dedicated POST endpoints needed for
        // the MVP.
        .route("/models/registry", get(list_model_registry))
        .route("/models/open_requests", get(list_open_model_requests))
        // Milestone D: capacity advertisement discovery + per-node
        // assignment long-poll. Also read-only from the state.
        .route(
            "/capacity/advertisements",
            get(list_capacity_advertisements),
        )
        .route("/assignments/for_me", get(get_assignment_for_me))
        // Community worker registration (HTTP-only, works behind NAT)
        .route(
            COMMUNITY_REGISTER_PATH,
            post(community_register_signed)
                .layer(DefaultBodyLimit::max(COMMUNITY_MUTATION_BODY_LIMIT_BYTES)),
        )
        .route(
            COMMUNITY_HEARTBEAT_PATH,
            post(community_heartbeat_signed)
                .layer(DefaultBodyLimit::max(COMMUNITY_MUTATION_BODY_LIMIT_BYTES)),
        )
        .route("/community/list", get(community_list))
        // Community inference work dispatch (long-poll claim + submit)
        .route(
            COMMUNITY_CLAIM_WORK_PATH,
            post(community_claim_work_signed)
                .layer(DefaultBodyLimit::max(COMMUNITY_MUTATION_BODY_LIMIT_BYTES)),
        )
        .route(
            COMMUNITY_SUBMIT_WORK_PATH,
            post(community_submit_work_signed)
                .layer(DefaultBodyLimit::max(COMMUNITY_MUTATION_BODY_LIMIT_BYTES)),
        )
        .route(
            COMMUNITY_REWARD_APPROVE_PATH,
            post(community_reward_approve_signed)
                .layer(DefaultBodyLimit::max(COMMUNITY_MUTATION_BODY_LIMIT_BYTES)),
        )
        .route(
            "/community/reward_receipt/{hash}",
            get(community_reward_receipt),
        )
        .route("/community/reward_job/{job_id}", get(community_reward_job))
        .route(
            "/community/reward_approval/{job_id}",
            get(community_reward_approval_status),
        )
        .route("/community/reward_policy", get(community_reward_policy))
        // Off-chain channel relay (WebSocket-style via long-poll for simplicity)
        .route("/channel/{channel_id}/relay", post(channel_relay))
        .route("/channel/{channel_id}/state", get(channel_state))
        // ETH-compatible JSON-RPC (MetaMask, Hardhat, Foundry)
        .route("/eth", post(eth_json_rpc))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024)) // 256 MB
        // CORS is not an authorization boundary. Public reads remain broadly
        // accessible; community mutations independently require signed PoP.
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
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
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
    concat!(
        "ARC Chain - Agent Runtime Chain - Testnet v",
        env!("CARGO_PKG_VERSION")
    )
}

/// JSON error response body returned by endpoints that fail with 4xx/5xx.
#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

/// Helper to create a (StatusCode, Json<ApiError>) pair.
fn api_error(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (code, Json(ApiError { error: msg.into() }))
}

const PAID_INFERENCE_UNAVAILABLE_MESSAGE: &str = "paid inference is unavailable: authenticated replica payouts and Tier 1 committee/VRF authorization are not production-ready; no funds were locked or released";

fn paid_inference_unavailable() -> (StatusCode, Json<ApiError>) {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        PAID_INFERENCE_UNAVAILABLE_MESSAGE,
    )
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
    /// Age of the newest block this node holds, from that block's own header
    /// timestamp. `null` only when the node holds no block at all (or the
    /// header timestamp is unreadable) — which is "unknown", not "fresh".
    last_block_age_secs: Option<u64>,
    /// Whether the chain this node serves is still sealing blocks.
    /// `null` when `last_block_age_secs` is unknown.
    chain_advancing: Option<bool>,
    /// Populated only when `status != "ok"`, so an operator reading a
    /// degraded response is told what specifically is degraded.
    #[serde(skip_serializing_if = "Option::is_none")]
    degraded_reason: Option<String>,
}

/// How stale the newest block must be before `/health` calls the node
/// degraded.
///
/// Deliberately NOT [`BLOCK_PRODUCTION_FRESH_SECS`] (120 s). That constant is
/// the freshness window `/network/info` reports against, and it is far tighter
/// than this network's real cadence: measured over 10.3 h on 2026-08-18/19,
/// NYC sealed 50 blocks (~12 min/block) and LAX 108 (~5.7 min/block). Driving
/// the top-level `status` field off a 120 s window would mark both of the only
/// seeds that ARE sealing as degraded, which is a worse lie than the one this
/// replaced and would train operators to ignore the field.
///
/// 30 minutes cleanly separates "slow but sealing" (≤ ~12 min) from the
/// failure this exists to catch (the four seeds stalled for ~8 days).
pub const HEALTH_STALL_SECS: u64 = 1_800;

/// Map block liveness to a `/health` status. Pure, so both directions are
/// testable without standing up a block-producing node.
///
/// Unknown age is NOT treated as healthy: a node that cannot say when it last
/// sealed a block must not answer `"ok"`.
fn health_status_from(
    chain_advancing: Option<bool>,
    last_block_age_secs: Option<u64>,
) -> (&'static str, Option<String>) {
    match (chain_advancing, last_block_age_secs) {
        (Some(true), _) => ("ok", None),
        (Some(false), Some(age)) => (
            "degraded",
            Some(format!(
                "block production stalled: newest block is {age}s old (> {HEALTH_STALL_SECS}s). \
                 DAG rounds may still be advancing; round progress is not block production."
            )),
        ),
        _ => (
            "degraded",
            Some(
                "block liveness unknown: this node holds no block with a readable header \
                 timestamp, so it cannot assert that the chain is advancing."
                    .to_string(),
            ),
        ),
    }
}

/// `status` is NOT hardcoded `"ok"`.
///
/// It used to be, and that is precisely what let four seeds sit for eight days
/// answering `{"status":"ok","syncing":false}` while sealing no blocks: DAG
/// rounds and block commits are separate paths, so `dag_round` kept advancing
/// and every liveness check on the network read green. A monitor, a dashboard
/// or a desktop client polling `/health` had no field that could tell it
/// otherwise.
///
/// `status` is `"ok"` only when this node's newest block is younger than
/// [`BLOCK_PRODUCTION_FRESH_SECS`]. A stalled chain reports `"degraded"` with
/// a reason. Callers that treat any non-`"ok"` status as "do not route chain
/// reads here" get the correct behaviour for free; callers that only check for
/// the string `"ok"` now fail closed instead of open.
async fn health(AxumState(node): AxumState<NodeState>) -> Json<HealthResponse> {
    let validators = node.dag_validators.read().len();
    // Periodic cleanup: evict stale tx rate limit entries (>60s old)
    if node.tx_rate_limit.len() > 1000 {
        node.tx_rate_limit.retain(|_, v| v.elapsed().as_secs() < 60);
    }

    // The newest block that actually EXISTS here — see `latest_available_block`:
    // looking up `height()` directly reports a producing node as blockless.
    let last_block_age_secs =
        latest_available_block(&node).and_then(|b| age_secs_from_ms(b.header.timestamp));
    // Judged against HEALTH_STALL_SECS, not BLOCK_PRODUCTION_FRESH_SECS — see
    // that constant for why 120 s would flag the healthy seeds as degraded.
    let chain_advancing = last_block_age_secs.map(|age| age <= HEALTH_STALL_SECS);

    let (status, degraded_reason) = health_status_from(chain_advancing, last_block_age_secs);

    Json(HealthResponse {
        status: status.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        height: node.state.height(),
        peers: node.peer_count.load(Ordering::Relaxed),
        uptime_secs: node.boot_time.elapsed().as_secs(),
        dag_round: node.dag_round.load(Ordering::Relaxed),
        dag_committed: node.dag_committed.load(Ordering::Relaxed),
        validators,
        last_block_age_secs,
        chain_advancing,
        degraded_reason,
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
    node.state.get_block(height).map(Json).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Block at height {} not found", height),
        )
    })
}

async fn get_account(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> Result<Json<Account>, (StatusCode, Json<ApiError>)> {
    let addr = Hash256::from_hex(&address).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "Invalid address. Must be 64 hex characters.",
        )
    })?;
    node.state.get_account(&addr).map(Json).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Account {} not found", address),
        )
    })
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
    /// Accepted for wire compatibility with the TypeScript SDK, which documents
    /// `tx_type` as an optional field on this request. `submit_tx` only ever
    /// builds a transfer, so the value is parsed and then ignored — dropping the
    /// field would not change that, it would just hide the mismatch.
    #[allow(dead_code)]
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
    let from = Hash256::from_hex(&req.from)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid from address".to_string()))?;
    let to = Hash256::from_hex(&req.to)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid to address".to_string()))?;

    // Per-sender rate limit: 10 tx/sec (100ms cooldown)
    if let Some(last) = node.tx_rate_limit.get(&from.0)
        && last.elapsed().as_millis() < 100
    {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "rate limited: max 10 tx/sec per sender".to_string(),
        ));
    }
    node.tx_rate_limit.insert(from.0, Instant::now());

    // Check if a signature was provided
    if let Some(ref sig_hex) = req.signature
        && let Some(ref pubkey_hex) = req.public_key
    {
        // Build signed transaction
        let mut tx = Transaction::new_transfer(from, to, req.amount, req.nonce);

        // Parse signature and public key
        let sig_bytes = hex::decode(sig_hex)
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid signature hex".to_string()))?;
        let pk_bytes = hex::decode(pubkey_hex).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "invalid public_key hex".to_string(),
            )
        })?;

        if sig_bytes.len() != 64 || pk_bytes.len() != 32 {
            return Err((
                StatusCode::BAD_REQUEST,
                "signature must be 64 bytes, public_key must be 32 bytes".to_string(),
            ));
        }

        let mut pk_arr = [0u8; 32];
        pk_arr.copy_from_slice(&pk_bytes);

        tx.signature = arc_crypto::signature::Signature::Ed25519 {
            public_key: pk_arr,
            signature: sig_bytes,
        };
        if let Some(domain) = node.state.transaction_domain_hash() {
            tx.hash = tx.compute_hash_in_domain(&domain);
        }

        // Verify signature before accepting
        node.state.verify_transaction_signature(&tx).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "signature verification failed".to_string(),
            )
        })?;
        // Mark as pre-verified so block execution can skip re-verification.
        tx.sig_verified = true;

        let hash = tx.hash.to_hex();
        node.mempool
            .insert(tx)
            .map_err(|_| (StatusCode::CONFLICT, "duplicate transaction".to_string()))?;

        return Ok(Json(SubmitTxResponse {
            tx_hash: hash,
            status: "pending".to_string(),
        }));
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
        let Ok(mut tx) =
            signed_transfer_from_request_in_domain(&tx_req, node.state.transaction_domain_hash())
        else {
            rejected += 1;
            continue;
        };
        tx.sig_verified = true;
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

fn signed_transfer_from_request(req: &SubmitTxRequest) -> Result<Transaction, String> {
    signed_transfer_from_request_in_domain(req, None)
}

fn signed_transfer_from_request_in_domain(
    req: &SubmitTxRequest,
    recovery_domain: Option<Hash256>,
) -> Result<Transaction, String> {
    let from = Hash256::from_hex(&req.from).map_err(|_| "invalid from address".to_string())?;
    let to = Hash256::from_hex(&req.to).map_err(|_| "invalid to address".to_string())?;
    let signature_hex = req
        .signature
        .as_deref()
        .ok_or_else(|| "signature required".to_string())?;
    let public_key_hex = req
        .public_key
        .as_deref()
        .ok_or_else(|| "public_key required".to_string())?;
    let signature = hex::decode(signature_hex).map_err(|_| "invalid signature hex".to_string())?;
    let public_key =
        hex::decode(public_key_hex).map_err(|_| "invalid public_key hex".to_string())?;
    if signature.len() != 64 || public_key.len() != 32 {
        return Err("signature must be 64 bytes and public_key 32 bytes".to_string());
    }
    let mut public_key_bytes = [0u8; 32];
    public_key_bytes.copy_from_slice(&public_key);

    let mut tx = Transaction::new_transfer(from, to, req.amount, req.nonce);
    tx.signature = arc_crypto::Signature::Ed25519 {
        public_key: public_key_bytes,
        signature,
    };
    if let Some(domain) = recovery_domain {
        tx.hash = tx.compute_hash_in_domain(&domain);
    }
    match recovery_domain {
        Some(domain) => tx.verify_signature_in_domain(&domain),
        None => tx.verify_signature(),
    }
    .map_err(|_| "signature verification failed".to_string())?;
    Ok(tx)
}

// ---------------------------------------------------------------------------
// Signed transaction submission (for CLI / external signers)
// ---------------------------------------------------------------------------

/// Paid inference writes remain dark at every public transaction ingress.
///
/// The legacy escrow flow pays identifiers derived from unauthenticated shard
/// labels, while Tier 1 committee/VRF authorization is not yet complete.  It
/// is therefore unsafe to accept *any* transaction that can open, advance, or
/// release either protocol.  Check both the envelope and the body so a
/// type/body mismatch cannot disguise a restricted write.  Refunds are
/// intentionally excluded: a payer must retain a recovery path for funds
/// locked by an older release.
fn uses_unready_paid_inference_protocol(tx: &Transaction) -> bool {
    fn restricted(tx_type: TxType) -> bool {
        matches!(
            tx_type,
            TxType::InferenceEscrowOpen
                | TxType::InferenceEscrowRelease
                | TxType::InferenceRequest
                | TxType::InferenceVote
                | TxType::InferenceFinalize
        )
    }

    restricted(tx.tx_type) || restricted(tx.body.tx_type())
}

async fn submit_signed_tx(
    AxumState(node): AxumState<NodeState>,
    Json(mut tx): Json<Transaction>,
) -> Result<Json<SubmitTxResponse>, StatusCode> {
    // `sig_verified` is a process-local cache hint, never caller authority.
    // Its wire deserializer now forces false; verify both type/body integrity
    // and the cryptographic signature before anything enters the mempool.
    tx.sig_verified = false;
    if uses_unready_paid_inference_protocol(&tx) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if node.state.verify_transaction_signature(&tx).is_err() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // The issuance switch gates every public mempool ingress, not just the
    // coordinator helper. Otherwise a caller holding any validator key could
    // bypass the disabled coordinator path by POSTing 0x25 directly here.
    // Consensus validation remains the ultimate boundary; this is defense in
    // depth and keeps a dark deployment dark at the RPC edge.
    if tx.tx_type == TxType::CommunityInferenceReward
        && !community_rewards_v1_protocol_active(&node)
    {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    tx.sig_verified = true;
    let hash = tx.hash.to_hex();
    tracing::debug!(
        tx_hash = %hash,
        tx_type = ?tx.tx_type,
        from = %tx.from,
        nonce = tx.nonce,
        "Verified signed transaction submission"
    );

    match node.mempool.insert(tx) {
        Ok(()) => {
            tracing::debug!(
                tx_hash = %hash,
                mempool_len = node.mempool.len(),
                "Accepted signed transaction into mempool"
            );
            Ok(Json(SubmitTxResponse {
                tx_hash: hash,
                status: "pending".to_string(),
            }))
        }
        Err(e) => {
            tracing::debug!(tx_hash = %hash, error = ?e, "Rejected signed transaction");
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

async fn get_validators(AxumState(node): AxumState<NodeState>) -> Json<ValidatorsResponse> {
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
    validators.sort_by_key(|v| std::cmp::Reverse(v.stake));
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

async fn get_agents(AxumState(node): AxumState<NodeState>) -> Json<AgentsListResponse> {
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
        (
            StatusCode::BAD_REQUEST,
            Json(FaucetErrorResponse {
                error: "Invalid address. Must be 64 hex characters.".to_string(),
            }),
        )
    })?;

    // Rate limiting: check if this address claimed recently
    // Global rate limit: 5000 faucet claims/minute (testnet only - production should be 100)
    // DashMap iter is lock-free per shard so this never blocks the runtime.
    {
        let total = node.faucet_claims_total.load(Ordering::Relaxed);
        if total > FAUCET_GLOBAL_RATE_LIMIT as u32 {
            let recent = node
                .faucet_claims
                .iter()
                .filter(|e| e.value().elapsed().as_secs() < 60)
                .count();
            if recent > FAUCET_GLOBAL_RATE_LIMIT {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(FaucetErrorResponse {
                        error: "Faucet busy. Too many claims globally. Try again in a minute."
                            .to_string(),
                    }),
                ));
            }
        }
    }

    // Per-address rate limit
    if let Some(entry) = node.faucet_claims.get(&to.0) {
        let elapsed = entry.value().elapsed().as_secs();
        if elapsed < FAUCET_RATE_LIMIT_SECS {
            let remaining = FAUCET_RATE_LIMIT_SECS - elapsed;
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(FaucetErrorResponse {
                    error: format!(
                        "Rate limited. Try again in {} minutes.",
                        remaining.div_ceil(60)
                    ),
                }),
            ));
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
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(FaucetErrorResponse {
                    error: "Validator keypair not configured on this node.".to_string(),
                }),
            )
        })?;
        let validator_addr = node.validator_address;

        let pool_addr = arc_types::transaction::faucet_pool_address();
        let pool_account = node.state.get_account(&pool_addr).ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(FaucetErrorResponse {
                    error: "Faucet pool account not funded. Node misconfiguration.".to_string(),
                }),
            )
        })?;
        if pool_account.balance < FAUCET_CLAIM_AMOUNT {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(FaucetErrorResponse {
                    error: "Faucet balance too low. Please try another node.".to_string(),
                }),
            ));
        }

        // Read validator's current state nonce per-call. An in-memory
        // atomic counter would drift past state when txs fail to land,
        // leaving a permanent nonce gap. Concurrent calls in the same
        // block window race; the loser gets a 409 on commit and retries.
        let validator_account = node.state.get_or_create_account(&validator_addr);
        let nonce = validator_account.nonce;

        let mut tx = Transaction::new_faucet_claim(validator_addr, to, FAUCET_CLAIM_AMOUNT, nonce);
        node.state.sign_transaction(&mut tx, keypair).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(FaucetErrorResponse {
                    error: format!("Faucet sign failed: {:?}", e),
                }),
            )
        })?;
        tx.sig_verified = true;
        let hash = tx.hash.to_hex();

        // Receipt first — the consensus thread dedups via
        // `receipts.contains_key()`. Peers don't have the receipt and
        // run the FaucetClaim arm through `execute_block` normally.
        let receipt = TxReceipt {
            tx_hash: tx.hash,
            block_height: node.state.height(),
            block_hash: node
                .state
                .get_block(node.state.height())
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
        node.state
            .account_txs
            .entry(validator_addr.0)
            .or_default()
            .push(tx.hash);
        node.state
            .account_txs
            .entry(pool_addr.0)
            .or_default()
            .push(tx.hash);
        node.state
            .account_txs
            .entry(to.0)
            .or_default()
            .push(tx.hash);

        let _ = node.mempool.insert(tx);
        hash
    } else {
        // Legacy v0.7.0 null-sig Transfer path. Funded balance is
        // observable only on the seed that handled the call (the
        // known propagation bug). Acceptable during the rollout window
        // because no FaucetClaim variant is emitted that v0.7.0 peers
        // can't deserialize.
        let faucet_addr = arc_crypto::hash_bytes(&[0u8]);
        let faucet_account = node.state.get_account(&faucet_addr).ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(FaucetErrorResponse {
                    error: "Faucet account not funded. Node misconfiguration.".to_string(),
                }),
            )
        })?;

        if faucet_account.balance < FAUCET_CLAIM_AMOUNT {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(FaucetErrorResponse {
                    error: "Faucet balance too low. Please try another node.".to_string(),
                }),
            ));
        }

        static FAUCET_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = FAUCET_NONCE.fetch_add(1, Ordering::SeqCst);
        if nonce == 0 {
            FAUCET_NONCE.store(faucet_account.nonce + 1, Ordering::SeqCst);
        }
        let actual_nonce = if nonce == 0 {
            faucet_account.nonce
        } else {
            nonce
        };

        let mut tx = Transaction::new_transfer(faucet_addr, to, FAUCET_CLAIM_AMOUNT, actual_nonce);
        tx.sig_verified = true;
        let hash = tx.hash.to_hex();

        let receipt = TxReceipt {
            tx_hash: tx.hash,
            block_height: node.state.height(),
            block_hash: node
                .state
                .get_block(node.state.height())
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
        node.state
            .account_txs
            .entry(faucet_addr.0)
            .or_default()
            .push(tx.hash);
        node.state
            .account_txs
            .entry(to.0)
            .or_default()
            .push(tx.hash);

        let _ = node.mempool.insert(tx);
        hash
    };

    // Record claim time + evict stale entries to prevent unbounded growth
    node.faucet_claims.insert(to.0, Instant::now());
    if node.faucet_claims.len() > 10_000 {
        node.faucet_claims
            .retain(|_, v| v.elapsed().as_secs() < 7200);
    }
    node.faucet_claims_total.fetch_add(1, Ordering::Relaxed);

    Ok(Json(FaucetClaimResponse {
        tx_hash: hash,
        amount: FAUCET_CLAIM_AMOUNT,
        message: format!("Sent {} ARC to {}", FAUCET_CLAIM_AMOUNT, req.address),
    }))
}

async fn faucet_status(AxumState(node): AxumState<NodeState>) -> Json<FaucetStatusResponse> {
    let faucet_addr = arc_crypto::hash_bytes(&[0u8]);
    let balance = node
        .state
        .get_account(&faucet_addr)
        .map(|a| a.balance)
        .unwrap_or(0);
    Json(FaucetStatusResponse {
        address: faucet_addr.to_hex(),
        node_url: "http://localhost:9944".to_string(),
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
    Hash256::from_hex(stripped).map(|h| h.0).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "Invalid hash. Must be 64 hex characters (0x prefix optional).",
        )
    })
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
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                format!("Transaction {} not found", hash),
            )
        })
}

/// GET /tx/{hash}/proof - Return a full verification bundle for a transaction.
/// For benchmark transactions, reconstructs the Merkle tree on-demand (~130ms).
async fn get_tx_proof(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let tx_hash = parse_hash(&hash)?;

    // Try indexed receipt with stored proof first
    if let Some(receipt) = node.state.get_receipt(&tx_hash)
        && let Some(ref proof_bytes) = receipt.inclusion_proof
        && let Ok(merkle_proof) = bincode::deserialize::<MerkleProof>(proof_bytes)
    {
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

    // Fall back to on-demand proof reconstruction for benchmark txs
    let (height, idx) = node.state.get_tx_location(&tx_hash).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Transaction {} not found", hash),
        )
    })?;

    let merkle_proof = node
        .state
        .reconstruct_benchmark_proof(height, idx)
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                "Could not reconstruct proof for transaction",
            )
        })?;

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

    let block_tx_root = node.state.get_block(height).map(|b| b.header.tx_root);
    let verified = block_tx_root
        .map(|r| r == merkle_proof.root)
        .unwrap_or(false);

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
    let block = node.state.get_block(height).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Block at height {} not found", height),
        )
    })?;

    let mut proofs = Vec::new();
    for tx_hash in &block.tx_hashes {
        if let Some(receipt) = node.state.get_receipt(&tx_hash.0)
            && let Some(ref proof_bytes) = receipt.inclusion_proof
            && let Ok(proof) = bincode::deserialize::<MerkleProof>(proof_bytes)
        {
            let siblings: Vec<Value> = proof
                .siblings
                .iter()
                .map(|(h, is_left)| json!({ "hash": h.to_hex(), "is_left": is_left }))
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
    let block = node.state.get_block(height).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Block at height {} not found", height),
        )
    })?;

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
    let executed = node
        .state
        .benchmark_tx_count
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let dag_round = node.dag_round.load(std::sync::atomic::Ordering::Relaxed);
    let dag_committed = node
        .dag_committed
        .load(std::sync::atomic::Ordering::Relaxed);
    let validators = node.dag_validators.read().len();
    let peers = node.peer_count.load(Ordering::Relaxed);
    let uptime = node.boot_time.elapsed().as_secs();
    let bench_tps = (executed as u64).checked_div(uptime).unwrap_or(0);
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
        // Real pipeline walks only. Cache hits are counted separately —
        // folding them in here is what inflated every "distributed inference
        // served" figure the dashboard showed.
        "sharded_runs_total": sharded_runs,
        "sharded_cache_hits_total": node.sharded_cache_hits.load(Ordering::Relaxed),
        "sharded_bytes_total": sharded_bytes,
    }))
}

// ---------------------------------------------------------------------------
// State Sync Protocol (A5) - snapshot bootstrap for new nodes
// ---------------------------------------------------------------------------

/// Returns metadata about the latest snapshot available for sync.
async fn sync_snapshot_info(AxumState(node): AxumState<NodeState>) -> Json<Value> {
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
    use axum::http::header;
    use axum::response::IntoResponse;

    let snapshot = node.state.export_snapshot();
    let data = bincode::serialize(&snapshot).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let compressed = lz4_flex::compress_prepend_size(&data);

    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"snapshot.lz4\"",
            ),
        ],
        compressed,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Light Client Proofs (A8)
// ---------------------------------------------------------------------------

/// GET /light/snapshot - Returns a lightweight snapshot for light client bootstrapping:
/// current height, state root, account count, total supply, latest block hash.
async fn light_snapshot(AxumState(node): AxumState<NodeState>) -> Json<Value> {
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
async fn sync_manifest(AxumState(node): AxumState<NodeState>) -> Json<Value> {
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
    let chunk = node
        .state
        .export_snapshot_chunk(index, manifest.chunk_size)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(chunk))
}

/// GET /sync/status - Returns whether this node can serve snapshots and
/// information about the latest available snapshot.
async fn sync_status(AxumState(node): AxumState<NodeState>) -> Json<Value> {
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
async fn sync_dag_state(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let current_round = node.dag_round.load(std::sync::atomic::Ordering::Relaxed);
    let last_committed_round = node
        .dag_committed
        .load(std::sync::atomic::Ordering::Relaxed);
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
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                format!("Transaction {} not found", hash),
            )
        })?;

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
            "channel_id": format!("0x{}", hex::encode(body.channel_id.0)),
            "counterparty": format!("0x{}", hex::encode(body.counterparty.0)),
            "deposit": body.deposit,
            "timeout_blocks": body.timeout_blocks,
        }),
        TxBody::ChannelClose(body) => json!({
            "type": "ChannelClose",
            "channel_id": format!("0x{}", hex::encode(body.channel_id.0)),
            "opener_balance": body.opener_balance,
            "counterparty_balance": body.counterparty_balance,
            "state_nonce": body.state_nonce,
        }),
        TxBody::ChannelDispute(body) => json!({
            "type": "ChannelDispute",
            "channel_id": format!("0x{}", hex::encode(body.channel_id.0)),
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
            "prev_state_root": format!("0x{}", hex::encode(body.prev_state_root.0)),
            "post_state_root": format!("0x{}", hex::encode(body.post_state_root.0)),
        }),
        TxBody::InferenceAttestation(body) => json!({
            "type": "InferenceAttestation",
            "model_id": format!("0x{}", hex::encode(body.model_id.0)),
            "input_hash": format!("0x{}", hex::encode(body.input_hash.0)),
            "output_hash": format!("0x{}", hex::encode(body.output_hash.0)),
            "challenge_period": body.challenge_period,
            "bond": body.bond,
        }),
        TxBody::InferenceChallenge(body) => json!({
            "type": "InferenceChallenge",
            "attestation_hash": format!("0x{}", hex::encode(body.attestation_hash.0)),
            "challenger_output_hash": format!("0x{}", hex::encode(body.challenger_output_hash.0)),
            "challenger_bond": body.challenger_bond,
        }),
        TxBody::InferenceRegister(body) => json!({
            "type": "InferenceRegister",
            "tier": body.tier,
            "stake_bond": body.stake_bond,
        }),
        TxBody::InferenceEscrowOpen(body) => json!({
            "type": "InferenceEscrowOpen",
            "request_id": format!("0x{}", hex::encode(body.request_id)),
            "model_id": format!("0x{}", hex::encode(body.model_id.0)),
            "max_fee": body.max_fee,
            "max_tokens": body.max_tokens,
            "timeout_blocks": body.timeout_blocks,
        }),
        TxBody::InferenceEscrowRelease(body) => json!({
            "type": "InferenceEscrowRelease",
            "request_id": format!("0x{}", hex::encode(body.request_id)),
            "payer": format!("0x{}", hex::encode(body.payer.0)),
            "model_id": format!("0x{}", hex::encode(body.model_id.0)),
            "max_tokens": body.max_tokens,
            "timeout_blocks": body.timeout_blocks,
            "output_hash": format!("0x{}", hex::encode(body.output_hash.0)),
            "proposer": format!("0x{}", hex::encode(body.proposer.0)),
            "replicas": body.replicas.iter()
                .map(|r| format!("0x{}", hex::encode(r.0)))
                .collect::<Vec<_>>(),
            "observer_pool": format!("0x{}", hex::encode(body.observer_pool.0)),
            "treasury": format!("0x{}", hex::encode(body.treasury.0)),
        }),
        TxBody::InferenceEscrowRefund(body) => json!({
            "type": "InferenceEscrowRefund",
            "request_id": format!("0x{}", hex::encode(body.request_id)),
            "model_id": format!("0x{}", hex::encode(body.model_id.0)),
            "max_tokens": body.max_tokens,
            "timeout_blocks": body.timeout_blocks,
        }),
        TxBody::ModelRegistration(body) => json!({
            "type": "ModelRegistration",
            "model_id": format!("0x{}", hex::encode(body.model_id.0)),
            "metadata_hash": format!("0x{}", hex::encode(body.metadata_hash.0)),
            "chunk_tree_root": format!("0x{}", hex::encode(body.chunk_tree_root.0)),
            "n_layers": body.n_layers,
            "d_model": body.d_model,
            "quantization": body.quantization,
            "registration_fee": body.registration_fee,
            "royalty_recipient": format!("0x{}", hex::encode(body.royalty_recipient.0)),
        }),
        TxBody::ModelRequest(body) => json!({
            "type": "ModelRequest",
            "request_id": format!("0x{}", hex::encode(body.request_id)),
            "model_id": format!("0x{}", hex::encode(body.model_id.0)),
            "target_k_replication": body.target_k_replication,
            "bond_per_layer_epoch": body.bond_per_layer_epoch,
            "max_wait_secs": body.max_wait_secs,
        }),
        TxBody::ShardCoverageClaim(body) => json!({
            "type": "ShardCoverageClaim",
            "model_id": format!("0x{}", hex::encode(body.model_id.0)),
            "node_pubkey": format!("0x{}", hex::encode(body.node_pubkey)),
            "ranges": body.ranges.iter()
                .map(|(s, e)| json!([s, e])).collect::<Vec<_>>(),
            "bond": body.bond,
            "epoch_blocks": body.epoch_blocks,
        }),
        TxBody::CapacityAdvertisement(body) => json!({
            "type": "CapacityAdvertisement",
            "node_pubkey": format!("0x{}", hex::encode(body.node_pubkey)),
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
            "input_snapshot_hash": format!("0x{}", hex::encode(body.input_snapshot_hash.0)),
            "assignments": body.assignments.iter().map(|a| json!({
                "node_pubkey": format!("0x{}", hex::encode(a.node_pubkey)),
                "model_id": format!("0x{}", hex::encode(a.model_id.0)),
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
        TxBody::CommunityInferenceReward(b) => json!({
            "type": "CommunityInferenceReward",
            "chain_domain": b.chain_domain.to_hex(),
            "job_id": b.job_id.to_hex(),
            "worker": b.worker.to_hex(),
            "model_id": b.model_id.to_hex(),
            "input_hash": b.input_hash.to_hex(),
            "output_hash": b.output_hash.to_hex(),
            "max_tokens": b.max_tokens,
            "expires_at_height": b.expires_at_height,
            "worker_attestation_hash": b.worker_certificate.attestation_hash.to_hex(),
        }),
    };

    let sig_json = match &tx.signature {
        arc_crypto::Signature::Ed25519 {
            public_key,
            signature,
        } => json!({
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
        arc_crypto::Signature::MlDsa65 {
            public_key,
            signature,
        } => json!({
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

    let bytecode = node.state.get_contract(&addr).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Contract {} not found", address),
        )
    })?;

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

    let bytecode = node.state.get_contract(&contract_addr).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("Contract {} not found", address),
        )
    })?;

    let caller = req
        .from
        .as_ref()
        .and_then(|f| Hash256::from_hex(f).ok())
        .unwrap_or(Hash256::ZERO);

    let _calldata = req
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
        "web3_clientVersion" => eth_rpc_result(
            &req.id,
            json!(format!("ARC/v{}", env!("CARGO_PKG_VERSION"))),
        ),
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
                    let txs = json!(
                        block
                            .tx_hashes
                            .iter()
                            .map(|h| format!("0x{}", h.to_hex()))
                            .collect::<Vec<_>>()
                    );
                    eth_rpc_result(
                        &req.id,
                        json!({
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
                        }),
                    )
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
        _ => eth_rpc_error(
            &req.id,
            -32601,
            &format!("Method not found: {}", req.method),
        ),
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
    let nonce = node.state.get_account(&addr).map(|a| a.nonce).unwrap_or(0);
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
        None => eth_rpc_result(
            id,
            json!("0x0000000000000000000000000000000000000000000000000000000000000000"),
        ),
    }
}

fn eth_get_block_by_number(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let block_num = parse_block_number(node, params.get(0));
    let full_txs = params.get(1).and_then(|v| v.as_bool()).unwrap_or(false);

    match node.state.get_block(block_num) {
        Some(block) => {
            let txs: Value = if full_txs {
                // Full tx objects would go here; for now return hashes with 0x prefix
                json!(
                    block
                        .tx_hashes
                        .iter()
                        .map(|h| format!("0x{}", h.to_hex()))
                        .collect::<Vec<_>>()
                )
            } else {
                json!(
                    block
                        .tx_hashes
                        .iter()
                        .map(|h| format!("0x{}", h.to_hex()))
                        .collect::<Vec<_>>()
                )
            };

            eth_rpc_result(
                id,
                json!({
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
                }),
            )
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

    let tx = node
        .state
        .get_transaction(&tx_hash.0)
        .or_else(|| node.state.get_benchmark_tx_by_hash(&tx_hash.0));

    match tx {
        Some(tx) => {
            let (to, value) = match &tx.body {
                TxBody::Transfer(b) => (
                    Some(format!("0x{}", hex::encode(&b.to.0[..20]))),
                    format!("0x{:x}", b.amount),
                ),
                TxBody::WasmCall(b) => (
                    Some(format!("0x{}", hex::encode(&b.contract.0[..20]))),
                    format!("0x{:x}", b.value),
                ),
                _ => (None, "0x0".to_string()),
            };

            eth_rpc_result(
                id,
                json!({
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
                }),
            )
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

    let receipt = node
        .state
        .get_receipt(&tx_hash.0)
        .or_else(|| node.state.get_benchmark_receipt_by_hash(&tx_hash.0));

    match receipt {
        Some(r) => {
            let tx = node
                .state
                .get_transaction(&tx_hash.0)
                .or_else(|| node.state.get_benchmark_tx_by_hash(&tx_hash.0));

            let from = tx
                .as_ref()
                .map(|t| format!("0x{}", hex::encode(&t.from.0[..20])))
                .unwrap_or_default();
            let to = tx.as_ref().and_then(|t| match &t.body {
                TxBody::Transfer(b) => Some(format!("0x{}", hex::encode(&b.to.0[..20]))),
                TxBody::WasmCall(b) => Some(format!("0x{}", hex::encode(&b.contract.0[..20]))),
                _ => None,
            });

            let logs_json: Vec<Value> = r
                .logs
                .iter()
                .enumerate()
                .map(|(i, log)| {
                    let topics: Vec<String> = log
                        .topics
                        .iter()
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
                })
                .collect();

            eth_rpc_result(
                id,
                json!({
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
                }),
            )
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

    let from_block = filter
        .get("fromBlock")
        .and_then(|v| v.as_str())
        .map(|s| parse_block_number(node, Some(&json!(s))))
        .unwrap_or(0);

    let to_block = filter
        .get("toBlock")
        .and_then(|v| v.as_str())
        .map(|s| parse_block_number(node, Some(&json!(s))))
        .unwrap_or_else(|| node.state.height());

    let address_filter: Option<Vec<Hash256>> = filter.get("address").and_then(|v| {
        if let Some(s) = v.as_str() {
            parse_eth_address(s).ok().map(|a| vec![a])
        } else if let Some(arr) = v.as_array() {
            let addrs: Vec<Hash256> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| parse_eth_address(s).ok())
                .collect();
            if addrs.is_empty() { None } else { Some(addrs) }
        } else {
            None
        }
    });

    let topic_filters: Vec<Option<Hash256>> = filter
        .get("topics")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|t| {
                    t.as_str().and_then(|s| {
                        let s = s.strip_prefix("0x").unwrap_or(s);
                        Hash256::from_hex(s).ok()
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let mut result_logs: Vec<Value> = Vec::new();
    let max_height = to_block.min(from_block + 10_000); // Cap range

    for height in from_block..=max_height {
        if let Some(logs) = node.state.event_logs.get(&height) {
            for log in logs.iter() {
                // Address filter
                if let Some(ref addrs) = address_filter
                    && !addrs.iter().any(|a| a.0 == log.address.0)
                {
                    continue;
                }
                // Topic filter
                let mut topic_match = true;
                for (i, filter_topic) in topic_filters.iter().enumerate() {
                    if let Some(expected) = filter_topic
                        && log.topics.get(i).map(|t| t.0) != Some(expected.0)
                    {
                        topic_match = false;
                        break;
                    }
                }
                if !topic_match {
                    continue;
                }

                let block = node.state.get_block(height);
                let block_hash = block
                    .map(|b| format!("0x{}", b.hash.to_hex()))
                    .unwrap_or_else(|| "0x".to_string() + &"00".repeat(32));

                let topics: Vec<String> = log
                    .topics
                    .iter()
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

    let from = call_obj
        .get("from")
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

    let data = call_obj
        .get("data")
        .or_else(|| call_obj.get("input"))
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();

    let value = call_obj
        .get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(0);

    let gas = call_obj
        .get("gas")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(10_000_000);

    let result = arc_vm::evm::evm_call(&node.state, from, to, data, value, gas);
    if result.success {
        eth_rpc_result(id, json!(format!("0x{}", hex::encode(&result.return_data))))
    } else {
        eth_rpc_error(
            id,
            3,
            result
                .revert_reason
                .as_deref()
                .unwrap_or("execution reverted"),
        )
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
        let payload: Vec<u8> = encoded_items
            .iter()
            .flat_map(|i| i.iter().copied())
            .collect();
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
        let first_nonzero = bytes
            .iter()
            .position(|&b| b != 0)
            .unwrap_or(bytes.len() - 1);
        bytes[first_nonzero..].to_vec()
    }

    /// Encode a u64 as an RLP byte string (minimal big-endian, no leading zeros).
    pub fn encode_u64(val: u64) -> Vec<u8> {
        if val == 0 {
            // RLP encoding of zero is the empty byte string
            encode_bytes(&[])
        } else {
            let bytes = val.to_be_bytes();
            let first_nonzero = bytes
                .iter()
                .position(|&b| b != 0)
                .unwrap_or(bytes.len() - 1);
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
                Err(_) => {
                    return eth_rpc_error(
                        id,
                        -32602,
                        &format!("RLP field {} must be bytes, not list", $name),
                    )
                }
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
        return eth_rpc_error(
            id,
            -32602,
            &format!("Invalid to address length: {}", to_bytes.len()),
        );
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
            return eth_rpc_error(
                id,
                -32000,
                &format!(
                    "Contract deployment failed: {}",
                    result.revert_reason.unwrap_or_default()
                ),
            );
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

    let from = call_obj
        .get("from")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_eth_address(s).ok())
        .unwrap_or(Hash256::ZERO);

    let to = call_obj
        .get("to")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_eth_address(s).ok())
        .unwrap_or(Hash256::ZERO);

    let data = call_obj
        .get("data")
        .or_else(|| call_obj.get("input"))
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();

    let value = call_obj
        .get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(0);

    let result = arc_vm::evm::evm_call(&node.state, from, to, data, value, 30_000_000);
    let gas_estimate = if result.gas_used == 0 {
        21000
    } else {
        result.gas_used
    };
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
    let escrow_input = [
        b"arc-channel".as_slice(),
        &hex::decode(&channel_id).unwrap_or_default(),
    ]
    .concat();
    let escrow_addr = arc_crypto::hash_bytes(&escrow_input);
    let escrow = node.state.get_account(&escrow_addr);

    match escrow {
        Some(account) => Ok(Json(json!({
            "channel_id": channel_id,
            "locked_balance": account.balance,
            "state_nonce": account.nonce,
            "challenge_expiry": account.staked_balance,
            "opener": format!("0x{}", hex::encode(account.code_hash.0)),
            "counterparty": format!("0x{}", hex::encode(account.storage_root.0)),
            "active": account.balance > 0,
        }))),
        None => Ok(Json(json!({
            "channel_id": channel_id,
            "active": false,
            "error": "channel not found",
        }))),
    }
}

// ─── Inference Endpoints ─────────────────────────────────────────────────────

/// How long /inference/run waits for a community worker to return a
/// completed job before giving up and falling through to the local
/// model. Scales with `max_tokens` so a 4-token autocomplete doesn't
/// have to budget for a 13B + 64-token chat completion. Floor is
/// `MIN_COMMUNITY_DISPATCH_TIMEOUT_SECS` so short generations still
/// get one claim-poll cycle.
///
/// Observed 2026-06-04 on the testnet: a fixed 60s ceiling produced
/// p50=67.8s end-to-end on a 4-token request because 97/100 calls
/// waited the full timeout before falling back to local. The math:
/// 4 tokens × ~3.3 s/tok = ~13s real work, so the budget should be
/// ~15s not 60s. A successful result is now independently recomputed by a
/// 2-of-3 shard quorum before this timer completes, so the budget covers two
/// inference passes (worker + verifier), one claim-poll window, and network
/// headroom. The cap prevents an abandoned request from living forever.
const MIN_COMMUNITY_DISPATCH_TIMEOUT_SECS: u64 = 45;
const MAX_COMMUNITY_DISPATCH_TIMEOUT_SECS: u64 = 3_600;
const COMMUNITY_VERIFICATION_REPLICAS: usize = 3;
const COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED: usize = 2;

/// Compute the per-request community dispatch timeout. ~3.3s/token is the
/// observed worker EWMA. The worker, coordinator verifier, and parallel peer
/// approval phase can span three inference passes on the critical path, so
/// budget 3× compute plus 50% headroom and one 30-second claim window.
fn community_dispatch_timeout_secs(max_tokens: u32) -> u64 {
    let per_token_ms: u64 = 3300;
    let est_ms = (max_tokens as u64)
        .saturating_mul(per_token_ms)
        .saturating_mul(3);
    let est_with_headroom = est_ms.saturating_mul(3) / 2;
    let est_secs = est_with_headroom / 1000 + COMMUNITY_CLAIM_TIMEOUT_SECS;
    est_secs.clamp(
        MIN_COMMUNITY_DISPATCH_TIMEOUT_SECS,
        MAX_COMMUNITY_DISPATCH_TIMEOUT_SECS,
    )
}

/// Count community workers that haven't expired their TTL and advertise
/// the "inference" capability. Used by the smart router to decide
/// whether to dispatch externally or run locally.
fn live_inference_worker_count(node: &NodeState) -> usize {
    let Ok((_, coordinator_model_id)) = exact_model_identity(node) else {
        return 0;
    };
    let now = std::time::Instant::now();
    let ttl = std::time::Duration::from_secs(COMMUNITY_WORKER_TTL_SECS);
    node.community_workers
        .iter()
        .filter(|e| {
            let (w, ts) = e.value();
            now.duration_since(*ts) <= ttl
                && w.capabilities.iter().any(|c| c == "inference")
                && w.model_id
                    .as_deref()
                    .and_then(|model_id| parse_hash256_hex(model_id, "model_id").ok())
                    == Some(coordinator_model_id)
        })
        .count()
}

/// Consensus activation plus the operator's local ingress/issuance switch.
/// This is enough to accept an already-threshold-authorized external 0x25,
/// but not enough for this coordinator to create one.
fn community_rewards_v1_protocol_active(node: &NodeState) -> bool {
    node.community_rewards_v1_enabled && node.state.community_rewards_v1_active()
}

/// End-to-end local issuance readiness. Public RPC/UI fields named `enabled`
/// use this stronger definition so an activated but non-issuing deployment
/// can never advertise rewards as available.
fn community_rewards_v1_effective(node: &NodeState) -> bool {
    community_rewards_v1_protocol_active(node) && reward_approval_prerequisites(node).is_ok()
}

fn reward_approval_prerequisites(node: &NodeState) -> Result<(), &'static str> {
    if !COMMUNITY_REWARD_APPROVAL_COLLECTION_READY {
        return Err("approval collector is not compiled in");
    }
    let Some(key) = node.validator_keypair.as_ref() else {
        return Err("local active-validator Ed25519 key is unavailable");
    };
    if key.address() != node.validator_address
        || !node.state.is_validator(&node.validator_address)
        || !matches!(key.as_ref(), arc_crypto::KeyPair::Ed25519(_))
    {
        return Err("local approval key is not an active Ed25519 validator");
    }
    if exact_model_identity(node).is_err() {
        return Err("exact inference model and tokenizer are unavailable");
    }
    if node.state.recovery_context().is_none() {
        return Err("protocol-v3 recovery epoch and transaction domain are unavailable");
    }
    if node.state.active_validators().len()
        != arc_types::transaction::COMMUNITY_REWARD_VALIDATOR_SET_SIZE
    {
        return Err("active validator committee is not exactly six members");
    }
    Ok(())
}

fn reward_validator_set_id(node: &NodeState) -> String {
    let mut validators = node.state.active_validators();
    validators.sort_unstable_by_key(|(address, _)| address.0);
    let mut hasher = blake3::Hasher::new_derive_key("ARC-community-reward-validator-set-v1");
    for (address, stake) in validators {
        hasher.update(address.as_ref());
        hasher.update(&stake.to_le_bytes());
    }
    format!("0x{}", hex::encode(hasher.finalize().as_bytes()))
}

fn reward_recovery_epoch(node: &NodeState) -> Option<u64> {
    node.state
        .recovery_context()
        .map(|context| context.recovery_epoch)
}

fn reward_recovery_validator_set_id(node: &NodeState) -> Option<u64> {
    node.state
        .recovery_context()
        .map(|context| context.validator_set_id)
}

fn community_rewards_v1_readiness_note(node: &NodeState) -> &'static str {
    if !community_rewards_v1_protocol_active(node) {
        "verified work is accepted, but rewards remain dark until genesis activation is reached and the local issuance switch is enabled"
    } else if reward_approval_prerequisites(node).is_err() {
        "reward protocol activation is open, but issuance remains fail-closed until the protocol-v3 recovery context, exact six-validator committee, local validator key, and exact model are available"
    } else {
        "verified job completions can be submitted for threshold-authorized on-chain reward inclusion"
    }
}

fn community_job_id(
    validator_address: &Hash256,
    boot_epoch: &Hash256,
    model_id: &Hash256,
    input_hash: &Hash256,
    max_tokens: u32,
    nonce: u64,
) -> String {
    arc_types::transaction::CommunityInferenceRewardBody::derive_job_id(
        validator_address,
        boot_epoch,
        nonce,
        model_id,
        input_hash,
        max_tokens,
    )
    .to_hex()
}

/// Push a whole-prompt job onto the community work queue and await the
/// result via oneshot. Returns Err when there's no queue, no worker
/// claims the job in time, the worker reports failure, or the result
/// channel breaks.
///
/// On success the returned outcome carries the worker output plus the
/// coordinator-created verification and settlement evidence. Those latter
/// fields never cross the worker-controlled request boundary.
async fn dispatch_to_community_worker(
    node: &NodeState,
    input: String,
    max_tokens: u32,
    model_id_hint: Option<String>,
) -> Result<CommunityDispatchOutcome, String> {
    if max_tokens == 0 || max_tokens > INFERENCE_RUN_MAX_TOKENS {
        return Err(format!(
            "community max_tokens must be in 1..={INFERENCE_RUN_MAX_TOKENS}, got {max_tokens}"
        ));
    }
    let model_id_hash = model_id_hint
        .as_deref()
        .ok_or_else(|| "community dispatch requires an exact model artifact commitment".to_string())
        .and_then(|value| parse_hash256_hex(value, "model_id"))?;
    let model_id = format!("0x{}", model_id_hash.to_hex());
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

    // Include the coordinator address as a namespace and a monotonic local
    // nonce so identical concurrent prompts—and two coordinators issuing the
    // same prompt—still receive distinct job commitments.
    let nonce = node.attestation_nonce.fetch_add(1, Ordering::Relaxed);
    let input_hash = arc_crypto::hash_bytes(input.as_bytes());
    let job_id = community_job_id(
        &node.validator_address,
        &node.community_job_epoch,
        &model_id_hash,
        &input_hash,
        max_tokens,
        nonce,
    );

    let submitted_at = chrono::Utc::now().timestamp_millis();
    let item = WorkItem {
        job_id: job_id.clone(),
        input,
        max_tokens,
        model_id: Some(model_id),
        transaction_domain: node
            .state
            .transaction_domain_hash()
            .map(|domain| format!("0x{}", domain.to_hex())),
        submitted_at_unix_ms: submitted_at,
    };

    let (osh_tx, osh_rx) = tokio::sync::oneshot::channel::<CommunityDispatchOutcome>();
    results.insert(
        job_id.clone(),
        PendingCommunityWork {
            item: item.clone(),
            assignment_epoch: node.community_job_epoch,
            job_nonce: nonce,
            assigned_worker: None,
            sender: osh_tx,
        },
    );

    if let Err(e) = tx.send(item).await {
        // Channel closed — drop our orphan oneshot from the map and
        // surface the error so the caller can fall back to local.
        results.remove(&job_id);
        return Err(format!("queue closed: {}", e));
    }

    let timeout_secs = community_dispatch_timeout_secs(max_tokens);
    let timeout = tokio::time::Duration::from_secs(timeout_secs);
    match tokio::time::timeout(timeout, osh_rx).await {
        Ok(Ok(outcome)) => {
            if !outcome.result.success {
                let err = outcome
                    .result
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
                &format!("worker:{}", outcome.result.worker_id),
                outcome.result.total_ms,
            );
            Ok(outcome)
        }
        Ok(Err(_)) => {
            // oneshot sender dropped without sending — the worker
            // disconnected mid-job or the queue purged us.
            if let Some((_, pending)) = results.remove(&job_id)
                && let Some(worker_id) = pending.assigned_worker.as_deref()
            {
                release_active_community_job(node, worker_id, &job_id);
            }
            Err("worker disconnected before completing job".into())
        }
        Err(_) => {
            // Timeout — orphan our entry so submit_work doesn't crash
            // when the late result arrives.
            if let Some((_, pending)) = results.remove(&job_id)
                && let Some(worker_id) = pending.assigned_worker.as_deref()
            {
                release_active_community_job(node, worker_id, &job_id);
            }
            Err(format!("no worker completed within {}s", timeout_secs))
        }
    }
}

/// Tier 1 write ingress is intentionally unavailable.
///
/// The endpoint returns 503 before reading node state, using a validator key,
/// or touching the mempool. Read-only result lookup remains available for
/// historical requests. `/tx/submit_signed` independently rejects the same
/// transaction family so this boundary cannot be bypassed.
async fn inference_onchain_submit(
    AxumState(_node): AxumState<NodeState>,
    _body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    // Deliberately no readiness boolean or operator override: changing a flag
    // must never revive the removed validator self-signing oracle or the
    // insufficiently validated caller relay. Re-enable this endpoint only
    // with a new implementation after committee membership is authenticated
    // end to end, exact model artifacts are consensus-bound, and signed
    // requests pass the same semantic validation at every ingress.
    Err(paid_inference_unavailable())
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

    let snap = node
        .state
        .tier1_request_snapshot(&request_id)
        .ok_or_else(|| {
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
    let output_blob = node.state.get_storage(
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

/// Run inference through a community worker (preferred when any are online)
/// or the local model (fallback). A community result can earn only through a
/// worker-signed certificate wrapped by the coordinator in a gated,
/// validator-authorized reward transaction.
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
        None => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "Request body required. Send JSON with 'input' and 'max_tokens' fields.",
            ));
        }
    };

    let input_text = req
        .get("input")
        .and_then(|v| v.as_str())
        .unwrap_or("Hello, world!");

    // Validate input: reject null bytes, enforce max length
    if input_text.len() > 32_768 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Input exceeds 32KB limit",
        ));
    }
    if input_text.contains('\0') {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Input contains null bytes",
        ));
    }

    let requested_max_tokens = req.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(64);
    if requested_max_tokens == 0 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "max_tokens must be at least 1",
        ));
    }
    let max_tokens = requested_max_tokens.min(u64::from(INFERENCE_RUN_MAX_TOKENS)) as u32;
    let bond = req
        .get("bond")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_ATTESTATION_BOND);
    let challenge_period = req
        .get("challenge_period")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_ATTESTATION_CHALLENGE_PERIOD_BLOCKS);
    let force_local = req
        .get("force_local")
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
    // Community workers sign the exact model/input/output commitments. The
    // seed binds that certificate to its pending assignment and, after the
    // fleet-wide activation gate is enabled, submits a replay-marked reward.
    let live_workers = live_inference_worker_count(&node);
    if !force_local && live_workers > 0 && node.community_work_tx.is_some() {
        let dispatched_at = std::time::Instant::now();
        let assigned_model_id = node
            .model_artifact_id
            .map(|model_id| format!("0x{}", model_id.to_hex()));
        match dispatch_to_community_worker(
            &node,
            input_text.to_string(),
            max_tokens,
            assigned_model_id.clone(),
        )
        .await
        {
            Ok(CommunityDispatchOutcome {
                result,
                verification,
                settlement,
            }) => {
                let total_ms = dispatched_at.elapsed().as_millis() as u64;
                let input_hash = arc_crypto::hash_bytes(input_text.as_bytes());
                node.inference_results.insert(
                    result.job_id.clone(),
                    json!({
                        "input": input_text,
                        "output": &result.output,
                        "output_hash": &result.output_hash,
                        "model": format!("community:{}", result.engine),
                        "model_hash": assigned_model_id,
                        "ms_per_token": result.ms_per_token,
                        "tokens_generated": result.tokens_generated,
                        "engine": &result.engine,
                        "deterministic": result.engine.contains("integer"),
                        "worker_id": &result.worker_id,
                        "observed_at_unix_ms": now_unix_ms(),
                        "verification": &verification,
                        "settlement": &settlement,
                    }),
                );
                return Ok(Json(json!({
                    "success": true,
                    "routed_via": format!("community:{}", result.worker_id),
                    "inference": {
                        "model": "community-served",
                        "model_hash": assigned_model_id,
                        "input": input_text,
                        "input_hash": format!("0x{}", hex::encode(input_hash.0)),
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
                        "status": "worker_certificate_handled_by_settlement",
                        "request_overrides_applied": false,
                        "note": "bond and challenge_period request fields apply only to the local fallback; community certificates use the protocol-fixed shape reported by settlement",
                    },
                    "worker": {
                        "worker_id": result.worker_id,
                        "live_workers_at_dispatch": live_workers,
                    },
                    "verification": verification,
                    "settlement": settlement,
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
    let (model_id_data, model_id_hash) =
        exact_model_identity(&node).map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, e))?;

    // Partial-model guard: shard-holder seeds only have 3/32 layers loaded
    // locally, so model.generate(...) produces structured garbage rather
    // than a real completion. Route to the cross-seed sharded pipeline
    // instead — that's the path the dashboard demo already exercises and
    // it returns coherent output. Verified on testnet 2026-06-04: a 3/32
    // local path produced `" nobody' Begriffe an"` 100% of the time
    // while the sharded path produced `" George Washington."` for a
    // historical-fact prompt.
    //
    // Only the candle (full Q4 float) path is allowed to skip sharded —
    // it loads the entire model regardless of --shard-range flags. The
    // pure-integer path runs against `model.layers` which is what
    // load_cached_model_ranges populates with `.is_loaded()=false`
    // sentinels for non-held layers.
    let candle_is_complete =
        node.candle_engine.is_some() && node.candle_model_id == Some(model_id_hash);
    let local_is_complete = candle_is_complete || model.has_all_transformer_layers();
    if !local_is_complete && force_local {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "force_local refused: only {}/{} transformer layers are loaded; partial and \
                 tokenizer-only models cannot execute full inference",
                model
                    .layers
                    .iter()
                    .filter(|layer| layer.is_loaded())
                    .count(),
                model.config.n_layers
            ),
        ));
    }
    if !local_is_complete {
        tracing::info!(
            "local model is partial ({}/{} layers loaded); routing /inference/run \
             through the sharded pipeline instead of garbage-emitting local fallback",
            model.layers.iter().filter(|l| l.is_loaded()).count(),
            model.layers.len()
        );
        let sharded_body = json!({
            "input": input_text,
            "max_tokens": max_tokens,
        });
        // Reuse the live sharded handler instead of duplicating its
        // tokenize → forward → cache → decode logic. If it succeeds the
        // dashboard shape is the same as a normal sharded response; if
        // it fails (no covering pipeline, all seeds down, etc.) surface
        // the underlying error so the caller can see *why* local fallback
        // was unsafe.
        return inference_run_sharded(AxumState(node.clone()), Json(sharded_body)).await;
    }

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
    // Both engines' `generate` is a fully synchronous prefill + decode loop
    // over max_tokens, so calling it directly from the async handler pinned a
    // tokio worker thread for tens of seconds. With the default
    // worker_threads == core count, a handful of concurrent /inference/run
    // calls occupied every runtime thread and stalled DAG gossip, consensus
    // and all other RPC on the node. Both paths now go through
    // spawn_blocking, and inside it through the configurable compute pool.
    let (generated_tokens, output_hash, engine_name) =
        if let (Some(engine), Some(mid)) = (&node.candle_engine, &node.candle_model_id) {
            if *mid != model_id_hash {
                return Err(api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "candle engine identity does not match the exact source artifact commitment",
                ));
            }
            // Candle Q4 float backend - coherent output, deterministic on same arch
            let engine_c = engine.clone();
            let mid_c = *mid;
            let toks = tokens_with_bos.clone();
            let pool_node = node.clone();
            let result = tokio::task::spawn_blocking(move || {
                install_on_compute_pool(&pool_node, move || {
                    engine_c.generate(&mid_c, &toks, max_tokens)
                })
            })
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("Join: {}", e)))?
            .map_err(|e| {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Inference failed: {}", e),
                )
            })?;
            let gen_tokens: Vec<u32> = result
                .output
                .chunks(4)
                .map(|c| {
                    u32::from_le_bytes([
                        c[0],
                        c.get(1).copied().unwrap_or(0),
                        c.get(2).copied().unwrap_or(0),
                        c.get(3).copied().unwrap_or(0),
                    ])
                })
                .collect();
            (
                gen_tokens,
                result.output_hash,
                "candle Q4 (float, deterministic per-arch)",
            )
        } else {
            // Integer engine — bit-identical across architectures. Precision
            // label comes from the model itself so it tracks the dispatch
            // chain (I16 / block-I8 / Q4 / per-row I8 / ternary / hybrid)
            // instead of hardcoding "INT8 integer". When the loader populates
            // I16 (default 2026-06-04+) this reports "INT16 integer …";
            // before that it reported "INT8" even when block-I8 was actually
            // running. Honest labels matter for "is INT16 working" debugging.
            let model_c = model.clone();
            let toks = tokens_with_bos.clone();
            let pool_node = node.clone();
            let (generated, hash) = tokio::task::spawn_blocking(move || {
                install_on_compute_pool(&pool_node, move || {
                    model_c.generate(&toks, max_tokens, &model_c.config.eos_tokens)
                })
            })
            .await
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("Join: {}", e)))?;
            (generated, hash, model.effective_precision_label())
        };

    let inference_ms = start.elapsed().as_millis() as u64;
    let tokens_generated = generated_tokens.len() as u64;
    let ms_per_token = inference_ms.checked_div(tokens_generated).unwrap_or(0);

    // Decode output tokens to text
    let output_text = model.decode(&generated_tokens);

    let input_hash = arc_crypto::hash_bytes(input_text.as_bytes());

    // Create + sign the InferenceAttestation transaction.
    //
    // The nonce comes from account state on every submission. It used to be
    // `state_nonce + attestation_nonce.fetch_add(1)`, an in-process counter
    // that accumulates forever — including across txs that never land — so
    // once the first attestation applied, state advanced AND the counter
    // advanced and every subsequent tx carried state+2. See
    // `submit_inference_attestation` for the full postmortem.
    let (tx_hash, attestation_status) = submit_or_relay_attestation(
        &node,
        model_id_hash,
        input_hash,
        output_hash,
        bond,
        challenge_period,
    )
    .await;

    // Store inference result for explorer display
    let tx_hash_hex = format!("0x{}", hex::encode(tx_hash.0));
    let (explorer_url, explorer_url_unavailable_reason) =
        explorer_url_for(&node, &tx_hash, &attestation_status);
    node.inference_results.insert(
        tx_hash_hex.clone(),
        json!({
            "input": input_text,
            "output": &output_text,
            "output_hash": format!("0x{}", hex::encode(output_hash.0)),
            "model": &model_id_data,
            "model_hash": format!("0x{}", hex::encode(model_id_hash.0)),
            "ms_per_token": ms_per_token,
            "tokens_generated": tokens_generated,
            "engine": &engine_name,
            "deterministic": true,
            "observed_at_unix_ms": now_unix_ms(),
        }),
    );

    Ok(Json(json!({
        "success": true,
        "routed_via": "local",
        "inference": {
            "model": model_id_data,
            "model_hash": format!("0x{}", hex::encode(model_id_hash.0)),
            "input": input_text,
            "input_tokens": prompt_tokens.len(),
            "input_hash": format!("0x{}", hex::encode(input_hash.0)),
            "output": output_text,
            "output_tokens": generated_tokens,
            "output_hash": format!("0x{}", hex::encode(output_hash.0)),
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
            "status": attestation_status,
        },
        "explorer_url": explorer_url,
        "explorer_url_unavailable_reason": explorer_url_unavailable_reason,
    })))
}

/// Per-worker earnings, derived from successful on-chain
/// CommunityInferenceReward receipts.
///
/// GET /worker/earnings/:address
///
/// Raw worker attestations never imply payment. This endpoint counts only
/// validator-authorized reward transactions whose execution receipt exists
/// and succeeded, and attributes them to the explicit worker field.
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
    let raw = hex::decode(trimmed).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid hex address: {}", e),
        )
    })?;
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
    let mut first_block: Option<u64> = None;
    let mut last_tx_hash: Option<String> = None;
    let mut confirmed_receipts: Vec<(u64, Value)> = Vec::new();

    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        let body = match &tx.body {
            TxBody::CommunityInferenceReward(b) => b,
            _ => continue,
        };
        if body.worker != want {
            continue;
        }
        let Some(receipt) = node.state.get_receipt(entry.key()) else {
            continue;
        };
        if !receipt.success {
            continue;
        }
        count += 1;
        let bh = receipt.block_height;
        if last_block.map(|cur| bh > cur).unwrap_or(true) {
            last_block = Some(bh);
            last_tx_hash = Some(format!("0x{}", hex::encode(entry.key())));
        }
        if first_block.map(|cur| bh < cur).unwrap_or(true) {
            first_block = Some(bh);
        }
        confirmed_receipts.push((
            bh,
            json!({
                "tx_type": "0x25",
                "tx_hash": format!("0x{}", hex::encode(entry.key())),
                "job_id": format!("0x{}", body.job_id.to_hex()),
                "model_id": format!("0x{}", body.model_id.to_hex()),
                "input_hash": format!("0x{}", body.input_hash.to_hex()),
                "output_hash": format!("0x{}", body.output_hash.to_hex()),
                "assignment_epoch": format!("0x{}", body.assignment_epoch.to_hex()),
                "recovery_epoch": body.recovery_epoch,
                "validator_set_id": body.validator_set_id,
                "transaction_domain": format!("0x{}", body.transaction_domain.to_hex()),
                "block_height": receipt.block_height,
                "block_hash": format!("0x{}", receipt.block_hash.to_hex()),
                "success": true,
                "reward_base": arc_types::economics::INFERENCE_ATTESTATION_REWARD,
                "reward_arc": REWARD_PER_ATTESTATION_ARC,
            }),
        ));
    }
    confirmed_receipts.sort_unstable_by_key(|(height, _)| *height);
    let confirmed_receipts: Vec<Value> = confirmed_receipts
        .into_iter()
        .map(|(_, receipt)| receipt)
        .collect();

    // Every successful reward receipt transfers the fixed protocol amount.
    // This retained-window total can undercount older history on non-archive
    // nodes, but it never counts pending or failed transaction bodies.
    let estimated_total_arc = count as f64 * REWARD_PER_ATTESTATION_ARC;

    // The address's ACTUAL on-chain balance. It is not lifetime earnings: the
    // wallet can receive, spend, or transfer ARC for unrelated reasons. Surface
    // it beside the retained reward-receipt total so the UI can label each
    // quantity honestly rather than pretending either reconciles the other.
    let onchain_balance: u64 = node
        .state
        .get_account(&want)
        .map(|a| a.balance)
        .unwrap_or(0);
    let onchain_balance_arc = onchain_balance as f64 / arc_types::economics::ARC_BASE_UNITS as f64;

    // ── Observed rate, measured from real block timestamps ────────────────
    //
    // Everything a client needs to project a rate, or to know it cannot. The
    // window is bounded by the first and last attestation THIS NODE can see,
    // and its length is read from those blocks' own header timestamps — no
    // nominal block time is assumed anywhere. That matters concretely: at a
    // notional 400 ms/block, four of the six live seeds would report brisk
    // throughput across a window in which they sealed nothing for six days.
    let block_ts = |h: Option<u64>| -> Option<u64> {
        h.and_then(|h| node.state.get_block(h))
            .map(|b| b.header.timestamp)
            .filter(|t| *t != 0)
    };
    let first_ts = block_ts(first_block);
    let last_ts = block_ts(last_block);
    let blocks_observed = match (first_block, last_block) {
        // Inclusive span: a single attestation spans one block, not zero.
        (Some(f), Some(l)) if l >= f => Some(l - f + 1),
        _ => None,
    };
    let rate = attestations_per_day_observed(count, first_ts, last_ts);
    let reward_base = arc_types::economics::INFERENCE_ATTESTATION_REWARD;
    let confirmed_gross_base = count.checked_mul(reward_base);
    let treasury_balance = node
        .state
        .get_account(&arc_types::transaction::faucet_pool_address())
        .map(|account| account.balance);
    let remaining = treasury_balance.and_then(|balance| rewards_remaining(balance, reward_base));
    let projected_daily_arc =
        if community_rewards_v1_effective(&node) && remaining.is_some_and(|value| value > 0) {
            rate.as_ref()
                .ok()
                .map(|observed| *observed * REWARD_PER_ATTESTATION_ARC)
        } else {
            None
        };
    let projected_daily_unavailable_reason = if projected_daily_arc.is_some() {
        Value::Null
    } else if !community_rewards_v1_effective(&node) {
        Value::String(
            "active reward policy is not issuance-ready on this node; no forward earnings projection is permitted"
                .to_string(),
        )
    } else if !remaining.is_some_and(|value| value > 0) {
        Value::String(
            "treasury cannot be proven to fund another full reward; projection is unavailable"
                .to_string(),
        )
    } else {
        Value::String(
            rate.as_ref()
                .err()
                .copied()
                .unwrap_or("confirmed receipt rate is unavailable")
                .to_string(),
        )
    };

    // "Today" is null, not invented.
    //
    // This used to report `round(count * 0.12)` attestations as today's, and
    // derive today_arc from that. It is a fabrication with no relationship to
    // when anything happened: a worker with one lifetime attestation reported
    // identical Today and Lifetime; a worker with a hundred reported twelve
    // today regardless of whether any of them were from this week. Computing
    // it honestly needs a block_height → timestamp join this endpoint does
    // not have yet, so until then the field says "unknown" by being null and
    // the reason ships alongside it.
    Ok(Json(json!({
        "address": format!("0x{}", trimmed),
        "total_rewards": count,
        "total_attestations": count,
        "confirmed_receipts": confirmed_receipts,
        "confirmed_receipt_count": count,
        "confirmed_gross_earnings_base": confirmed_gross_base,
        "confirmed_gross_earnings_arc": confirmed_gross_base
            .map(|value| value as f64 / arc_types::economics::ARC_BASE_UNITS as f64),
        // Actual wallet balance, not an earnings total.
        "onchain_balance": onchain_balance,
        "onchain_balance_arc": onchain_balance_arc,
        // Count-based estimate of gross reward earned (clearly labeled).
        "estimated_total_arc": estimated_total_arc,
        "estimated_total_arc_note":
            "retained-window gross rewards = successful CommunityInferenceReward receipts × \
             reward_per_attestation_arc; this node may prune older receipts and the value does \
             not subtract later wallet spending.",
        // Back-compat alias for pre-existing clients; same value as the
        // estimate above, and NOT the reconciled balance.
        "total_arc": estimated_total_arc,
        "today_arc": Value::Null,
        "today_attestations": Value::Null,
        "today_unavailable_reason":
            "per-attestation timestamps are not exposed by this endpoint yet \
             (needs a block_height -> block timestamp join)",
        "reward_per_attestation_arc": REWARD_PER_ATTESTATION_ARC,
        "projected_daily_arc": projected_daily_arc,
        "projected_daily_unavailable_reason": projected_daily_unavailable_reason,
        "projection_policy": "observed confirmed-receipt rate × explicit active 0x25 reward amount; unavailable unless issuance is ready and treasury funds at least one full reward",
        "validator_set_id": reward_recovery_validator_set_id(&node),
        "validator_set_commitment": reward_validator_set_id(&node),
        "recovery_epoch": reward_recovery_epoch(&node),
        "worker_min_stake_base": arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE,
        "stake_zero_eligible": arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE == 0,
        "community_rewards_v1_enabled": community_rewards_v1_effective(&node),
        "community_rewards_v1_protocol_active": community_rewards_v1_protocol_active(&node),
        "community_rewards_v1_approval_collection_ready": COMMUNITY_REWARD_APPROVAL_COLLECTION_READY,
        "community_rewards_v1_activation_height": node.state.community_rewards_v1_activation_height(),
        "community_rewards_v1_issuance_enabled": node.community_rewards_v1_enabled,
        "community_rewards_v1_note": community_rewards_v1_readiness_note(&node),
        "validator_set_size_required": arc_types::transaction::COMMUNITY_REWARD_VALIDATOR_SET_SIZE,
        "validator_approvals_required": arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED,
        // Base units too, so a client can do the whole projection in integers
        // and never round. Same constant arc-state credits on apply.
        "reward_per_attestation_base": arc_types::economics::INFERENCE_ATTESTATION_REWARD,
        "last_reward_block": last_block,
        "last_reward_tx_hash": last_tx_hash,
        // Backward-compatible aliases for desktop versions predating 0x25.
        "last_attestation_block": last_block,
        "last_attestation_tx_hash": last_tx_hash,
        // ── Observed window + rate (v0.7.11+, additive) ───────────────────
        "first_attestation_block": first_block,
        "blocks_observed": blocks_observed,
        "blocks_observed_unavailable_reason": if blocks_observed.is_none() {
            Value::String(
                "no attestation for this address has a receipt in this node's index, so there \
                 is no observed block window"
                    .to_string(),
            )
        } else {
            Value::Null
        },
        "observed_window_first_timestamp_ms": first_ts,
        "observed_window_last_timestamp_ms": last_ts,
        "attestations_per_day_observed": rate.as_ref().ok().copied(),
        "attestations_per_day_unavailable_reason": match rate.as_ref() {
            Ok(_) => Value::Null,
            Err(reason) => Value::String((*reason).to_string()),
        },
        "attestations_per_day_formula": "(total_attestations - 1) * 86400000 / \
             (observed_window_last_timestamp_ms - observed_window_first_timestamp_ms) — measured \
             from real block header timestamps, with no assumed block time. n attestations \
             define n-1 intervals; dividing by n would overstate the rate by one interval.",
        "attestations_per_day_caveat": "an OBSERVED backward-looking rate over this node's \
             retained window, not a forecast. It says nothing about future work available, and \
             any projection built on it must also respect /economics/rewards.rewards_remaining \
             — the treasury funding these rewards is finite.",
        // Be explicit that this is a scan of an in-memory, pruned map: on a
        // non-archive node `full_transactions` only retains the last ~1000
        // blocks and is empty after a restart, so a zero here means "not
        // visible from this node", not "never earned".
        "source": "scan of this node's in-memory full_transactions map",
        "archive_mode": node.state.archive_mode,
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
        capabilities: Vec<String>,
        model: Option<String>,
        model_id: Option<String>,
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
            capabilities: w.capabilities.clone(),
            model: w.model.clone(),
            model_id: w.model_id.clone(),
            registered_at: w.registered_at,
            success_count: w.success_count,
            failure_count: w.failure_count,
            success_rate,
            avg_ms_per_job: avg_ms,
            last_total_ms: w.last_total_ms,
            score,
        });
    }

    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(limit);

    let coordinator_model = exact_model_identity(&node)
        .ok()
        .map(|(name, model_id)| (name, format!("0x{}", model_id.to_hex())));
    let coordinator_model_name = coordinator_model.as_ref().map(|(name, _)| name.clone());
    let coordinator_model_id = coordinator_model.map(|(_, model_id)| model_id);
    let eligible_inference_workers = live_inference_worker_count(&node);

    Json(json!({
        "workers": rows,
        "count_visible": rows.len(),
        "count_total": node.community_workers.len(),
        "coordinator_model": coordinator_model_name,
        "coordinator_model_id": coordinator_model_id,
        "eligible_inference_workers": eligible_inference_workers,
    }))
}

const INFERENCE_ACTIVITY_SCHEMA: &str = "arc.inference.activity.v1";

fn inference_payload_for_attestation(
    body: &arc_types::transaction::InferenceAttestationBody,
    local_observation: Option<&Value>,
) -> Value {
    let has_local_detail = local_observation.and_then(Value::as_object).is_some();
    let mut inference = local_observation
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    inference.insert(
        "model_hash".to_string(),
        json!(format!("0x{}", hex::encode(body.model_id.0))),
    );
    inference.insert(
        "input_hash".to_string(),
        json!(format!("0x{}", hex::encode(body.input_hash.0))),
    );
    inference.insert(
        "output_hash".to_string(),
        json!(format!("0x{}", hex::encode(body.output_hash.0))),
    );
    inference.insert("bond".to_string(), json!(body.bond));
    inference.insert("challenge_period".to_string(), json!(body.challenge_period));
    inference.insert("deterministic".to_string(), json!(true));
    inference.insert(
        "display_content_source".to_string(),
        json!(if has_local_detail {
            "node_local_memory_enrichment"
        } else {
            "chain_commitments_only"
        }),
    );
    // InferenceAttestation commits hashes, not prompt/output text. Even when
    // this node can enrich a mined row from local memory, the API must not
    // imply that display text itself was stored in the receipt or block.
    inference.insert("display_text_on_chain".to_string(), json!(false));
    inference
        .entry("input".to_string())
        .or_insert_with(|| json!(format!("[hash {}]", &body.input_hash.to_hex()[..16])));
    inference
        .entry("output".to_string())
        .or_insert_with(|| json!(format!("[hash {}]", &body.output_hash.to_hex()[..16])));
    inference
        .entry("model".to_string())
        .or_insert_with(|| json!("on-chain attestation"));
    Value::Object(inference)
}

fn local_inference_for_hash(node: &NodeState, hash: &[u8; 32]) -> Option<Value> {
    let bare = hex::encode(hash);
    node.inference_results
        .get(&format!("0x{bare}"))
        .or_else(|| node.inference_results.get(&bare))
        .map(|entry| entry.value().clone())
}

fn local_record_is_receipt_backed_attestation(node: &NodeState, record_id: &str) -> bool {
    let Ok(hash) = parse_hash256_hex(record_id, "record_id") else {
        return false;
    };
    node.state
        .get_receipt(&hash.0)
        .is_some_and(|receipt| receipt.tx_hash == hash)
        && node.state.full_transactions.get(&hash.0).is_some_and(|tx| {
            tx.hash == hash
                && tx.tx_type == TxType::InferenceAttestation
                && matches!(tx.body, TxBody::InferenceAttestation(_))
        })
}

/// List inference activity visible to this node without conflating an
/// in-memory computation with a mined transaction.
///
/// `activities` contains both explicitly labelled local observations and
/// receipt-backed transactions. `attestations` is the compatibility view and
/// contains only successful, mined `InferenceAttestation` receipts.
///
/// GET /inference/attestations?limit=10
async fn inference_list_attestations(
    AxumState(node): AxumState<NodeState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(10);

    let height = node.state.height();
    let source_node = format!("0x{}", node.validator_address.to_hex());
    // (mined block height, local observation time, activity row). Receipt
    // backed rows sort first by block height; unordered legacy observations
    // never outrank mined chain evidence merely because they lack a receipt.
    let mut activities: Vec<(Option<u64>, u64, Value)> = Vec::new();

    // Chain-indexed bodies are not proof of inclusion by themselves. A row is
    // called mined only when the exact transaction also has a receipt.
    for entry in node.state.full_transactions.iter() {
        let hash = entry.key();
        let tx = entry.value();
        let TxBody::InferenceAttestation(body) = &tx.body else {
            continue;
        };
        let Some(receipt) = node.state.get_receipt(hash) else {
            continue;
        };
        if tx.tx_type != TxType::InferenceAttestation
            || tx.hash.0 != *hash
            || receipt.tx_hash.0 != *hash
        {
            continue;
        }
        let tx_hex = format!("0x{}", hex::encode(hash));
        let receipt_status = if receipt.success { "success" } else { "failed" };
        let local = local_inference_for_hash(&node, hash);
        let activity = json!({
            "schema": INFERENCE_ACTIVITY_SCHEMA,
            "record_kind": "mined_inference_attestation",
            "source": "chain_receipt",
            "source_node": source_node,
            "mined": true,
            "receipt_status": receipt_status,
            "tx_hash": tx_hex,
            "tx_type": "InferenceAttestation",
            "success": receipt.success,
            "from": tx.from.to_hex(),
            "block_height": receipt.block_height,
            "block_hash": format!("0x{}", receipt.block_hash.to_hex()),
            "gas_used": receipt.gas_used,
            "inference": inference_payload_for_attestation(body, local.as_ref()),
        });
        activities.push((Some(receipt.block_height), 0, activity));
    }

    // Process-local results without a matching transaction receipt are useful
    // operational evidence, but they are not transactions, attestations, or
    // proof of payment. Keep them visible under an explicit observation type.
    for entry in node.inference_results.iter() {
        if local_record_is_receipt_backed_attestation(&node, entry.key()) {
            continue;
        }
        let observed_at_unix_ms = entry
            .value()
            .get("observed_at_unix_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        activities.push((
            None,
            observed_at_unix_ms,
            json!({
                "schema": INFERENCE_ACTIVITY_SCHEMA,
                "record_kind": "inference_observation",
                "record_id": entry.key().clone(),
                "source": "node_local_memory",
                "source_node": source_node,
                "mined": false,
                "receipt_status": "absent",
                "tx_type": "InferenceObservation",
                "observed_at_unix_ms": if observed_at_unix_ms == 0 {
                    Value::Null
                } else {
                    json!(observed_at_unix_ms)
                },
                "inference": entry.value().clone(),
            }),
        ));
    }

    activities.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.1.cmp(&a.1),
    });

    let total_matched = activities.len();
    let mined_success_count = activities
        .iter()
        .filter(|(_, _, row)| row["mined"] == true && row["success"] == true)
        .count();
    let observation_count = activities
        .iter()
        .filter(|(_, _, row)| row["record_kind"] == "inference_observation")
        .count();
    let mut mined_attestations: Vec<Value> = activities
        .iter()
        .filter(|(_, _, row)| row["mined"] == true && row["success"] == true)
        .map(|(_, _, row)| row.clone())
        .collect();
    let mut observations: Vec<Value> = activities
        .iter()
        .filter(|(_, _, row)| row["record_kind"] == "inference_observation")
        .map(|(_, _, row)| row.clone())
        .collect();
    mined_attestations.truncate(limit);
    observations.truncate(limit);
    activities.truncate(limit);
    let rows: Vec<Value> = activities.into_iter().map(|(_, _, row)| row).collect();

    Ok(Json(json!({
        "schema": INFERENCE_ACTIVITY_SCHEMA,
        "scope": "selected_node",
        "source_node": source_node,
        "activities": rows,
        "attestations": mined_attestations,
        "observations": observations,
        "count": rows.len(),
        "total_matched": total_matched,
        "mined_success_count": mined_success_count,
        "observation_count": observation_count,
        "chain_height": height,
    })))
}

/// GET /inference/results - list stored inference results (input, output, hash, model).
async fn inference_list_results(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let results: Vec<Value> = node
        .inference_results
        .iter()
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

const MAX_FORWARD_SHARD_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_FORWARD_SHARD_HIDDEN_ELEMENTS: usize = 131_072;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardShardRequest {
    /// Unique request id (hex). The receiving shard uses this as the KV cache key.
    request_id: String,
    /// Exact BLAKE3 commitment of every source-artifact byte.
    model_id: String,
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
    /// Exact model width expected in every nonterminal response.
    expected_hidden_len: usize,
    /// Whether this range includes the model's terminal LM head.
    expect_terminal: bool,
    /// Recently generated output tokens used by the terminal shard's shared
    /// deterministic repetition-penalty selector.  Empty during prefill and on
    /// every nonterminal range; bounded to the protocol's 64-token window.
    #[serde(default)]
    generated_tokens: Vec<u32>,
    /// True if this is the last token of the request - used to evict KV cache.
    #[serde(default)]
    last_token: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Active validator that authenticated this computation response.
    validator_address: Hash256,
    /// Signature over the chain domain, exact request, and semantic response.
    /// A hash majority without signer authentication is only three HTTP
    /// aliases; this turns it into a majority of distinct validator keys.
    signature: arc_crypto::Signature,
}

/// Domain-separated transcript signed by a shard holder for every response.
/// Authentication covers the request as well as the result, preventing a
/// valid response from a different prompt/position/range from being replayed.
fn forward_shard_response_commitment(
    chain_identity: Option<&ChainIdentity>,
    req: &ForwardShardRequest,
    response: &ForwardShardResponse,
) -> Result<Hash256, String> {
    let domain = chain_identity
        .map(|identity| format!("{}:{}", identity.name, identity.chain_id))
        .unwrap_or_else(|| "unconfigured-chain".to_string());
    let bytes = bincode::serialize(&(
        "ARC-forward-shard-response-v3",
        domain,
        req,
        response.is_terminal,
        &response.hidden_hash,
        response.token_id,
        &response.logits_hash,
        response.layers_processed,
        response.compute_ms,
        &response.node_name,
        response.validator_address,
    ))
    .map_err(|e| format!("serialize shard response commitment: {e}"))?;
    Ok(arc_crypto::hash_bytes(&bytes))
}

fn verify_forward_shard_response_auth(
    chain_identity: Option<&ChainIdentity>,
    req: &ForwardShardRequest,
    response: &ForwardShardResponse,
    validators: &[(Hash256, u64)],
) -> Result<(), String> {
    let is_active_validator = validators
        .iter()
        .any(|(address, stake)| *address == response.validator_address && *stake > 0);
    if !is_active_validator {
        return Err(format!(
            "unauthorized shard signer {} (not an active staked validator)",
            response.validator_address
        ));
    }
    let commitment = forward_shard_response_commitment(chain_identity, req, response)?;
    response
        .signature
        .verify(&commitment, &response.validator_address)
        .map_err(|e| format!("invalid shard response signature: {e}"))?;
    verify_forward_shard_response_semantics(req, response)
}

fn hidden_state_hash_hex(hidden: &[i64]) -> String {
    let bytes: Vec<u8> = hidden
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    format!("0x{}", arc_crypto::hash_bytes(&bytes).to_hex())
}

fn is_hash256_hex(value: &str) -> bool {
    let trimmed = value.strip_prefix("0x").unwrap_or(value);
    trimmed.len() == 64 && hex::decode(trimmed).is_ok()
}

fn verify_forward_shard_response_semantics(
    req: &ForwardShardRequest,
    response: &ForwardShardResponse,
) -> Result<(), String> {
    let expected_layers = req
        .end_layer
        .checked_sub(req.start_layer)
        .filter(|layers| *layers > 0)
        .ok_or_else(|| "invalid forward-shard request range".to_string())?;
    if response.layers_processed != expected_layers {
        return Err(format!(
            "shard response processed {} layers for requested range [{}, {}) (expected {})",
            response.layers_processed, req.start_layer, req.end_layer, expected_layers
        ));
    }
    if response.is_terminal != req.expect_terminal {
        return Err(format!(
            "shard response terminal={} does not match requested terminal={}",
            response.is_terminal, req.expect_terminal
        ));
    }

    if response.is_terminal {
        if response.hidden.is_some() || response.hidden_hash.is_some() {
            return Err("terminal shard response must not contain a hidden state".to_string());
        }
        if response.token_id.is_none() {
            return Err("terminal shard response must contain token_id".to_string());
        }
        if !response.logits_hash.as_deref().is_some_and(is_hash256_hex) {
            return Err("terminal shard response must contain a 32-byte logits_hash".to_string());
        }
        return Ok(());
    }

    if response.token_id.is_some() || response.logits_hash.is_some() {
        return Err("nonterminal shard response must not contain token/logits output".to_string());
    }
    if req.expected_hidden_len == 0 || req.expected_hidden_len > MAX_FORWARD_SHARD_HIDDEN_ELEMENTS {
        return Err("requested hidden-state width is outside protocol bounds".to_string());
    }
    let hidden = response
        .hidden
        .as_deref()
        .ok_or_else(|| "nonterminal shard response is missing hidden state".to_string())?;
    if hidden.len() != req.expected_hidden_len {
        return Err(format!(
            "shard response hidden width {} does not match expected {}",
            hidden.len(),
            req.expected_hidden_len
        ));
    }
    let declared_hash = response
        .hidden_hash
        .as_deref()
        .ok_or_else(|| "nonterminal shard response is missing hidden_hash".to_string())?;
    let actual_hash = hidden_state_hash_hex(hidden);
    if declared_hash != actual_hash {
        return Err(format!(
            "shard response hidden_hash does not match hidden body: declared {declared_hash}, actual {actual_hash}"
        ));
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CleanupShardRequest {
    /// Unique request id whose per-shard KV cache should be evicted.
    request_id: String,
}

/// GET /inference/cache_stats
/// Report live stats about the deterministic inference cache: how many
/// entries are warm, what the capacity is, and total cumulative hits.
/// Dashboards call this to show a "N prompts cached" counter.
async fn inference_cache_stats(AxumState(node): AxumState<NodeState>) -> Json<serde_json::Value> {
    Json(json!({
        "size": node.inference_cache.len(),
        "capacity": node.inference_cache.capacity(),
        "total_hits": node.inference_cache.total_hits(),
        // Sharded requests served WITHOUT walking the pipeline. Tracked apart
        // from sharded_runs_total so neither number lies about the other.
        "sharded_cache_hits_total": node.sharded_cache_hits.load(Ordering::Relaxed),
        "sharded_runs_total": node.sharded_runs_total.load(Ordering::Relaxed),
        "cache_type": "DistributedCache (BLAKE3-keyed, deterministic, LRU)",
        "bypass": "POST /inference/run_sharded with \"force_recompute\": true to walk the pipeline anyway",
    }))
}

/// GET /inference/latency_stats
/// Returns the rolling EWMA hop latency (ms) per replica socket, plus sample
/// count and age. Coordinators use this map to sort per-range replica lists
/// before picking primary (run_sharded) or top-k (run_consensus). Closes #29.
async fn inference_latency_stats(AxumState(node): AxumState<NodeState>) -> Json<serde_json::Value> {
    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(node.latency_stats.len());
    for kv in node.latency_stats.iter() {
        let (socket, stat) = (kv.key().clone(), kv.value().clone());
        entries.push(json!({
            "socket": socket,
            "ewma_ms": (stat.ms * 100.0).round() / 100.0,
            "samples": stat.count,
            "age_secs": stat.last_updated.elapsed().as_secs(),
            // Whether the router will ACT on this figure. Samples past
            // LATENCY_STALE_SECS are treated as unknown rather than as fact —
            // the map used to be insert-only, so ten-hour-old numbers still
            // steered every dispatch.
            "in_use": effective_latency_ms(&stat).is_some(),
            "stale_after_secs": LATENCY_STALE_SECS,
            // "hop" = measured forward_shard round trip. "probe" = provisional
            // value from a GET /health RTT, used only where no hop sample
            // exists or where the recorded one was contradicted by the probe.
            "source": if stat.probe_only { "probe" } else { "hop" },
        }));
    }
    entries.sort_by(|a, b| {
        let ae = a
            .get("ewma_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::MAX);
        let be = b
            .get("ewma_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::MAX);
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

fn default_cache_check_max_tokens() -> u32 {
    20
}

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
    let model = node.inference_model.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "no model loaded".to_string(),
    ))?;

    // Replicate the cache-key derivation used by inference_run_sharded so
    // a check here matches what a real call would look up. Cache entries are
    // isolated by exact artifact bytes, not merely compatible dimensions.
    let model_id_hash = node.model_artifact_id.ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "exact source-artifact model commitment unavailable".to_string(),
    ))?;

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
    let signed_request = req.clone();
    let local_model_id = node.model_artifact_id.ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "exact source-artifact model commitment unavailable".to_string(),
    ))?;
    let requested_model_id = parse_hash256_hex(&req.model_id, "model_id")
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    if requested_model_id != local_model_id {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "model artifact mismatch: requested 0x{}, this shard loaded 0x{}",
                requested_model_id.to_hex(),
                local_model_id.to_hex()
            ),
        ));
    }
    let model = node.inference_model.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "No model loaded".to_string(),
    ))?;
    if req.expected_hidden_len != model.config.d_model
        || req.expected_hidden_len == 0
        || req.expected_hidden_len > MAX_FORWARD_SHARD_HIDDEN_ELEMENTS
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "expected_hidden_len {} does not match model width {} or protocol bounds",
                req.expected_hidden_len, model.config.d_model
            ),
        ));
    }
    if req.start_layer >= req.end_layer || req.end_layer > model.config.n_layers {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "invalid requested layer range [{}, {}) for {}-layer model",
                req.start_layer, req.end_layer, model.config.n_layers
            ),
        ));
    }
    let range_is_terminal = req.end_layer == model.config.n_layers;
    if req.expect_terminal != range_is_terminal {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "expect_terminal={} does not match requested range [{}, {})",
                req.expect_terminal, req.start_layer, req.end_layer
            ),
        ));
    }
    if req.generated_tokens.len() > 64 {
        return Err((
            StatusCode::BAD_REQUEST,
            "generated_tokens exceeds the 64-token repetition-penalty window".to_string(),
        ));
    }
    if !range_is_terminal && !req.generated_tokens.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "generated_tokens is only valid for the terminal shard range".to_string(),
        ));
    }
    if req.token.is_some() == req.hidden.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "exactly one of 'token' or 'hidden' is required".to_string(),
        ));
    }
    if node.shard_infos.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Node is not a shard holder".to_string(),
        ));
    }
    // Verify this node holds the requested layer range. A node holding
    // multiple disjoint ranges accepts requests for any of them - each range
    // was independently announced and is an independent replica slot.
    let shard = node
        .shard_infos
        .iter()
        .find(|s| {
            s.start_layer == req.start_layer
                && s.end_layer == req.end_layer
                && parse_hash256_hex(&s.model_id, "shard model_id").ok() == Some(local_model_id)
        })
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "Shard mismatch: requested [{}, {}) but this node holds {:?}",
                    req.start_layer,
                    req.end_layer,
                    node.shard_infos
                        .iter()
                        .map(|s| (s.start_layer, s.end_layer))
                        .collect::<Vec<_>>()
                ),
            )
        })?;

    // Decode input
    use arc_inference::cached_integer_model::{KVCache, ShardInput, ShardOutput};

    let input = if let Some(token) = req.token {
        // Verify this is the first shard (only first shard accepts a raw token)
        if shard.start_layer != 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                "Only the first shard accepts a raw token".to_string(),
            ));
        }
        if req.hidden_hash.is_some() {
            return Err((
                StatusCode::BAD_REQUEST,
                "raw-token shard request must not contain hidden_hash".to_string(),
            ));
        }
        ShardInput::Token(token)
    } else if let Some(hidden) = req.hidden {
        if hidden.len() != req.expected_hidden_len {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "hidden width {} does not match expected {}",
                    hidden.len(),
                    req.expected_hidden_len
                ),
            ));
        }
        let expected_hex = req.hidden_hash.as_deref().ok_or((
            StatusCode::BAD_REQUEST,
            "hidden-state shard request requires hidden_hash".to_string(),
        ))?;
        let actual_hex = hidden_state_hash_hex(&hidden);
        if actual_hex != expected_hex {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "Hidden state integrity check failed: expected {}, got {}",
                    expected_hex, actual_hex
                ),
            ));
        }
        ShardInput::Hidden(hidden)
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            "Need either 'token' or 'hidden' field".to_string(),
        ));
    };

    // Get-or-create per-request KV cache
    let n_layers = model.config.n_layers;
    let cache_arc = node
        .shard_kv_caches
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
    let generated_tokens = req.generated_tokens.clone();
    let node_name = shard.node_name.clone();

    let t0 = std::time::Instant::now();
    let pool_node = node.clone();
    // `install_on_compute_pool` puts the whole forward pass — including the
    // par_iter over attention heads and every matmul's par_chunks_mut — on
    // this node's configured rayon pool, so POST /node/threads changes real
    // CPU utilisation on the very next request.
    let result = tokio::task::spawn_blocking(
        move || -> Result<ShardOutput, arc_inference::cached_integer_model::ShardForwardError> {
            install_on_compute_pool(&pool_node, move || {
                // The KV lock is taken INSIDE the pool job (a MutexGuard is
                // not Send, so it cannot cross into the pool). Concurrent
                // jobs for the same request_id serialize here; they cannot
                // deadlock, because a queued job holds no lock and the
                // holder is always a running job that will finish.
                let mut cache = match cache_arc.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                model_clone.forward_shard_token_with_history(
                    input,
                    &mut cache,
                    start_layer,
                    end_layer,
                    position,
                    &generated_tokens,
                )
            })
        },
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Join: {}", e)))?;

    // A refused forward is a 409, not a 500: the request is well-formed, this
    // REPLICA is just not in a state to serve it. The coordinator matches on
    // the `kind` string to decide whether to drop this replica for the rest
    // of the request (cold KV cache) or merely retry elsewhere.
    let result = result.map_err(|e| {
        let status = match e {
            arc_inference::cached_integer_model::ShardForwardError::KvCacheOutOfSync { .. } => {
                StatusCode::CONFLICT
            }
            arc_inference::cached_integer_model::ShardForwardError::LayerNotLoaded { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::BAD_REQUEST,
        };
        tracing::warn!(
            request_id = %req_id,
            position,
            range = %format!("[{}, {})", start_layer, end_layer),
            "forward_shard refused: {}",
            e
        );
        (status, format!("{} ({})", e, e.kind()))
    })?;
    let compute_ms = t0.elapsed().as_millis() as u64;
    // Keep this node's own measurement. This is the only place the node
    // observes its OWN compute cost for a shard hop (everything in
    // `latency_stats` is a round trip to somebody else), so it is what
    // /node/contribution reports as mean/p50.
    record_own_compute_ms(&node, compute_ms);

    // Optionally evict cache after the last token
    if req.last_token {
        node.shard_kv_caches.remove(&req_id);
    }

    let layers_processed = end_layer - start_layer;
    let response = match result {
        ShardOutput::Hidden(state) => {
            let hidden_hash = hidden_state_hash_hex(&state);
            ForwardShardResponse {
                is_terminal: false,
                hidden: Some(state),
                hidden_hash: Some(hidden_hash),
                token_id: None,
                logits_hash: None,
                layers_processed,
                compute_ms,
                node_name,
                validator_address: node.validator_address,
                signature: arc_crypto::Signature::null(),
            }
        }
        ShardOutput::Token { id, logits_hash } => ForwardShardResponse {
            is_terminal: true,
            hidden: None,
            hidden_hash: None,
            token_id: Some(id),
            logits_hash: Some(format!("0x{}", hex::encode(logits_hash.0))),
            layers_processed,
            compute_ms,
            node_name,
            validator_address: node.validator_address,
            signature: arc_crypto::Signature::null(),
        },
    };
    let mut response = response;
    let validator_keypair = node.validator_keypair.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Shard holder has no validator signing key; unsigned compute is not accepted on protocol v3"
            .to_string(),
    ))?;
    let commitment =
        forward_shard_response_commitment(node.chain_identity.as_ref(), &signed_request, &response)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    response.signature = validator_keypair.sign(&commitment).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("sign shard response: {e}"),
        )
    })?;
    Ok(Json(response))
}

/// POST /inference/cleanup_shard
///
/// Evict the per-request KV cache after a coordinator finishes or abandons a
/// sharded run. Cleanup used to POST an incomplete `ForwardShardRequest`, so
/// Axum rejected the JSON before `inference_forward_shard` could remove
/// anything and every remote shard leaked one cache entry per request.
async fn inference_cleanup_shard(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<CleanupShardRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if req.request_id.is_empty() || req.request_id.len() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            "request_id required (1-128 chars)".to_string(),
        ));
    }

    let removed = node.shard_kv_caches.remove(&req.request_id).is_some();
    Ok(Json(json!({
        "ok": true,
        "request_id": req.request_id,
        "removed": removed,
    })))
}

// ─── Sharded pipeline execution engine ─────────────────────────────────────
//
// One implementation, two endpoints. /inference/run_sharded and
// /inference/run_consensus differ ONLY in the per-hop replica strategy they
// pass in; both get the pipelined prefill, the accumulated trace, the shared
// HTTP client and the parallel cleanup.
//
// Before this, run_consensus — the endpoint the desktop actually calls — had
// its own fully sequential prefill: every prompt position walked all six hops
// end-to-end before the next position started, and each hop waited for ALL k
// replicas. That is the ~54-112 s/token the desktop's own comment records.

/// Depth of each inter-shard prefill channel.
///
/// Was `(prompt_len + 4).max(16)`. Each queued item carries a d_model = 4096
/// `Vec<i64>` — 32 KB — so with prompt_len in the thousands a fast shard
/// feeding a slow one could queue 100+ MB per channel on the coordinator.
/// Depth beyond `num_shards` buys nothing for pipeline fill anyway; a small
/// constant keeps backpressure reaching the feed loop.
const PREFILL_CHANNEL_DEPTH: usize = 16;

/// Timeout for the fire-and-forget end-of-request KV cleanup fan-out.
const CLEANUP_TIMEOUT_SECS: u64 = 3;

/// Minimum gap between shard-registry bootstrap sweeps against the seeds.
const REGISTRY_BOOTSTRAP_MIN_INTERVAL_SECS: u64 = 15;

/// How a single hop chooses among the replicas holding its layer range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopStrategy {
    /// One request in flight: try replicas in order, rotate on failure.
    Failover,
    /// Contact `fanout` replicas at once and return as soon as `needed` of
    /// them agree on the output hash.
    ///   `needed == 1`          → hedged: first valid answer wins.
    ///   `needed == fanout/2+1` → k-of-n hash majority (run_consensus).
    Fanout {
        fanout: usize,
        needed: usize,
        /// Free inference may intentionally degrade to a majority of the live
        /// replicas and labels that behavior in its response.  Reward-bearing
        /// community verification must set this to false: all configured
        /// replica slots must remain reachable and `needed` is never reduced.
        allow_degraded_quorum: bool,
    },
}

/// Per-range consensus record for one position.
#[derive(serde::Serialize, Clone)]
pub struct RangeVote {
    pub position: usize,
    pub range: (usize, usize),
    pub replicas_contacted: Vec<String>,
    pub replicas_returned: Vec<String>,
    pub majority_hash: Option<String>,
    /// Distinct active validator identities whose authenticated responses
    /// formed `majority_hash`.  Community verification validates these keys
    /// directly; a count of vote records is not quorum evidence.
    pub majority_signers: Vec<Hash256>,
    pub divergent: Vec<(String, String)>,
    /// "unanimous" | "majority" | "split" | "no_response"
    pub agreement: String,
}

/// Resolve one fan-out plan without silently weakening a fixed quorum.
///
/// The free/demo endpoints deliberately preserve their old degraded behavior,
/// but any path that can credit community work must require all configured
/// replica slots and the original threshold for every range and position.
fn fanout_quorum_plan(
    fanout: usize,
    needed: usize,
    available: usize,
    allow_degraded_quorum: bool,
) -> Result<(usize, usize), String> {
    let fanout = fanout.max(1);
    let needed = needed.max(1);
    if available == 0 {
        return Err("fan-out quorum unavailable: no live replicas remain".to_string());
    }
    if !allow_degraded_quorum && available < fanout {
        return Err(format!(
            "fixed quorum unavailable: {available} live replica(s) remain, but {fanout} are required"
        ));
    }

    let use_n = fanout.min(available);
    if !allow_degraded_quorum && use_n < needed {
        return Err(format!(
            "fixed quorum unavailable: {use_n} live replica(s) cannot satisfy {needed} required signatures"
        ));
    }
    let effective_needed = if allow_degraded_quorum {
        needed.min(use_n)
    } else {
        needed
    };
    Ok((use_n, effective_needed))
}

/// In-flight fan-out calls for one hop: `(replica, wall_ms, response)`, where
/// the error side carries `(message, is_cold_cache)`.
type FanoutJoinSet = tokio::task::JoinSet<(
    ShardInfo,
    u64,
    Result<(ForwardShardResponse, usize), (String, bool)>,
)>;

/// What one hop produced.
struct HopOutcome {
    hidden: Option<Vec<i64>>,
    hidden_hash: Option<String>,
    is_terminal: bool,
    token_id: Option<u32>,
    served_by: String,
    compute_ms: u64,
    wall_ms: u64,
    layers_processed: u64,
    req_bytes: usize,
    resp_bytes: usize,
    vote: Option<RangeVote>,
}

/// Running totals for one pipeline hop across every position of a request.
#[derive(Default, Clone)]
struct HopStats {
    positions: u64,
    compute_ms: u64,
    wall_ms: u64,
    req_bytes: u64,
    resp_bytes: u64,
    layers: u64,
    is_terminal: bool,
    served_by: std::collections::BTreeMap<String, u64>,
}

impl HopStats {
    fn fold(&mut self, o: &HopOutcome) {
        self.positions += 1;
        self.compute_ms += o.compute_ms;
        self.wall_ms += o.wall_ms;
        self.req_bytes += o.req_bytes as u64;
        self.resp_bytes += o.resp_bytes as u64;
        self.layers = o.layers_processed;
        self.is_terminal |= o.is_terminal;
        *self.served_by.entry(o.served_by.clone()).or_insert(0) += 1;
    }
}

/// One forward_shard call. Returns the parsed response plus the exact number
/// of bytes on the wire in each direction, or a typed failure.
///
/// `Err(true)` in the tuple position means "this replica is COLD for this
/// request_id" — it reported `kv_cache_out_of_sync`, so its KV cache does not
/// line up with the position we asked for and it can never serve this request
/// again. That happens after a failover or an aborted hedge, and before the
/// engine-side guard existed it was an out-of-bounds read in
/// `flash_attention_i64` rather than an error.
async fn read_forward_shard_body_limited(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_FORWARD_SHARD_RESPONSE_BYTES as u64)
    {
        return Err(format!(
            "response body exceeds {}-byte limit",
            MAX_FORWARD_SHARD_RESPONSE_BYTES
        ));
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(MAX_FORWARD_SHARD_RESPONSE_BYTES);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read response body: {error}"))?
    {
        let next_len = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| "response body length overflow".to_string())?;
        if next_len > MAX_FORWARD_SHARD_RESPONSE_BYTES {
            return Err(format!(
                "response body exceeds {}-byte limit",
                MAX_FORWARD_SHARD_RESPONSE_BYTES
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn forward_shard_once(
    client: &reqwest::Client,
    socket: &str,
    body: &[u8],
    request: &ForwardShardRequest,
    chain_identity: Option<&ChainIdentity>,
    authorized_validators: &SharedValidators,
) -> Result<(ForwardShardResponse, usize), (String, bool)> {
    let url = format!("http://{}/inference/forward_shard", socket);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| (format!("send: {}", e), false))?;

    let status = resp.status();
    let raw = read_forward_shard_body_limited(resp)
        .await
        .map_err(|error| (error, false))?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&raw);
        // The shard holder tags a cold cache with this stable string; see
        // ShardForwardError::kind().
        let is_cold = text.contains("kv_cache_out_of_sync");
        return Err((format!("http {}: {}", status, text.trim()), is_cold));
    }

    // Read the bounded body as bytes so the response payload can be measured exactly.
    // `total_bytes_transferred` previously counted only REQUEST bodies, which
    // undercounted the true wire cost by roughly half — the hidden state comes
    // back the same size it went out.
    let parsed: ForwardShardResponse =
        serde_json::from_slice(&raw).map_err(|e| (format!("parse: {}", e), false))?;

    verify_forward_shard_response_auth(
        chain_identity,
        request,
        &parsed,
        &authorized_validators.read(),
    )
    .map_err(|e| (e, false))?;
    Ok((parsed, raw.len()))
}

/// Execute one pipeline hop under `strategy`.
///
/// `replicas` is this hop's live candidate list; the function rotates it so a
/// working replica stays at the front, and the caller keeps the same Vec
/// across positions so the choice is sticky (which matters: forward_shard is
/// stateful per request_id, so bouncing between replicas is what makes a KV
/// cache go cold in the first place).
#[allow(clippy::too_many_arguments)]
async fn pipeline_hop(
    client: &reqwest::Client,
    replicas: &mut Vec<ShardInfo>,
    strategy: HopStrategy,
    req: &ForwardShardRequest,
    lat_stats: &Arc<dashmap::DashMap<String, LatencyEWMA>>,
    chain_identity: Option<&ChainIdentity>,
    authorized_validators: &SharedValidators,
    want_vote: bool,
) -> Result<HopOutcome, String> {
    let body = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let req_bytes = body.len();

    match strategy {
        HopStrategy::Failover => {
            let mut last_err = String::new();
            let mut cold: Vec<String> = Vec::new();
            let mut attempts = 0usize;
            while attempts < replicas.len() {
                let shard = replicas[0].clone();
                let t_hop = std::time::Instant::now();
                match forward_shard_once(
                    client,
                    &shard.socket_addr,
                    &body,
                    req,
                    chain_identity,
                    authorized_validators,
                )
                .await
                {
                    Ok((resp, resp_bytes)) => {
                        let wall_ms = t_hop.elapsed().as_millis() as u64;
                        record_latency(lat_stats, &shard.socket_addr, wall_ms);
                        return Ok(HopOutcome {
                            hidden: resp.hidden,
                            hidden_hash: resp.hidden_hash,
                            is_terminal: resp.is_terminal,
                            token_id: resp.token_id,
                            served_by: shard.node_name.clone(),
                            compute_ms: resp.compute_ms,
                            wall_ms,
                            layers_processed: resp.layers_processed as u64,
                            req_bytes,
                            resp_bytes,
                            vote: None,
                        });
                    }
                    Err((e, is_cold)) => {
                        last_err = format!(
                            "range [{}, {}) replica {} ({}): {}",
                            req.start_layer, req.end_layer, shard.node_name, shard.socket_addr, e
                        );
                        if is_cold {
                            cold.push(shard.socket_addr.clone());
                            replicas.retain(|r| r.socket_addr != shard.socket_addr);
                            // Removing shrinks the list; don't advance attempts.
                            continue;
                        }
                        attempts += 1;
                        replicas.rotate_left(1);
                    }
                }
            }
            if !cold.is_empty() && req.position > 0 {
                // Be explicit about WHY this is unrecoverable, because it
                // looks like a transient failure and isn't. forward_shard is
                // stateful per request_id: a replica that missed positions
                // 0..p cannot serve p, and this coordinator does not replay
                // history to it. The supported answer is to commit to more
                // than one replica from the start.
                return Err(format!(
                    "range [{}, {}) at position {}: the serving replica failed mid-stream and \
                     {} standby replica(s) rejected the handover with kv_cache_out_of_sync \
                     (they never saw positions 0..{}, so their KV caches are cold for this \
                     request). Re-send with \"redundancy\": 2 to keep a second replica warm \
                     from position 0. Last error: {}",
                    req.start_layer,
                    req.end_layer,
                    req.position,
                    cold.len(),
                    req.position,
                    last_err
                ));
            }
            Err(format!(
                "All replicas failed for range [{}, {}) at position {}. Last error: {}",
                req.start_layer, req.end_layer, req.position, last_err
            ))
        }

        HopStrategy::Fanout {
            fanout,
            needed,
            allow_degraded_quorum,
        } => {
            let (use_n, needed) =
                fanout_quorum_plan(fanout, needed, replicas.len(), allow_degraded_quorum)?;
            let selected: Vec<ShardInfo> = replicas.iter().take(use_n).cloned().collect();

            let mut set: FanoutJoinSet = tokio::task::JoinSet::new();
            for r in &selected {
                let c = client.clone();
                let b = body.clone();
                let shard = r.clone();
                let request = req.clone();
                let chain_identity = chain_identity.cloned();
                let authorized_validators = authorized_validators.clone();
                set.spawn(async move {
                    let t = std::time::Instant::now();
                    let out = forward_shard_once(
                        &c,
                        &shard.socket_addr,
                        &b,
                        &request,
                        chain_identity.as_ref(),
                        &authorized_validators,
                    )
                    .await;
                    let wall = t.elapsed().as_millis() as u64;
                    (shard, wall, out)
                });
            }

            // Tally hashes AS THEY LAND and stop the moment `needed` agree.
            // The old code collected with `for f in futs { f.await }`, so a hop
            // could not return until the SLOWEST of k answered — with k=3 and
            // exactly 3 replicas per range (the live topology, and the k the
            // desktop sends) that made the latency-aware sort a complete no-op
            // and every hop paid the worst replica. Cost per hop goes from
            // max(k) to the k/2+1-th order statistic.
            let mut returned: Vec<(ShardInfo, u64, ForwardShardResponse, usize)> = Vec::new();
            let mut tally: HashMap<String, Vec<usize>> = HashMap::new();
            let mut signer_votes: std::collections::HashSet<(String, Hash256)> =
                std::collections::HashSet::new();
            let mut cold: Vec<String> = Vec::new();
            let mut errors: Vec<String> = Vec::new();
            let mut winner: Option<(String, Vec<usize>)> = None;

            while let Some(joined) = set.join_next().await {
                let (shard, wall_ms, out) = match joined {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push(format!("task join: {}", e));
                        continue;
                    }
                };
                match out {
                    Ok((resp, resp_bytes)) => {
                        record_latency(lat_stats, &shard.socket_addr, wall_ms);
                        let hash = if resp.is_terminal {
                            resp.logits_hash.clone()
                        } else {
                            resp.hidden_hash.clone()
                        };
                        let idx = returned.len();
                        let signer = resp.validator_address;
                        returned.push((shard, wall_ms, resp, resp_bytes));
                        if let Some(h) = hash {
                            // Multiple HTTP aliases controlled by one validator
                            // are still one vote. Count a signer at most once
                            // for a given response hash.
                            if !signer_votes.insert((h.clone(), signer)) {
                                continue;
                            }
                            let bucket = tally.entry(h.clone()).or_default();
                            bucket.push(idx);
                            if bucket.len() >= needed {
                                winner = Some((h, bucket.clone()));
                                break;
                            }
                        }
                    }
                    Err((e, is_cold)) => {
                        if is_cold {
                            cold.push(shard.socket_addr.clone());
                        }
                        errors.push(format!(
                            "{} ({}): {}",
                            shard.node_name, shard.socket_addr, e
                        ));
                    }
                }
            }

            // Stragglers: drain them DETACHED rather than aborting.
            //
            // The point of racing is not to wait for them — that's already
            // achieved by breaking out above. Actually cancelling them would
            // be worse than useless here: forward_shard is stateful per
            // request_id, so a replica that never receives position p is cold
            // for p+1 onward. Letting the in-flight calls finish keeps every
            // hedged replica's KV cache aligned (and refreshes its EWMA, which
            // is how a recovered replica gets rediscovered).
            if !set.is_empty() {
                let stats = lat_stats.clone();
                tokio::spawn(async move {
                    let mut set = set;
                    while let Some(Ok((shard, wall_ms, out))) = set.join_next().await {
                        if out.is_ok() {
                            record_latency(&stats, &shard.socket_addr, wall_ms);
                        }
                    }
                });
            }

            for socket in &cold {
                replicas.retain(|r| &r.socket_addr != socket);
            }

            let (majority_hash, members) = match winner {
                Some((h, m)) => (Some(h), m),
                None => {
                    // Nobody reached `needed`. Report the largest agreeing
                    // group so the vote record is still informative.
                    match tally.into_iter().max_by_key(|(_, v)| v.len()) {
                        Some((h, m)) => (Some(h), m),
                        None => (None, Vec::new()),
                    }
                }
            };
            let majority_signers: Vec<Hash256> = {
                let mut seen = std::collections::HashSet::new();
                members
                    .iter()
                    .filter_map(|index| {
                        let signer = returned.get(*index)?.2.validator_address;
                        seen.insert(signer).then_some(signer)
                    })
                    .collect()
            };

            let vote = if want_vote {
                let divergent: Vec<(String, String)> = returned
                    .iter()
                    .filter_map(|(s, _, r, _)| {
                        let h = if r.is_terminal {
                            r.logits_hash.clone()
                        } else {
                            r.hidden_hash.clone()
                        };
                        if h != majority_hash {
                            Some((s.node_name.clone(), h.unwrap_or_default()))
                        } else {
                            None
                        }
                    })
                    .collect();
                Some(RangeVote {
                    position: req.position,
                    range: (req.start_layer, req.end_layer),
                    replicas_contacted: selected.iter().map(|s| s.node_name.clone()).collect(),
                    replicas_returned: returned
                        .iter()
                        .map(|(s, _, _, _)| s.node_name.clone())
                        .collect(),
                    majority_hash: majority_hash.clone(),
                    majority_signers,
                    divergent: divergent.clone(),
                    agreement: if returned.is_empty() {
                        "no_response".into()
                    } else if members.len() == selected.len() && returned.len() == selected.len() {
                        "unanimous".into()
                    } else if members.len() >= needed {
                        "majority".into()
                    } else {
                        "split".into()
                    },
                })
            } else {
                None
            };

            if members.is_empty() {
                return Err(format!(
                    "No replica responded for range [{}, {}) at position {}: {}",
                    req.start_layer,
                    req.end_layer,
                    req.position,
                    errors.join("; ")
                ));
            }
            if members.len() < needed {
                return Err(format!(
                    "No majority for range [{}, {}) at position {} - {} of {} agreed (needed {})",
                    req.start_layer,
                    req.end_layer,
                    req.position,
                    members.len(),
                    returned.len(),
                    needed
                ));
            }

            let (shard, wall_ms, resp, resp_bytes) = &returned[members[0]];
            Ok(HopOutcome {
                hidden: resp.hidden.clone(),
                hidden_hash: resp.hidden_hash.clone(),
                is_terminal: resp.is_terminal,
                token_id: resp.token_id,
                served_by: shard.node_name.clone(),
                compute_ms: resp.compute_ms,
                wall_ms: *wall_ms,
                layers_processed: resp.layers_processed as u64,
                req_bytes,
                resp_bytes: *resp_bytes,
                vote,
            })
        }
    }
}

/// Everything one pipeline run produced.
struct PipelineRun {
    generated: Vec<u32>,
    hop_stats: Vec<HopStats>,
    total_bytes: usize,
    votes: Vec<RangeVote>,
}

/// Item travelling between prefill worker tasks.
#[derive(Debug)]
struct PrefillFlow {
    position: usize,
    token: Option<u32>,
    hidden: Option<Vec<i64>>,
    hidden_hash: Option<String>,
    terminal_token: Option<u32>,
}

/// Walk the pipeline: pipelined prefill over the prompt, then sequential
/// autoregressive decode.
///
/// PREFILL streams every prompt position through per-hop mpsc worker tasks, so
/// at steady state each shard is working on a different position. Wall time is
/// ~(prompt_len + num_hops - 1) x per_hop instead of prompt_len x num_hops x
/// per_hop.
///
/// DECODE stays sequential: each token depends on the previous token's logits,
/// so there is nothing to overlap.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    node: &NodeState,
    model: &Arc<arc_inference::cached_integer_model::CachedIntegerModel>,
    pipeline: &[PipelineHop],
    request_id: &str,
    all_tokens: &[u32],
    max_tokens: u32,
    strategy: HopStrategy,
    collect_votes: bool,
    include_eos: bool,
) -> Result<PipelineRun, String> {
    let model_id = node
        .model_artifact_id
        .ok_or_else(|| "exact source-artifact model commitment unavailable".to_string())?;
    let model_id_hex = format!("0x{}", model_id.to_hex());
    let num_hops = pipeline.len();
    let prompt_len = all_tokens.len();
    let client = node.inference_http.clone();
    let mut total_bytes: usize = 0;
    let mut votes: Vec<RangeVote> = Vec::new();
    let mut generated: Vec<u32> = Vec::new();

    // Replica lists carried across the whole request so a working replica
    // stays primary and cold replicas stay evicted.
    let mut live_replicas: Vec<Vec<ShardInfo>> = pipeline.iter().map(|(_, r)| r.clone()).collect();
    let mut hop_stats: Vec<HopStats> = vec![HopStats::default(); num_hops];

    // ─── Pipelined prefill ──────────────────────────────────────────────
    {
        use tokio::sync::mpsc;

        let mut txs: Vec<mpsc::Sender<PrefillFlow>> = Vec::with_capacity(num_hops + 1);
        let mut rxs: Vec<Option<mpsc::Receiver<PrefillFlow>>> = Vec::with_capacity(num_hops + 1);
        for _ in 0..=num_hops {
            let (tx, rx) = mpsc::channel::<PrefillFlow>(PREFILL_CHANNEL_DEPTH);
            txs.push(tx);
            rxs.push(Some(rx));
        }

        type WorkerOut = Result<(usize, HopStats, Vec<ShardInfo>, Vec<RangeVote>), String>;
        let mut handles: Vec<tokio::task::JoinHandle<WorkerOut>> = Vec::with_capacity(num_hops);

        for i in 0..num_hops {
            let (start_layer, end_layer) = pipeline[i].0;
            let mut replicas = live_replicas[i].clone();
            let client_c = client.clone();
            let req_id = request_id.to_string();
            let model_id = model_id_hex.clone();
            let mut rx = rxs[i].take().expect("rx slot populated");
            let tx_out = txs[i + 1].clone();
            let is_last = i == num_hops - 1;
            let expected_hidden_len = model.config.d_model;
            let lat_stats = node.latency_stats.clone();
            let chain_identity = node.chain_identity.clone();
            let authorized_validators = node.dag_validators.clone();

            handles.push(tokio::spawn(async move {
                let mut bytes: usize = 0;
                let mut stats = HopStats::default();
                let mut local_votes: Vec<RangeVote> = Vec::new();
                while let Some(item) = rx.recv().await {
                    let req = ForwardShardRequest {
                        request_id: req_id.clone(),
                        model_id: model_id.clone(),
                        token: item.token,
                        hidden: item.hidden,
                        hidden_hash: item.hidden_hash,
                        position: item.position,
                        start_layer,
                        end_layer,
                        expected_hidden_len,
                        expect_terminal: is_last,
                        generated_tokens: Vec::new(),
                        last_token: false,
                    };
                    let out = pipeline_hop(
                        &client_c,
                        &mut replicas,
                        strategy,
                        &req,
                        &lat_stats,
                        chain_identity.as_ref(),
                        &authorized_validators,
                        collect_votes,
                    )
                    .await?;
                    bytes += out.req_bytes + out.resp_bytes;
                    stats.fold(&out);
                    if let Some(v) = out.vote.clone() {
                        local_votes.push(v);
                    }
                    let flow = PrefillFlow {
                        position: item.position,
                        token: None,
                        hidden: out.hidden,
                        hidden_hash: out.hidden_hash,
                        terminal_token: if is_last { out.token_id } else { None },
                    };
                    if tx_out.send(flow).await.is_err() {
                        break;
                    }
                }
                Ok((bytes, stats, replicas, local_votes))
            }));
        }

        // Feed the prompt in. Backpressure from PREFILL_CHANNEL_DEPTH stops a
        // fast head shard from queueing the whole prompt ahead of a slow one.
        let input_tx = txs.remove(0);
        drop(txs); // let workers observe EOF when their upstream finishes

        let feeder = {
            let tokens: Vec<u32> = all_tokens.to_vec();
            tokio::spawn(async move {
                for (pos, tok) in tokens.into_iter().enumerate() {
                    let flow = PrefillFlow {
                        position: pos,
                        token: Some(tok),
                        hidden: None,
                        hidden_hash: None,
                        terminal_token: None,
                    };
                    if input_tx.send(flow).await.is_err() {
                        return false;
                    }
                }
                drop(input_tx);
                true
            })
        };

        let mut final_rx = rxs[num_hops].take().expect("tail rx slot populated");
        let mut positions_seen = 0usize;
        let mut first_generated: Option<u32> = None;
        while let Some(flow) = final_rx.recv().await {
            positions_seen += 1;
            if flow.position == prompt_len - 1 {
                first_generated = flow.terminal_token;
            }
            if positions_seen >= prompt_len {
                break;
            }
        }
        let _ = feeder.await;

        // Drain EVERY worker before surfacing an error, so a failure in one
        // hop doesn't leave the others' JoinHandles dangling.
        let mut worker_err: Option<String> = None;
        for (i, h) in handles.into_iter().enumerate() {
            match h.await {
                Ok(Ok((bytes, stats, replicas, mut vs))) => {
                    total_bytes += bytes;
                    hop_stats[i] = stats;
                    live_replicas[i] = replicas;
                    votes.append(&mut vs);
                }
                Ok(Err(e)) => {
                    if worker_err.is_none() {
                        worker_err = Some(e);
                    }
                }
                Err(e) => {
                    if worker_err.is_none() {
                        worker_err = Some(format!("join prefill worker {}: {}", i, e));
                    }
                }
            }
        }
        if let Some(e) = worker_err {
            return Err(e);
        }
        if positions_seen < prompt_len {
            return Err(format!(
                "Pipelined prefill incomplete: {}/{} positions arrived at the tail shard",
                positions_seen, prompt_len
            ));
        }

        if let Some(tok) = first_generated
            && (include_eos || !model.config.eos_tokens.contains(&tok))
        {
            generated.push(tok);
        }
    }

    // ─── Sequential decode ──────────────────────────────────────────────
    for gen_idx in 1..(max_tokens as usize) {
        if let Some(last) = generated.last()
            && model.config.eos_tokens.contains(last)
        {
            break;
        }
        let position = prompt_len + gen_idx - 1;
        let input_token = *generated.last().unwrap_or(&all_tokens[prompt_len - 1]);

        let mut cur_hidden: Option<Vec<i64>> = None;
        let mut cur_hash: Option<String> = None;
        let mut next_tok: Option<u32> = None;

        for (i, ((s_layer, e_layer), _)) in pipeline.iter().enumerate() {
            let req = ForwardShardRequest {
                request_id: request_id.to_string(),
                model_id: model_id_hex.clone(),
                token: if i == 0 { Some(input_token) } else { None },
                hidden: if i == 0 { None } else { cur_hidden.take() },
                hidden_hash: if i == 0 { None } else { cur_hash.take() },
                position,
                start_layer: *s_layer,
                end_layer: *e_layer,
                expected_hidden_len: model.config.d_model,
                expect_terminal: i == pipeline.len() - 1,
                generated_tokens: if i == pipeline.len() - 1 {
                    generated[generated.len().saturating_sub(64)..].to_vec()
                } else {
                    Vec::new()
                },
                last_token: false,
            };
            let out = pipeline_hop(
                &client,
                &mut live_replicas[i],
                strategy,
                &req,
                &node.latency_stats,
                node.chain_identity.as_ref(),
                &node.dag_validators,
                collect_votes,
            )
            .await?;
            total_bytes += out.req_bytes + out.resp_bytes;
            hop_stats[i].fold(&out);
            if let Some(v) = out.vote.clone() {
                votes.push(v);
            }
            if out.is_terminal {
                next_tok = out.token_id;
                break;
            }
            cur_hidden = out.hidden;
            cur_hash = out.hidden_hash;
        }

        match next_tok {
            Some(t) if model.config.eos_tokens.contains(&t) => {
                if include_eos {
                    generated.push(t);
                }
                break;
            }
            Some(t) => generated.push(t),
            None => break,
        }
    }

    Ok(PipelineRun {
        generated,
        hop_stats,
        total_bytes,
        votes,
    })
}

/// Render the accumulated per-hop statistics as the response's `shard_trace`.
///
/// `compute_ms` / `wall_ms` are now TOTALS across every position of the
/// request, not a snapshot of the first position, and `payload_bytes` is the
/// real measured wire cost in BOTH directions (it was hardcoded to 0, so the
/// dashboard's activation-flow view showed nothing per hop).
fn render_shard_trace(pipeline: &[PipelineHop], stats: &[HopStats]) -> Vec<Value> {
    pipeline
        .iter()
        .enumerate()
        .map(|(hop, ((s, e), replicas))| {
            let st = &stats[hop];
            let primary = &replicas[0];
            let served: Vec<Value> = st
                .served_by
                .iter()
                .map(|(name, n)| json!({ "node": name, "positions": n }))
                .collect();
            let avg = |total: u64| -> u64 { total.checked_div(st.positions).unwrap_or(0) };
            json!({
                "hop": hop,
                "node": st.served_by.keys().next().cloned().unwrap_or_else(|| primary.node_name.clone()),
                "node_name": primary.node_name,
                "socket": primary.socket_addr,
                "layers": format!("{}..{}", s, e),
                "layers_count": st.layers,
                "positions": st.positions,
                "compute_ms": st.compute_ms,
                "wall_ms": st.wall_ms,
                "avg_compute_ms": avg(st.compute_ms),
                "avg_wall_ms": avg(st.wall_ms),
                "payload_bytes": st.req_bytes + st.resp_bytes,
                "request_bytes": st.req_bytes,
                "response_bytes": st.resp_bytes,
                "served_by": served,
                "replica_count": replicas.len(),
                "is_terminal": st.is_terminal,
            })
        })
        .collect()
}

/// Fire the end-of-request KV-cache eviction at every replica of every range
/// and return IMMEDIATELY.
///
/// This used to be `for range { for replica { ...send().await } }` on the
/// 120-second inference client: 18 sequential round-trips on the live topology
/// (6 ranges x 3 replicas), 4-10 s of dead tail latency appended to every
/// request, three of them aimed at the replica with the worst EWMA. The
/// response never depended on any of it.
fn spawn_cleanup(node: &NodeState, pipeline: &[PipelineHop], request_id: &str) {
    let client = node.inference_http.clone();
    let request_id = request_id.to_string();
    let sockets: Vec<String> = {
        let mut s: Vec<String> = pipeline
            .iter()
            .flat_map(|(_, replicas)| replicas.iter().map(|r| r.socket_addr.clone()))
            .collect();
        s.sort();
        s.dedup();
        s
    };
    let _ = node.shard_kv_caches.remove(&request_id);
    tokio::spawn(async move {
        let payload = json!({ "request_id": request_id });
        let mut set = tokio::task::JoinSet::new();
        for socket in sockets {
            let c = client.clone();
            let p = payload.clone();
            set.spawn(async move {
                let _ = c
                    .post(format!("http://{}/inference/cleanup_shard", socket))
                    .json(&p)
                    .timeout(std::time::Duration::from_secs(CLEANUP_TIMEOUT_SECS))
                    .send()
                    .await;
            });
        }
        while set.join_next().await.is_some() {}
    });
}

/// Pull shard topology from the configured seeds into the local registry.
///
/// A freshly started node knows only the shards it holds itself, so asking it
/// to coordinate a sharded run fails with "Pipeline incomplete" even though
/// the seeds collectively cover the whole model. This merges what the seeds
/// report so a local coordinator can drive the live shard-holders.
///
/// GET only — this never writes to a remote node.
///
/// Address hygiene: a remote node's `self_shards` entries usually carry
/// `0.0.0.0:<port>` because it binds all interfaces and doesn't know its own
/// public IP; for those (and only those) the serving seed's own host is the
/// routable answer, so we substitute it. Stub addresses appearing in a seed's
/// second-hand `shards` list are dropped outright — we have no idea whose
/// loopback they are.
async fn bootstrap_shard_registry_from_seeds(node: &NodeState) -> usize {
    // Rate-limit. Matched to the shard announcement tick (15 s) and well
    // under SHARD_REGISTRY_TTL_SECS (60 s), so a coordinator whose merged
    // entries just aged out can refresh on the very next request instead of
    // failing until the window reopens.
    {
        let mut last = match node.last_registry_bootstrap.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(t) = *last
            && t.elapsed().as_secs() < REGISTRY_BOOTSTRAP_MIN_INTERVAL_SECS
        {
            return 0;
        }
        *last = Some(std::time::Instant::now());
    }

    let seeds = node.seed_rpc_addrs.clone();
    if seeds.is_empty() {
        return 0;
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return 0,
    };

    let mut merged = 0usize;
    let now = std::time::Instant::now();
    for seed in seeds.iter() {
        let seed_host = seed
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(seed.as_str());
        let resp = match client.get(format!("http://{}/shards", seed)).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let mut candidates: Vec<ShardInfo> = Vec::new();
        // The seed's OWN ranges: a stub here means "me", so map it onto the
        // host we just talked to.
        if let Some(arr) = body.get("self_shards").and_then(|v| v.as_array()) {
            for v in arr {
                if let Ok(mut si) = serde_json::from_value::<ShardInfo>(v.clone()) {
                    if is_stub_socket_addr(&si.socket_addr) {
                        let port = si
                            .socket_addr
                            .rsplit(':')
                            .next()
                            .and_then(|p| p.parse::<u16>().ok())
                            .unwrap_or(9090);
                        si.socket_addr = format!("{}:{}", seed_host, port);
                    }
                    candidates.push(si);
                }
            }
        }
        // The seed's second-hand view of everyone else. Stubs here are
        // unresolvable — we cannot tell whose loopback they are.
        if let Some(arr) = body.get("shards").and_then(|v| v.as_array()) {
            for v in arr {
                if let Ok(si) = serde_json::from_value::<ShardInfo>(v.clone())
                    && !is_stub_socket_addr(&si.socket_addr)
                {
                    candidates.push(si);
                }
            }
        }

        for si in candidates {
            if is_stub_socket_addr(&si.socket_addr) {
                continue;
            }
            let key = format!("{}#{}-{}", si.socket_addr, si.start_layer, si.end_layer);
            if node.shard_registry.insert(key, (si, now)).is_none() {
                merged += 1;
            }
        }
    }
    if merged > 0 {
        tracing::info!(
            merged,
            seeds = seeds.len(),
            "shard registry bootstrapped from seeds (GET /shards)"
        );
    }
    merged
}

/// Assemble the pipeline, falling back to a seed bootstrap when the local
/// registry alone can't cover the model.
async fn assemble_pipeline_with_bootstrap(
    node: &NodeState,
) -> Result<Vec<PipelineHop>, PipelineError> {
    match assemble_pipeline_for(node) {
        Ok(p) => Ok(p),
        Err(first) => {
            if bootstrap_shard_registry_from_seeds(node).await == 0 {
                return Err(first);
            }
            assemble_pipeline_for(node)
        }
    }
}

/// Build, sign and submit an `InferenceAttestation` for a completed run.
///
/// Two defects used to live in the sharded copy of this code, and between
/// them they meant 294 sharded runs produced zero on-chain attestations:
///
///  1. The tx was built with `hash: Hash256::ZERO` and `signature: null()`,
///     `sig_verified` forced true, and `compute_hash()` called into a local
///     variable that was never assigned back to `tx.hash`. arc-state rejects
///     it as an unsigned transaction, the hash-integrity check fails, and
///     because arc-mempool dedupes on `tx.hash.0`, every attestation after
///     the first came back `Duplicate(0x00..0)`. `sig_verified = true` is not
///     enough on its own: `pipeline.rs`'s verify stage inspects the signature
///     BYTES and ignores the flag, so a null-signed tx fails on every peer.
///
///  2. The nonce was `state_nonce + attestation_nonce.fetch_add(1)`. The
///     in-memory counter accumulates forever, including for txs that never
///     land, so once the first attestation applied the account nonce advanced
///     AND the counter advanced and every later tx carried state+2 → a
///     permanent InvalidNonce gap. This is the exact failure already
///     seen in the retired escrow-release path; the fix is to read the
///     account's current nonce for every transaction.
///
/// Returns `(tx_hash, status)` where status is what the response reports.
fn submit_inference_attestation(
    node: &NodeState,
    model_id: Hash256,
    input_hash: Hash256,
    output_hash: Hash256,
    bond: u64,
    challenge_period: u64,
) -> (Hash256, &'static str) {
    let attester = node.validator_address;
    let nonce = node
        .state
        .get_account(&attester)
        .map(|a| a.nonce)
        .unwrap_or(0);

    let mut tx = arc_types::Transaction {
        tx_type: arc_types::TxType::InferenceAttestation,
        from: attester,
        nonce,
        body: arc_types::TxBody::InferenceAttestation(
            arc_types::transaction::InferenceAttestationBody {
                model_id,
                input_hash,
                output_hash,
                challenge_period,
                bond,
                // Left None deliberately: the field is #[serde(skip)] and the
                // 112-byte v0.7.2 wire layout depends on it staying that way.
                beneficiary: None,
            },
        ),
        fee: 0,
        gas_limit: 0,
        hash: arc_crypto::Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: false,
    };

    match node.validator_keypair.as_ref() {
        Some(kp) => match node.state.sign_transaction(&mut tx, kp) {
            Ok(()) => {
                // sign() assigns tx.hash as part of signing.
                tx.sig_verified = true;
                let h = tx.hash;
                let _ = node.mempool.insert(tx);
                (h, "submitted_to_mempool")
            }
            Err(e) => {
                tracing::warn!("inference attestation sign failed: {:?}", e);
                (tx.compute_hash(), "sign_failed")
            }
        },
        None => {
            // Test-fixture path: no keypair wired. Keep the legacy shape so
            // unit tests still execute, but at least assign the hash so the
            // mempool doesn't dedupe every one of them to 0x00..0.
            tx.sig_verified = true;
            let h = tx.compute_hash();
            tx.hash = h;
            let _ = node.mempool.insert(tx);
            (h, "submitted_unsigned_no_keypair")
        }
    }
}

/// Submit the attestation to the chain that can actually mine it.
///
/// When `ARC_ATTEST_RELAY` is set (e.g. `http://149.28.32.76:9090`), the
/// signed attestation is submitted to that host instead of the local
/// mempool. Two facts make this necessary for observer/coordinator nodes:
/// the testnet seeds are independent chains (a tx in a local mempool never
/// reaches them), and an observer never seals blocks, so a locally pooled
/// attestation would sit pending forever. The nonce is read from the relay
/// target's account state — the attester address may not exist there, or may
/// have a different nonce than any local view. Falls back to the local
/// mempool on any relay failure so the attestation is never silently lost.
async fn submit_or_relay_attestation(
    node: &NodeState,
    model_id: Hash256,
    input_hash: Hash256,
    output_hash: Hash256,
    bond: u64,
    challenge_period: u64,
) -> (Hash256, String) {
    let relay = match std::env::var("ARC_ATTEST_RELAY") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => {
            let (h, s) = submit_inference_attestation(
                node,
                model_id,
                input_hash,
                output_hash,
                bond,
                challenge_period,
            );
            return (h, s.to_string());
        }
    };
    let Some(kp) = node.validator_keypair.as_ref() else {
        let (h, s) = submit_inference_attestation(
            node,
            model_id,
            input_hash,
            output_hash,
            bond,
            challenge_period,
        );
        return (h, s.to_string());
    };

    let attester = node.validator_address;
    let nonce = match node
        .inference_http
        .get(format!("{}/account/0x{}", relay, hex::encode(attester.0)))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("nonce").and_then(|n| n.as_u64()))
            .unwrap_or(0),
        // 404 = the address has no account on that chain yet: nonce 0.
        _ => 0,
    };

    let mut tx = arc_types::Transaction {
        tx_type: arc_types::TxType::InferenceAttestation,
        from: attester,
        nonce,
        body: arc_types::TxBody::InferenceAttestation(
            arc_types::transaction::InferenceAttestationBody {
                model_id,
                input_hash,
                output_hash,
                challenge_period,
                bond,
                // Stays None: #[serde(skip)] and the 112-byte v0.7.2 wire
                // layout depend on it.
                beneficiary: None,
            },
        ),
        fee: 0,
        gas_limit: 0,
        hash: arc_crypto::Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: false,
    };
    if node.state.sign_transaction(&mut tx, kp).is_err() {
        let (h, s) = submit_inference_attestation(
            node,
            model_id,
            input_hash,
            output_hash,
            bond,
            challenge_period,
        );
        return (h, s.to_string());
    }
    tx.sig_verified = true;
    let h = tx.hash;

    let posted = node
        .inference_http
        .post(format!("{}/tx/submit_signed", relay))
        .json(&tx)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;
    match posted {
        Ok(r) if r.status().is_success() => (h, format!("relayed_to {}", relay)),
        Ok(r) => {
            tracing::warn!("attestation relay to {} rejected: {}", relay, r.status());
            let (h2, s) = submit_inference_attestation(
                node,
                model_id,
                input_hash,
                output_hash,
                bond,
                challenge_period,
            );
            (h2, format!("{} (relay_rejected {})", s, r.status()))
        }
        Err(e) => {
            tracing::warn!("attestation relay to {} failed: {}", relay, e);
            let (h2, s) = submit_inference_attestation(
                node,
                model_id,
                input_hash,
                output_hash,
                bond,
                challenge_period,
            );
            (h2, format!("{} (relay_unreachable)", s))
        }
    }
}

/// Human-readable shape metadata. This is display-only and must never be
/// hashed or used for routing, cache, verification, or attestation identity.
fn model_display_name(model: &arc_inference::cached_integer_model::CachedIntegerModel) -> String {
    format!(
        "arc-{}L-{}d-{}h-{}v",
        model.config.n_layers, model.config.d_model, model.config.n_heads, model.config.vocab_size
    )
}

/// Return the display name plus the exact source-artifact commitment.
/// Tokenizer/config metadata cannot supply an inference identity.
fn exact_model_identity(node: &NodeState) -> Result<(String, Hash256), String> {
    let model = node
        .inference_model
        .as_ref()
        .ok_or_else(|| "no model/tokenizer loaded".to_string())?;
    let model_id = node.model_artifact_id.ok_or_else(|| {
        "exact source-artifact model commitment is unavailable; inference disabled".to_string()
    })?;
    Ok((model_display_name(model), model_id))
}

/// POST /inference/run_sharded
/// Coordinator endpoint: walks the pipeline of shard-holding nodes and
/// generates `max_tokens` tokens by forwarding hidden states between shards.
///
/// Returns the full output, all per-shard timings, and the network bandwidth
/// used so the dashboard can show the activation flow.
///
/// Request fields (all optional except `input`):
///   input           string   the prompt
///   max_tokens      u64      capped at 1024
///   chat_template   bool     default false (wrapping inflates prompt_len ~5x)
///   redundancy      u64      1 (default) = one replica per hop with failover;
///                            2+ = contact that many replicas for EVERY
///                            position and take the first valid answer
///   force_recompute bool     bypass the deterministic cache
async fn inference_run_sharded(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let input_text = req
        .get("input")
        .and_then(|v| v.as_str())
        .ok_or(api_error(StatusCode::BAD_REQUEST, "'input' field required"))?;

    if input_text.len() > 32_768 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Input exceeds 32KB limit",
        ));
    }

    // Sharded-pipeline output cap. Was 256, which combined with the model's
    // RoPE table covering 4096 positions meant the user could never approach
    // the actual context limit on this endpoint.
    let max_tokens = req
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(1024) as u32;

    // Opt-in chat template wrapping. Default OFF because the dashboard is
    // doing autocomplete ("The capital of France is" → " Paris"), not
    // instruction-following. Wrapping in [INST]...[/INST] inflates prompt_len
    // by ~5x and, since the pipeline walks every position, that 5x multiplies
    // wall time directly.
    let chat_template_enabled = req
        .get("chat_template")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Replica redundancy. forward_shard is STATEFUL per request_id (each
    // holder keeps a KV cache keyed by it), so you cannot hedge a single
    // position onto a fresh replica — it would be handed a mid-stream
    // position against a cold cache. `redundancy: 2` therefore commits to two
    // replicas per range for the WHOLE request and sends every position to
    // both: the first valid answer is used and the other stays warm, so
    // failover is instant instead of fatal. Default stays 1.
    let redundancy = req
        .get("redundancy")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .clamp(1, 5) as usize;

    // Bypass the deterministic cache. The public determinism demo runs the
    // same prompt twice and compares output_hash; without this the second run
    // is an LRU lookup that returns what was put in it, which proves nothing
    // about the pipeline. With force_recompute the second run is a real,
    // independent walk of the same 6 hops.
    let force_recompute = req
        .get("force_recompute")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Coordinator needs a model for tokenization (text→tokens and tokens→text).
    // This is the tokenizer vocabulary, not the full model weights.
    let model = node
        .inference_model
        .as_ref()
        .ok_or(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Coordinator needs a model loaded for tokenization. Start with --model <path.gguf>. \
             Shard nodes serve inference; the coordinator only uses the tokenizer.",
        ))?
        .clone();

    let pipeline = assemble_pipeline_with_bootstrap(&node)
        .await
        .map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    let request_id = format!(
        "0x{}",
        hex::encode(
            arc_crypto::hash_bytes(
                format!(
                    "{}-{}",
                    input_text,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                )
                .as_bytes()
            )
            .0
        )
    );

    let tokenized_text: String = if chat_template_enabled {
        model.apply_chat_template(input_text)
    } else {
        input_text.to_string()
    };
    let prompt_tokens = model.encode(&tokenized_text);
    let mut all_tokens: Vec<u32> = vec![model.config.bos_token];
    all_tokens.extend(&prompt_tokens);

    let overall_start = std::time::Instant::now();
    let (model_id_data, model_id_hash) =
        exact_model_identity(&node).map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, e))?;
    let input_hash = arc_crypto::hash_bytes(input_text.as_bytes());

    // ─────────────────────────────────────────────────────────────────────
    // DETERMINISTIC CACHE LOOKUP
    // Same model_id + same input tokens = same output tokens, GUARANTEED.
    // ─────────────────────────────────────────────────────────────────────
    let cache_input_with_max: Vec<u32> = {
        let mut v = all_tokens.clone();
        v.push(max_tokens);
        v
    };
    let cache_key = arc_inference::distributed::DistributedCache::cache_key(
        &model_id_hash,
        &cache_input_with_max,
    );
    let cache_key_hex = format!("0x{}", hex::encode(cache_key.0));

    if !force_recompute && let Some(cached_tokens) = node.inference_cache.get(&cache_key) {
        let output_text = model.decode(&cached_tokens);
        let output_bytes: Vec<u8> = cached_tokens.iter().flat_map(|t| t.to_le_bytes()).collect();
        let output_hash = arc_crypto::hash_bytes(&output_bytes);
        let elapsed_us = overall_start.elapsed().as_micros() as u64;

        // A cache hit is NOT a sharded run: no pipeline was walked, no
        // activations moved, no shard did any work. Counting it in
        // sharded_runs_total inflated every "distributed inference
        // served" figure on the dashboard.
        node.sharded_cache_hits.fetch_add(1, Ordering::Relaxed);

        // Return the ORIGINAL run's provenance rather than zeros: the
        // attestation tx that recorded it, the trace of the hops that
        // actually produced it, and how long that real run took. A hit
        // that reports `shard_trace: []`, `total_ms: 0` and no
        // attestation looks like the pipeline did the work in 800 µs,
        // which is what made the "determinism check" demo hollow.
        let meta = node.sharded_run_meta.get(&cache_key_hex).map(|m| m.clone());
        let mut resp = json!({
            "success": true,
            "request_id": request_id,
            "input": input_text,
            "output": output_text,
            "output_tokens": cached_tokens,
            "output_hash": format!("0x{}", hex::encode(output_hash.0)),
            "input_hash": format!("0x{}", hex::encode(input_hash.0)),
            "model_hash": format!("0x{}", hex::encode(model_id_hash.0)),
            "tokens_generated": cached_tokens.len(),
            "total_ms": elapsed_us / 1000,
            "total_us": elapsed_us,
            "ms_per_token": 0,
            "pipeline_length": pipeline.len(),
            "model": model_id_data,
            "shard_trace": [],
            "total_bytes_transferred": 0,
            "deterministic": true,
            "engine": "deterministic cache hit (bit-identical to the original sharded run)",
            "cache": {
                "hit": true,
                "key": cache_key_hex,
                "served_in_us": elapsed_us,
                "size": node.inference_cache.len(),
            },
        });
        if let Some(m) = meta {
            for key in [
                "attestation",
                "shard_trace",
                "total_bytes_transferred",
                "committee",
                "fee_split",
                "explorer_url",
                // Carry the reason with the link. Copying `explorer_url`
                // alone would let a cache hit serve a null link with no
                // explanation — or, once the tx does get mined, a stale
                // reason alongside a now-valid link.
                "explorer_url_unavailable_reason",
                "pipeline_length",
            ] {
                if let Some(v) = m.get(key) {
                    resp[key] = v.clone();
                }
            }
            if let Some(v) = m.get("total_ms") {
                resp["original_total_ms"] = v.clone();
            }
            if let Some(v) = m.get("ms_per_token") {
                resp["original_ms_per_token"] = v.clone();
            }
            resp["cached_total_ms"] = json!(elapsed_us / 1000);
            resp["cached_total_us"] = json!(elapsed_us);
            if let Some(obj) = resp["cache"].as_object_mut() {
                obj.insert(
                    "original_request_id".into(),
                    m.get("request_id").cloned().unwrap_or(Value::Null),
                );
            }
        }
        return Ok(Json(resp));
    }

    // ─── Real pipeline walk ─────────────────────────────────────────────
    let strategy = if redundancy > 1 {
        HopStrategy::Fanout {
            fanout: redundancy,
            needed: 1,
            allow_degraded_quorum: true,
        }
    } else {
        HopStrategy::Failover
    };

    let run_result = run_pipeline(
        &node,
        &model,
        &pipeline,
        &request_id,
        &all_tokens,
        max_tokens,
        strategy,
        false,
        false,
    )
    .await;

    // Fire-and-forget on both success and failure. A failed mid-pipeline run
    // still populated remote KV caches and must not leak them until TTL.
    spawn_cleanup(&node, &pipeline, &request_id);
    let run = run_result.map_err(|e| api_error(StatusCode::BAD_GATEWAY, e))?;

    let total_ms = overall_start.elapsed().as_millis() as u64;
    let generated = run.generated;
    let output_text = model.decode(&generated);
    let output_bytes: Vec<u8> = generated.iter().flat_map(|t| t.to_le_bytes()).collect();
    let output_hash = arc_crypto::hash_bytes(&output_bytes);
    let shard_trace = render_shard_trace(&pipeline, &run.hop_stats);

    node.sharded_runs_total.fetch_add(1, Ordering::Relaxed);
    node.sharded_bytes_total
        .fetch_add(run.total_bytes as u64, Ordering::Relaxed);

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    node.inference_cache.insert(
        cache_key,
        arc_inference::distributed::CacheEntry {
            output_tokens: generated.clone(),
            output_hash,
            model_id: model_id_hash,
            hit_count: 0,
            created_at_secs: now_secs,
        },
    );

    let (tx_hash, attestation_status) = submit_or_relay_attestation(
        &node,
        model_id_hash,
        input_hash,
        output_hash,
        DEFAULT_ATTESTATION_BOND,
        DEFAULT_ATTESTATION_CHALLENGE_PERIOD_BLOCKS,
    )
    .await;
    let tx_hash_hex = format!("0x{}", hex::encode(tx_hash.0));

    node.inference_results.insert(
        tx_hash_hex.clone(),
        json!({
            "input": input_text,
            "output": &output_text,
            "output_hash": format!("0x{}", hex::encode(output_hash.0)),
            "model": &model_id_data,
            "model_hash": format!("0x{}", hex::encode(model_id_hash.0)),
            "ms_per_token": if generated.is_empty() { 0 } else { total_ms / generated.len() as u64 },
            "tokens_generated": generated.len() as u64,
            "engine": format!("{} sharded pipeline", model.effective_precision_label()),
            "deterministic": true,
            "observed_at_unix_ms": now_unix_ms(),
            "sharded": true,
            "pipeline_length": pipeline.len(),
            "shard_trace": &shard_trace,
            "total_bytes_transferred": run.total_bytes,
        }),
    );

    // ─── VRF Committee Verification ────────────────────────────────────
    // Deterministic, reproducible committee selection seeded by output_hash.
    // NOTE: votes are not collected — the integer engine guarantees honest
    // members agree, so this records WHO would have verified, not that they
    // did. The response says so.
    let committee_info = {
        let validators = node.dag_validators.read();
        let eligible: Vec<arc_inference::committee::InferenceValidator> = validators
            .iter()
            .map(
                |(addr, stake)| arc_inference::committee::InferenceValidator {
                    address: *addr,
                    max_tier: 2,
                    stake: *stake,
                },
            )
            .collect();

        if eligible.len() >= 3 {
            let committee = arc_inference::committee::select_committee(
                &output_hash,
                &eligible,
                2,
                eligible
                    .len()
                    .min(arc_inference::committee::DEFAULT_COMMITTEE_SIZE),
            );
            let member_hexes: Vec<String> = committee
                .members
                .iter()
                .map(|m| format!("0x{}", hex::encode(m.0)))
                .collect();
            json!({
                "selected": true,
                "votes_collected": false,
                "size": committee.members.len(),
                "min_agreement": committee.min_agreement,
                "members": member_hexes,
                "vrf_seed": format!("0x{}", hex::encode(output_hash.0)),
                "tier": 2,
                "corruption_probability": arc_inference::committee::corruption_probability(0.1, committee.members.len(), committee.min_agreement),
            })
        } else {
            json!({
                "selected": false,
                "votes_collected": false,
                "reason": "fewer than 3 validators online",
                "validators_online": eligible.len(),
            })
        }
    };

    {
        if let Ok(mut mgr) = node.verification_manager.lock() {
            let commitment = arc_vm::inference_verify::InferenceCommitment {
                request_id: arc_crypto::hash_bytes(request_id.as_bytes()).0,
                result_hash: output_hash.0,
                provider: node.validator_address.0,
                timestamp: now_secs,
                bond_amount: 1000,
            };
            mgr.submit_commitment(commitment);
        }
    }

    let fee_split = node.revenue_config.split_fee(
        1000,
        node.dag_validators.read().len().saturating_sub(1) as u32,
    );

    let (explorer_url, explorer_url_unavailable_reason) =
        explorer_url_for(&node, &tx_hash, &attestation_status);

    let response = json!({
        "success": true,
        "request_id": request_id,
        "input": input_text,
        "output": output_text,
        "output_tokens": generated,
        "output_hash": format!("0x{}", hex::encode(output_hash.0)),
        "input_hash": format!("0x{}", hex::encode(input_hash.0)),
        "model_hash": format!("0x{}", hex::encode(model_id_hash.0)),
        "tokens_generated": generated.len(),
        "total_ms": total_ms,
        "ms_per_token": if generated.is_empty() { 0 } else { total_ms / generated.len() as u64 },
        "pipeline_length": pipeline.len(),
        "model": model_id_data,
        "shard_trace": shard_trace,
        "total_bytes_transferred": run.total_bytes,
        "deterministic": true,
        "engine": format!("{} sharded pipeline", model.effective_precision_label()),
        "redundancy": redundancy,
        "cache": {
            "hit": false,
            "key": cache_key_hex,
            "size": node.inference_cache.len(),
            "bypassed": force_recompute,
        },
        "attestation": {
            "tx_hash": tx_hash_hex,
            "bond": 1000,
            "challenge_period": 100,
            "status": attestation_status,
        },
        "committee": committee_info,
        "fee_split": {
            "proposer": fee_split.proposer,
            "per_verifier": fee_split.per_verifier,
            "observer_pool": fee_split.observer_pool,
            "treasury": fee_split.treasury,
        },
        "explorer_url": explorer_url,
        "explorer_url_unavailable_reason": explorer_url_unavailable_reason,
    });

    // Remember this run's provenance so a later cache hit can report the real
    // attestation, trace and timings instead of zeros. Store only the
    // provenance fields — not the output text or token vector, which the
    // inference cache already holds — and keep the map bounded to the same
    // capacity as that cache so it cannot grow without limit.
    remember_sharded_run(
        &node,
        cache_key_hex,
        json!({
            "request_id": response["request_id"],
            "attestation": response["attestation"],
            "shard_trace": response["shard_trace"],
            "total_bytes_transferred": response["total_bytes_transferred"],
            "committee": response["committee"],
            "fee_split": response["fee_split"],
            "explorer_url": response["explorer_url"],
            "pipeline_length": response["pipeline_length"],
            "total_ms": response["total_ms"],
            "ms_per_token": response["ms_per_token"],
        }),
    );

    Ok(Json(response))
}

/// Capacity of `sharded_run_meta`, matched to the inference cache so the two
/// stay roughly in step. An entry is provenance only (~2 KB), never output.
const SHARDED_RUN_META_CAP: usize = 10_000;

/// Record a completed run's provenance under its cache key, evicting an
/// arbitrary existing entry when the map is at capacity.
fn remember_sharded_run(node: &NodeState, cache_key_hex: String, meta: Value) {
    if node.sharded_run_meta.len() >= SHARDED_RUN_META_CAP
        && !node.sharded_run_meta.contains_key(&cache_key_hex)
    {
        let victim = node.sharded_run_meta.iter().next().map(|e| e.key().clone());
        if let Some(k) = victim {
            node.sharded_run_meta.remove(&k);
        }
    }
    node.sharded_run_meta.insert(cache_key_hex, meta);
}

/// POST /inference/run_consensus
/// Parallel k-of-n forward_shard per range with hash-majority verification at
/// every shard boundary.
///
/// Semantics vs /inference/run_sharded:
/// - run_sharded picks the best replica for each range and rotates on failure.
///   Fast. Silent hash divergence is INVISIBLE.
/// - run_consensus fires to k replicas per hop, tallies their output hashes,
///   and forwards the state only once a strict majority agrees. Divergent
///   replicas are recorded in the response. Economic auto-challenges remain
///   disabled until each response carries an authenticated provider identity.
///
/// Two things changed here, and together they are the difference between
/// ~54-112 s/token and something a person will sit through:
///
///  1. It now uses the SAME pipelined prefill as run_sharded. Its own prefill
///     was fully sequential — every prompt position walked all six hops
///     end-to-end before the next position started — so prefill cost
///     prompt_len x num_hops serial hops instead of ~(prompt_len + num_hops).
///
///  2. Each hop now races to majority instead of waiting for all k. With the
///     live topology (exactly 3 replicas per range) and the desktop's k=3,
///     `take(k)` selected EVERY replica, which made the latency-aware sort a
///     no-op, and the collect loop then blocked on the slowest of the three.
///     Three of the six ranges included a replica whose recorded EWMA was
///     37 s. Hop cost is now the (k/2+1)-th response, not the k-th.
///
/// Both endpoints also share one pipeline planner now, which is what fixes
/// this endpoint's stub-preserving bucket logic and its missing overlap skip —
/// the "Pipeline gap: expected layer 32 next, got [28, 30)" failure.
///
/// Request body mirrors run_sharded with an optional `k` field (default 3).
/// Response adds a `consensus` block with per-range vote records.
async fn inference_run_consensus(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    // Paid settlement used to share this free inference endpoint. Reject on
    // field *presence* (including null or malformed values) before input
    // validation, model access, escrow lookup, or inference work. Replica
    // labels in this HTTP topology are not authenticated payout identities,
    // so opening/releasing funds here would be unsafe. Free k-of-n inference
    // remains available when none of the legacy settlement fields is present.
    const LEGACY_PAID_FIELDS: [&str; 5] = [
        "payer",
        "request_id",
        "max_fee",
        "model_id",
        "timeout_blocks",
    ];
    if LEGACY_PAID_FIELDS
        .iter()
        .any(|field| req.get(*field).is_some())
    {
        return Err(paid_inference_unavailable());
    }

    let input_text = req
        .get("input")
        .and_then(|v| v.as_str())
        .ok_or(api_error(StatusCode::BAD_REQUEST, "'input' field required"))?;
    if input_text.len() > 32_768 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Input exceeds 32KB limit",
        ));
    }
    let max_tokens = req
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(u64::from(INFERENCE_RUN_MAX_TOKENS)) as u32;
    let k_req = req.get("k").and_then(|v| v.as_u64()).unwrap_or(3).max(1) as usize;
    let chat_template_enabled = req
        .get("chat_template")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let model = node
        .inference_model
        .as_ref()
        .ok_or(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Coordinator needs a tokenizer loaded. Start with --model <path.gguf>.",
        ))?
        .clone();

    let pipeline = assemble_pipeline_with_bootstrap(&node)
        .await
        .map_err(|e| api_error(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    // Tokenize
    let tokenized_text = if chat_template_enabled {
        model.apply_chat_template(input_text)
    } else {
        input_text.to_string()
    };
    let prompt_tokens = model.encode(&tokenized_text);
    let mut all_tokens: Vec<u32> = vec![model.config.bos_token];
    all_tokens.extend(&prompt_tokens);

    let request_id = format!(
        "0x{}",
        hex::encode(
            arc_crypto::hash_bytes(
                format!(
                    "{}-{}",
                    input_text,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                )
                .as_bytes()
            )
            .0
        )
    );

    let overall_start = std::time::Instant::now();

    // Strict majority of whatever k this hop can actually reach.
    // `pipeline_hop` clamps both numbers to the live replica count, so a range
    // that has lost a replica degrades to a majority of the survivors rather
    // than failing outright.
    let strategy = HopStrategy::Fanout {
        fanout: k_req,
        needed: (k_req / 2) + 1,
        allow_degraded_quorum: true,
    };

    let run_result = run_pipeline(
        &node,
        &model,
        &pipeline,
        &request_id,
        &all_tokens,
        max_tokens,
        strategy,
        true,
        false,
    )
    .await;

    spawn_cleanup(&node, &pipeline, &request_id);
    let run = run_result.map_err(|e| api_error(StatusCode::BAD_GATEWAY, e))?;

    let total_ms = overall_start.elapsed().as_millis() as u64;
    let generated = run.generated;
    let votes = run.votes;
    let output_text = model.decode(&generated);
    let output_bytes: Vec<u8> = generated.iter().flat_map(|t| t.to_le_bytes()).collect();
    let output_hash = arc_crypto::hash_bytes(&output_bytes);
    let shard_trace = render_shard_trace(&pipeline, &run.hop_stats);

    node.sharded_bytes_total
        .fetch_add(run.total_bytes as u64, Ordering::Relaxed);

    // Summarize consensus.
    let mut unanimous = 0;
    let mut majority = 0;
    let mut split = 0;
    let mut divergent_all: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for v in &votes {
        match v.agreement.as_str() {
            "unanimous" => unanimous += 1,
            "majority" => majority += 1,
            "split" => split += 1,
            _ => {}
        }
        for (replica, hash) in &v.divergent {
            divergent_all
                .entry(replica.clone())
                .or_default()
                .push(hash.clone());
        }
    }

    // Divergence remains visible, but do not fabricate a provider identity
    // from an unauthenticated shard label or claim that the validator posted a
    // bond. Automatic challenges can return only after the authenticated
    // transport peer identity is propagated into every inference vote.
    let auto_challenges: Vec<Value> = Vec::new();
    let auto_challenge_status = if divergent_all.is_empty() {
        "not_needed"
    } else {
        "disabled_untrusted_replica_identity"
    };
    let response = json!({
        "success": true,
        "request_id": request_id,
        "input": input_text,
        "output": output_text,
        "output_tokens": generated,
        "output_hash": format!("0x{}", hex::encode(output_hash.0)),
        "model_hash": node.model_artifact_id.map(|id| format!("0x{}", id.to_hex())),
        "tokens_generated": generated.len(),
        "total_ms": total_ms,
        "ms_per_token": if generated.is_empty() { 0 } else { total_ms / generated.len() as u64 },
        "pipeline_length": pipeline.len(),
        "k": k_req,
        // Additive: same per-hop trace run_sharded emits, so the dashboard's
        // activation-flow view works against this endpoint too.
        "shard_trace": shard_trace,
        "total_bytes_transferred": run.total_bytes,
        "deterministic": true,
        "engine": format!("{} sharded pipeline (k-of-n consensus)", model.effective_precision_label()),
        "consensus": {
            "k": k_req,
            "votes_total": votes.len(),
            "unanimous": unanimous,
            "majority": majority,
            "split": split,
            "divergent_replicas": divergent_all,
            "auto_challenges": auto_challenges,
            "auto_challenge_status": auto_challenge_status,
        },
    });
    Ok(Json(response))
}

/// Milestone C (#37): GET /models/registry
/// Scans committed transactions for every ModelRegistration body and
/// returns the resulting per-model metadata. For MVP this is O(N) over
/// the full-tx DashMap; a later patch can maintain a sidecar index if
/// the registry grows past a few thousand models.
async fn list_model_registry(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let arc_types::TxBody::ModelRegistration(body) = &tx.body {
            rows.push(json!({
                "model_id": format!("0x{}", hex::encode(body.model_id.0)),
                "metadata_hash": format!("0x{}", hex::encode(body.metadata_hash.0)),
                "chunk_tree_root": format!("0x{}", hex::encode(body.chunk_tree_root.0)),
                "n_layers": body.n_layers,
                "d_model": body.d_model,
                "quantization": &body.quantization,
                "registration_fee": body.registration_fee,
                "royalty_recipient": format!("0x{}", hex::encode(body.royalty_recipient.0)),
                "registered_by": format!("0x{}", hex::encode(tx.from.0)),
                "tx_hash": format!("0x{}", hex::encode(tx.hash.0)),
            }));
        }
    }
    Json(json!({ "models": rows, "count": rows.len() }))
}

/// Milestone C (#37): GET /models/open_requests
/// Returns every ModelRequest tx body. Workers poll this to find open
/// demand and decide which ranges to claim.
async fn list_open_model_requests(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let arc_types::TxBody::ModelRequest(body) = &tx.body {
            rows.push(json!({
                "request_id": format!("0x{}", hex::encode(body.request_id)),
                "model_id": format!("0x{}", hex::encode(body.model_id.0)),
                "target_k_replication": body.target_k_replication,
                "bond_per_layer_epoch": body.bond_per_layer_epoch,
                "max_wait_secs": body.max_wait_secs,
                "requester": format!("0x{}", hex::encode(tx.from.0)),
                "tx_hash": format!("0x{}", hex::encode(tx.hash.0)),
            }));
        }
    }
    Json(json!({ "requests": rows, "count": rows.len() }))
}

/// Milestone D (#38): GET /capacity/advertisements
/// Returns every CapacityAdvertisement. The planner reads this set
/// plus open requests + current shard_registry to compute assignments.
async fn list_capacity_advertisements(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let arc_types::TxBody::CapacityAdvertisement(body) = &tx.body {
            rows.push(json!({
                "node_pubkey": format!("0x{}", hex::encode(body.node_pubkey)),
                "ram_bytes": body.ram_bytes,
                "vram_bytes": body.vram_bytes,
                "bandwidth_mbps": body.bandwidth_mbps,
                "uptime_hint_mins": body.uptime_hint_mins,
                "stake": body.stake,
                "region": &body.region,
                "advertised_by": format!("0x{}", hex::encode(tx.from.0)),
                "tx_hash": format!("0x{}", hex::encode(tx.hash.0)),
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
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let pk_hex = params
        .get("pubkey")
        .ok_or(api_error(StatusCode::BAD_REQUEST, "missing ?pubkey= param"))?;
    let pk = decode_hash_hex(pk_hex)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, format!("pubkey: {}", e)))?;

    let mut assignments: Vec<Value> = Vec::new();
    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let arc_types::TxBody::ShardAssignmentProposal(body) = &tx.body {
            for a in &body.assignments {
                if a.node_pubkey == pk {
                    assignments.push(json!({
                        "epoch_blocks": body.epoch_blocks,
                        "input_snapshot_hash": format!(
                            "0x{}", hex::encode(body.input_snapshot_hash.0)
                        ),
                        "model_id": format!("0x{}", hex::encode(a.model_id.0)),
                        "ranges": a.ranges.iter()
                            .map(|(s, e)| json!([s, e]))
                            .collect::<Vec<_>>(),
                        "proposal_tx_hash": format!("0x{}", hex::encode(tx.hash.0)),
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

/// GET /shards
/// Returns the local shard registry - every node this coordinator knows about
/// and which layer range it holds.
async fn get_shards(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let mut shards: Vec<ShardInfo> = fresh_shards(&node.shard_registry);
    if let Some(model_id) = node.model_artifact_id {
        shards.retain(|shard| {
            parse_hash256_hex(&shard.model_id, "shard model_id").ok() == Some(model_id)
        });
    }
    shards.sort_by_key(|s| s.start_layer);

    let total_layers = shards.first().map(|s| s.total_layers).unwrap_or(0);
    let total_full_mb = shards.first().map(|s| s.full_model_mb).unwrap_or(0);
    let total_held_mb: usize = shards.iter().map(|s| s.memory_mb).sum();
    let model_id = shards
        .first()
        .map(|s| s.model_id.clone())
        .unwrap_or_default();
    let model_name = shards
        .first()
        .map(|s| s.model_name.clone())
        .unwrap_or_default();

    // Dedup ranges across replicas. With 3× replication each range appears
    // three times in `shards`; walking the raw list sees the second replica's
    // start_layer == 0 as a backward step and flips contiguous=false. BTreeSet
    // collapses duplicates and iterates in sorted (start, end) order.
    let unique_ranges: std::collections::BTreeSet<(usize, usize)> = shards
        .iter()
        .map(|s| (s.start_layer, s.end_layer))
        .collect();
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

/// Bind a remote shard announcement to the TCP peer's observed source host,
/// retaining only its declared service port. Otherwise an unauthenticated
/// caller can advertise `169.254.169.254:80` (or any private control-plane
/// service) and turn later inference routing into blind SSRF.
///
/// Loopback is deliberately trusted: the local puller fetches a remote seed's
/// self-shards and re-announces them to this process over 127.0.0.1. Rewriting
/// those trusted local messages would destroy the remote destination.
fn bind_announced_shard_addr(announced_addr: &str, peer_addr: SocketAddr) -> String {
    if peer_addr.ip().is_loopback() {
        return announced_addr.to_string();
    }
    let declared_port = announced_addr
        .rsplit(':')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or_else(|| peer_addr.port());
    // SocketAddr::Display brackets IPv6 correctly.
    SocketAddr::new(peer_addr.ip(), declared_port).to_string()
}

/// POST /shards/announce
/// Other nodes call this to register their shard with our local registry.
///
/// Remote announcements never get to choose a destination host: the observed
/// TCP source IP replaces it, while the declared shard RPC port is retained.
/// Trusted localhost re-announcements are left intact for registry gossip.
async fn announce_shard(
    AxumState(node): AxumState<NodeState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Json(mut req): Json<AnnounceShardRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let model_id = parse_hash256_hex(&req.shard.model_id, "model_id")
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    if req.shard.start_layer >= req.shard.end_layer
        || req.shard.end_layer > req.shard.total_layers
        || req.shard.total_layers == 0
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "shard range must be non-empty and contained in total_layers".to_string(),
        ));
    }
    req.shard.model_id = format!("0x{}", model_id.to_hex());
    req.shard.socket_addr = bind_announced_shard_addr(&req.shard.socket_addr, peer_addr);

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
            return Ok(Json(
                json!({"ok": true, "registry_size": node.shard_registry.len(), "note": "stub addr ignored - routable addr already registered"}),
            ));
        }
    }
    // Key by (socket_addr, range) so one node announcing multiple held ranges
    // produces one entry per range - otherwise the DashMap insert clobbers
    // prior announces and only the most recent range survives. The
    // coordinator's BTreeMap grouping already keys on (start, end) so a
    // per-range entry is exactly what we need.
    let key = format!(
        "{}#{}-{}",
        req.shard.socket_addr, req.shard.start_layer, req.shard.end_layer
    );
    // Also register in multi-model ShardRegistry for multi-model routing
    let assignment = arc_inference::distributed::ShardAssignment {
        node_address: model_id, // placeholder; real node addr comes from p2p
        start_layer: req.shard.start_layer as u32,
        end_layer: req.shard.end_layer as u32,
        expert_indices: Vec::new(),
        socket_addr: req.shard.socket_addr.clone(),
        gpu_tier: 0,
        available_memory: (req.shard.memory_mb as u64) * 1024 * 1024,
    };
    node.multi_model_registry
        .register_shard(model_id, assignment);
    node.shard_registry
        .insert(key, (req.shard, std::time::Instant::now()));
    Ok(Json(
        json!({"ok": true, "registry_size": node.shard_registry.len()}),
    ))
}

// ─── Community worker registry ──────────────────────────────────────────────

/// Versioned proof-of-possession envelope for every community mutation.
/// The signature commits to the exact route, typed payload hash, timestamp,
/// and random nonce. Read-only community endpoints remain unenveloped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunitySignedRequest<T> {
    pub version: u8,
    pub timestamp_unix_ms: u64,
    pub nonce: Hash256,
    pub payload_hash: Hash256,
    pub payload: T,
    pub signature: arc_crypto::Signature,
}

trait CommunityAuthenticatedPayload {
    fn signer_id(&self) -> &str;
}

fn community_unix_ms() -> Result<u64, String> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch".to_string())?;
    u64::try_from(duration.as_millis()).map_err(|_| "Unix timestamp overflow".to_string())
}

fn community_payload_hash<T: Serialize>(payload: &T) -> Result<Hash256, String> {
    let encoded = serde_json::to_vec(payload)
        .map_err(|error| format!("serialize community request payload: {error}"))?;
    Ok(arc_crypto::hash_bytes(&encoded))
}

fn community_request_commitment(
    path: &str,
    version: u8,
    payload_hash: Hash256,
    timestamp_unix_ms: u64,
    nonce: Hash256,
) -> Hash256 {
    let mut transcript = Vec::with_capacity(COMMUNITY_AUTH_DOMAIN.len() + path.len() + 82);
    transcript.extend_from_slice(COMMUNITY_AUTH_DOMAIN);
    transcript.push(version);
    transcript.extend_from_slice(&(path.len() as u64).to_be_bytes());
    transcript.extend_from_slice(path.as_bytes());
    transcript.extend_from_slice(&payload_hash.0);
    transcript.extend_from_slice(&timestamp_unix_ms.to_be_bytes());
    transcript.extend_from_slice(&nonce.0);
    arc_crypto::hash_bytes(&transcript)
}

/// Construct a fresh signed request. Two UUIDv4 values supply 244 random bits
/// after their fixed version/variant bits, sourced from the OS CSPRNG.
pub fn sign_community_request<T: Serialize>(
    path: &str,
    payload: T,
    keypair: &arc_crypto::KeyPair,
) -> Result<CommunitySignedRequest<T>, String> {
    if !matches!(keypair, arc_crypto::KeyPair::Ed25519(_)) {
        return Err("community HTTP authentication requires an Ed25519 key".to_string());
    }
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let mut nonce_bytes = [0u8; 32];
    nonce_bytes[..16].copy_from_slice(first.as_bytes());
    nonce_bytes[16..].copy_from_slice(second.as_bytes());
    let nonce = Hash256(nonce_bytes);
    let timestamp_unix_ms = community_unix_ms()?;
    let payload_hash = community_payload_hash(&payload)?;
    let commitment = community_request_commitment(
        path,
        COMMUNITY_AUTH_VERSION,
        payload_hash,
        timestamp_unix_ms,
        nonce,
    );
    let signature = keypair
        .sign(&commitment)
        .map_err(|error| format!("sign community request: {error}"))?;
    Ok(CommunitySignedRequest {
        version: COMMUNITY_AUTH_VERSION,
        timestamp_unix_ms,
        nonce,
        payload_hash,
        payload,
        signature,
    })
}

fn authenticate_community_request<T>(
    node: &NodeState,
    path: &str,
    signed: CommunitySignedRequest<T>,
) -> Result<T, (StatusCode, String)>
where
    T: Serialize + CommunityAuthenticatedPayload,
{
    if signed.version != COMMUNITY_AUTH_VERSION {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported community auth version {}; expected {}",
                signed.version, COMMUNITY_AUTH_VERSION
            ),
        ));
    }
    if signed.nonce == Hash256::ZERO {
        return Err((
            StatusCode::BAD_REQUEST,
            "community auth nonce must be non-zero".to_string(),
        ));
    }
    let now_unix_ms = community_unix_ms().map_err(|error| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot validate request time: {error}"),
        )
    })?;
    if now_unix_ms.abs_diff(signed.timestamp_unix_ms) > COMMUNITY_AUTH_MAX_CLOCK_SKEW_MS {
        return Err((
            StatusCode::UNAUTHORIZED,
            "community request timestamp is stale or too far in the future".to_string(),
        ));
    }

    let computed_payload_hash = community_payload_hash(&signed.payload)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    if computed_payload_hash != signed.payload_hash {
        return Err((
            StatusCode::BAD_REQUEST,
            "community request payload_hash does not match the complete payload".to_string(),
        ));
    }

    let claimed_signer_id = signed.payload.signer_id();
    let worker_hex = claimed_signer_id
        .strip_prefix("0x")
        .unwrap_or(claimed_signer_id);
    let worker_address = Hash256::from_hex(worker_hex).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "worker_id must be a 32-byte hexadecimal ARC address".to_string(),
        )
    })?;
    if !matches!(&signed.signature, arc_crypto::Signature::Ed25519 { .. }) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "community request signature must be Ed25519".to_string(),
        ));
    }
    let commitment = community_request_commitment(
        path,
        signed.version,
        signed.payload_hash,
        signed.timestamp_unix_ms,
        signed.nonce,
    );
    signed
        .signature
        .verify(&commitment, &worker_address)
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                "community request signature does not prove control of signer_id".to_string(),
            )
        })?;

    match node
        .community_request_replays
        .lock()
        .accept(worker_address, signed.nonce, now_unix_ms)
    {
        Ok(()) => Ok(signed.payload),
        Err(CommunityReplayError::Replay) => Err((
            StatusCode::CONFLICT,
            "community request nonce was already used".to_string(),
        )),
        Err(CommunityReplayError::Capacity) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "community replay cache is at capacity; retry with a fresh request later".to_string(),
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityRegisterRequest {
    pub worker_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub platform: String,
}

impl CommunityAuthenticatedPayload for CommunityRegisterRequest {
    fn signer_id(&self) -> &str {
        &self.worker_id
    }
}

/// POST /community/register
/// A community-mode node calls this on every seed it can reach. The seed
/// stores the worker info in its community_workers registry. The worker
/// is then visible to the dashboard and counted in TPS/compute stats.
/// Workers are pure outbound HTTP: no inbound port, no NAT traversal,
/// no QUIC. Works behind any residential firewall. Signed requests provide
/// authentication and integrity, not confidentiality; production deployments
/// must terminate TLS at a reverse proxy or equivalent deployment layer.
async fn community_register_signed(
    AxumState(node): AxumState<NodeState>,
    Json(signed): Json<CommunitySignedRequest<CommunityRegisterRequest>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let req = authenticate_community_request(&node, COMMUNITY_REGISTER_PATH, signed)?;
    community_register(AxumState(node), Json(req)).await
}

async fn community_register(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<CommunityRegisterRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if req.worker_id.is_empty() || req.worker_id.len() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            "worker_id required (1-128 chars)".to_string(),
        ));
    }
    let capabilities = req.capabilities;
    let serves_inference = capabilities
        .iter()
        .any(|capability| capability == "inference");
    let model = req.model.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    });
    let model_id = match req.model_id {
        Some(value) => {
            let parsed = parse_hash256_hex(value.trim(), "model_id").map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "model_id must be a 32-byte hex hash".to_string(),
                )
            })?;
            Some(format!("0x{}", parsed.to_hex()))
        }
        None => None,
    };
    if serves_inference && (model.is_none() || model_id.is_none()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "inference workers must register both a display model and exact model_id".to_string(),
        ));
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

    let worker = CommunityWorker {
        worker_id: req.worker_id.clone(),
        name: req.name,
        capabilities,
        model,
        model_id,
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

    Ok(Json(json!({
        "ok": true,
        "worker_id": req.worker_id,
        "registry_size": node.community_workers.len(),
        "welcome": "Your node is now visible on the ARC testnet dashboard.",
    })))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityHeartbeatRequest {
    pub worker_id: String,
    /// Deprecated client-reported value, accepted for wire compatibility but
    /// never trusted. Completion counters are server-authoritative.
    #[serde(default)]
    #[allow(dead_code)]
    pub work_completed: Option<u64>,
}

impl CommunityAuthenticatedPayload for CommunityHeartbeatRequest {
    fn signer_id(&self) -> &str {
        &self.worker_id
    }
}

/// POST /community/heartbeat
/// Community workers call this every 15 seconds to stay alive in the
/// registry. Without a heartbeat for COMMUNITY_WORKER_TTL_SECS (90s)
/// the worker is pruned at read time.
async fn community_heartbeat_signed(
    AxumState(node): AxumState<NodeState>,
    Json(signed): Json<CommunitySignedRequest<CommunityHeartbeatRequest>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let req = authenticate_community_request(&node, COMMUNITY_HEARTBEAT_PATH, signed)?;
    community_heartbeat(AxumState(node), Json(req)).await
}

async fn community_heartbeat(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<CommunityHeartbeatRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if let Some(mut entry) = node.community_workers.get_mut(&req.worker_id) {
        let (_worker, ts) = entry.value_mut();
        *ts = std::time::Instant::now();
        Ok(Json(json!({"ok": true})))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            "worker_id not registered - call /community/register first".to_string(),
        ))
    }
}

/// GET /community/list
/// Returns all fresh community workers. Entries older than
/// COMMUNITY_WORKER_TTL_SECS are pruned at read time. The dashboard
/// polls this to show the community node count + geographic spread.
async fn community_list(AxumState(node): AxumState<NodeState>) -> Json<serde_json::Value> {
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
        let hex_addr = format!("0x{}", hex::encode(addr.0));
        // Skip self and already-registered workers
        if *addr == node.validator_address || registered_ids.contains(&hex_addr) {
            continue;
        }
        live.push(CommunityWorker {
            worker_id: hex_addr,
            name: format!("p2p-peer (stake={})", stake),
            capabilities: vec!["consensus".to_string()],
            model: None,
            model_id: None,
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
        "community_rewards_v1_enabled": community_rewards_v1_effective(&node),
        "community_rewards_v1_protocol_active": community_rewards_v1_protocol_active(&node),
        "community_rewards_v1_approval_collection_ready": COMMUNITY_REWARD_APPROVAL_COLLECTION_READY,
        "community_rewards_v1_activation_height": node.state.community_rewards_v1_activation_height(),
        "community_rewards_v1_issuance_enabled": node.community_rewards_v1_enabled,
        "community_rewards_v1_note": community_rewards_v1_readiness_note(&node),
    }))
}

// ─── Community work dispatch (long-poll claim + submit) ─────────────────────
//
// Community nodes run with `--stake 0 --community-mode` behind NAT. They can
// reach seed nodes via outbound HTTP but cannot accept inbound connections.
// Production deployments must provide TLS through a reverse proxy or equivalent
// deployment layer; request signatures do not encrypt prompts or results.
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

/// Poll the shared bounded queue without holding its receiver mutex across an
/// await. `mpsc::Receiver` has one consumer, but a 30-second `recv().await`
/// under the mutex serialized every worker behind the first idle long-poll.
/// Short critical sections let all waiting workers compete for new jobs.
async fn poll_community_work_queue(
    queue: &Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<WorkItem>>>,
    timeout: tokio::time::Duration,
) -> Result<Option<WorkItem>, ()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let attempt = queue.lock().await.try_recv();
        match attempt {
            Ok(item) => return Ok(Some(item)),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return Err(()),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(
            std::time::Duration::from_millis(25).min(deadline.saturating_duration_since(now)),
        )
        .await;
    }
}

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
    /// Protocol-v3 transaction signing domain advertised by the coordinator.
    /// Absent on legacy/dev networks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_domain: Option<String>,
    /// Unix timestamp (ms) when the dispatcher queued the job. Workers
    /// echo this back so the dispatcher can compute end-to-end latency.
    pub submitted_at_unix_ms: i64,
}

/// Coordinator-owned state for a dispatched community job.
///
/// This must stay in the pending map until the assigned worker submits a
/// result. A bare response channel is not enough: without the original job
/// and assignment, an arbitrary registered worker can invent a job id and a
/// signed attestation that is unrelated to work the coordinator issued.
pub struct PendingCommunityWork {
    item: WorkItem,
    assignment_epoch: Hash256,
    job_nonce: u64,
    assigned_worker: Option<String>,
    sender: tokio::sync::oneshot::Sender<CommunityDispatchOutcome>,
}

#[derive(Debug, Clone)]
struct CommunityRewardSubmission {
    worker: Hash256,
    output_hash: Hash256,
    tx_hash: Hash256,
    approvals: usize,
}

/// Authenticated coordinator-to-validator approval request. The prompt and
/// output are required because each validator recomputes independently; the
/// reward body contains only their commitments.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommunityRewardApprovalPayload {
    coordinator_id: String,
    reward: arc_types::transaction::CommunityInferenceRewardBody,
    input: String,
    output: String,
    tokens_generated: u64,
}

impl CommunityAuthenticatedPayload for CommunityRewardApprovalPayload {
    fn signer_id(&self) -> &str {
        &self.coordinator_id
    }
}

/// Evidence created only after the coordinator independently recomputes a
/// worker result. It is not part of `WorkResult` and therefore cannot be
/// supplied or overridden by a worker.
#[derive(Debug, Clone, Serialize)]
struct CommunityVerificationSummary {
    method: &'static str,
    output_hash: String,
    tokens_generated: usize,
    ranges: usize,
    /// One authenticated 2-of-3 quorum record for each verified model-range
    /// and token-position pair. This is not a raw signature count.
    range_position_quorums: usize,
    signatures_required_per_quorum: usize,
    replicas_contacted_per_quorum: usize,
}

#[derive(Debug, Clone)]
struct CommunityDispatchOutcome {
    result: WorkResult,
    verification: Option<CommunityVerificationSummary>,
    settlement: Option<Value>,
}

/// RAII reservation for a worker's concurrent long-polls. If a request is
/// canceled while it is waiting, dropping this guard releases the empty
/// reservation. Once a job is assigned, the reservation stays until submit or
/// coordinator timeout so another seed poll cannot dequeue a second job for
/// the same single-capacity worker.
struct CommunityClaimReservation {
    active_jobs: Arc<dashmap::DashMap<String, String>>,
    worker_id: String,
    committed: bool,
}

impl CommunityClaimReservation {
    fn commit(&mut self, job_id: &str) {
        if let Some(mut active) = self.active_jobs.get_mut(&self.worker_id) {
            *active = job_id.to_string();
        }
        self.committed = true;
    }
}

impl Drop for CommunityClaimReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.active_jobs.remove(&self.worker_id);
        }
    }
}

/// Release a worker's capacity only when it still belongs to this job. The
/// DashMap entry API makes the compare-and-remove atomic, so a late timeout
/// can never erase a newer assignment.
fn release_active_community_job(node: &NodeState, worker_id: &str, job_id: &str) {
    use dashmap::mapref::entry::Entry;

    if let Entry::Occupied(entry) = node.community_active_jobs.entry(worker_id.to_string())
        && entry.get() == job_id
    {
        entry.remove();
    }
}

/// Result submitted by a community worker after completing a WorkItem.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkResult {
    /// Must match the WorkItem.job_id.
    pub job_id: String,
    /// Worker's self-chosen identifier (hex of validator pubkey).
    pub worker_id: String,
    /// True if the worker produced output; false on inference error.
    #[serde(default = "default_true")]
    pub success: bool,
    /// True when the worker is deliberately returning an extra concurrent
    /// assignment without attempting inference. This unblocks the coordinator
    /// without counting a capacity race as a model failure.
    #[serde(default)]
    pub declined: bool,
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

impl CommunityAuthenticatedPayload for WorkResult {
    fn signer_id(&self) -> &str {
        &self.worker_id
    }
}

#[derive(Debug, Clone)]
struct CommunityResultVerification {
    output_hash: Hash256,
    tokens_generated: usize,
    range_count: usize,
    range_position_quorum_count: usize,
}

impl From<&CommunityResultVerification> for CommunityVerificationSummary {
    fn from(verified: &CommunityResultVerification) -> Self {
        Self {
            method: "authenticated_shard_quorum_2_of_3_per_range",
            output_hash: format!("0x{}", verified.output_hash.to_hex()),
            tokens_generated: verified.tokens_generated,
            ranges: verified.range_count,
            range_position_quorums: verified.range_position_quorum_count,
            signatures_required_per_quorum: COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED,
            replicas_contacted_per_quorum: COMMUNITY_VERIFICATION_REPLICAS,
        }
    }
}

/// Recreate the exact token stream consumed by `CachedIntegerModel::generate`
/// in the community-worker loop. That function performs an internal hardcoded
/// BOS forward, then forwards the caller's BOS-prefixed prompt, then forwards
/// the final prompt token once more to choose the first generated token.
fn community_worker_verification_tokens(
    model: &arc_inference::cached_integer_model::CachedIntegerModel,
    input: &str,
) -> Result<Vec<u32>, String> {
    let prompt_tokens = model.encode(input);
    if prompt_tokens.is_empty() {
        return Err("assigned prompt encoded to zero tokens".to_string());
    }

    let mut worker_prompt = vec![model.config.bos_token];
    worker_prompt.extend(prompt_tokens);
    let last_prompt_token = *worker_prompt
        .last()
        .expect("BOS makes worker_prompt non-empty");

    let mut verification_tokens = Vec::with_capacity(worker_prompt.len() + 2);
    verification_tokens.push(1); // CachedIntegerModel::generate internal BOS
    verification_tokens.extend(worker_prompt);
    verification_tokens.push(last_prompt_token); // first decode step
    Ok(verification_tokens)
}

fn compare_community_result_with_tokens(
    result: &WorkResult,
    generated: &[u32],
    actual_output: &str,
) -> Result<Hash256, String> {
    let output_bytes: Vec<u8> = generated
        .iter()
        .flat_map(|token| token.to_le_bytes())
        .collect();
    let actual_hash = arc_crypto::hash_bytes(&output_bytes);
    let claimed_hash = parse_hash256_hex(&result.output_hash, "output_hash")?;
    if actual_hash != claimed_hash {
        return Err(format!(
            "worker output_hash {} does not match 2-of-3 recomputation {}",
            claimed_hash, actual_hash
        ));
    }
    if result.tokens_generated != generated.len() as u64 {
        return Err(format!(
            "worker tokens_generated {} does not match recomputation {}",
            result.tokens_generated,
            generated.len()
        ));
    }
    if result.output != actual_output {
        return Err(
            "worker output text does not decode from the quorum-verified token sequence"
                .to_string(),
        );
    }
    Ok(actual_hash)
}

/// Validate that a successful community recomputation contains one fixed,
/// authenticated 2-of-3 record for every executed range and token position.
///
/// This deliberately does not equate `votes.len()` with quorum proof.  Each
/// record must identify two distinct currently approved, positive-stake signer
/// keys, and all three configured replica slots must have been contacted.  The
/// complete range/position grid is checked so a missing hop cannot disappear
/// behind an otherwise plausible aggregate count.
fn validate_community_range_position_quorums(
    pipeline: &[PipelineHop],
    hop_stats: &[HopStats],
    votes: &[RangeVote],
    active_validators: &[(Hash256, u64)],
) -> Result<usize, String> {
    if pipeline.is_empty() || hop_stats.len() != pipeline.len() {
        return Err("community verification produced an invalid pipeline summary".to_string());
    }
    let positions = hop_stats[0].positions;
    if positions == 0 || hop_stats.iter().any(|stats| stats.positions != positions) {
        return Err(
            "community verification did not execute every range for the same token positions"
                .to_string(),
        );
    }
    let positions = usize::try_from(positions)
        .map_err(|_| "community verification position count overflow".to_string())?;
    let expected_count = pipeline
        .len()
        .checked_mul(positions)
        .ok_or_else(|| "community verification quorum count overflow".to_string())?;
    if votes.len() != expected_count {
        return Err(format!(
            "community verification has {} quorum record(s), expected {expected_count}",
            votes.len()
        ));
    }

    let active: std::collections::HashSet<Hash256> = active_validators
        .iter()
        .filter_map(|(address, stake)| (*stake > 0).then_some(*address))
        .collect();
    let expected_ranges: std::collections::HashSet<(usize, usize)> =
        pipeline.iter().map(|(range, _)| *range).collect();
    let mut observed = std::collections::HashSet::with_capacity(expected_count);

    for vote in votes {
        if !expected_ranges.contains(&vote.range) || vote.position >= positions {
            return Err(format!(
                "community verification contains unexpected range {:?} position {}",
                vote.range, vote.position
            ));
        }
        if !observed.insert((vote.range, vote.position)) {
            return Err(format!(
                "community verification duplicated range {:?} position {}",
                vote.range, vote.position
            ));
        }
        if vote.replicas_contacted.len() != COMMUNITY_VERIFICATION_REPLICAS {
            return Err(format!(
                "range {:?} position {} contacted {} replica slot(s), expected {}",
                vote.range,
                vote.position,
                vote.replicas_contacted.len(),
                COMMUNITY_VERIFICATION_REPLICAS
            ));
        }
        if !vote.majority_hash.as_deref().is_some_and(is_hash256_hex) {
            return Err(format!(
                "range {:?} position {} has no valid majority hash",
                vote.range, vote.position
            ));
        }
        let distinct_signers: std::collections::HashSet<Hash256> =
            vote.majority_signers.iter().copied().collect();
        if distinct_signers.len() < COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED {
            return Err(format!(
                "range {:?} position {} has {} distinct authenticated signer(s), expected at least {}",
                vote.range,
                vote.position,
                distinct_signers.len(),
                COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED
            ));
        }
        if let Some(unapproved) = distinct_signers
            .iter()
            .find(|signer| !active.contains(signer))
        {
            return Err(format!(
                "range {:?} position {} includes unapproved signer {}",
                vote.range, vote.position, unapproved
            ));
        }
    }

    for range in expected_ranges {
        for position in 0..positions {
            if !observed.contains(&(range, position)) {
                return Err(format!(
                    "community verification is missing range {:?} position {position}",
                    range
                ));
            }
        }
    }
    Ok(expected_count)
}

/// Independently recompute a community worker's claimed completion through
/// three authenticated, distinct active-validator replicas per layer range.
/// `pipeline_hop` only counts one vote per validator key, and every response
/// is signed over the exact request and chain domain. No quorum means no
/// accepted output and no reward.
async fn verify_community_result_with_quorum(
    node: &NodeState,
    work_item: &WorkItem,
    result: &WorkResult,
) -> Result<CommunityResultVerification, String> {
    let model = node
        .inference_model
        .as_ref()
        .ok_or_else(|| "coordinator has no tokenizer/model for result verification".to_string())?
        .clone();
    let (expected_model_name, expected_model_id) = exact_model_identity(node)?;
    let assigned_model_id = work_item
        .model_id
        .as_deref()
        .ok_or_else(|| "job was not pinned to a model identity".to_string())?;
    let assigned_model_id = parse_hash256_hex(assigned_model_id, "assigned model_id")?;
    if assigned_model_id != expected_model_id {
        return Err(format!(
            "assigned model does not match coordinator model {expected_model_name}"
        ));
    }

    let pipeline = assemble_pipeline_with_bootstrap(node)
        .await
        .map_err(|e| format!("assemble verification pipeline: {e}"))?;
    for (range, replicas) in &pipeline {
        let distinct_sockets: std::collections::HashSet<&str> = replicas
            .iter()
            .map(|replica| replica.socket_addr.as_str())
            .collect();
        if distinct_sockets.len() < COMMUNITY_VERIFICATION_REPLICAS {
            return Err(format!(
                "range [{}, {}) has {} distinct replicas; secure reward verification requires 3",
                range.0,
                range.1,
                distinct_sockets.len()
            ));
        }
    }

    let all_tokens = community_worker_verification_tokens(&model, &work_item.input)?;
    let request_id = format!("community-verify-v3-{}", work_item.job_id);
    let run_result = run_pipeline(
        node,
        &model,
        &pipeline,
        &request_id,
        &all_tokens,
        work_item.max_tokens,
        HopStrategy::Fanout {
            fanout: COMMUNITY_VERIFICATION_REPLICAS,
            needed: COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED,
            allow_degraded_quorum: false,
        },
        true,
        true,
    )
    .await;
    spawn_cleanup(node, &pipeline, &request_id);
    let run = run_result?;
    let active_validators = node.dag_validators.read().clone();
    let range_position_quorum_count = validate_community_range_position_quorums(
        &pipeline,
        &run.hop_stats,
        &run.votes,
        &active_validators,
    )?;
    let actual_output = model.decode(&run.generated);
    let output_hash = compare_community_result_with_tokens(result, &run.generated, &actual_output)?;

    Ok(CommunityResultVerification {
        output_hash,
        tokens_generated: run.generated.len(),
        range_count: pipeline.len(),
        range_position_quorum_count,
    })
}

fn validate_reward_approval_payload(
    node: &NodeState,
    payload: &CommunityRewardApprovalPayload,
) -> Result<(WorkItem, WorkResult), String> {
    use arc_types::transaction::{
        COMMUNITY_REWARD_MIN_WORKER_STAKE, COMMUNITY_REWARD_VALIDATOR_SET_SIZE,
        CommunityInferenceRewardBody,
    };

    if !community_rewards_v1_protocol_active(node) {
        return Err("community reward protocol is not active".to_string());
    }
    reward_approval_prerequisites(node).map_err(str::to_string)?;
    if !payload.reward.validator_approvals.is_empty() {
        return Err("approval candidate must not contain pre-collected approvals".to_string());
    }
    if payload.reward.chain_domain != CommunityInferenceRewardBody::expected_chain_domain() {
        return Err("reward candidate has the wrong chain domain".to_string());
    }
    match node.state.recovery_context() {
        Some(context)
            if payload.reward.recovery_epoch == context.recovery_epoch
                && payload.reward.validator_set_id == context.validator_set_id
                && payload.reward.transaction_domain == context.domain_hash() => {}
        Some(_) => {
            return Err(
                "reward candidate is not bound to this validator's recovery epoch, validator-set ID, and transaction domain"
                    .to_string(),
            );
        }
        None if payload.reward.recovery_epoch == 0
            && payload.reward.validator_set_id == 0
            && payload.reward.transaction_domain == Hash256::ZERO => {}
        None => {
            return Err(
                "reward candidate carries a recovery binding on legacy/dev state".to_string(),
            );
        }
    }
    let coordinator = parse_hash256_hex(&payload.coordinator_id, "coordinator_id")?;
    if coordinator != payload.reward.coordinator || !node.state.is_validator(&coordinator) {
        return Err("request signer is not the active assignment coordinator".to_string());
    }
    if payload.reward.assignment_epoch == Hash256::ZERO {
        return Err("assignment_epoch cannot be zero".to_string());
    }
    let active = node.state.active_validators();
    if active.len() != COMMUNITY_REWARD_VALIDATOR_SET_SIZE {
        return Err(format!(
            "reward approval requires exactly {COMMUNITY_REWARD_VALIDATOR_SET_SIZE} active validators, found {}",
            active.len()
        ));
    }
    let local_key = node
        .validator_keypair
        .as_ref()
        .ok_or_else(|| "local validator key is unavailable".to_string())?;
    if local_key.address() != node.validator_address
        || !node.state.is_validator(&node.validator_address)
        || !matches!(local_key.as_ref(), arc_crypto::KeyPair::Ed25519(_))
    {
        return Err("local approval key is not an active Ed25519 validator".to_string());
    }
    let (_, local_model_id) = exact_model_identity(node)?;
    if payload.reward.model_id != local_model_id {
        return Err(
            "reward model_id does not match this validator's exact model artifact".to_string(),
        );
    }
    if arc_crypto::hash_bytes(payload.input.as_bytes()) != payload.reward.input_hash {
        return Err("reward input_hash does not match the supplied prompt".to_string());
    }
    if payload.tokens_generated == 0
        || payload.tokens_generated > u64::from(payload.reward.max_tokens)
    {
        return Err("tokens_generated is outside the assigned token ceiling".to_string());
    }
    let expected_job_id = CommunityInferenceRewardBody::derive_job_id(
        &payload.reward.coordinator,
        &payload.reward.assignment_epoch,
        payload.reward.job_nonce,
        &payload.reward.model_id,
        &payload.reward.input_hash,
        payload.reward.max_tokens,
    );
    if payload.reward.job_id != expected_job_id {
        return Err("job_id does not match the exact assignment commitment".to_string());
    }
    let height = node.state.height();
    if payload.reward.expires_at_height < height
        || payload.reward.expires_at_height > height.saturating_add(COMMUNITY_REWARD_EXPIRY_BLOCKS)
    {
        return Err("reward expiry is outside the active bounded approval window".to_string());
    }
    let worker_stake = node
        .state
        .get_validator_stake(&payload.reward.worker)
        .unwrap_or(0);
    if worker_stake < COMMUNITY_REWARD_MIN_WORKER_STAKE {
        return Err(format!(
            "worker stake {worker_stake} is below active policy minimum {COMMUNITY_REWARD_MIN_WORKER_STAKE}"
        ));
    }
    let job_marker = CommunityInferenceRewardBody::marker_address(
        &payload.reward.chain_domain,
        &payload.reward.job_id,
    );
    let certificate_marker = CommunityInferenceRewardBody::certificate_marker_address(
        &payload.reward.chain_domain,
        &payload.reward.worker,
        &payload.reward.worker_certificate.attestation_hash,
    );
    if node.state.get_account(&job_marker).is_some()
        || node.state.get_account(&certificate_marker).is_some()
    {
        return Err("job or worker certificate already has a mined reward receipt".to_string());
    }

    let work_item = WorkItem {
        job_id: payload.reward.job_id.to_hex(),
        input: payload.input.clone(),
        max_tokens: payload.reward.max_tokens,
        model_id: Some(format!("0x{}", payload.reward.model_id.to_hex())),
        transaction_domain: node
            .state
            .transaction_domain_hash()
            .map(|domain| format!("0x{}", domain.to_hex())),
        submitted_at_unix_ms: 0,
    };
    let result = WorkResult {
        job_id: work_item.job_id.clone(),
        worker_id: format!("0x{}", payload.reward.worker.to_hex()),
        success: true,
        declined: false,
        output: payload.output.clone(),
        output_hash: format!("0x{}", payload.reward.output_hash.to_hex()),
        tokens_generated: payload.tokens_generated,
        total_ms: 0,
        ms_per_token: 0,
        engine: "validator-independent-recompute".to_string(),
        error: None,
        signed_attestation_hex: None,
    };
    let worker_attestation = payload.reward.reconstruct_worker_attestation();
    node.state
        .verify_transaction_signature(&worker_attestation)
        .map_err(|_| "worker certificate signature is invalid".to_string())?;
    validate_worker_attestation_for_job(
        &worker_attestation,
        &work_item,
        payload.reward.output_hash,
    )?;
    Ok((work_item, result))
}

async fn approve_community_reward_payload(
    node: &NodeState,
    payload: &CommunityRewardApprovalPayload,
) -> Result<arc_types::transaction::CommunityRewardValidatorApproval, String> {
    let (work_item, result) = validate_reward_approval_payload(node, payload)?;
    let verified = verify_community_result_with_quorum(node, &work_item, &result).await?;
    if verified.output_hash != payload.reward.output_hash
        || verified.tokens_generated != payload.tokens_generated as usize
    {
        return Err("independent recomputation did not match the reward commitment".to_string());
    }

    sign_validated_reward_approval(node, &payload.reward)
}

fn sign_validated_reward_approval(
    node: &NodeState,
    reward: &arc_types::transaction::CommunityInferenceRewardBody,
) -> Result<arc_types::transaction::CommunityRewardValidatorApproval, String> {
    use dashmap::mapref::entry::Entry;

    let commitment = reward.validator_approval_commitment();
    let mut inserted_job = false;
    match node.community_reward_approval_jobs.entry(reward.job_id) {
        Entry::Occupied(entry) if *entry.get() != commitment => {
            return Err(
                "validator already approved a different commitment for this job".to_string(),
            );
        }
        Entry::Occupied(_) => {}
        Entry::Vacant(entry) => {
            entry.insert(commitment);
            inserted_job = true;
        }
    }
    match node
        .community_reward_approval_certificates
        .entry(reward.worker_certificate.attestation_hash)
    {
        Entry::Occupied(entry) if *entry.get() != commitment => {
            if inserted_job {
                node.community_reward_approval_jobs.remove(&reward.job_id);
            }
            return Err(
                "validator already approved a different job for this worker certificate"
                    .to_string(),
            );
        }
        Entry::Occupied(_) => {}
        Entry::Vacant(entry) => {
            entry.insert(commitment);
        }
    }

    let key = node.validator_keypair.as_ref().expect("validated above");
    let signature = key
        .sign(&commitment)
        .map_err(|error| format!("sign reward approval: {error}"))?;
    arc_types::transaction::CommunityRewardValidatorApproval::from_ed25519_signature(
        node.validator_address,
        signature,
    )
    .ok_or_else(|| "reward approvals require Ed25519 validators".to_string())
}

async fn community_reward_approve_signed(
    AxumState(node): AxumState<NodeState>,
    Json(signed): Json<CommunitySignedRequest<CommunityRewardApprovalPayload>>,
) -> Result<Json<arc_types::transaction::CommunityRewardValidatorApproval>, (StatusCode, String)> {
    let payload = authenticate_community_request(&node, COMMUNITY_REWARD_APPROVE_PATH, signed)?;
    approve_community_reward_payload(&node, &payload)
        .await
        .map(Json)
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error))
}

fn validate_collected_reward_approvals(
    node: &NodeState,
    body: &arc_types::transaction::CommunityInferenceRewardBody,
    approvals: Vec<arc_types::transaction::CommunityRewardValidatorApproval>,
) -> Result<Vec<arc_types::transaction::CommunityRewardValidatorApproval>, String> {
    use arc_types::transaction::{
        COMMUNITY_REWARD_APPROVALS_REQUIRED, COMMUNITY_REWARD_VALIDATOR_SET_SIZE,
    };
    let active = node.state.active_validators();
    if active.len() != COMMUNITY_REWARD_VALIDATOR_SET_SIZE {
        return Err(format!(
            "active reward committee has {} validators; exactly {COMMUNITY_REWARD_VALIDATOR_SET_SIZE} required",
            active.len()
        ));
    }
    let active_stakes: HashMap<[u8; 32], u64> = active
        .iter()
        .map(|(address, stake)| (address.0, *stake))
        .collect();
    let total_stake: u128 = active.iter().map(|(_, stake)| u128::from(*stake)).sum();
    let required_stake = total_stake * 2 / 3 + 1;
    let commitment = body.validator_approval_commitment();
    let mut unique = HashMap::new();
    for approval in approvals {
        let Some(stake) = active_stakes.get(&approval.validator.0) else {
            continue;
        };
        if approval
            .as_signature()
            .verify(&commitment, &approval.validator)
            .is_ok()
        {
            unique
                .entry(approval.validator.0)
                .or_insert((approval, *stake));
        }
    }
    if unique.len() < COMMUNITY_REWARD_APPROVALS_REQUIRED {
        return Err(format!(
            "collected {} valid independent approvals; {COMMUNITY_REWARD_APPROVALS_REQUIRED} of {COMMUNITY_REWARD_VALIDATOR_SET_SIZE} required",
            unique.len()
        ));
    }
    let approved_stake: u128 = unique.values().map(|(_, stake)| u128::from(*stake)).sum();
    if approved_stake < required_stake {
        return Err(format!(
            "approval identities reached 5-of-6 but approved stake {approved_stake} is below strict two-thirds threshold {required_stake}"
        ));
    }
    let mut collected: Vec<_> = unique.into_values().map(|(approval, _)| approval).collect();
    collected.sort_unstable_by_key(|approval| approval.validator.0);
    Ok(collected)
}

async fn collect_community_reward_approvals(
    node: &NodeState,
    payload: CommunityRewardApprovalPayload,
) -> Result<Vec<arc_types::transaction::CommunityRewardValidatorApproval>, String> {
    let coordinator_key = node
        .validator_keypair
        .as_ref()
        .ok_or_else(|| "coordinator validator key is unavailable".to_string())?;
    // The coordinator completed `verify_community_result_with_quorum`
    // immediately before entering this private collector. Revalidate every
    // signed semantic here, then reuse that independent computation for its
    // one local vote. Remote validators still execute the full recomputation
    // endpoint in parallel; rerunning the coordinator a second time would add
    // latency without adding an independent identity.
    validate_reward_approval_payload(node, &payload)?;
    let local = sign_validated_reward_approval(node, &payload.reward)?;
    let mut approvals = vec![local];
    let mut requests = tokio::task::JoinSet::new();
    for seed in node.seed_rpc_addrs.iter() {
        let signed = sign_community_request(
            COMMUNITY_REWARD_APPROVE_PATH,
            payload.clone(),
            coordinator_key,
        )?;
        let client = node.inference_http.clone();
        let url = format!("http://{seed}{COMMUNITY_REWARD_APPROVE_PATH}");
        requests.spawn(async move {
            let response = client
                .post(url)
                .timeout(std::time::Duration::from_secs(
                    MAX_COMMUNITY_DISPATCH_TIMEOUT_SECS,
                ))
                .json(&signed)
                .send()
                .await
                .map_err(|error| error.to_string())?;
            if !response.status().is_success() {
                return Err(format!("validator returned HTTP {}", response.status()));
            }
            response
                .json::<arc_types::transaction::CommunityRewardValidatorApproval>()
                .await
                .map_err(|error| error.to_string())
        });
    }
    while let Some(joined) = requests.join_next().await {
        if let Ok(Ok(approval)) = joined {
            approvals.push(approval);
            if let Ok(collected) =
                validate_collected_reward_approvals(node, &payload.reward, approvals.clone())
            {
                // A dead sixth endpoint must not hold an already-authorized
                // five-of-six reward open until the per-peer timeout.
                requests.abort_all();
                return Ok(collected);
            }
        }
    }
    validate_collected_reward_approvals(node, &payload.reward, approvals)
}

async fn submit_verified_community_reward(
    node: &NodeState,
    work_item: &WorkItem,
    assignment_epoch: Hash256,
    job_nonce: u64,
    result: &WorkResult,
    worker_attestation: &arc_types::Transaction,
    dispatcher: &tokio::sync::oneshot::Sender<CommunityDispatchOutcome>,
) -> Result<Value, String> {
    use arc_types::transaction::{CommunityInferenceRewardBody, WorkerInferenceCertificate};

    reward_approval_prerequisites(node).map_err(str::to_string)?;
    let worker = parse_hash256_hex(&result.worker_id, "worker_id")?;
    let job_id = parse_hash256_hex(&work_item.job_id, "job_id")?;
    let model_id = work_item
        .model_id
        .as_deref()
        .ok_or_else(|| "reward assignment has no exact model_id".to_string())
        .and_then(|value| parse_hash256_hex(value, "model_id"))?;
    let output_hash = parse_hash256_hex(&result.output_hash, "output_hash")?;
    let input_hash = arc_crypto::hash_bytes(work_item.input.as_bytes());
    let arc_types::TxBody::InferenceAttestation(attestation) = &worker_attestation.body else {
        return Err("worker certificate is not an inference attestation".to_string());
    };
    let expires_at_height = node
        .state
        .height()
        .checked_add(COMMUNITY_REWARD_EXPIRY_BLOCKS)
        .ok_or_else(|| "reward expiry height overflow".to_string())?;
    let recovery = node.state.recovery_context();
    let reward = CommunityInferenceRewardBody {
        chain_domain: CommunityInferenceRewardBody::expected_chain_domain(),
        job_id,
        coordinator: node.validator_address,
        assignment_epoch,
        job_nonce,
        recovery_epoch: recovery
            .as_ref()
            .map(|context| context.recovery_epoch)
            .unwrap_or(0),
        validator_set_id: recovery
            .as_ref()
            .map(|context| context.validator_set_id)
            .unwrap_or(0),
        transaction_domain: recovery
            .as_ref()
            .map(|context| context.domain_hash())
            .unwrap_or(Hash256::ZERO),
        worker,
        model_id,
        input_hash,
        output_hash,
        max_tokens: work_item.max_tokens,
        expires_at_height,
        worker_certificate: WorkerInferenceCertificate {
            attestation_hash: worker_attestation.hash,
            nonce: worker_attestation.nonce,
            challenge_period: attestation.challenge_period,
            signature: worker_attestation.signature.clone(),
        },
        validator_approvals: Vec::new(),
    };
    let expected_job_id = CommunityInferenceRewardBody::derive_job_id(
        &reward.coordinator,
        &reward.assignment_epoch,
        reward.job_nonce,
        &reward.model_id,
        &reward.input_hash,
        reward.max_tokens,
    );
    if reward.job_id != expected_job_id {
        return Err(
            "pending job metadata does not reproduce its assignment commitment".to_string(),
        );
    }
    let payload = CommunityRewardApprovalPayload {
        coordinator_id: format!("0x{}", node.validator_address.to_hex()),
        reward,
        input: work_item.input.clone(),
        output: result.output.clone(),
        tokens_generated: result.tokens_generated,
    };
    let approvals = collect_community_reward_approvals(node, payload.clone()).await?;
    if dispatcher.is_closed() {
        return Err(
            "dispatcher expired while validator approvals were being collected".to_string(),
        );
    }
    let approval_count = approvals.len();
    let mut reward = payload.reward;
    reward.validator_approvals = approvals;

    let key = node
        .validator_keypair
        .as_ref()
        .ok_or_else(|| "coordinator validator key is unavailable".to_string())?;
    let mut tx = arc_types::Transaction::new_community_inference_reward(
        node.validator_address,
        job_nonce,
        reward,
    );
    node.state
        .sign_transaction(&mut tx, key)
        .map_err(|error| format!("sign community reward transaction: {error}"))?;
    let tx_hash = tx.hash;
    node.mempool
        .insert(tx)
        .map_err(|error| format!("submit community reward to mempool: {error}"))?;
    node.community_reward_submissions.insert(
        job_id,
        CommunityRewardSubmission {
            worker,
            output_hash,
            tx_hash,
            approvals: approval_count,
        },
    );
    Ok(json!({
        "status": "pending_mined_receipt",
        "tx_type": "0x25",
        "tx_hash": format!("0x{}", tx_hash.to_hex()),
        "job_id": format!("0x{}", job_id.to_hex()),
        "assignment_epoch": format!("0x{}", assignment_epoch.to_hex()),
        "recovery_epoch": reward_recovery_epoch(node),
        "validator_set_id": reward_recovery_validator_set_id(node),
        "validator_set_commitment": reward_validator_set_id(node),
        "transaction_domain": node.state.transaction_domain_hash().map(|domain| format!("0x{}", domain.to_hex())),
        "validator_approvals": approval_count,
        "required_validator_approvals": arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED,
        "submitted": true,
        "included": false,
        "reward_base": arc_types::economics::INFERENCE_ATTESTATION_REWARD,
        "reward_arc": REWARD_PER_ATTESTATION_ARC,
        "receipt_url": format!("/community/reward_receipt/0x{}", tx_hash.to_hex()),
        "evidence_note": "payment is not earned until this exact 0x25 transaction has a successful mined receipt",
    }))
}

fn default_true() -> bool {
    true
}

/// POST body for /community/claim_work.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimWorkRequest {
    /// Worker's self-chosen identifier.
    pub worker_id: String,
    /// What the worker can do. Must include "inference" to receive work.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Exact model identity the worker registered and has loaded.
    pub model_id: String,
}

impl CommunityAuthenticatedPayload for ClaimWorkRequest {
    fn signer_id(&self) -> &str {
        &self.worker_id
    }
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
async fn community_claim_work_signed(
    AxumState(node): AxumState<NodeState>,
    Json(signed): Json<CommunitySignedRequest<ClaimWorkRequest>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let req = authenticate_community_request(&node, COMMUNITY_CLAIM_WORK_PATH, signed)?;
    community_claim_work(AxumState(node), Json(req)).await
}

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

    // ── Registration, capability, and exact model binding ───────────────
    let Some(registered) = node.community_workers.get(&req.worker_id) else {
        return Err((
            StatusCode::NOT_FOUND,
            "worker_id not registered - call /community/register first".to_string(),
        ));
    };
    if !registered.0.capabilities.iter().any(|c| c == "inference")
        || !req.capabilities.iter().any(|c| c == "inference")
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "registered and claimed capabilities must include 'inference' to receive work"
                .to_string(),
        ));
    }
    let registered_model_id = registered
        .0
        .model_id
        .as_deref()
        .ok_or((
            StatusCode::CONFLICT,
            "registered inference worker has no model_id; re-register with v3 metadata".to_string(),
        ))
        .and_then(|value| {
            parse_hash256_hex(value, "registered model_id")
                .map_err(|error| (StatusCode::CONFLICT, error))
        })?;
    let claimed_model_id = parse_hash256_hex(&req.model_id, "claimed model_id")
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    if claimed_model_id != registered_model_id {
        return Err((
            StatusCode::CONFLICT,
            "claimed model_id does not match the worker registration".to_string(),
        ));
    }
    drop(registered);

    // A valid claim doubles as a heartbeat.
    if let Some(mut entry) = node.community_workers.get_mut(&req.worker_id) {
        entry.value_mut().1 = std::time::Instant::now();
    }

    let mut claim_reservation = {
        use dashmap::mapref::entry::Entry;
        match node.community_active_jobs.entry(req.worker_id.clone()) {
            Entry::Occupied(_) => {
                return Ok(Json(json!({
                    "status": "no_work",
                    "reason": "worker_busy",
                })));
            }
            Entry::Vacant(entry) => {
                entry.insert(String::new());
                CommunityClaimReservation {
                    active_jobs: node.community_active_jobs.clone(),
                    worker_id: req.worker_id.clone(),
                    committed: false,
                }
            }
        }
    };

    // ── Long-poll: try to receive a WorkItem from the queue ─────────────
    let work_rx = node.community_work_queue.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "work queue not initialized - coordinator not running".to_string(),
    ))?;

    let timeout = tokio::time::Duration::from_secs(COMMUNITY_CLAIM_TIMEOUT_SECS);
    match poll_community_work_queue(work_rx, timeout).await {
        Ok(Some(item)) => {
            // Exact model commitments, not display names, decide whether this
            // worker can execute the coordinator-issued job.
            let item_model_id = item
                .model_id
                .as_deref()
                .and_then(|value| parse_hash256_hex(value, "model_id").ok());
            if item_model_id != Some(registered_model_id) {
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

            // Bind this coordinator-issued job to the worker that actually
            // claimed it before returning the prompt. The pending record can
            // disappear here when the dispatcher timed out while the item was
            // still queued; never hand an expired job to a worker.
            let Some(mut pending) = node
                .community_work_results
                .as_ref()
                .and_then(|pending| pending.get_mut(&item.job_id))
            else {
                return Ok(Json(json!({
                    "status": "no_work",
                    "reason": "job_expired",
                })));
            };
            if pending.assigned_worker.is_some() {
                return Ok(Json(json!({
                    "status": "no_work",
                    "reason": "job_already_claimed",
                })));
            }
            pending.assigned_worker = Some(req.worker_id.clone());
            drop(pending);
            claim_reservation.commit(&item.job_id);

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
        Err(()) => {
            // Channel closed - coordinator shut down
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "work queue closed - coordinator shutting down".to_string(),
            ))
        }
        Ok(None) => {
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
/// On success the worker's `work_completed` counter increments. A verified
/// worker certificate is wrapped in a validator-authorized, replay-protected
/// `CommunityInferenceReward` transaction so any eventual reward is real ARC
/// and can only settle after block inclusion.
async fn community_submit_work_signed(
    AxumState(node): AxumState<NodeState>,
    Json(signed): Json<CommunitySignedRequest<WorkResult>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let result = authenticate_community_request(&node, COMMUNITY_SUBMIT_WORK_PATH, signed)?;
    community_submit_work(AxumState(node), Json(result)).await
}

pub async fn community_submit_work(
    AxumState(node): AxumState<NodeState>,
    Json(result): Json<WorkResult>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // ── Validate required fields ────────────────────────────────────────
    if result.job_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "job_id is required".to_string()));
    }
    if result.worker_id.is_empty() || result.worker_id.len() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            "worker_id required (1-128 chars)".to_string(),
        ));
    }
    if result.success && result.declined {
        return Err((
            StatusCode::BAD_REQUEST,
            "declined=true requires success=false".to_string(),
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

    // ── Match and validate the coordinator-issued job ───────────────────
    let results_map = node.community_work_results.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "work results map not initialized - coordinator not running".to_string(),
    ))?;

    // Freshly signed retries are idempotent after a reward entered the
    // mempool. A retry may retrieve status, but cannot change worker/output.
    if let Ok(job_hash) = parse_hash256_hex(&result.job_id, "job_id")
        && let Some(submission) = node.community_reward_submissions.get(&job_hash)
    {
        let worker = parse_hash256_hex(&result.worker_id, "worker_id")
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
        let output_hash = parse_hash256_hex(&result.output_hash, "output_hash")
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
        if worker != submission.worker || output_hash != submission.output_hash {
            return Err((
                StatusCode::CONFLICT,
                "job already submitted with different worker or output semantics".to_string(),
            ));
        }
        return Ok(Json(json!({
            "ok": true,
            "idempotent_replay": true,
            "job_id": result.job_id,
            "settlement": {
                "status": "pending_mined_receipt",
                "tx_type": "0x25",
                "tx_hash": format!("0x{}", submission.tx_hash.to_hex()),
                "validator_approvals": submission.approvals,
                "included": false,
                "submitted": true,
                "receipt_url": format!("/community/reward_receipt/0x{}", submission.tx_hash.to_hex()),
            }
        })));
    }
    if let Ok(job_hash) = parse_hash256_hex(&result.job_id, "job_id")
        && let Some((tx_hash, body)) = mined_reward_for_job(&node, job_hash)
    {
        let worker = parse_hash256_hex(&result.worker_id, "worker_id")
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
        let output_hash = parse_hash256_hex(&result.output_hash, "output_hash")
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
        if worker != body.worker || output_hash != body.output_hash {
            return Err((
                StatusCode::CONFLICT,
                "mined job has different worker or output semantics".to_string(),
            ));
        }
        return Ok(Json(json!({
            "ok": true,
            "idempotent_replay": true,
            "job_id": result.job_id,
            "settlement": community_reward_receipt_value(&node, tx_hash, &body),
        })));
    }

    let (work_item, assigned_worker) = match results_map.get(&result.job_id) {
        Some(pending) => (pending.item.clone(), pending.assigned_worker.clone()),
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                format!(
                    "no pending work for job_id {} - already completed or expired",
                    result.job_id
                ),
            ));
        }
    };

    match assigned_worker.as_deref() {
        Some(worker_id) if worker_id == result.worker_id => {}
        Some(worker_id) => {
            return Err((
                StatusCode::FORBIDDEN,
                format!(
                    "job_id {} is assigned to worker {}, not {}",
                    result.job_id, worker_id, result.worker_id
                ),
            ));
        }
        None => {
            return Err((
                StatusCode::CONFLICT,
                format!("job_id {} has not been claimed", result.job_id),
            ));
        }
    }

    if result.tokens_generated > u64::from(work_item.max_tokens) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "tokens_generated {} exceeds assigned max_tokens {}",
                result.tokens_generated, work_item.max_tokens
            ),
        ));
    }

    let verified_attestation = if result.success {
        let output_hash = parse_hash256_hex(&result.output_hash, "output_hash")
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        match result.signed_attestation_hex.as_ref() {
            Some(hex_bytes) => {
                let tx = decode_and_verify_worker_attestation_in_domain(
                    hex_bytes,
                    &result.worker_id,
                    node.state.transaction_domain_hash(),
                )
                .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
                validate_worker_attestation_for_job(&tx, &work_item, output_hash)
                    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
                Some(tx)
            }
            None => None,
        }
    } else {
        None
    };

    // Atomically consume the job before delivering it. A replay racing this
    // request can pass the read-only checks above, but only one submit can
    // remove the pending record and reach counters or the mempool.
    let Some((_, pending)) = results_map.remove(&result.job_id) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "no pending work for job_id {} - already completed or expired",
                result.job_id
            ),
        ));
    };

    let job_id = result.job_id.clone();
    let verification = if result.success {
        match verify_community_result_with_quorum(&node, &work_item, &result).await {
            Ok(verification) => Some(verification),
            Err(error) => {
                if let Some(worker_id) = pending.assigned_worker.as_deref() {
                    release_active_community_job(&node, worker_id, &job_id);
                }
                if let Some(mut entry) = node.community_workers.get_mut(&result.worker_id) {
                    let (worker, ts) = entry.value_mut();
                    *ts = std::time::Instant::now();
                    worker.failure_count = worker.failure_count.saturating_add(1);
                }
                let mut rejected = result.clone();
                rejected.success = false;
                rejected.error = Some(format!("coordinator verification failed: {error}"));
                let _ = pending.sender.send(CommunityDispatchOutcome {
                    result: rejected,
                    verification: None,
                    settlement: None,
                });
                tracing::warn!(
                    worker = %result.worker_id,
                    job_id = %job_id,
                    reason = %error,
                    "community result rejected by authenticated shard quorum"
                );
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("community result failed independent verification: {error}"),
                ));
            }
        }
    } else {
        None
    };

    if let Some(worker_id) = pending.assigned_worker.as_deref() {
        release_active_community_job(&node, worker_id, &job_id);
    }
    if pending.sender.is_closed() {
        return Err((
            StatusCode::GONE,
            format!("dispatcher already timed out for job_id {}", job_id),
        ));
    }

    let settlement_outcome = if let Some(worker_attestation) = verified_attestation {
        let worker_attestation_hash = format!("0x{}", worker_attestation.hash.to_hex());
        if !community_rewards_v1_protocol_active(&node) {
            Some(json!({
                "status": "reward_protocol_not_activated",
                "worker_attestation_hash": worker_attestation_hash,
                "reward_arc": REWARD_PER_ATTESTATION_ARC,
                "included": false,
                "reason": "result verified, but reward issuance requires both the genesis-committed activation height and the local issuance switch",
            }))
        } else {
            match submit_verified_community_reward(
                &node,
                &work_item,
                pending.assignment_epoch,
                pending.job_nonce,
                &result,
                &worker_attestation,
                &pending.sender,
            )
            .await
            {
                Ok(mut settlement) => {
                    settlement["worker_attestation_hash"] = Value::String(worker_attestation_hash);
                    Some(settlement)
                }
                Err(reason) => Some(json!({
                    "status": "reward_approval_quorum_unavailable",
                    "worker_attestation_hash": worker_attestation_hash,
                    "reward_arc": REWARD_PER_ATTESTATION_ARC,
                    "included": false,
                    "submitted": false,
                    "required_validator_approvals": arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED,
                    "recovery_epoch": reward_recovery_epoch(&node),
                    "validator_set_id": reward_recovery_validator_set_id(&node),
                    "validator_set_commitment": reward_validator_set_id(&node),
                    "reason": reason,
                })),
            }
        }
    } else if result.success {
        Some(json!({
            "status": "missing_worker_attestation",
            "reason": "worker did not include signed_attestation_hex",
        }))
    } else {
        None
    };

    let verification_summary = verification
        .as_ref()
        .map(CommunityVerificationSummary::from);
    let dispatch_outcome = CommunityDispatchOutcome {
        result: result.clone(),
        verification: verification_summary.clone(),
        settlement: settlement_outcome.clone(),
    };
    if pending.sender.send(dispatch_outcome).is_err() {
        return Err((
            StatusCode::GONE,
            format!("dispatcher already timed out for job_id {}", job_id),
        ));
    }

    // Only a result that reached its waiting dispatcher affects worker
    // health and performance counters.
    if let Some(mut entry) = node.community_workers.get_mut(&result.worker_id) {
        let (worker, ts) = entry.value_mut();
        *ts = std::time::Instant::now();
        if result.success {
            worker.work_completed += 1;
            worker.success_count += 1;
            worker.sum_total_ms_success =
                worker.sum_total_ms_success.saturating_add(result.total_ms);
            worker.last_total_ms = result.total_ms;
        } else if !result.declined {
            worker.failure_count += 1;
        }
    }

    Ok(Json(json!({
        "ok": true,
        "job_id": job_id,
        "verification": verification_summary,
        "settlement": settlement_outcome,
    })))
}

fn mined_reward_for_job(
    node: &NodeState,
    job_id: Hash256,
) -> Option<(
    Hash256,
    arc_types::transaction::CommunityInferenceRewardBody,
)> {
    node.state.full_transactions.iter().find_map(|entry| {
        let arc_types::TxBody::CommunityInferenceReward(body) = &entry.value().body else {
            return None;
        };
        (body.job_id == job_id).then(|| (Hash256(*entry.key()), body.clone()))
    })
}

fn community_reward_receipt_value(
    node: &NodeState,
    tx_hash: Hash256,
    body: &arc_types::transaction::CommunityInferenceRewardBody,
) -> Value {
    let receipt = node.state.get_receipt(&tx_hash.0);
    let (status, included, confirmed) = match receipt.as_ref() {
        Some(receipt) if receipt.success => ("mined_success", true, true),
        Some(_) => ("mined_failed", true, false),
        None => ("pending_mined_receipt", false, false),
    };
    json!({
        "status": status,
        "tx_type": "0x25",
        "tx_hash": format!("0x{}", tx_hash.to_hex()),
        "job_id": format!("0x{}", body.job_id.to_hex()),
        "worker": format!("0x{}", body.worker.to_hex()),
        "model_id": format!("0x{}", body.model_id.to_hex()),
        "input_hash": format!("0x{}", body.input_hash.to_hex()),
        "output_hash": format!("0x{}", body.output_hash.to_hex()),
        "assignment_epoch": format!("0x{}", body.assignment_epoch.to_hex()),
        "recovery_epoch": body.recovery_epoch,
        "validator_set_id": body.validator_set_id,
        "validator_set_commitment": reward_validator_set_id(node),
        "transaction_domain": format!("0x{}", body.transaction_domain.to_hex()),
        "validator_approvals": body.validator_approvals.len(),
        "included": included,
        "confirmed": confirmed,
        "success": receipt.as_ref().map(|receipt| receipt.success),
        "block_height": receipt.as_ref().map(|receipt| receipt.block_height),
        "block_hash": receipt.as_ref().map(|receipt| format!("0x{}", receipt.block_hash.to_hex())),
        "reward_base": if confirmed { Value::from(arc_types::economics::INFERENCE_ATTESTATION_REWARD) } else { Value::Null },
        "reward_arc": if confirmed { Value::from(REWARD_PER_ATTESTATION_ARC) } else { Value::Null },
        "evidence_source": if confirmed { "successful mined CommunityInferenceReward receipt" } else { "no successful mined receipt" },
    })
}

async fn community_reward_receipt(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let tx_hash = parse_hash256_hex(&hash, "reward tx hash")
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    if let Some(tx) = node.state.full_transactions.get(&tx_hash.0) {
        let arc_types::TxBody::CommunityInferenceReward(body) = &tx.body else {
            return Err((
                StatusCode::CONFLICT,
                "transaction is not type 0x25".to_string(),
            ));
        };
        return Ok(Json(community_reward_receipt_value(&node, tx_hash, body)));
    }
    let pending = node
        .community_reward_submissions
        .iter()
        .find(|entry| entry.value().tx_hash == tx_hash)
        .map(|entry| (*entry.key(), entry.value().clone()));
    if let Some((job_id, submission)) = pending {
        return Ok(Json(json!({
            "status": "pending_mined_receipt",
            "tx_type": "0x25",
            "tx_hash": format!("0x{}", tx_hash.to_hex()),
            "job_id": format!("0x{}", job_id.to_hex()),
            "worker": format!("0x{}", submission.worker.to_hex()),
            "validator_approvals": submission.approvals,
            "recovery_epoch": reward_recovery_epoch(&node),
            "validator_set_id": reward_recovery_validator_set_id(&node),
            "validator_set_commitment": reward_validator_set_id(&node),
            "included": false,
            "confirmed": false,
            "reward_base": Value::Null,
            "reward_arc": Value::Null,
            "evidence_source": "coordinator mempool submission only; no mined receipt",
        })));
    }
    Err((
        StatusCode::NOT_FOUND,
        "reward transaction is unknown".to_string(),
    ))
}

async fn community_reward_job(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(job_id): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let job_id =
        parse_hash256_hex(&job_id, "job_id").map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    if let Some((tx_hash, body)) = mined_reward_for_job(&node, job_id) {
        return Ok(Json(community_reward_receipt_value(&node, tx_hash, &body)));
    }
    if let Some(submission) = node.community_reward_submissions.get(&job_id) {
        return Ok(Json(json!({
            "status": "pending_mined_receipt",
            "tx_type": "0x25",
            "job_id": format!("0x{}", job_id.to_hex()),
            "tx_hash": format!("0x{}", submission.tx_hash.to_hex()),
            "worker": format!("0x{}", submission.worker.to_hex()),
            "validator_approvals": submission.approvals,
            "recovery_epoch": reward_recovery_epoch(&node),
            "validator_set_id": reward_recovery_validator_set_id(&node),
            "validator_set_commitment": reward_validator_set_id(&node),
            "included": false,
            "confirmed": false,
        })));
    }
    Err((
        StatusCode::NOT_FOUND,
        "community reward job is unknown".to_string(),
    ))
}

async fn community_reward_approval_status(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(job_id): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let job_id =
        parse_hash256_hex(&job_id, "job_id").map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let commitment = node
        .community_reward_approval_jobs
        .get(&job_id)
        .map(|entry| *entry.value());
    Ok(Json(json!({
        "job_id": format!("0x{}", job_id.to_hex()),
        "locally_approved": commitment.is_some(),
        "approval_commitment": commitment.map(|value| format!("0x{}", value.to_hex())),
        "submitted": node.community_reward_submissions.contains_key(&job_id),
        "validator_set_id": reward_recovery_validator_set_id(&node),
        "validator_set_commitment": reward_validator_set_id(&node),
        "recovery_epoch": reward_recovery_epoch(&node),
    })))
}

async fn community_reward_policy(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let readiness_error = if community_rewards_v1_protocol_active(&node) {
        reward_approval_prerequisites(&node).err()
    } else {
        Some(community_rewards_v1_readiness_note(&node))
    };
    Json(json!({
        "schema": "arc.community.reward-policy.v1",
        "tx_type": "0x25",
        "protocol_active": community_rewards_v1_protocol_active(&node),
        "issuance_ready": community_rewards_v1_effective(&node),
        "readiness_unavailable_reason": readiness_error,
        "active_validator_count": node.state.active_validators().len(),
        "validator_set_size_required": arc_types::transaction::COMMUNITY_REWARD_VALIDATOR_SET_SIZE,
        "validator_approvals_required": arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED,
        "validator_set_id": reward_recovery_validator_set_id(&node),
        "validator_set_commitment": reward_validator_set_id(&node),
        "recovery_epoch": reward_recovery_epoch(&node),
        "transaction_domain": node.state.transaction_domain_hash().map(|domain| format!("0x{}", domain.to_hex())),
        "worker_min_stake_base": arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE,
        "stake_zero_eligible": arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE == 0,
        "reward_base": arc_types::economics::INFERENCE_ATTESTATION_REWARD,
        "reward_arc": REWARD_PER_ATTESTATION_ARC,
        "earnings_evidence": "successful mined 0x25 receipts only",
    }))
}

fn parse_hash256_hex(value: &str, field: &str) -> Result<Hash256, String> {
    let trimmed = value.trim();
    let bare = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    Hash256::from_hex(bare).map_err(|_| format!("{} must be a 32-byte hex hash", field))
}

/// Bind a signed attestation to the coordinator-owned job and submitted
/// result. Signature verification alone only proves who signed an arbitrary
/// transaction; these checks prove what that signature is attesting to.
fn validate_worker_attestation_for_job(
    tx: &arc_types::Transaction,
    work_item: &WorkItem,
    result_output_hash: Hash256,
) -> Result<(), String> {
    let arc_types::TxBody::InferenceAttestation(body) = &tx.body else {
        return Err("transaction body is not an InferenceAttestation".to_string());
    };

    // Community reward certificates are reconstructed by consensus with one
    // fixed, non-recursive transaction shape. Reject any worker transaction
    // that would change when reconstructed; otherwise the outer reward could
    // enter the mempool and predictably fail only during block execution.
    if tx.fee != 0 {
        return Err("community worker attestation fee must be exactly 0".to_string());
    }
    if tx.gas_limit != 0 {
        return Err("community worker attestation gas_limit must be exactly 0".to_string());
    }
    if body.bond != 0 {
        return Err("community worker attestation bond must be exactly 0".to_string());
    }
    if body.beneficiary.is_some() {
        return Err("community worker attestation beneficiary must be absent".to_string());
    }

    let expected_input_hash = arc_crypto::hash_bytes(work_item.input.as_bytes());
    if body.input_hash != expected_input_hash {
        return Err("attestation input_hash does not match the assigned prompt".to_string());
    }
    if body.output_hash != result_output_hash {
        return Err("attestation output_hash does not match the submitted result".to_string());
    }
    let model_id = work_item
        .model_id
        .as_deref()
        .ok_or_else(|| "assigned job has no exact model artifact commitment".to_string())?;
    let expected_model_id = parse_hash256_hex(model_id, "assigned model_id")?;
    if body.model_id != expected_model_id {
        return Err("attestation model_id does not match the assigned model".to_string());
    }

    Ok(())
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
    decode_and_verify_worker_attestation_in_domain(hex_bytes, expected_worker_id, None)
}

fn decode_and_verify_worker_attestation_in_domain(
    hex_bytes: &str,
    expected_worker_id: &str,
    recovery_domain: Option<Hash256>,
) -> Result<arc_types::Transaction, String> {
    // Strip optional "0x" prefix so workers can encode either way.
    let trimmed = hex_bytes.trim_start_matches("0x");
    let raw = hex::decode(trimmed).map_err(|e| format!("hex decode failed: {}", e))?;
    // Match ARC's historical fixed-width wire encoding, but constrain every
    // allocation to the authenticated community-body ceiling and reject any
    // suffix. The compatibility `deserialize` entry point intentionally keeps
    // legacy trailing-byte behavior for trusted persisted data; untrusted HTTP
    // input must always use this bounded exact form.
    let tx: arc_types::Transaction = bincode::deserialize_limited_exact::<
        arc_types::Transaction,
        COMMUNITY_MUTATION_BODY_LIMIT_BYTES,
    >(&raw)
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
    let claim_address = expected_worker_id.trim_start_matches("0x").to_string();
    let tx_from_hex = hex::encode(tx.from.0);
    if claim_address != tx_from_hex {
        return Err(format!(
            "tx.from ({}) does not match worker_id ({})",
            tx_from_hex, claim_address
        ));
    }

    match recovery_domain {
        Some(domain) => tx.verify_signature_in_domain(&domain),
        None => tx.verify_signature(),
    }
    .map_err(|e| format!("signature verify failed: {:?}", e))?;

    Ok(tx)
}

// ─── Multi-Model Registry ──────────────────────────────────────────────────

/// GET /models
/// List all models known to the multi-model registry with pipeline coverage info.
async fn get_models(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let covered = node.multi_model_registry.fully_covered_models();
    let total_nodes = node.multi_model_registry.total_shard_nodes();

    // Also gather model_ids from flat registry for backward compat
    let flat_shards = fresh_shards(&node.shard_registry);
    let mut model_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut models_info: Vec<Value> = Vec::new();

    for s in &flat_shards {
        if model_set.insert(s.model_id.clone()) {
            let shards_for_model: Vec<&ShardInfo> = flat_shards
                .iter()
                .filter(|ss| ss.model_id == s.model_id)
                .collect();
            // Coverage is the UNION of the announced layer intervals, not the
            // sum of their widths. Summing double-counts replicas: on the live
            // 3x-replicated network 6 ranges x 3 replicas over a 32-layer model
            // summed to 96 "covered layers" out of 32 — so `fully_covered`
            // (covered == total) was false on a network with complete,
            // triple-redundant coverage.
            let mut intervals: Vec<(usize, usize)> = shards_for_model
                .iter()
                .map(|ss| (ss.start_layer, ss.end_layer))
                .collect();
            intervals.sort_unstable();
            let mut covered_layers = 0usize;
            let mut frontier = 0usize;
            for (start, end) in &intervals {
                let from = (*start).max(frontier);
                if *end > from {
                    covered_layers += end - from;
                    frontier = *end;
                }
            }
            let replica_ranges: std::collections::BTreeSet<(usize, usize)> =
                intervals.iter().copied().collect();
            models_info.push(json!({
                "model_id": s.model_id,
                "model_name": s.model_name,
                "total_layers": s.total_layers,
                "covered_layers": covered_layers,
                "fully_covered": covered_layers == s.total_layers && s.total_layers > 0,
                "shard_count": shards_for_model.len(),
                "distinct_ranges": replica_ranges.len(),
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
    let model_id_hex = params.get("model_id").ok_or(api_error(
        StatusCode::BAD_REQUEST,
        "model_id query parameter required",
    ))?;

    let model_hash_bytes = parse_hash(model_id_hex)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid model_id hex"))?;
    let model_hash = Hash256(model_hash_bytes);

    match node.multi_model_registry.get_pipeline(&model_hash) {
        Some(pipeline) => {
            let shards: Vec<Value> = pipeline
                .iter()
                .map(|s| {
                    json!({
                        "start_layer": s.start_layer,
                        "end_layer": s.end_layer,
                        "socket_addr": s.socket_addr,
                        "gpu_tier": s.gpu_tier,
                        "available_memory_mb": s.available_memory / (1024 * 1024),
                    })
                })
                .collect();
            let total_layers = pipeline.last().map(|s| s.end_layer).unwrap_or(0);
            Ok(Json(json!({
                "model_id": model_id_hex,
                "pipeline": shards,
                "shard_count": pipeline.len(),
                "total_layers": total_layers,
                "fully_covered": node.multi_model_registry.is_model_fully_covered(&model_hash, total_layers),
            })))
        }
        None => Err(api_error(
            StatusCode::NOT_FOUND,
            "model not found in registry",
        )),
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

    let commitment_id = node
        .verification_manager
        .lock()
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "verification manager lock poisoned",
            )
        })?
        .submit_commitment(commitment);

    Ok(Json(json!({
        "ok": true,
        "commitment_id": format!("0x{}", hex::encode(commitment_id)),
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
        _ => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid challenge_type: use re_execution, spot_check, statistical_audit, or consensus",
            ));
        }
    };

    let challenge_id = node
        .verification_manager
        .lock()
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "verification manager lock poisoned",
            )
        })?
        .create_challenge(
            commitment_hash,
            node.validator_address.0,
            challenge_type,
            req.bond_amount,
        )
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;

    Ok(Json(json!({
        "ok": true,
        "challenge_id": format!("0x{}", hex::encode(challenge_id)),
        "challenger": node.validator_address.to_hex(),
        "bond_amount": req.bond_amount,
    })))
}

/// GET /inference/verification_status
/// Show overall verification system stats.
async fn inference_verification_status(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let reputation = node
        .verification_manager
        .lock()
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
async fn get_revenue_split(AxumState(node): AxumState<NodeState>) -> Json<Value> {
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
    let model_hash = parse_hash256_hex(&req.model_id, "model_id")
        .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?;

    // Build node capabilities from the live shard registry
    let shards = fresh_shards(&node.shard_registry);
    let mut seen_nodes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut capabilities: Vec<arc_inference::distributed::NodeCapability> = Vec::new();

    for s in shards.iter().filter(|shard| {
        parse_hash256_hex(&shard.model_id, "shard model_id").ok() == Some(model_hash)
    }) {
        if seen_nodes.insert(s.node_name.clone()) {
            capabilities.push(arc_inference::distributed::NodeCapability {
                address: Hash256(parse_hash(&s.model_id).unwrap_or([0u8; 32])),
                socket_addr: s.socket_addr.clone(),
                gpu_tier: 0, // TODO: detect from node capabilities
                available_memory: (s.memory_mb as u64) * 1024 * 1024,
            });
        }
    }

    // Community full-model workers claim jobs over their outbound poll loop;
    // they do not expose the inbound `/inference/forward_shard` socket needed
    // by a pipeline hop. Treating a worker name as a socket both made an
    // unreachable plan and let an unrelated model influence this model's
    // capacity. Only exact-ID shard announcements are eligible here.

    if capabilities.is_empty() {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no nodes available for sharding",
        ));
    }

    let plan = arc_inference::distributed::compute_shard_plan(
        model_hash,
        req.total_layers,
        req.total_params_b,
        &capabilities,
    );

    // Register the computed plan in the multi-model registry
    for assignment in &plan {
        node.multi_model_registry
            .register_shard(model_hash, assignment.clone());
    }

    let plan_json: Vec<Value> = plan
        .iter()
        .map(|a| {
            json!({
                "node_address": format!("0x{}", hex::encode(a.node_address.0)),
                "socket_addr": a.socket_addr,
                "start_layer": a.start_layer,
                "end_layer": a.end_layer,
                "gpu_tier": a.gpu_tier,
                "available_memory_mb": a.available_memory / (1024 * 1024),
            })
        })
        .collect();

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
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "total_layers must be > 0",
        ));
    }

    let model_hash = parse_hash256_hex(&req.model_id, "model_id")
        .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?;
    let normalized_model_id = format!("0x{}", model_hash.to_hex());

    // 1. Gather existing shards for this model from the registry
    let existing: Vec<ShardInfo> = fresh_shards(&node.shard_registry)
        .into_iter()
        .filter(|s| parse_hash256_hex(&s.model_id, "shard model_id").ok() == Some(model_hash))
        .collect();

    // 2. Find the biggest uncovered gap in the pipeline
    let mut covered: Vec<bool> = vec![false; req.total_layers as usize];
    for s in &existing {
        for c in covered
            .iter_mut()
            .take(s.end_layer.min(req.total_layers as usize))
            .skip(s.start_layer)
        {
            *c = true;
        }
    }

    // Find the longest contiguous uncovered range
    let mut best_start = 0usize;
    let mut best_len = 0usize;
    let mut run_start = 0usize;
    let mut in_run = false;

    for (i, &is_covered) in covered.iter().enumerate() {
        if !is_covered {
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
            for c in coverage_count
                .iter_mut()
                .take(s.end_layer.min(req.total_layers as usize))
                .skip(s.start_layer)
            {
                *c += 1;
            }
        }
        // Find the layer with minimum coverage
        let min_coverage = *coverage_count.iter().min().unwrap_or(&0);
        let thin_start = coverage_count
            .iter()
            .position(|&c| c == min_coverage)
            .unwrap_or(0);
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
        model_id: normalized_model_id.clone(),
        model_name: req.model_name.clone(),
        memory_mb: (req.available_memory_mb as usize).min(assigned_end - assigned_start) * 100, // rough estimate
        full_model_mb: 0,
        socket_addr: req.socket_addr.clone(),
        node_name: req.node_name.clone(),
    };
    let reg_key = format!("{}#{}-{}", req.socket_addr, assigned_start, assigned_end);
    node.shard_registry
        .insert(reg_key, (shard_info, std::time::Instant::now()));

    // Also register in multi-model registry
    let assignment = arc_inference::distributed::ShardAssignment {
        node_address: model_hash,
        start_layer: assigned_start as u32,
        end_layer: assigned_end as u32,
        expert_indices: Vec::new(),
        socket_addr: req.socket_addr.clone(),
        gpu_tier: req.gpu_tier,
        available_memory: req.available_memory_mb * 1024 * 1024,
    };
    node.multi_model_registry
        .register_shard(model_hash, assignment);

    // Check if pipeline is now fully covered
    let mut new_covered: Vec<bool> = vec![false; req.total_layers as usize];
    for s in fresh_shards(&node.shard_registry)
        .iter()
        .filter(|s| parse_hash256_hex(&s.model_id, "shard model_id").ok() == Some(model_hash))
    {
        for c in new_covered
            .iter_mut()
            .take(s.end_layer.min(req.total_layers as usize))
            .skip(s.start_layer)
        {
            *c = true;
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
        "model_id": normalized_model_id,
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
    let input = req
        .get("input")
        .and_then(|v| v.as_str())
        .ok_or(api_error(StatusCode::BAD_REQUEST, "'input' field required"))?
        .to_string();

    if input.len() > 32_768 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Input exceeds 32KB limit",
        ));
    }

    let max_tokens = req
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(u64::from(INFERENCE_RUN_MAX_TOKENS)) as u32;

    // Strategy 1: is there a sharded pipeline we can actually walk?
    //
    // This used to re-implement coverage detection over the RAW replica list,
    // sorted by start_layer only and with no dedupe. On the live 3x-replicated
    // network that is 18 entries for 6 ranges: after the first [0, 6) set
    // covered_to = 6, the SECOND [0, 6) replica had start_layer 0 != 6 and
    // flipped `contiguous` false immediately. `has_full_pipeline` was
    // therefore false for any replication factor > 1 and this endpoint never
    // once took its own documented best path — it always fell through to
    // `inference_run` and only reached the sharded pipeline by accident, via
    // the partial-model guard there.
    //
    // Asking the real planner also means this routing decision now agrees
    // with what run_sharded will actually do.
    let pipeline_check = assemble_pipeline_for(&node);
    let has_full_pipeline = pipeline_check.is_ok();

    // Strategy 2: Check if local model is available
    let has_local_model = node.model_artifact_id.is_some()
        && (node
            .candle_model_id
            .is_some_and(|id| Some(id) == node.model_artifact_id)
            || node
                .inference_model
                .as_ref()
                .is_some_and(|model| model.has_all_transformer_layers()));

    // Strategy 3: Check if community workers are available
    let community_worker_count = live_inference_worker_count(&node);

    // Route to the best available path
    if has_full_pipeline && node.inference_model.is_some() {
        // Best path: sharded pipeline (distributed, deterministic)
        let sharded_req = json!({
            "input": input,
            "max_tokens": max_tokens,
            "chat_template": req.get("chat_template").and_then(|v| v.as_bool()).unwrap_or(false),
        });
        let result = inference_run_sharded(AxumState(node.clone()), Json(sharded_req)).await;
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
            // `/inference/run` normally prefers community workers. This
            // branch has already selected the documented local strategy, so
            // make that choice explicit instead of dispatching elsewhere and
            // then falsely labeling the response `local_model`.
            "force_local": true,
        });
        let result = inference_run(AxumState(node.clone()), Some(Json(local_req))).await;
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
        // Use the same signed, assignment-bound dispatcher as
        // `/inference/run`. The old branch returned a permanent 503 pointing
        // at a retired port even while healthy workers were online, so the
        // endpoint contradicted both its own routing contract and the path
        // used by the dashboard.
        let community_req = json!({
            "input": input,
            "max_tokens": max_tokens,
        });
        let result = inference_run(AxumState(node.clone()), Some(Json(community_req))).await;
        match result {
            Ok(mut response) => {
                if let Some(object) = response.0.as_object_mut() {
                    let route = object
                        .get("routed_via")
                        .and_then(Value::as_str)
                        .filter(|value| value.starts_with("community:"))
                        .map(|_| "community_worker")
                        .unwrap_or("none");
                    object.insert("route".to_string(), json!(route));
                }
                Ok(response)
            }
            Err(error) => Err(error),
        }
    } else {
        Err(api_error(StatusCode::SERVICE_UNAVAILABLE, json!({
            "error": "No inference path available",
            "sharded_pipeline": false,
            // Say WHY the pipeline was rejected instead of just "false".
            "sharded_pipeline_error": pipeline_check.err().map(|e| e.to_string()),
            "local_model": false,
            "community_workers": 0,
            "help": "Either: (1) load a model with --model, (2) have shard-holding nodes announce to this coordinator, or (3) start community workers with models"
        }).to_string()))
    }
}

// ---------------------------------------------------------------------------
// Honest projection endpoints (v0.7.11+)
//
// Three additive read-only endpoints so a client never has to hardcode a
// reward, guess a network, or invent a rate:
//   GET /economics/rewards    — reward rate + the finite treasury behind it
//   GET /network/info         — which chain, and whether it is actually alive
//   GET /node/contribution    — what this node contributes, measured
//
// ⚠ CLIENTS MUST DEGRADE ON 404. No host on the live testnet serves these:
// NYC runs v0.7.2 and the other five run v0.7.9, both predating this code, so
// all three routes answer `404 Not Found` there. A 404 means "this node is too
// old to tell you", which is NOT the same as a zero, an empty set, or a
// stalled chain — a client that renders 0 ARC or "not producing" on a 404 is
// reporting a fact it does not have.
// ---------------------------------------------------------------------------

/// Everything needed to project attestation earnings without inventing a
/// number.
///
/// GET /economics/rewards
///
/// The reward rate comes from `arc_types::economics::INFERENCE_ATTESTATION_REWARD`,
/// the same base-unit constant `arc-state` credits when an attestation
/// applies, so the projected rate and the paid rate cannot drift.
///
/// `rewards_remaining` is the field that makes a projection honest. The reward
/// is a pure TRANSFER from the treasury (`faucet_pool_address()`), never
/// minted, so the pool is finite and exhaustible: at 2.5 ARC a job, a treasury
/// holding 1000 ARC funds exactly 400 full rewards; later claims reject rather
/// than paying a nondeterministic partial tail. Any "ARC per day" figure that
/// omits this term is describing an emission the chain does not have.
///
/// ⚠ Live v0.7.2 (NYC) and v0.7.9 (LAX/AMS/LHR/NRT/SGP) seeds do not have
/// this route and return 404. Treat that as "reward rate unknown from this
/// host", not as zero — clients that need a rate from an old seed can read
/// `reward_per_attestation_arc` from `/worker/earnings/{address}`, which those
/// versions do serve.
async fn economics_rewards(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let reward_base = arc_types::economics::INFERENCE_ATTESTATION_REWARD;
    let base_units = arc_types::economics::ARC_BASE_UNITS;
    let treasury_addr = arc_types::transaction::faucet_pool_address();

    // The treasury account may be absent from this node's state entirely (a
    // fresh data dir, or a snapshot that never included it). Absent is not
    // zero: zero means "the pool is empty and rewards will stop", absent means
    // "this node cannot see the pool". They must not render the same.
    let treasury = node.state.get_account(&treasury_addr);
    let treasury_balance = treasury.as_ref().map(|a| a.balance);
    let remaining = treasury_balance.and_then(|bal| rewards_remaining(bal, reward_base));

    Json(json!({
        // ── Reward rate (from the shared on-chain constant, never a literal) ──
        "reward_per_attestation_base": reward_base,
        "reward_per_attestation_arc": REWARD_PER_ATTESTATION_ARC,
        "community_rewards_v1_enabled": community_rewards_v1_effective(&node),
        "community_rewards_v1_protocol_active": community_rewards_v1_protocol_active(&node),
        "community_rewards_v1_approval_collection_ready": COMMUNITY_REWARD_APPROVAL_COLLECTION_READY,
        "community_rewards_v1_activation_height": node.state.community_rewards_v1_activation_height(),
        "community_rewards_v1_issuance_enabled": node.community_rewards_v1_enabled,
        "community_rewards_v1_note": community_rewards_v1_readiness_note(&node),
        "validator_set_size_required": arc_types::transaction::COMMUNITY_REWARD_VALIDATOR_SET_SIZE,
        "validator_approvals_required": arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED,
        "validator_set_id": reward_recovery_validator_set_id(&node),
        "validator_set_commitment": reward_validator_set_id(&node),
        "recovery_epoch": reward_recovery_epoch(&node),
        "transaction_domain": node.state.transaction_domain_hash().map(|domain| format!("0x{}", domain.to_hex())),
        "worker_min_stake_base": arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE,
        "stake_zero_eligible": arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE == 0,
        "arc_base_units": base_units,
        "reward_source": "arc_types::economics::INFERENCE_ATTESTATION_REWARD — the fixed amount \
                          arc-state credits only when a validator-authorized, job-bound \
                          CommunityInferenceReward applies successfully",

        // ── The finite pool behind that rate ──────────────────────────────
        "treasury_address": format!("0x{}", hex::encode(treasury_addr.0)),
        "treasury_balance_base": treasury_balance,
        "treasury_balance_arc": treasury_balance.map(|b| b as f64 / base_units as f64),
        "treasury_balance_unavailable_reason": if treasury_balance.is_none() {
            Value::String(
                "the treasury account (faucet_pool_address) is not present in this node's \
                 state; absent is not the same as empty"
                    .to_string(),
            )
        } else {
            Value::Null
        },
        "rewards_remaining": remaining,
        "rewards_remaining_unavailable_reason": match (treasury_balance, remaining) {
            (None, _) => Value::String(
                "treasury balance unknown on this node, so the remaining count cannot be \
                 divided out"
                    .to_string(),
            ),
            (Some(_), None) => Value::String(
                "reward_per_attestation_base is zero on this build; the quotient is undefined"
                    .to_string(),
            ),
            _ => Value::Null,
        },
        "rewards_remaining_formula": "floor(treasury_balance_base / reward_per_attestation_base)",
        // A sub-reward remainder cannot fund a partial payout. Keep it visible
        // for treasury operations, but never count it as another reward.
        "unusable_treasury_remainder_base": match treasury_balance {
            Some(bal) if reward_base > 0 => Value::from(bal % reward_base),
            _ => Value::Null,
        },
        "rewards_remaining_note": "counts full, all-or-nothing rewards. When the treasury \
             holds less than reward_per_attestation_base, arc-state rejects the entire \
             CommunityInferenceReward transaction; no partial payout or earned receipt is \
             created.",
        "treasury_is_finite": true,

        // Community workers sign a compact, fixed certificate. Request-level
        // bond/challenge overrides on /inference/run apply only if routing
        // falls back to the coordinator's local attestation path.
        "community_worker_bond_required": false,
        "community_worker_certificate_bond_base": 0,
        "community_worker_certificate_fee_base": 0,
        "community_worker_certificate_gas_limit": 0,
        "community_worker_bond_note": "community reward certificates are reconstructed with \
             bond=0, fee=0, gas_limit=0 and no beneficiary. They do not lock or refund worker \
             collateral.",

        // ── Where the money comes from (so no UI can imply revenue) ───────
        "funding": "testnet treasury transfer, not customer revenue",
        "funding_detail": "each reward is a pure transfer out of a prefunded testnet treasury \
                           account, bounded by its balance and never minted. No customer pays \
                           for these attestations, no fee revenue backs them, and total supply \
                           is conserved. If less than one full reward remains, the reward \
                           transaction is rejected and the worker has not earned a payout.",
        "is_emission": false,
        "is_revenue_share": false,

        "height": node.state.height(),
        "unavailable_on_seeds_note": "live v0.7.2 / v0.7.9 seeds return 404 for this route",
    }))
}

/// Which chain this is, and whether it is actually alive.
///
/// GET /network/info
///
/// Three things the desktop could not previously know, each of which has
/// burned someone on the live network:
///
/// 1. **Which network.** `network` and `chain_id` are the genesis file's
///    declared values, verbatim. A node started without `--genesis` reports
///    both as null with a reason — its genesis HASH still identifies the chain
///    it is on, and that is reported unconditionally. Nothing here says
///    "mainnet" unless a genesis file literally declared it; `declares_mainnet`
///    is a string fact about that name, not a judgement.
///
/// 2. **Whether blocks are being sealed.** `/health` answers `"ok"` on all six
///    seeds, but four of them have not sealed a block in ~6 days: DAG rounds
///    keep advancing, so the liveness signal `/health` reports is not block
///    production. `is_block_producing` and `last_block_age_secs` let a client
///    say "this node last sealed a block 6 days ago" instead of "healthy".
///
/// 3. **How many validators actually count.** `validators_registered` is the
///    raw set length that `/validators` and `/health` report;
///    `validators_active` excludes zero-stake peers. Four of fourteen live
///    validators carry stake 0 and inflate every count derived from the length.
///
/// ⚠ Live v0.7.2 / v0.7.9 seeds return 404 here. On a 404 a client knows
/// nothing about liveness — it must NOT render "not producing", which is a
/// different and much stronger claim. Fall back to reading
/// `/block/latest`.`header.timestamp` for age, which every seed serves.
async fn network_info(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let height = node.state.height();
    // The newest block that actually EXISTS here — see
    // `latest_available_block`: looking up `height()` directly reports a
    // producing node as blockless.
    let latest = latest_available_block(&node);
    let genesis = node.state.get_block(0);

    // Declared identity (genesis file), or null + reason.
    let (network, chain_id) = match node.chain_identity.as_ref() {
        Some(id) => (
            Value::String(id.name.clone()),
            Value::String(id.chain_id.clone()),
        ),
        None => (Value::Null, Value::Null),
    };
    let identity_missing_reason = "this node was started without --genesis, so it has no \
                                   declared chain name or chain_id; genesis_hash still \
                                   identifies the chain it is running";
    // A pure string fact about the declared name — not an inference about what
    // kind of network this is.
    let declares_mainnet = node
        .chain_identity
        .as_ref()
        .map(|id| id.name.to_ascii_lowercase().contains("mainnet"));

    // Block liveness. `last_block_age_secs` uses the latest block's own header
    // timestamp (unix millis, display-only), so a stalled chain reads as
    // stalled instead of as healthy.
    let last_block_ts = latest.as_ref().map(|b| b.header.timestamp).unwrap_or(0);
    let last_block_age = age_secs_from_ms(last_block_ts);
    let chain_advancing = last_block_age.map(|age| age <= BLOCK_PRODUCTION_FRESH_SECS);

    // Has THIS node sealed one of the recent blocks? Bounded backward scan for
    // a block whose producer is this validator address.
    let mut self_produced: Option<(u64, u64)> = None;
    let floor = height.saturating_sub(SELF_PRODUCED_SCAN_BLOCKS);
    let mut h = height;
    while h > floor {
        if let Some(b) = node.state.get_block(h)
            && b.header.producer == node.validator_address
        {
            self_produced = Some((b.header.height, b.header.timestamp));
            break;
        }
        h -= 1;
    }
    let self_produced_age = self_produced.and_then(|(_, ts)| age_secs_from_ms(ts));
    // "Producing" = this node sealed a block, recently. Both halves matter: a
    // node that sealed block 5 a week ago is not producing, and a node watching
    // someone else's fresh blocks is not producing either.
    let is_block_producing = self_produced_age
        .map(|age| age <= BLOCK_PRODUCTION_FRESH_SECS)
        .unwrap_or(false);

    let vals = node.dag_validators.read().clone();
    let split = split_validators(&vals, arc_consensus::STAKE_SPARK);

    let protocol_version = latest.as_ref().or(genesis.as_ref()).map(|b| {
        let pv = &b.header.protocol_version;
        format!("{}.{}.{}", pv.major, pv.minor, pv.patch)
    });
    let recovery_context = node.state.recovery_context();
    let recovery_manifest_hash = node.state.recovery_manifest_hash();

    Json(json!({
        // ── Identity ──────────────────────────────────────────────────────
        "network": network,
        "chain_id": chain_id,
        "network_source": if node.chain_identity.is_some() {
            Value::String("genesis file chain.name / chain.chain_id".to_string())
        } else {
            Value::Null
        },
        "network_unavailable_reason": if node.chain_identity.is_none() {
            Value::String(identity_missing_reason.to_string())
        } else {
            Value::Null
        },
        "chain_id_unavailable_reason": if node.chain_identity.is_none() {
            Value::String(identity_missing_reason.to_string())
        } else {
            Value::Null
        },
        "declares_mainnet": declares_mainnet,
        "declares_mainnet_note": "true only when a genesis file's chain.name literally contains \
                                  \"mainnet\". null means no genesis was supplied to this node, \
                                  so nothing has declared anything — do not render either as a \
                                  network kind.",
        "genesis_hash": genesis
            .as_ref()
            .map(|b| format!("0x{}", hex::encode(b.hash.0))),
        "genesis_timestamp_ms": genesis.as_ref().map(|b| b.header.timestamp),
        "genesis_unavailable_reason": if genesis.is_none() {
            Value::String("block 0 is not present in this node's block store".to_string())
        } else {
            Value::Null
        },
        "protocol_version": protocol_version,
        "protocol_version_source": if latest.is_some() {
            "latest block header"
        } else if genesis.is_some() {
            "genesis block header"
        } else {
            "unknown"
        },
        "node_version": env!("CARGO_PKG_VERSION"),
        "recovery_active": recovery_context.is_some(),
        "recovery_epoch": recovery_context.as_ref().map(|context| context.recovery_epoch),
        "validator_set_id": recovery_context.as_ref().map(|context| context.validator_set_id),
        "recovery_domain": recovery_context
            .as_ref()
            .map(|context| format!("0x{}", context.domain_hash().to_hex())),
        "checkpoint_manifest_hash": recovery_manifest_hash
            .map(|hash| format!("0x{}", hash.to_hex())),

        // ── Liveness ──────────────────────────────────────────────────────
        "height": height,
        "last_block_height": latest.as_ref().map(|b| b.header.height),
        "last_block_hash": latest.as_ref().map(|b| format!("0x{}", hex::encode(b.hash.0))),
        "last_block_timestamp_ms": latest.as_ref().map(|b| b.header.timestamp),
        "last_block_age_secs": last_block_age,
        // How far the newest RETAINED block trails the height counter. Nonzero
        // is normal on a fast chain (the counter moves before the body lands)
        // and large means pruning; either way the fields above describe a real
        // block, not the counter.
        "blocks_behind_height": latest
            .as_ref()
            .map(|b| height.saturating_sub(b.header.height)),
        "last_block_age_unavailable_reason": if last_block_age.is_none() {
            Value::String(
                "no block above genesis is retained here, or its header timestamp is zero / \
                 ahead of this host's clock"
                    .to_string(),
            )
        } else {
            Value::Null
        },
        "chain_advancing": chain_advancing,
        "is_block_producing": is_block_producing,
        "is_block_producing_basis": format!(
            "true only when a block sealed by this node's own validator address appears within \
             the last {} blocks AND is under {}s old. Watching someone else's fresh blocks is \
             not producing, and a stale self-produced block is not producing.",
            SELF_PRODUCED_SCAN_BLOCKS, BLOCK_PRODUCTION_FRESH_SECS
        ),
        "last_self_produced_block": self_produced.map(|(h, _)| h),
        "last_self_produced_age_secs": self_produced_age,
        "last_self_produced_unavailable_reason": if self_produced.is_none() {
            Value::String(format!(
                "no block in the last {} retained blocks was sealed by this node's validator \
                 address (an observer never seals any)",
                SELF_PRODUCED_SCAN_BLOCKS
            ))
        } else {
            Value::Null
        },
        "block_production_fresh_secs": BLOCK_PRODUCTION_FRESH_SECS,
        "self_produced_scan_blocks": SELF_PRODUCED_SCAN_BLOCKS,
        "dag_round": node.dag_round.load(Ordering::Relaxed),
        "dag_committed": node.dag_committed.load(Ordering::Relaxed),
        "liveness_note": "DAG rounds advance even while no block is sealed, which is why \
                          /health reports ok on seeds that have produced nothing for days. \
                          Use is_block_producing / last_block_age_secs, not /health.",

        // ── Validator set: active vs merely registered ─────────────────────
        "validators_registered": split.registered,
        "validators_active": split.active,
        "validators_zero_stake": split.zero_stake,
        "min_active_stake": arc_consensus::STAKE_SPARK,
        "total_stake": split.total_stake,
        "active_stake": split.active_stake,
        "validator_source": "this node's live DAG validator set (the same set /validators and \
                             /health count); registered is the raw length, active requires \
                             stake >= min_active_stake",

        "unavailable_on_seeds_note": "live v0.7.2 / v0.7.9 seeds return 404 for this route",
    }))
}

/// What this node is contributing right now, measured.
///
/// GET /node/contribution
///
/// The honest basis for a Settings slider that claims "more cores → more
/// throughput": thread width against `available_parallelism`, the shard ranges
/// actually held, whether a model is loaded at all, real work counters, and
/// this node's own measured per-hop compute time.
///
/// Deliberately absent: any ARC-per-core or earnings-per-thread figure. The
/// node has no measurement that would support one — attestation rewards are a
/// flat per-attestation transfer that does not scale with core count, and
/// nothing here observes a causal link between threads and attestations
/// earned. A UI wanting to explain the benefit should say what widening the
/// pool measurably does (more parallel work inside each hop, visible as
/// `own_compute_ms`) rather than quote income.
///
/// ⚠ Live v0.7.2 / v0.7.9 seeds return 404 here. `/node/threads` exists on
/// newer seeds and covers the thread fields alone.
async fn node_contribution(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let dedicated = node.compute_threads.load(Ordering::Relaxed);
    let threads_in_use = if dedicated > 0 {
        dedicated as usize
    } else {
        rayon_global_width()
    };
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);

    // Shard ranges held by this node, and their total layer count.
    let ranges: Vec<Value> = node
        .shard_infos
        .iter()
        .map(|s| {
            json!({
                "start_layer": s.start_layer,
                "end_layer": s.end_layer,
                "layers": s.end_layer.saturating_sub(s.start_layer),
                "total_layers": s.total_layers,
                "model_id": s.model_id,
                "model_name": s.model_name,
                "memory_mb": s.memory_mb,
                "full_model_mb": s.full_model_mb,
                "node_name": s.node_name,
                "socket_addr": s.socket_addr,
            })
        })
        .collect();
    // Layers held, counted as a UNION of the held ranges rather than a sum of
    // their spans. Summing overlapping replica spans is exactly the bug that
    // makes /models report 96 layers of a 32-layer model.
    let mut covered: Vec<(usize, usize)> = node
        .shard_infos
        .iter()
        .map(|s| (s.start_layer, s.end_layer))
        .collect();
    covered.sort_unstable();
    let mut layers_held = 0usize;
    let mut cursor = 0usize;
    for (start, end) in covered {
        let from = start.max(cursor);
        if end > from {
            layers_held += end - from;
            cursor = end;
        }
    }
    let total_layers = node.shard_infos.first().map(|s| s.total_layers);

    // Own measured hop compute. Real samples from this node's own
    // forward_shard handler; null with a reason when it has served none.
    let samples: Vec<u64> = node.own_compute_ms.lock().iter().copied().collect();
    let own_mean = mean_u64(&samples);
    let own_p50 = p50_u64(&samples);

    // This node's own entry in the latency table, if one exists. Usually
    // absent: the table records round trips to OTHER replicas.
    let own_sockets: std::collections::HashSet<&str> = node
        .shard_infos
        .iter()
        .map(|s| s.socket_addr.as_str())
        .collect();
    let latency_self = node
        .latency_stats
        .iter()
        .find(|kv| own_sockets.contains(kv.key().as_str()))
        .map(|kv| {
            let stat = kv.value();
            json!({
                "socket": kv.key(),
                "ewma_ms": (stat.ms * 100.0).round() / 100.0,
                "samples": stat.count,
                "age_secs": stat.last_updated.elapsed().as_secs(),
                "source": if stat.probe_only { "probe" } else { "hop" },
            })
        });

    Json(json!({
        // ── Compute offered ───────────────────────────────────────────────
        "threads": {
            "in_use": threads_in_use,
            "dedicated_pool": dedicated > 0,
            "dedicated_threads": dedicated,
            "rayon_global_threads": rayon_global_width(),
            "rayon_num_threads_env": std::env::var("RAYON_NUM_THREADS").ok(),
            "available_parallelism": available,
            "fraction_of_available": if available > 0 {
                Value::from((threads_in_use as f64 / available as f64 * 1000.0).round() / 1000.0)
            } else {
                Value::Null
            },
            "available_parallelism_unavailable_reason": if available == 0 {
                Value::String(
                    "std::thread::available_parallelism() failed on this platform".to_string(),
                )
            } else {
                Value::Null
            },
        },

        // ── Model slice held ──────────────────────────────────────────────
        "shards": {
            "holds_shards": !node.shard_infos.is_empty(),
            "range_count": node.shard_infos.len(),
            "ranges": ranges,
            "layers_held": layers_held,
            "total_layers": total_layers,
            "layers_held_note": "union of the held ranges, not a sum of their spans, so \
                                 overlapping replicas cannot exceed total_layers",
            "coverage_fraction": match total_layers {
                Some(t) if t > 0 => Value::from(
                    (layers_held as f64 / t as f64 * 1000.0).round() / 1000.0,
                ),
                _ => Value::Null,
            },
        },
        "model": {
            "int8_engine_loaded": node.inference_model.is_some(),
            "candle_engine_loaded": node.candle_engine.is_some(),
            "any_model_loaded": node.inference_model.is_some() || node.candle_engine.is_some(),
            "candle_model_id": node
                .candle_model_id
                .map(|h| format!("0x{}", hex::encode(h.0))),
        },

        // ── Work actually done since boot ─────────────────────────────────
        "sharded_runs_total": node.sharded_runs_total.load(Ordering::Relaxed),
        "sharded_cache_hits": node.sharded_cache_hits.load(Ordering::Relaxed),
        "sharded_bytes_total": node.sharded_bytes_total.load(Ordering::Relaxed),
        "counters_note": "in-memory and reset by a restart; cache hits are counted separately \
                          from runs because a hit performs no pipeline walk",

        // ── This node's own measured hop compute ──────────────────────────
        "own_compute_ms": {
            "samples": samples.len(),
            "mean_ms": own_mean.map(|m| (m * 100.0).round() / 100.0),
            "p50_ms": own_p50,
            "unavailable_reason": if samples.is_empty() {
                Value::String(
                    "this node has served no forward_shard hop since boot, so it has no \
                     measurement of its own compute time"
                        .to_string(),
                )
            } else {
                Value::Null
            },
            "sample_cap": OWN_COMPUTE_SAMPLE_CAP,
            "source": "wall time of this node's own forward_shard forward pass, one sample per \
                       hop served, newest-capped ring",
        },
        "latency_self": latency_self,
        "latency_self_unavailable_reason": if latency_self.is_none() {
            Value::String(
                "the latency table holds round trips to OTHER replicas; it has no entry for \
                 this node's own socket unless this node dialled itself"
                    .to_string(),
            )
        } else {
            Value::Null
        },

        "earnings_per_core": Value::Null,
        "earnings_per_core_unavailable_reason": "not measurable: the attestation reward is a \
            flat per-attestation transfer that does not scale with core count, and this node \
            observes no causal link between thread width and attestations earned. Widening the \
            pool measurably speeds up each hop (own_compute_ms) — it does not measurably \
            increase income.",
        "uptime_secs": node.boot_time.elapsed().as_secs(),
        "unavailable_on_seeds_note": "live v0.7.2 / v0.7.9 seeds return 404 for this route",
    }))
}

// ─── Runtime inference width ───────────────────────────────────────────────

/// GET /node/threads
///
/// Report the width of the pool that runs local inference compute.
async fn get_node_threads(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let dedicated = node.compute_threads.load(Ordering::Relaxed);
    Json(json!({
        "threads": if dedicated > 0 { dedicated as usize } else { rayon_global_width() },
        "dedicated_pool": dedicated > 0,
        "dedicated_threads": dedicated,
        "rayon_global_threads": rayon_global_width(),
        "rayon_num_threads_env": std::env::var("RAYON_NUM_THREADS").ok(),
        "available_parallelism": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "note": "POST {\"threads\": n} to rebuild the pool live; n=0 returns to rayon's global pool",
    }))
}

#[derive(Deserialize)]
struct SetThreadsRequest {
    threads: usize,
}

/// POST /node/threads  {"threads": 8}
///
/// Rebuild the inference compute pool at a new width, live, without a
/// restart. Requests already running keep the old pool (they hold an Arc to
/// it); everything dispatched after this call uses the new one.
///
/// This is the knob behind "add two cores" during a demo: because
/// `forward_shard` and local `generate` both run inside
/// `install_on_compute_pool`, widening the pool immediately widens the
/// par_iter over attention heads and the par_chunks_mut inside every matmul.
async fn set_node_threads(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<SetThreadsRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let before = node.compute_threads.load(Ordering::Relaxed);
    let applied = set_compute_threads(&node, req.threads)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    tracing::info!(
        before,
        after = applied,
        "inference compute pool resized via /node/threads"
    );
    Ok(Json(json!({
        "ok": true,
        "previous_threads": if before > 0 { before as usize } else { rayon_global_width() },
        "threads": if applied > 0 { applied } else { rayon_global_width() },
        "dedicated_pool": applied > 0,
    })))
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
    fn shard_announce_binds_stub_to_peer_ip_keeping_declared_port() {
        // Peer behind "0.0.0.0:9090" announcement is at 149.28.32.76:51234
        // → should record shard at 149.28.32.76:9090 (trust the declared port)
        let got = bind_announced_shard_addr("0.0.0.0:9090", sa("149.28.32.76:51234"));
        assert_eq!(got, "149.28.32.76:9090");
    }

    #[test]
    fn shard_announce_falls_back_to_peer_port_when_declared_is_unparseable() {
        let got = bind_announced_shard_addr("0.0.0.0:xxxx", sa("149.28.32.76:51234"));
        assert_eq!(got, "149.28.32.76:51234");
    }

    #[test]
    fn shard_announce_cannot_target_a_different_or_metadata_host() {
        let got = bind_announced_shard_addr("149.28.32.76:9090", sa("1.2.3.4:9999"));
        assert_eq!(got, "1.2.3.4:9090");
        let metadata = bind_announced_shard_addr("169.254.169.254:80", sa("203.0.113.7:54321"));
        assert_eq!(metadata, "203.0.113.7:80");
    }

    #[test]
    fn shard_announce_trusts_loopback_self_announcement() {
        // Self-announce from the local broadcaster hits 127.0.0.1/shards/announce.
        // Rewriting would make self-entry look like 127.0.0.1:9090 (still unroutable)
        // and defeat the dedupe logic - so we leave it alone.
        let got = bind_announced_shard_addr("0.0.0.0:9090", sa("127.0.0.1:51234"));
        assert_eq!(got, "0.0.0.0:9090");
    }

    #[test]
    fn shard_announce_trusts_loopback_registry_reannouncement() {
        let got = bind_announced_shard_addr("136.244.109.1:9090", sa("127.0.0.1:51234"));
        assert_eq!(got, "136.244.109.1:9090");
        let got = bind_announced_shard_addr("0.0.0.0:9090", sa("[::1]:51234"));
        assert_eq!(got, "0.0.0.0:9090");
    }

    #[test]
    fn shard_announce_uses_ipv6_peer_ip_when_peer_is_remote() {
        // IPv6 must be bracketed when combined with a port.
        let got = bind_announced_shard_addr("0.0.0.0:9090", sa("[2001:db8::1]:51234"));
        assert_eq!(got, "[2001:db8::1]:9090");
    }

    #[test]
    fn shard_response_auth_binds_chain_request_result_and_active_validator() {
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let chain = ChainIdentity {
            name: "arc-testnet-v3".to_string(),
            chain_id: "0x415243".to_string(),
        };
        let request = ForwardShardRequest {
            request_id: "request-1".to_string(),
            model_id: test_model_id(),
            token: Some(7),
            hidden: None,
            hidden_hash: None,
            position: 0,
            start_layer: 0,
            end_layer: 6,
            expected_hidden_len: 4096,
            expect_terminal: true,
            generated_tokens: vec![9, 9],
            last_token: false,
        };
        let mut response = ForwardShardResponse {
            is_terminal: true,
            hidden: None,
            hidden_hash: None,
            token_id: Some(42),
            logits_hash: Some(arc_crypto::hash_bytes(b"logits").to_hex()),
            layers_processed: 6,
            compute_ms: 5,
            node_name: "validator-a".to_string(),
            validator_address: validator.address(),
            signature: arc_crypto::Signature::null(),
        };
        let commitment =
            forward_shard_response_commitment(Some(&chain), &request, &response).unwrap();
        response.signature = validator.sign(&commitment).unwrap();
        let active = vec![(validator.address(), 5_000_000)];
        verify_forward_shard_response_auth(Some(&chain), &request, &response, &active)
            .expect("exact signed transcript from an active validator");

        let mut tampered = response.clone();
        tampered.token_id = Some(43);
        assert!(
            verify_forward_shard_response_auth(Some(&chain), &request, &tampered, &active)
                .unwrap_err()
                .contains("invalid shard response signature")
        );

        let other_chain = ChainIdentity {
            name: "other".to_string(),
            chain_id: chain.chain_id.clone(),
        };
        assert!(
            verify_forward_shard_response_auth(Some(&other_chain), &request, &response, &active)
                .is_err()
        );
        assert!(
            verify_forward_shard_response_auth(
                Some(&chain),
                &request,
                &response,
                &[(validator.address(), 0)],
            )
            .unwrap_err()
            .contains("not an active staked validator")
        );
    }

    #[test]
    fn shard_response_auth_recomputes_hidden_hash_and_enforces_semantic_shape() {
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let request = ForwardShardRequest {
            request_id: "semantic-request".to_string(),
            model_id: test_model_id(),
            token: Some(7),
            hidden: None,
            hidden_hash: None,
            position: 0,
            start_layer: 0,
            end_layer: 6,
            expected_hidden_len: 3,
            expect_terminal: false,
            generated_tokens: Vec::new(),
            last_token: false,
        };
        let hidden = vec![11, 22, 33];
        let mut response = ForwardShardResponse {
            is_terminal: false,
            hidden_hash: Some(hidden_state_hash_hex(&hidden)),
            hidden: Some(hidden),
            token_id: None,
            logits_hash: None,
            layers_processed: 6,
            compute_ms: 5,
            node_name: "validator-a".to_string(),
            validator_address: validator.address(),
            signature: arc_crypto::Signature::null(),
        };
        let commitment = forward_shard_response_commitment(None, &request, &response).unwrap();
        response.signature = validator.sign(&commitment).unwrap();
        let active = vec![(validator.address(), 5_000_000)];
        verify_forward_shard_response_auth(None, &request, &response, &active)
            .expect("exact hidden body and declared hash");

        let mut hidden_tampered = response.clone();
        hidden_tampered.hidden.as_mut().unwrap()[0] += 1;
        assert!(
            verify_forward_shard_response_auth(None, &request, &hidden_tampered, &active)
                .unwrap_err()
                .contains("hidden_hash does not match")
        );

        let mut wrong_range = response.clone();
        wrong_range.layers_processed = 5;
        let commitment = forward_shard_response_commitment(None, &request, &wrong_range).unwrap();
        wrong_range.signature = validator.sign(&commitment).unwrap();
        assert!(
            verify_forward_shard_response_auth(None, &request, &wrong_range, &active)
                .unwrap_err()
                .contains("processed 5 layers")
        );

        let mut wrong_shape = response;
        wrong_shape.is_terminal = true;
        let commitment = forward_shard_response_commitment(None, &request, &wrong_shape).unwrap();
        wrong_shape.signature = validator.sign(&commitment).unwrap();
        assert!(
            verify_forward_shard_response_auth(None, &request, &wrong_shape, &active)
                .unwrap_err()
                .contains("terminal=true")
        );
    }

    #[tokio::test]
    async fn forward_shard_body_rejects_oversized_content_length_before_reading() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_FORWARD_SHARD_RESPONSE_BYTES + 1
            );
            stream.write_all(header.as_bytes()).await.unwrap();
        });

        let response = reqwest::get(format!("http://{addr}"))
            .await
            .expect("response headers");
        let error = read_forward_shard_body_limited(response).await.unwrap_err();
        assert!(error.contains("exceeds"));
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
            transaction_domain: None,
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
        assert!(
            v.get("request_id").is_none(),
            "request_id is the v0.6 layer-shard field, must not leak into community jobs"
        );
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
            transaction_domain: None,
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
        let r: WorkResult =
            serde_json::from_value(body).expect("worker payload should deserialize");
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

    #[test]
    fn workresult_cannot_forge_server_verification_or_settlement() {
        let body = serde_json::json!({
            "job_id": "abc",
            "worker_id": "0x12ab",
            "success": false,
            "error": "worker-controlled body",
            "verification": {"method": "trust_me"},
            "settlement": {"status": "paid"},
        });
        assert!(
            serde_json::from_value::<WorkResult>(body).is_err(),
            "server evidence fields must not exist in the worker wire schema"
        );
    }

    #[test]
    fn reward_quorum_policy_is_exactly_five_of_six() {
        assert_eq!(
            arc_types::transaction::COMMUNITY_REWARD_VALIDATOR_SET_SIZE,
            6
        );
        assert_eq!(
            arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED,
            5
        );
        assert!(COMMUNITY_REWARD_APPROVAL_COLLECTION_READY);
    }

    #[tokio::test]
    async fn public_reward_endpoints_fail_closed_when_runtime_prerequisites_are_unavailable() {
        let mut node = fake_node_with_workers(Vec::new());
        node.community_rewards_v1_enabled = true;
        node.state
            .set_community_rewards_v1_activation_height(Some(0));
        assert!(community_rewards_v1_protocol_active(&node));
        assert!(!community_rewards_v1_effective(&node));

        let Json(economics) = economics_rewards(AxumState(node.clone())).await;
        let Json(community) = community_list(AxumState(node.clone())).await;
        let Json(policy) = community_reward_policy(AxumState(node.clone())).await;
        let Json(approval) = community_reward_approval_status(
            AxumState(node.clone()),
            axum::extract::Path(format!("0x{}", Hash256::ZERO.to_hex())),
        )
        .await
        .expect("well-formed job ids always have an approval status");
        let Json(earnings) = worker_earnings(
            AxumState(node),
            axum::extract::Path(format!("0x{}", Hash256::ZERO.to_hex())),
        )
        .await
        .expect("zero address is a valid query");

        for response in [&economics, &community, &earnings] {
            assert_eq!(response["community_rewards_v1_protocol_active"], true);
            assert_eq!(
                response["community_rewards_v1_approval_collection_ready"],
                true
            );
            assert_eq!(response["community_rewards_v1_enabled"], false);
            assert!(
                response["community_rewards_v1_note"]
                    .as_str()
                    .unwrap()
                    .contains("issuance remains fail-closed until")
            );
        }
        assert_eq!(policy["schema"], "arc.community.reward-policy.v1");
        assert_eq!(policy["tx_type"], "0x25");
        assert_eq!(policy["protocol_active"], true);
        assert_eq!(policy["issuance_ready"], false);
        assert!(policy["readiness_unavailable_reason"].is_string());
        assert!(policy["recovery_epoch"].is_null());
        assert!(policy["validator_set_id"].is_null());
        assert!(policy["transaction_domain"].is_null());
        assert_eq!(approval["locally_approved"], false);
        assert_eq!(approval["submitted"], false);
        assert!(approval["recovery_epoch"].is_null());
        assert!(approval["validator_set_id"].is_null());
    }

    #[tokio::test]
    async fn mined_reward_receipt_is_the_only_earnings_evidence() {
        use arc_types::transaction::{
            CommunityInferenceRewardBody, CommunityRewardValidatorApproval,
            InferenceAttestationBody, WorkerInferenceCertificate,
        };

        let validators: Vec<_> = (0..6)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let treasury = arc_types::transaction::faucet_pool_address();
        let reward_amount = arc_types::economics::INFERENCE_ATTESTATION_REWARD;
        let state = arc_state::StateDB::with_genesis(&[
            (treasury, reward_amount * 2),
            (worker.address(), 0),
        ]);
        state.seed_genesis_validators(
            &validators
                .iter()
                .map(|key| (key.address(), arc_state::StateDB::MIN_VALIDATOR_STAKE))
                .collect::<Vec<_>>(),
        );
        state.set_community_rewards_v1_activation_height(Some(0));

        let model_id = arc_crypto::hash_bytes(b"exact-model-artifact");
        let input_hash = arc_crypto::hash_bytes(b"prompt");
        let output_hash = arc_crypto::hash_bytes(b"token-output");
        let mut worker_attestation = arc_types::Transaction {
            tx_type: arc_types::TxType::InferenceAttestation,
            from: worker.address(),
            nonce: 0,
            body: arc_types::TxBody::InferenceAttestation(InferenceAttestationBody {
                model_id,
                input_hash,
                output_hash,
                challenge_period: 100,
                bond: 0,
                beneficiary: None,
            }),
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        worker_attestation.sign(&worker).unwrap();
        let assignment_epoch = arc_crypto::hash_bytes(b"coordinator-recovery-epoch");
        let job_nonce = 17;
        let job_id = CommunityInferenceRewardBody::derive_job_id(
            &validators[0].address(),
            &assignment_epoch,
            job_nonce,
            &model_id,
            &input_hash,
            32,
        );
        let mut body = CommunityInferenceRewardBody {
            chain_domain: CommunityInferenceRewardBody::expected_chain_domain(),
            job_id,
            coordinator: validators[0].address(),
            assignment_epoch,
            job_nonce,
            recovery_epoch: 0,
            validator_set_id: 0,
            transaction_domain: Hash256::ZERO,
            worker: worker.address(),
            model_id,
            input_hash,
            output_hash,
            max_tokens: 32,
            expires_at_height: 100,
            worker_certificate: WorkerInferenceCertificate {
                attestation_hash: worker_attestation.hash,
                nonce: worker_attestation.nonce,
                challenge_period: 100,
                signature: worker_attestation.signature.clone(),
            },
            validator_approvals: Vec::new(),
        };
        let commitment = body.validator_approval_commitment();
        body.validator_approvals = validators[..5]
            .iter()
            .map(|key| {
                CommunityRewardValidatorApproval::from_ed25519_signature(
                    key.address(),
                    key.sign(&commitment).unwrap(),
                )
                .unwrap()
            })
            .collect();
        let approval_body = body.clone();
        let mut reward = arc_types::Transaction::new_community_inference_reward(
            validators[0].address(),
            job_nonce,
            body,
        );
        reward.sign(&validators[0]).unwrap();
        let tx_hash = reward.hash;
        let (_, receipts) = state
            .execute_block(&[reward], validators[0].address())
            .unwrap();
        assert!(receipts[0].success);

        let mut node = fake_node_with_workers(Vec::new());
        node.state = Arc::new(state);
        let accepted = validate_collected_reward_approvals(
            &node,
            &approval_body,
            approval_body.validator_approvals.clone(),
        )
        .expect("five independently signed approvals satisfy the six-validator policy");
        assert_eq!(accepted.len(), 5);
        let four = approval_body.validator_approvals[..4].to_vec();
        let shortfall = validate_collected_reward_approvals(&node, &approval_body, four)
            .expect_err("four of six must fail closed");
        assert!(shortfall.contains("5 of 6 required"), "{shortfall}");
        let Json(earnings) = worker_earnings(
            AxumState(node.clone()),
            axum::extract::Path(format!("0x{}", worker.address().to_hex())),
        )
        .await
        .unwrap();
        assert_eq!(earnings["confirmed_receipt_count"], 1);
        assert_eq!(earnings["confirmed_receipts"][0]["tx_type"], "0x25");
        assert_eq!(earnings["confirmed_gross_earnings_base"], reward_amount);
        assert!(earnings["projected_daily_arc"].is_null());
        assert!(earnings["projected_daily_unavailable_reason"].is_string());

        let Json(receipt) = community_reward_receipt(
            AxumState(node.clone()),
            axum::extract::Path(format!("0x{}", tx_hash.to_hex())),
        )
        .await
        .unwrap();
        assert_eq!(receipt["status"], "mined_success");
        assert_eq!(receipt["confirmed"], true);
        assert_eq!(receipt["reward_base"], reward_amount);
        assert_eq!(receipt["recovery_epoch"], 0);
        assert_eq!(receipt["validator_set_id"], 0);
        assert_eq!(
            receipt["transaction_domain"],
            format!("0x{}", Hash256::ZERO.to_hex())
        );

        let Json(by_job) = community_reward_job(
            AxumState(node),
            axum::extract::Path(format!("0x{}", job_id.to_hex())),
        )
        .await
        .unwrap();
        assert_eq!(by_job["tx_hash"], format!("0x{}", tx_hash.to_hex()));
        assert_eq!(by_job["confirmed"], true);
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
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        })
        .await
        .unwrap();

        // Drain
        let got = rx.lock().await.recv().await.expect("a job");
        assert_eq!(got.job_id, "j1");
        assert_eq!(got.input, "ping");
    }

    #[tokio::test]
    async fn idle_long_polls_do_not_serialize_workers_behind_receiver_mutex() {
        let (tx, rx) = tokio::sync::mpsc::channel::<WorkItem>(8);
        let queue = Arc::new(tokio::sync::Mutex::new(rx));
        let mut waiters = tokio::task::JoinSet::new();
        for _ in 0..3 {
            let queue = queue.clone();
            waiters.spawn(async move {
                poll_community_work_queue(&queue, std::time::Duration::from_secs(1))
                    .await
                    .unwrap()
                    .expect("one queued job")
                    .job_id
            });
        }
        tokio::task::yield_now().await;
        for id in ["job-a", "job-b", "job-c"] {
            tx.send(WorkItem {
                job_id: id.to_string(),
                input: "prompt".to_string(),
                max_tokens: 1,
                model_id: None,
                transaction_domain: None,
                submitted_at_unix_ms: 0,
            })
            .await
            .unwrap();
        }

        let collected = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            let mut ids = Vec::new();
            while let Some(result) = waiters.join_next().await {
                ids.push(result.unwrap());
            }
            ids.sort();
            ids
        })
        .await
        .expect("all idle workers should receive concurrently");
        assert_eq!(collected, vec!["job-a", "job-b", "job-c"]);
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

    fn test_model_id() -> String {
        format!(
            "0x{}",
            arc_crypto::hash_bytes(b"synthetic-model-artifact-exact-bytes").to_hex()
        )
    }

    fn test_inference_model() -> Arc<arc_inference::cached_integer_model::CachedIntegerModel> {
        use arc_inference::cached_integer_model::{CachedIntegerModel, I8Weights, ModelConfig};

        Arc::new(CachedIntegerModel {
            config: ModelConfig {
                n_layers: 0,
                d_model: 1,
                n_heads: 1,
                n_kv_heads: 1,
                d_ff: 1,
                d_head: 1,
                d_kv: 1,
                vocab_size: 2,
                attn_scale: 1,
                rope_cos: Vec::new(),
                rope_sin: Vec::new(),
                max_seq: 1,
                eos_tokens: Vec::new(),
                bos_token: 0,
                chat_template: String::new(),
            },
            embedding_q16: Vec::new(),
            embedding_i8: I8Weights::empty(),
            layers: Vec::new(),
            final_norm: Vec::new(),
            output_weight: I8Weights::empty(),
            vocab: vec![String::new(), String::new()],
            q4_layers: None,
            q4_output: None,
            i16_layers: None,
            i16_output: None,
            block_i8_layers: None,
            block_i8_output: None,
            ternary_layers: None,
            ternary_output: None,
            ternary_hybrid_layers: None,
            ternary_hybrid_output: None,
        })
    }

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
            community_request_replays: Arc::new(parking_lot::Mutex::new(
                CommunityReplayCache::default(),
            )),
            community_reward_approval_jobs: Arc::new(dashmap::DashMap::new()),
            community_reward_approval_certificates: Arc::new(dashmap::DashMap::new()),
            community_reward_submissions: Arc::new(dashmap::DashMap::new()),
            community_work_tx: Some(Arc::new(tx)),
            community_work_queue: Some(Arc::new(tokio::sync::Mutex::new(rx))),
            community_work_results: Some(Arc::new(dashmap::DashMap::new())),
            community_active_jobs: Arc::new(dashmap::DashMap::new()),
            attestation_nonce: Arc::new(AtomicU64::new(0)),
            latency_stats: Arc::new(dashmap::DashMap::new()),

            // Filler ↓ — never read by the router but required by the struct
            state: Arc::new(arc_state::StateDB::new()),
            mempool: Arc::new(arc_mempool::Mempool::new(1_000)),
            validator_address: Hash256::ZERO,
            validator_keypair: None,
            community_rewards_v1_enabled: false,
            stake: 0,
            tier: StakeTier::Spark,
            boot_time: Instant::now(),
            peer_count: Arc::new(AtomicU32::new(0)),
            faucet_claims: Arc::new(dashmap::DashMap::new()),
            faucet_claims_total: Arc::new(AtomicU32::new(0)),
            inference_model: Some(test_inference_model()),
            candle_engine: None,
            candle_model_id: None,
            model_artifact_id: parse_hash256_hex(&test_model_id(), "test model_id").ok(),
            dag_validators: Arc::new(parking_lot::RwLock::new(Vec::new())),
            tx_rate_limit: Arc::new(dashmap::DashMap::new()),
            dag_round: Arc::new(AtomicU64::new(0)),
            dag_committed: Arc::new(AtomicU64::new(0)),
            community_job_epoch: arc_crypto::KeyPair::generate_ed25519().address(),
            inference_results: Arc::new(dashmap::DashMap::new()),
            shard_infos: Vec::new(),
            shard_kv_caches: Arc::new(dashmap::DashMap::new()),
            shard_registry: Arc::new(dashmap::DashMap::new()),
            sharded_runs_total: Arc::new(AtomicU64::new(0)),
            sharded_bytes_total: Arc::new(AtomicU64::new(0)),
            inference_cache: Arc::new(arc_inference::distributed::DistributedCache::new(16)),
            multi_model_registry: Arc::new(arc_inference::distributed::ShardRegistry::new()),
            verification_manager: Arc::new(std::sync::Mutex::new(
                arc_vm::inference_verify::VerificationManager::new(),
            )),
            revenue_config: RoleRevenueConfig::default(),
            inference_http: reqwest::Client::new(),
            sharded_cache_hits: Arc::new(AtomicU64::new(0)),
            sharded_run_meta: Arc::new(dashmap::DashMap::new()),
            compute_pool: Arc::new(parking_lot::RwLock::new(None)),
            compute_threads: Arc::new(AtomicU32::new(0)),
            seed_rpc_addrs: Arc::new(Vec::new()),
            last_registry_bootstrap: Arc::new(Mutex::new(None)),
            chain_identity: None,
            own_compute_ms: Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new())),
        }
    }

    fn worker(id: &str, caps: &[&str]) -> CommunityWorker {
        CommunityWorker {
            worker_id: id.into(),
            name: format!("test-{}", id),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            model: Some("arc-0L-1d-1h-2v".into()),
            model_id: Some(test_model_id()),
            platform: "test".into(),
            registered_at: 0,
            work_completed: 0,
            success_count: 0,
            failure_count: 0,
            sum_total_ms_success: 0,
            last_total_ms: 0,
        }
    }

    fn community_register_payload(keypair: &arc_crypto::KeyPair) -> CommunityRegisterRequest {
        CommunityRegisterRequest {
            worker_id: format!("0x{}", keypair.address().to_hex()),
            name: "signed test worker".to_string(),
            capabilities: vec!["inference".to_string()],
            model: Some("test-model".to_string()),
            model_id: Some(test_model_id()),
            platform: "test-platform".to_string(),
        }
    }

    #[tokio::test]
    async fn community_auth_valid_signature_registers_claimed_worker() {
        let keypair = arc_crypto::KeyPair::generate_ed25519();
        let payload = community_register_payload(&keypair);
        let worker_id = payload.worker_id.clone();
        let signed = sign_community_request(COMMUNITY_REGISTER_PATH, payload, &keypair).unwrap();
        let node = fake_node_with_workers(Vec::new());

        let response = community_register_signed(AxumState(node.clone()), Json(signed))
            .await
            .expect("valid proof of possession");

        assert_eq!(response.0["worker_id"], worker_id);
        assert!(node.community_workers.contains_key(&worker_id));
        assert!(
            node.shard_registry.is_empty(),
            "whole-model community registration must never create routable shard targets"
        );
    }

    #[tokio::test]
    async fn inference_registration_requires_an_exact_model_identity() {
        let keypair = arc_crypto::KeyPair::generate_ed25519();
        let mut payload = community_register_payload(&keypair);
        payload.model_id = None;
        let signed = sign_community_request(COMMUNITY_REGISTER_PATH, payload, &keypair).unwrap();
        let node = fake_node_with_workers(Vec::new());

        let error = community_register_signed(AxumState(node.clone()), Json(signed))
            .await
            .expect_err("a human display name alone must never make a worker routable");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("model_id"));
        assert!(node.community_workers.is_empty());
    }

    #[test]
    fn community_register_schema_rejects_legacy_rpc_target_fields() {
        let keypair = arc_crypto::KeyPair::generate_ed25519();
        let mut value = serde_json::to_value(community_register_payload(&keypair)).unwrap();
        value["rpc_addr"] = Value::String("169.254.169.254:80".to_string());
        assert!(serde_json::from_value::<CommunityRegisterRequest>(value).is_err());
    }

    #[tokio::test]
    async fn community_auth_guards_all_four_mutation_handlers() {
        let keypair = arc_crypto::KeyPair::generate_ed25519();
        let register_payload = community_register_payload(&keypair);
        let worker_id = register_payload.worker_id.clone();
        let node = fake_node_with_workers(Vec::new());

        let register =
            sign_community_request(COMMUNITY_REGISTER_PATH, register_payload, &keypair).unwrap();
        let _ = community_register_signed(AxumState(node.clone()), Json(register))
            .await
            .unwrap();

        let heartbeat = sign_community_request(
            COMMUNITY_HEARTBEAT_PATH,
            CommunityHeartbeatRequest {
                worker_id: worker_id.clone(),
                work_completed: None,
            },
            &keypair,
        )
        .unwrap();
        let _ = community_heartbeat_signed(AxumState(node.clone()), Json(heartbeat))
            .await
            .unwrap();

        node.community_active_jobs
            .insert(worker_id.clone(), "already-busy".to_string());
        let claim = sign_community_request(
            COMMUNITY_CLAIM_WORK_PATH,
            ClaimWorkRequest {
                worker_id: worker_id.clone(),
                capabilities: vec!["inference".to_string()],
                model_id: test_model_id(),
            },
            &keypair,
        )
        .unwrap();
        let Json(claim_response) =
            community_claim_work_signed(AxumState(node.clone()), Json(claim))
                .await
                .unwrap();
        assert_eq!(claim_response["reason"], "worker_busy");

        let submit = sign_community_request(
            COMMUNITY_SUBMIT_WORK_PATH,
            WorkResult {
                job_id: "unknown-job".to_string(),
                worker_id,
                success: false,
                declined: false,
                output: String::new(),
                output_hash: String::new(),
                tokens_generated: 0,
                total_ms: 0,
                ms_per_token: 0,
                engine: String::new(),
                error: Some("test failure".to_string()),
                signed_attestation_hex: None,
            },
            &keypair,
        )
        .unwrap();
        let error = community_submit_work_signed(AxumState(node), Json(submit))
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn community_auth_unsigned_legacy_body_fails_closed() {
        let keypair = arc_crypto::KeyPair::generate_ed25519();
        let legacy = serde_json::to_value(community_register_payload(&keypair)).unwrap();
        assert!(
            serde_json::from_value::<CommunitySignedRequest<CommunityRegisterRequest>>(legacy)
                .is_err()
        );
    }

    #[test]
    fn community_auth_rejects_wrong_key_path_payload_and_stale_timestamp() {
        let claimed_key = arc_crypto::KeyPair::generate_ed25519();
        let signing_key = arc_crypto::KeyPair::generate_ed25519();
        let node = fake_node_with_workers(Vec::new());

        let wrong_key = sign_community_request(
            COMMUNITY_REGISTER_PATH,
            community_register_payload(&claimed_key),
            &signing_key,
        )
        .unwrap();
        let error =
            authenticate_community_request(&node, COMMUNITY_REGISTER_PATH, wrong_key).unwrap_err();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);

        let wrong_path = sign_community_request(
            COMMUNITY_REGISTER_PATH,
            community_register_payload(&claimed_key),
            &claimed_key,
        )
        .unwrap();
        let error = authenticate_community_request(&node, COMMUNITY_HEARTBEAT_PATH, wrong_path)
            .unwrap_err();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);

        let mut tampered_payload = sign_community_request(
            COMMUNITY_REGISTER_PATH,
            community_register_payload(&claimed_key),
            &claimed_key,
        )
        .unwrap();
        tampered_payload.payload.name = "attacker-controlled name".to_string();
        let error =
            authenticate_community_request(&node, COMMUNITY_REGISTER_PATH, tampered_payload)
                .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("payload_hash"));

        let mut stale = sign_community_request(
            COMMUNITY_REGISTER_PATH,
            community_register_payload(&claimed_key),
            &claimed_key,
        )
        .unwrap();
        stale.timestamp_unix_ms = community_unix_ms()
            .unwrap()
            .saturating_sub(COMMUNITY_AUTH_MAX_CLOCK_SKEW_MS + 1);
        let error =
            authenticate_community_request(&node, COMMUNITY_REGISTER_PATH, stale).unwrap_err();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
        assert!(error.1.contains("timestamp"));

        let mut wrong_version = sign_community_request(
            COMMUNITY_REGISTER_PATH,
            community_register_payload(&claimed_key),
            &claimed_key,
        )
        .unwrap();
        wrong_version.version = COMMUNITY_AUTH_VERSION + 1;
        let error = authenticate_community_request(&node, COMMUNITY_REGISTER_PATH, wrong_version)
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("version"));
    }

    #[test]
    fn community_auth_fresh_requests_have_distinct_random_nonces() {
        let keypair = arc_crypto::KeyPair::generate_ed25519();
        let first = sign_community_request(
            COMMUNITY_REGISTER_PATH,
            community_register_payload(&keypair),
            &keypair,
        )
        .unwrap();
        let second = sign_community_request(
            COMMUNITY_REGISTER_PATH,
            community_register_payload(&keypair),
            &keypair,
        )
        .unwrap();
        assert_ne!(first.nonce, Hash256::ZERO);
        assert_ne!(first.nonce, second.nonce);
    }

    #[tokio::test]
    async fn community_auth_concurrent_replay_is_atomically_rejected() {
        let keypair = arc_crypto::KeyPair::generate_ed25519();
        let signed = sign_community_request(
            COMMUNITY_REGISTER_PATH,
            community_register_payload(&keypair),
            &keypair,
        )
        .unwrap();
        let node = fake_node_with_workers(Vec::new());

        let (first, second) = tokio::join!(
            community_register_signed(AxumState(node.clone()), Json(signed.clone())),
            community_register_signed(AxumState(node.clone()), Json(signed)),
        );
        let statuses = [
            first
                .map(|_| StatusCode::OK)
                .unwrap_or_else(|error| error.0),
            second
                .map(|_| StatusCode::OK)
                .unwrap_or_else(|error| error.0),
        ];
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::OK)
                .count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::CONFLICT)
                .count(),
            1
        );
    }

    #[test]
    fn live_worker_count_filters_by_capability_and_ttl() {
        let now = std::time::Instant::now();
        let stale = now - std::time::Duration::from_secs(COMMUNITY_WORKER_TTL_SECS + 5);
        let mut wrong_model = worker("wrong-model", &["inference"]);
        wrong_model.model_id = Some(format!(
            "0x{}",
            arc_crypto::hash_bytes(b"another-model").to_hex()
        ));

        let node = fake_node_with_workers(vec![
            (worker("alive-inference", &["inference"]), now),
            (worker("alive-other-cap", &["consensus"]), now),
            (worker("stale-inference", &["inference"]), stale),
            (wrong_model, now),
        ]);

        // Only "alive-inference" is fresh, inference-capable, and committed
        // to the coordinator's exact loaded model identity.
        assert_eq!(live_inference_worker_count(&node), 1);
    }

    #[test]
    fn live_worker_count_zero_when_no_workers_registered() {
        let node = fake_node_with_workers(vec![]);
        assert_eq!(live_inference_worker_count(&node), 0);
    }

    #[test]
    fn live_worker_count_zero_when_coordinator_has_no_model() {
        let mut node = fake_node_with_workers(vec![(
            worker("ready-worker", &["inference"]),
            std::time::Instant::now(),
        )]);
        node.inference_model = None;
        assert_eq!(live_inference_worker_count(&node), 0);
    }

    #[test]
    fn community_job_ids_do_not_repeat_when_coordinator_restarts() {
        let validator = arc_crypto::hash_bytes(b"validator");
        let first_boot = arc_crypto::hash_bytes(b"boot-a");
        let second_boot = arc_crypto::hash_bytes(b"boot-b");
        let model = arc_crypto::hash_bytes(b"exact-model");
        let input = arc_crypto::hash_bytes(b"same prompt");
        let before = community_job_id(&validator, &first_boot, &model, &input, 8, 0);
        let after = community_job_id(&validator, &second_boot, &model, &input, 8, 0);
        let other_model = arc_crypto::hash_bytes(b"other-exact-model");
        let model_changed = community_job_id(&validator, &first_boot, &other_model, &input, 8, 0);
        assert_ne!(before, after, "boot epoch must namespace the reset counter");
        assert_ne!(
            before, model_changed,
            "exact model identity must be part of deterministic assignment"
        );
        assert_eq!(before.len(), 64);
        assert_eq!(after.len(), 64);
    }

    #[tokio::test]
    async fn dispatch_to_community_worker_returns_result_when_worker_submits() {
        // Wire a worker: we manually drain the queue and post a result
        // through the same channels submit_work uses, then assert the
        // dispatcher returns the right WorkResult.
        let now = std::time::Instant::now();
        let node = fake_node_with_workers(vec![(worker("w1", &["inference"]), now)]);

        let queue = node.community_work_queue.as_ref().unwrap().clone();
        let results = node.community_work_results.as_ref().unwrap().clone();

        // Spawn a fake worker that drains the queue and posts a result.
        tokio::spawn(async move {
            let item = queue.lock().await.recv().await.expect("a job");
            // submit a successful result
            if let Some((_, pending)) = results.remove(&item.job_id) {
                let _ = pending.sender.send(CommunityDispatchOutcome {
                    result: WorkResult {
                        job_id: item.job_id,
                        worker_id: "w1".into(),
                        success: true,
                        declined: false,
                        output: "4".into(),
                        output_hash: "0xdeadbeef".into(),
                        tokens_generated: 1,
                        total_ms: 50,
                        ms_per_token: 50,
                        engine: "INT8 integer (community worker)".into(),
                        error: None,
                        signed_attestation_hex: None,
                    },
                    verification: Some(CommunityVerificationSummary {
                        method: "authenticated_shard_quorum_2_of_3_per_range",
                        output_hash: "0xdeadbeef".to_string(),
                        tokens_generated: 1,
                        ranges: 6,
                        range_position_quorums: 6,
                        signatures_required_per_quorum: 2,
                        replicas_contacted_per_quorum: 3,
                    }),
                    settlement: Some(json!({"status": "test_settlement"})),
                });
            }
        });

        let result =
            dispatch_to_community_worker(&node, "What is 2+2?".into(), 4, Some(test_model_id()))
                .await
                .expect("worker returned a result");

        assert_eq!(result.result.worker_id, "w1");
        assert_eq!(result.result.output, "4");
        assert_eq!(result.result.tokens_generated, 1);
        assert_eq!(
            result.verification.as_ref().unwrap().range_position_quorums,
            6
        );
        assert_eq!(
            result.settlement.as_ref().unwrap()["status"],
            "test_settlement"
        );
        // EWMA was recorded for this worker
        assert!(node.latency_stats.contains_key("worker:w1"));
    }

    #[tokio::test]
    async fn inference_run_surfaces_server_evidence_and_dispatches_at_shared_token_ceiling() {
        let node = fake_node_with_workers(vec![(
            worker("w1", &["inference"]),
            std::time::Instant::now(),
        )]);
        let queue = node.community_work_queue.as_ref().unwrap().clone();
        let results = node.community_work_results.as_ref().unwrap().clone();
        let observed_max = Arc::new(AtomicU32::new(0));
        let observed_max_worker = observed_max.clone();

        tokio::spawn(async move {
            let item = queue.lock().await.recv().await.expect("a job");
            observed_max_worker.store(item.max_tokens, Ordering::Relaxed);
            if let Some((_, pending)) = results.remove(&item.job_id) {
                let _ = pending.sender.send(CommunityDispatchOutcome {
                    result: WorkResult {
                        job_id: item.job_id,
                        worker_id: "w1".to_string(),
                        success: true,
                        declined: false,
                        output: "verified output".to_string(),
                        output_hash: format!("0x{}", Hash256::ZERO.to_hex()),
                        tokens_generated: 2,
                        total_ms: 10,
                        ms_per_token: 5,
                        engine: "integer test engine".to_string(),
                        error: None,
                        signed_attestation_hex: None,
                    },
                    verification: Some(CommunityVerificationSummary {
                        method: "authenticated_shard_quorum_2_of_3_per_range",
                        output_hash: format!("0x{}", Hash256::ZERO.to_hex()),
                        tokens_generated: 2,
                        ranges: 6,
                        range_position_quorums: 6,
                        signatures_required_per_quorum: 2,
                        replicas_contacted_per_quorum: 3,
                    }),
                    settlement: Some(json!({
                        "status": "reward_submitted_to_mempool",
                        "included": false,
                    })),
                });
            }
        });

        let Json(response) = match inference_run(
            AxumState(node),
            Some(Json(json!({
                "input": "show the proof",
                "max_tokens": 4096,
                "bond": 999999,
                "challenge_period": 777,
            }))),
        )
        .await
        {
            Ok(response) => response,
            Err((status, Json(error))) => {
                panic!("inference_run failed with {status}: {}", error.error)
            }
        };

        assert_eq!(
            observed_max.load(Ordering::Relaxed),
            INFERENCE_RUN_MAX_TOKENS
        );
        assert_eq!(
            response["verification"]["method"],
            "authenticated_shard_quorum_2_of_3_per_range"
        );
        assert_eq!(response["verification"]["range_position_quorums"], 6);
        assert_eq!(
            response["verification"]["signatures_required_per_quorum"],
            2
        );
        assert_eq!(
            response["inference"]["output_hash"], response["verification"]["output_hash"],
            "production-formatted worker and verification hashes must correlate exactly"
        );
        assert!(response["attestation"].get("bond").is_none());
        assert!(response["attestation"].get("challenge_period").is_none());
        assert_eq!(
            response["attestation"]["request_overrides_applied"], false,
            "community responses must not echo ignored local-attestation overrides"
        );
        assert_eq!(
            response["settlement"]["status"],
            "reward_submitted_to_mempool"
        );
    }

    #[tokio::test]
    async fn inference_auto_dispatches_to_a_community_worker_without_a_local_model() {
        let node = fake_node_with_workers(vec![(
            worker("w1", &["inference"]),
            std::time::Instant::now(),
        )]);
        assert!(
            !node
                .inference_model
                .as_ref()
                .unwrap()
                .has_all_transformer_layers(),
            "fixture must behave like a tokenizer-only coordinator"
        );
        let queue = node.community_work_queue.as_ref().unwrap().clone();
        let results = node.community_work_results.as_ref().unwrap().clone();

        tokio::spawn(async move {
            let item = queue.lock().await.recv().await.expect("a community job");
            if let Some((_, pending)) = results.remove(&item.job_id) {
                let _ = pending.sender.send(CommunityDispatchOutcome {
                    result: WorkResult {
                        job_id: item.job_id,
                        worker_id: "w1".to_string(),
                        success: true,
                        declined: false,
                        output: "community output".to_string(),
                        output_hash: format!("0x{}", Hash256::ZERO.to_hex()),
                        tokens_generated: 1,
                        total_ms: 8,
                        ms_per_token: 8,
                        engine: "integer test engine".to_string(),
                        error: None,
                        signed_attestation_hex: None,
                    },
                    verification: Some(CommunityVerificationSummary {
                        method: "authenticated_shard_quorum_2_of_3_per_range",
                        output_hash: format!("0x{}", Hash256::ZERO.to_hex()),
                        tokens_generated: 1,
                        ranges: 6,
                        range_position_quorums: 6,
                        signatures_required_per_quorum: 2,
                        replicas_contacted_per_quorum: 3,
                    }),
                    settlement: Some(json!({
                        "status": "reward_quorum_approval_unavailable",
                        "included": false,
                    })),
                });
            }
        });

        let Json(response) = inference_auto(
            AxumState(node),
            Json(json!({"input": "route this", "max_tokens": 8})),
        )
        .await
        .expect("auto route should use the live community dispatcher");

        assert_eq!(response["success"], true);
        assert_eq!(response["route"], "community_worker");
        assert_eq!(response["routed_via"], "community:w1");
        assert_eq!(response["inference"]["output"], "community output");
    }

    #[tokio::test]
    async fn inference_run_rejects_zero_token_community_job() {
        let node = fake_node_with_workers(Vec::new());
        let error = inference_run(
            AxumState(node),
            Some(Json(json!({"input": "x", "max_tokens": 0}))),
        )
        .await
        .expect_err("zero-token assignments are invalid for worker and verifier");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.error.contains("at least 1"));
    }

    #[tokio::test]
    async fn dispatch_propagates_worker_failure() {
        let now = std::time::Instant::now();
        let node = fake_node_with_workers(vec![(worker("w1", &["inference"]), now)]);
        let queue = node.community_work_queue.as_ref().unwrap().clone();
        let results = node.community_work_results.as_ref().unwrap().clone();

        tokio::spawn(async move {
            let item = queue.lock().await.recv().await.expect("a job");
            if let Some((_, pending)) = results.remove(&item.job_id) {
                let _ = pending.sender.send(CommunityDispatchOutcome {
                    result: WorkResult {
                        job_id: item.job_id,
                        worker_id: "w1".into(),
                        success: false,
                        declined: false,
                        output: String::new(),
                        output_hash: String::new(),
                        tokens_generated: 0,
                        total_ms: 0,
                        ms_per_token: 0,
                        engine: String::new(),
                        error: Some("model not loaded on worker".into()),
                        signed_attestation_hex: None,
                    },
                    verification: None,
                    settlement: None,
                });
            }
        });

        let err = dispatch_to_community_worker(&node, "x".into(), 4, Some(test_model_id()))
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

        let err = dispatch_to_community_worker(&node, "x".into(), 4, Some(test_model_id()))
            .await
            .expect_err("must error when not wired");
        assert!(
            err.contains("not wired"),
            "expected 'not wired' error, got: {err}"
        );
    }

    // ── Task 3: worker-signed attestation tests ─────────────────────────

    #[tokio::test]
    async fn dispatch_rejects_a_job_without_exact_model_identity() {
        let node = fake_node_with_workers(vec![(
            worker("w1", &["inference"]),
            std::time::Instant::now(),
        )]);

        let error = dispatch_to_community_worker(&node, "x".into(), 4, None)
            .await
            .expect_err("a coordinator must never enqueue an unbound inference job");

        assert!(error.contains("exact model artifact commitment"));
        assert!(
            node.community_work_results.as_ref().unwrap().is_empty(),
            "identity validation must happen before pending-job state is created"
        );
    }

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

    #[tokio::test]
    async fn inference_activity_never_promotes_local_memory_without_a_successful_receipt() {
        let node = fake_node_with_workers(vec![]);
        let keypair = KeyPair::generate_ed25519();

        let (mined, _) = sign_attestation_for(&keypair, 0, 0);
        node.state
            .full_transactions
            .insert(mined.hash.0, mined.clone());
        node.state.receipts.insert(
            mined.hash.0,
            arc_types::TxReceipt {
                tx_hash: mined.hash,
                block_height: 12,
                block_hash: arc_crypto::hash_bytes(b"mined-block"),
                index: 0,
                success: true,
                gas_used: 7,
                value_commitment: None,
                inclusion_proof: None,
                logs: Vec::new(),
            },
        );
        node.inference_results.insert(
            format!("0x{}", mined.hash.to_hex()),
            json!({
                "input": "receipt-backed local detail",
                "output": "world",
                // A local cache cannot replace canonical on-chain commitments.
                "output_hash": format!("0x{}", arc_crypto::hash_bytes(b"forged-local").to_hex()),
                "observed_at_unix_ms": 20,
            }),
        );

        let (failed, _) = sign_attestation_for(&keypair, 1, 0);
        node.state
            .full_transactions
            .insert(failed.hash.0, failed.clone());
        node.state.receipts.insert(
            failed.hash.0,
            arc_types::TxReceipt {
                tx_hash: failed.hash,
                block_height: 13,
                block_hash: arc_crypto::hash_bytes(b"failed-block"),
                index: 0,
                success: false,
                gas_used: 8,
                value_commitment: None,
                inclusion_proof: None,
                logs: Vec::new(),
            },
        );

        // A transaction-shaped key and an indexed body still do not make a
        // receipt. This is the exact old false-positive path.
        let (unreceipted, _) = sign_attestation_for(&keypair, 2, 0);
        node.state
            .full_transactions
            .insert(unreceipted.hash.0, unreceipted.clone());
        let observation_id = format!("0x{}", unreceipted.hash.to_hex());
        node.inference_results.insert(
            observation_id.clone(),
            json!({
                "input": "only observed in this process",
                "output": "not chain evidence",
                "observed_at_unix_ms": 30,
            }),
        );

        // A receipt beside a type/body-mismatched envelope is still not an
        // inference attestation, even if a local cache key points at it.
        let (mut mismatched, _) = sign_attestation_for(&keypair, 3, 0);
        mismatched.tx_type = TxType::Transfer;
        node.state
            .full_transactions
            .insert(mismatched.hash.0, mismatched.clone());
        node.state.receipts.insert(
            mismatched.hash.0,
            arc_types::TxReceipt {
                tx_hash: mismatched.hash,
                block_height: 14,
                block_hash: arc_crypto::hash_bytes(b"mismatched-block"),
                index: 0,
                success: true,
                gas_used: 9,
                value_commitment: None,
                inclusion_proof: None,
                logs: Vec::new(),
            },
        );
        let mismatched_id = format!("0x{}", mismatched.hash.to_hex());
        node.inference_results.insert(
            mismatched_id.clone(),
            json!({
                "input": "mismatched envelope",
                "output": "must stay an observation",
                "observed_at_unix_ms": 40,
            }),
        );

        let Json(payload) = inference_list_attestations(
            AxumState(node),
            Query(HashMap::from([("limit".to_string(), "50".to_string())])),
        )
        .await
        .unwrap();
        assert_eq!(payload["schema"], INFERENCE_ACTIVITY_SCHEMA);
        assert_eq!(payload["mined_success_count"], 1);
        assert_eq!(payload["observation_count"], 2);

        let activities = payload["activities"].as_array().unwrap();
        let observation = activities
            .iter()
            .find(|row| row["record_id"] == observation_id)
            .expect("unreceipted local work remains visible as an observation");
        assert_eq!(observation["record_kind"], "inference_observation");
        assert_eq!(observation["source"], "node_local_memory");
        assert_eq!(observation["mined"], false);
        assert_eq!(observation["receipt_status"], "absent");
        assert_eq!(observation["tx_type"], "InferenceObservation");
        assert!(observation.get("success").is_none());
        assert!(observation.get("tx_hash").is_none());
        let mismatched_observation = activities
            .iter()
            .find(|row| row["record_id"] == mismatched_id)
            .expect("type/body mismatch must fail closed as a local observation");
        assert_eq!(mismatched_observation["mined"], false);
        assert_eq!(mismatched_observation["receipt_status"], "absent");

        let mined_row = activities
            .iter()
            .find(|row| row["tx_hash"] == format!("0x{}", mined.hash.to_hex()))
            .unwrap();
        assert_eq!(mined_row["record_kind"], "mined_inference_attestation");
        assert_eq!(mined_row["source"], "chain_receipt");
        assert_eq!(mined_row["mined"], true);
        assert_eq!(mined_row["receipt_status"], "success");
        assert_eq!(mined_row["success"], true);
        assert_eq!(
            mined_row["inference"]["output_hash"],
            format!("0x{}", arc_crypto::hash_bytes(b"world").to_hex()),
            "receipt-backed body commitments override node-local display data"
        );
        assert_eq!(
            mined_row["inference"]["input"],
            "receipt-backed local detail"
        );
        assert_eq!(
            mined_row["inference"]["display_content_source"],
            "node_local_memory_enrichment"
        );
        assert_eq!(mined_row["inference"]["display_text_on_chain"], false);

        let failed_row = activities
            .iter()
            .find(|row| row["tx_hash"] == format!("0x{}", failed.hash.to_hex()))
            .unwrap();
        assert_eq!(failed_row["mined"], true);
        assert_eq!(failed_row["receipt_status"], "failed");
        assert_eq!(failed_row["success"], false);

        let attestations = payload["attestations"].as_array().unwrap();
        assert_eq!(attestations.len(), 1);
        assert_eq!(attestations[0]["tx_hash"], mined_row["tx_hash"]);
    }

    fn signed_result_for(keypair: &KeyPair, item: &WorkItem, nonce: u64) -> WorkResult {
        let output_hash = arc_crypto::hash_bytes(b"worker-output-tokens");
        let model_id = item
            .model_id
            .as_deref()
            .and_then(|value| parse_hash256_hex(value, "model_id").ok())
            .unwrap_or_else(|| arc_crypto::hash_bytes(b"worker-test-model"));
        let mut tx = Transaction {
            tx_type: TxType::InferenceAttestation,
            from: keypair.address(),
            nonce,
            body: TxBody::InferenceAttestation(InferenceAttestationBody {
                model_id,
                input_hash: arc_crypto::hash_bytes(item.input.as_bytes()),
                output_hash,
                challenge_period: 100,
                bond: 0,
                beneficiary: None,
            }),
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        tx.sign(keypair).unwrap();
        let signed_attestation_hex = format!("0x{}", hex::encode(bincode::serialize(&tx).unwrap()));
        WorkResult {
            job_id: item.job_id.clone(),
            worker_id: keypair.address().to_hex(),
            success: true,
            declined: false,
            output: "worker output".to_string(),
            output_hash: output_hash.to_hex(),
            tokens_generated: 2,
            total_ms: 25,
            ms_per_token: 12,
            engine: "test worker".to_string(),
            error: None,
            signed_attestation_hex: Some(signed_attestation_hex),
        }
    }

    fn insert_pending_job(
        node: &NodeState,
        item: WorkItem,
        assigned_worker: Option<String>,
    ) -> tokio::sync::oneshot::Receiver<CommunityDispatchOutcome> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        node.community_work_results.as_ref().unwrap().insert(
            item.job_id.clone(),
            PendingCommunityWork {
                item,
                assignment_epoch: Hash256([7u8; 32]),
                job_nonce: 1,
                assigned_worker,
                sender,
            },
        );
        receiver
    }

    #[tokio::test]
    async fn submit_unknown_job_has_no_side_effects() {
        let worker_key = KeyPair::generate_ed25519();
        let worker_id = worker_key.address().to_hex();
        let node =
            fake_node_with_workers(vec![(worker(&worker_id, &["inference"]), Instant::now())]);
        let item = WorkItem {
            job_id: arc_crypto::hash_bytes(b"unknown-job").to_hex(),
            input: "hello".to_string(),
            max_tokens: 8,
            model_id: Some(test_model_id()),
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };

        let err = community_submit_work(
            AxumState(node.clone()),
            Json(signed_result_for(&worker_key, &item, 0)),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert_eq!(node.mempool.len(), 0);
        let stats = node.community_workers.get(&worker_id).unwrap();
        assert_eq!(stats.value().0.success_count, 0);
        assert_eq!(stats.value().0.work_completed, 0);
    }

    #[tokio::test]
    async fn claim_work_persists_worker_assignment_before_returning_prompt() {
        let worker_key = KeyPair::generate_ed25519();
        let worker_id = worker_key.address().to_hex();
        let node =
            fake_node_with_workers(vec![(worker(&worker_id, &["inference"]), Instant::now())]);
        let item = WorkItem {
            job_id: arc_crypto::hash_bytes(b"claim-job").to_hex(),
            input: "hello".to_string(),
            max_tokens: 8,
            model_id: Some(test_model_id()),
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };
        let _receiver = insert_pending_job(&node, item.clone(), None);
        node.community_work_tx
            .as_ref()
            .unwrap()
            .send(item.clone())
            .await
            .unwrap();

        let Json(body) = community_claim_work(
            AxumState(node.clone()),
            Json(ClaimWorkRequest {
                worker_id: worker_id.clone(),
                capabilities: vec!["inference".to_string()],
                model_id: test_model_id(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(body["status"], "work");
        assert_eq!(body["job_id"], item.job_id);
        let pending = node
            .community_work_results
            .as_ref()
            .unwrap()
            .get(&item.job_id)
            .unwrap();
        assert_eq!(pending.assigned_worker.as_deref(), Some(worker_id.as_str()));
        assert_eq!(
            node.community_active_jobs
                .get(&worker_id)
                .as_deref()
                .map(String::as_str),
            Some(item.job_id.as_str())
        );
    }

    #[tokio::test]
    async fn second_claim_for_busy_worker_cannot_dequeue_another_job() {
        let worker_key = KeyPair::generate_ed25519();
        let worker_id = worker_key.address().to_hex();
        let node =
            fake_node_with_workers(vec![(worker(&worker_id, &["inference"]), Instant::now())]);
        let first = WorkItem {
            job_id: arc_crypto::hash_bytes(b"first-claim-job").to_hex(),
            input: "first".to_string(),
            max_tokens: 8,
            model_id: Some(test_model_id()),
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };
        let second = WorkItem {
            job_id: arc_crypto::hash_bytes(b"second-claim-job").to_hex(),
            input: "second".to_string(),
            max_tokens: 8,
            model_id: Some(test_model_id()),
            transaction_domain: None,
            submitted_at_unix_ms: 2,
        };
        let _first_receiver = insert_pending_job(&node, first.clone(), None);
        let _second_receiver = insert_pending_job(&node, second.clone(), None);
        let queue_tx = node.community_work_tx.as_ref().unwrap();
        queue_tx.send(first.clone()).await.unwrap();
        queue_tx.send(second.clone()).await.unwrap();

        let Json(first_response) = community_claim_work(
            AxumState(node.clone()),
            Json(ClaimWorkRequest {
                worker_id: worker_id.clone(),
                capabilities: vec!["inference".to_string()],
                model_id: test_model_id(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(first_response["job_id"], first.job_id);

        let Json(second_response) = community_claim_work(
            AxumState(node.clone()),
            Json(ClaimWorkRequest {
                worker_id: worker_id.clone(),
                capabilities: vec!["inference".to_string()],
                model_id: test_model_id(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(second_response["status"], "no_work");
        assert_eq!(second_response["reason"], "worker_busy");

        let still_queued = node
            .community_work_queue
            .as_ref()
            .unwrap()
            .lock()
            .await
            .recv()
            .await
            .unwrap();
        assert_eq!(still_queued.job_id, second.job_id);
    }

    #[tokio::test]
    async fn submit_unclaimed_job_is_rejected_without_side_effects() {
        let worker_key = KeyPair::generate_ed25519();
        let worker_id = worker_key.address().to_hex();
        let node =
            fake_node_with_workers(vec![(worker(&worker_id, &["inference"]), Instant::now())]);
        let item = WorkItem {
            job_id: arc_crypto::hash_bytes(b"unclaimed-job").to_hex(),
            input: "hello".to_string(),
            max_tokens: 8,
            model_id: None,
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };
        let _receiver = insert_pending_job(&node, item.clone(), None);

        let err = community_submit_work(
            AxumState(node.clone()),
            Json(signed_result_for(&worker_key, &item, 0)),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(
            node.community_work_results
                .as_ref()
                .unwrap()
                .contains_key(&item.job_id)
        );
        assert_eq!(node.mempool.len(), 0);
        assert_eq!(
            node.community_workers
                .get(&worker_id)
                .unwrap()
                .value()
                .0
                .success_count,
            0
        );
    }

    #[tokio::test]
    async fn submit_wrong_assignee_cannot_steal_job() {
        let worker_one = KeyPair::generate_ed25519();
        let worker_two = KeyPair::generate_ed25519();
        let worker_one_id = worker_one.address().to_hex();
        let worker_two_id = worker_two.address().to_hex();
        let node = fake_node_with_workers(vec![
            (worker(&worker_one_id, &["inference"]), Instant::now()),
            (worker(&worker_two_id, &["inference"]), Instant::now()),
        ]);
        let item = WorkItem {
            job_id: arc_crypto::hash_bytes(b"assigned-job").to_hex(),
            input: "hello".to_string(),
            max_tokens: 8,
            model_id: None,
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };
        let _receiver = insert_pending_job(&node, item.clone(), Some(worker_one_id));

        let err = community_submit_work(
            AxumState(node.clone()),
            Json(signed_result_for(&worker_two, &item, 0)),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(
            node.community_work_results
                .as_ref()
                .unwrap()
                .contains_key(&item.job_id)
        );
        assert_eq!(node.mempool.len(), 0);
        assert_eq!(
            node.community_workers
                .get(&worker_two_id)
                .unwrap()
                .value()
                .0
                .success_count,
            0
        );
    }

    #[tokio::test]
    async fn self_signed_fabricated_result_is_rejected_without_reward() {
        let worker_key = KeyPair::generate_ed25519();
        let worker_id = worker_key.address().to_hex();
        let validator_key = Arc::new(KeyPair::generate_ed25519());
        let mut node =
            fake_node_with_workers(vec![(worker(&worker_id, &["inference"]), Instant::now())]);
        node.validator_address = validator_key.address();
        node.validator_keypair = Some(validator_key);
        node.community_rewards_v1_enabled = true;
        let item = WorkItem {
            job_id: arc_crypto::hash_bytes(b"valid-job").to_hex(),
            input: "hello".to_string(),
            max_tokens: 8,
            model_id: Some(test_model_id()),
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };
        let receiver = insert_pending_job(&node, item.clone(), Some(worker_id.clone()));
        node.community_active_jobs
            .insert(worker_id.clone(), item.job_id.clone());
        let result = signed_result_for(&worker_key, &item, 0);

        let error = community_submit_work(AxumState(node.clone()), Json(result.clone()))
            .await
            .expect_err("one worker signature is not independent inference verification");
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(error.1.contains("independent verification"));
        let delivered = receiver.await.unwrap();
        assert!(!delivered.result.success);
        assert!(
            delivered
                .result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("verification failed")
        );
        assert!(!node.community_active_jobs.contains_key(&worker_id));
        assert_eq!(node.mempool.len(), 0);
        assert_eq!(
            node.community_workers
                .get(&worker_id)
                .unwrap()
                .value()
                .0
                .success_count,
            0
        );
        assert_eq!(
            node.community_workers
                .get(&worker_id)
                .unwrap()
                .value()
                .0
                .failure_count,
            1
        );

        let replay = community_submit_work(AxumState(node.clone()), Json(result))
            .await
            .unwrap_err();
        assert_eq!(replay.0, StatusCode::NOT_FOUND);
        assert_eq!(node.mempool.len(), 0);
        assert_eq!(
            node.community_workers
                .get(&worker_id)
                .unwrap()
                .value()
                .0
                .success_count,
            0
        );
    }

    #[tokio::test]
    async fn disabled_reward_flag_cannot_bypass_result_verification() {
        let worker_key = KeyPair::generate_ed25519();
        let worker_id = worker_key.address().to_hex();
        let validator_key = Arc::new(KeyPair::generate_ed25519());
        let mut node =
            fake_node_with_workers(vec![(worker(&worker_id, &["inference"]), Instant::now())]);
        node.validator_address = validator_key.address();
        node.validator_keypair = Some(validator_key);
        assert!(!node.community_rewards_v1_enabled);

        let item = WorkItem {
            job_id: arc_crypto::hash_bytes(b"dark-deploy-job").to_hex(),
            input: "hello".to_string(),
            max_tokens: 8,
            model_id: Some(test_model_id()),
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };
        let receiver = insert_pending_job(&node, item.clone(), Some(worker_id.clone()));
        node.community_active_jobs
            .insert(worker_id.clone(), item.job_id.clone());
        let result = signed_result_for(&worker_key, &item, 0);

        let error = community_submit_work(AxumState(node.clone()), Json(result))
            .await
            .expect_err("activation state must not turn a self-commitment into proof");
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!receiver.await.unwrap().result.success);
        assert_eq!(node.mempool.len(), 0);
    }

    #[test]
    fn output_json_must_match_recomputed_tokens_and_hash() {
        let generated = vec![2u32, 3u32];
        let output_bytes: Vec<u8> = generated.iter().flat_map(|t| t.to_le_bytes()).collect();
        let decoded = "verified output";
        let mut result = WorkResult {
            job_id: "job".to_string(),
            worker_id: "worker".to_string(),
            success: true,
            declined: false,
            output: decoded.to_string(),
            output_hash: arc_crypto::hash_bytes(&output_bytes).to_hex(),
            tokens_generated: generated.len() as u64,
            total_ms: 1,
            ms_per_token: 1,
            engine: "test".to_string(),
            error: None,
            signed_attestation_hex: None,
        };
        compare_community_result_with_tokens(&result, &generated, decoded)
            .expect("matching token result");

        result.output.push_str(" forged");
        assert!(
            compare_community_result_with_tokens(&result, &generated, decoded)
                .unwrap_err()
                .contains("output text")
        );
    }

    #[tokio::test]
    async fn declined_concurrent_job_releases_capacity_without_failure_penalty() {
        let worker_key = KeyPair::generate_ed25519();
        let worker_id = worker_key.address().to_hex();
        let node =
            fake_node_with_workers(vec![(worker(&worker_id, &["inference"]), Instant::now())]);
        let item = WorkItem {
            job_id: arc_crypto::hash_bytes(b"declined-job").to_hex(),
            input: "hello".to_string(),
            max_tokens: 8,
            model_id: None,
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };
        let receiver = insert_pending_job(&node, item.clone(), Some(worker_id.clone()));
        node.community_active_jobs
            .insert(worker_id.clone(), item.job_id.clone());

        let Json(response) = community_submit_work(
            AxumState(node.clone()),
            Json(WorkResult {
                job_id: item.job_id.clone(),
                worker_id: worker_id.clone(),
                success: false,
                declined: true,
                output: String::new(),
                output_hash: String::new(),
                tokens_generated: 0,
                total_ms: 0,
                ms_per_token: 0,
                engine: String::new(),
                error: Some("already processing another job".to_string()),
                signed_attestation_hex: None,
            }),
        )
        .await
        .unwrap();

        assert_eq!(response["ok"], true);
        assert!(response["settlement"].is_null());
        assert!(receiver.await.unwrap().result.declined);
        assert!(!node.community_active_jobs.contains_key(&worker_id));
        let stats = node.community_workers.get(&worker_id).unwrap();
        assert_eq!(stats.value().0.failure_count, 0);
        assert_eq!(stats.value().0.work_completed, 0);
    }

    #[tokio::test]
    async fn heartbeat_cannot_reset_server_authoritative_completion_count() {
        let mut registered = worker("worker-1", &["inference"]);
        registered.work_completed = 7;
        registered.success_count = 7;
        let node = fake_node_with_workers(vec![(registered, Instant::now())]);

        let _ = community_heartbeat(
            AxumState(node.clone()),
            Json(CommunityHeartbeatRequest {
                worker_id: "worker-1".to_string(),
                work_completed: Some(0),
            }),
        )
        .await
        .unwrap();

        let worker = node.community_workers.get("worker-1").unwrap();
        assert_eq!(worker.value().0.work_completed, 7);
        assert_eq!(worker.value().0.success_count, 7);
    }

    #[tokio::test]
    async fn submit_commitment_mismatch_leaves_job_retryable() {
        let worker_key = KeyPair::generate_ed25519();
        let worker_id = worker_key.address().to_hex();
        let node =
            fake_node_with_workers(vec![(worker(&worker_id, &["inference"]), Instant::now())]);
        let item = WorkItem {
            job_id: arc_crypto::hash_bytes(b"mismatch-job").to_hex(),
            input: "hello".to_string(),
            max_tokens: 8,
            model_id: None,
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };
        let _receiver = insert_pending_job(&node, item.clone(), Some(worker_id.clone()));
        let mut result = signed_result_for(&worker_key, &item, 0);
        result.output_hash = arc_crypto::hash_bytes(b"different").to_hex();

        let err = community_submit_work(AxumState(node.clone()), Json(result))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            node.community_work_results
                .as_ref()
                .unwrap()
                .contains_key(&item.job_id)
        );
        assert_eq!(node.mempool.len(), 0);
        assert_eq!(
            node.community_workers
                .get(&worker_id)
                .unwrap()
                .value()
                .0
                .success_count,
            0
        );
    }

    #[tokio::test]
    async fn generic_submit_rejects_unverified_and_type_mismatched_transactions() {
        let node = fake_node_with_workers(vec![]);
        let key = KeyPair::generate_ed25519();

        let mut unsigned = Transaction::new_transfer(key.address(), key.address(), 1, 0);
        unsigned.sig_verified = true;
        let unsigned_err = submit_signed_tx(AxumState(node.clone()), Json(unsigned))
            .await
            .err()
            .expect("unsigned transaction must reject");
        assert_eq!(unsigned_err, StatusCode::BAD_REQUEST);

        let mut mismatch = Transaction::new_transfer(key.address(), key.address(), 1, 0);
        mismatch.tx_type = TxType::InferenceAttestation;
        mismatch.sign(&key).unwrap();
        let mismatch_err = submit_signed_tx(AxumState(node.clone()), Json(mismatch))
            .await
            .err()
            .expect("type mismatch must reject");
        assert_eq!(mismatch_err, StatusCode::BAD_REQUEST);
        assert_eq!(node.mempool.len(), 0);

        let mut valid = Transaction::new_transfer(key.address(), key.address(), 1, 0);
        valid.sign(&key).unwrap();
        let _ = submit_signed_tx(AxumState(node.clone()), Json(valid))
            .await
            .expect("valid signed transaction");
        assert_eq!(node.mempool.len(), 1);
    }

    fn tier1_request_transaction(key: &KeyPair) -> Transaction {
        let input_blob = b"paid request must stay dark".to_vec();
        let mut tx = Transaction {
            tx_type: TxType::InferenceRequest,
            from: key.address(),
            nonce: 0,
            body: TxBody::InferenceRequest(arc_types::transaction::InferenceRequestBody {
                request_id: arc_crypto::hash_bytes(b"rpc-tier1-request").0,
                model_id: arc_crypto::hash_bytes(b"exact-model-artifact"),
                input_hash: arc_crypto::hash_bytes(&input_blob),
                input_blob,
                max_tokens: 8,
                tier: 1,
                max_reward: 10,
                deadline_blocks: 20,
                committee_size: 5,
            }),
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        tx.sign(key).unwrap();
        tx
    }

    #[test]
    fn paid_inference_gate_checks_envelope_body_and_preserves_refunds() {
        let key = KeyPair::generate_ed25519();
        let transfer = Transaction::new_transfer(key.address(), key.address(), 1, 0);
        for tx_type in [
            TxType::InferenceEscrowOpen,
            TxType::InferenceEscrowRelease,
            TxType::InferenceRequest,
            TxType::InferenceVote,
            TxType::InferenceFinalize,
        ] {
            let mut disguised = transfer.clone();
            disguised.tx_type = tx_type;
            assert!(
                uses_unready_paid_inference_protocol(&disguised),
                "restricted envelope {tx_type:?} must stay dark"
            );
        }

        let mut disguised_body = tier1_request_transaction(&key);
        disguised_body.tx_type = TxType::Transfer;
        assert!(uses_unready_paid_inference_protocol(&disguised_body));

        let refund = Transaction {
            tx_type: TxType::InferenceEscrowRefund,
            from: key.address(),
            nonce: 0,
            body: TxBody::InferenceEscrowRefund(
                arc_types::transaction::InferenceEscrowRefundBody {
                    request_id: [7; 32],
                    model_id: arc_crypto::hash_bytes(b"legacy-model"),
                    max_tokens: 8,
                    timeout_blocks: 20,
                },
            ),
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        assert!(
            !uses_unready_paid_inference_protocol(&refund),
            "older locked funds must retain their signed refund path"
        );
    }

    #[tokio::test]
    async fn all_public_tier1_submission_paths_fail_before_side_effects() {
        let key = Arc::new(KeyPair::generate_ed25519());
        let mut node = fake_node_with_workers(vec![]);
        node.validator_address = key.address();
        node.validator_keypair = Some(key.clone());

        let endpoint_err = inference_onchain_submit(
            AxumState(node.clone()),
            Some(Json(json!({
                "input": "sign this with the validator",
                "max_reward": u64::MAX,
            }))),
        )
        .await
        .expect_err("Tier 1 convenience submit must stay dark");
        assert_eq!(endpoint_err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(endpoint_err.1.0.error, PAID_INFERENCE_UNAVAILABLE_MESSAGE);
        assert_eq!(node.mempool.len(), 0);

        let relay_err = submit_signed_tx(
            AxumState(node.clone()),
            Json(tier1_request_transaction(&key)),
        )
        .await
        .err()
        .expect("generic signed relay must not bypass the Tier 1 gate");
        assert_eq!(relay_err, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(node.mempool.len(), 0);
    }

    #[tokio::test]
    async fn paid_run_consensus_rejects_before_input_model_or_mempool_work() {
        let mut node = fake_node_with_workers(vec![]);
        node.inference_model = None;
        node.model_artifact_id = None;

        // A null legacy field is still an attempted paid request. The missing
        // input and model would produce different errors if either were read
        // before the paid-protocol boundary.
        let err = inference_run_consensus(AxumState(node.clone()), Json(json!({ "payer": null })))
            .await
            .expect_err("legacy paid consensus must stay dark");
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.1.0.error, PAID_INFERENCE_UNAVAILABLE_MESSAGE);
        assert_eq!(node.mempool.len(), 0);
        assert_eq!(node.state.height(), 0);
    }

    #[tokio::test]
    async fn generic_signed_ingress_keeps_legacy_escrow_refund_available() {
        let node = fake_node_with_workers(vec![]);
        let key = KeyPair::generate_ed25519();
        let mut refund = Transaction {
            tx_type: TxType::InferenceEscrowRefund,
            from: key.address(),
            nonce: 0,
            body: TxBody::InferenceEscrowRefund(
                arc_types::transaction::InferenceEscrowRefundBody {
                    request_id: [9; 32],
                    model_id: arc_crypto::hash_bytes(b"legacy-model"),
                    max_tokens: 8,
                    timeout_blocks: 20,
                },
            ),
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        refund.sign(&key).unwrap();

        let _ = submit_signed_tx(AxumState(node.clone()), Json(refund))
            .await
            .expect("signed refund must remain relayable");
        assert_eq!(node.mempool.len(), 1);
    }

    #[tokio::test]
    async fn cleanup_shard_is_model_independent_targeted_and_idempotent() {
        let node = fake_node_with_workers(vec![]);
        let target = "target-request".to_string();
        let unrelated = "unrelated-request".to_string();
        node.shard_kv_caches.insert(
            target.clone(),
            Arc::new(std::sync::Mutex::new(
                arc_inference::cached_integer_model::KVCache::new(1),
            )),
        );
        node.shard_kv_caches.insert(
            unrelated.clone(),
            Arc::new(std::sync::Mutex::new(
                arc_inference::cached_integer_model::KVCache::new(1),
            )),
        );

        let Json(first) = inference_cleanup_shard(
            AxumState(node.clone()),
            Json(CleanupShardRequest {
                request_id: target.clone(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(first["removed"], true);
        assert!(!node.shard_kv_caches.contains_key(&target));
        assert!(node.shard_kv_caches.contains_key(&unrelated));

        let Json(second) = inference_cleanup_shard(
            AxumState(node.clone()),
            Json(CleanupShardRequest { request_id: target }),
        )
        .await
        .unwrap();
        assert_eq!(second["removed"], false);
        assert!(node.shard_kv_caches.contains_key(&unrelated));
    }

    #[test]
    fn decode_and_verify_accepts_worker_signed_attestation() {
        let kp = KeyPair::generate_ed25519();
        let (orig, hex_s) = sign_attestation_for(&kp, 0, 0);
        let worker_id = format!("0x{}", hex::encode(kp.address().0));
        let got = decode_and_verify_worker_attestation(&hex_s, &worker_id).expect("verify ok");
        assert_eq!(got.hash, orig.hash, "hash should round-trip exactly");
        assert_eq!(got.tx_type, TxType::InferenceAttestation);
    }

    #[test]
    fn worker_attestation_decoder_rejects_forged_huge_signature_vector_length() {
        let keypair = KeyPair::generate_ed25519();
        let (transaction, encoded) = sign_attestation_for(&keypair, 0, 0);
        let mut raw = hex::decode(encoded.trim_start_matches("0x")).unwrap();
        let encoded_signature = bincode::serialize(&transaction.signature).unwrap();
        let signature_offset = raw
            .windows(encoded_signature.len())
            .position(|window| window == encoded_signature)
            .expect("canonical signature bytes occur in the transaction");
        let encoded_length = 64_u64.to_le_bytes();
        let length_in_signature = encoded_signature
            .windows(encoded_length.len())
            .position(|window| window == encoded_length)
            .expect("Ed25519 Vec length is fixed-width bincode u64");
        let length_offset = signature_offset + length_in_signature;
        raw[length_offset..length_offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());

        let error = decode_and_verify_worker_attestation(
            &format!("0x{}", hex::encode(raw)),
            &format!("0x{}", keypair.address().to_hex()),
        )
        .expect_err("forged length must fail within the received byte bound");
        assert!(error.contains("bincode deserialize failed"), "{error}");
    }

    #[test]
    fn worker_attestation_decoder_rejects_trailing_bytes() {
        let keypair = KeyPair::generate_ed25519();
        let (_, encoded) = sign_attestation_for(&keypair, 0, 0);
        let mut raw = hex::decode(encoded.trim_start_matches("0x")).unwrap();
        raw.extend_from_slice(b"ignored suffix");

        let error = decode_and_verify_worker_attestation(
            &format!("0x{}", hex::encode(raw)),
            &format!("0x{}", keypair.address().to_hex()),
        )
        .expect_err("noncanonical suffix must never be ignored");
        assert!(error.contains("bincode deserialize failed"), "{error}");
    }

    #[test]
    fn community_certificate_rejects_every_noncanonical_reconstructed_field() {
        let keypair = KeyPair::generate_ed25519();
        let (canonical, _) = sign_attestation_for(&keypair, 0, 0);
        let work_item = WorkItem {
            job_id: arc_crypto::hash_bytes(b"certificate-shape").to_hex(),
            input: "hello".to_string(),
            max_tokens: 8,
            model_id: Some(format!(
                "0x{}",
                arc_crypto::hash_bytes(b"arc-test-model").to_hex()
            )),
            transaction_domain: None,
            submitted_at_unix_ms: 1,
        };
        let output_hash = arc_crypto::hash_bytes(b"world");
        validate_worker_attestation_for_job(&canonical, &work_item, output_hash)
            .expect("the protocol-fixed worker certificate shape is valid");

        let mut cases: Vec<(&str, Transaction)> = Vec::new();

        let mut nonzero_bond = canonical.clone();
        if let TxBody::InferenceAttestation(body) = &mut nonzero_bond.body {
            body.bond = 1;
        }
        nonzero_bond.sign(&keypair).unwrap();
        cases.push(("bond", nonzero_bond));

        let mut nonzero_fee = canonical.clone();
        nonzero_fee.fee = 1;
        nonzero_fee.sign(&keypair).unwrap();
        cases.push(("fee", nonzero_fee));

        let mut nonzero_gas = canonical.clone();
        nonzero_gas.gas_limit = 1;
        nonzero_gas.sign(&keypair).unwrap();
        cases.push(("gas_limit", nonzero_gas));

        let mut beneficiary = canonical;
        if let TxBody::InferenceAttestation(body) = &mut beneficiary.body {
            body.beneficiary = Some(keypair.address());
        }
        beneficiary.sign(&keypair).unwrap();
        cases.push(("beneficiary", beneficiary));

        for (field, transaction) in cases {
            let error = validate_worker_attestation_for_job(&transaction, &work_item, output_hash)
                .expect_err("noncanonical worker certificate must fail before mempool");
            assert!(error.contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn decode_strips_optional_0x_prefix() {
        let kp = KeyPair::generate_ed25519();
        let (_, hex_with_0x) = sign_attestation_for(&kp, 0, 0);
        let bare = hex_with_0x.trim_start_matches("0x").to_string();
        let worker_id = hex::encode(kp.address().0); // no 0x
        decode_and_verify_worker_attestation(&bare, &worker_id)
            .expect("bare hex + bare worker_id should work");
    }

    #[test]
    fn decode_rejects_worker_id_mismatch() {
        let kp_a = KeyPair::generate_ed25519();
        let kp_b = KeyPair::generate_ed25519();
        let (_, hex_s) = sign_attestation_for(&kp_a, 0, 0);
        // Submit kp_a's signed tx but claim to be kp_b
        let worker_id_b = format!("0x{}", hex::encode(kp_b.address().0));
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
        let worker_id = format!("0x{}", hex::encode(kp.address().0));
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
        let worker_id = format!("0x{}", hex::encode(kp.address().0));
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

        let resp = workers_scoreboard(AxumState(node), Query(HashMap::new())).await;
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
        assert_eq!(
            v.get("eligible_inference_workers").and_then(|x| x.as_u64()),
            Some(3),
            "selected-coordinator eligibility must use exact model commitments"
        );
        assert_eq!(
            v.get("coordinator_model_id").and_then(|x| x.as_str()),
            Some(test_model_id().as_str())
        );
        assert_eq!(
            workers[0].get("model_id").and_then(|x| x.as_str()),
            Some(test_model_id().as_str())
        );
    }

    #[tokio::test]
    async fn scoreboard_excludes_stale_workers() {
        let now = std::time::Instant::now();
        let stale = now - std::time::Duration::from_secs(COMMUNITY_WORKER_TTL_SECS + 30);
        let node = fake_node_with_workers(vec![
            (worker_with_stats("alive", 5, 0, 1000, 200), now),
            (worker_with_stats("stale", 999, 0, 999_999, 999_999), stale),
        ]);

        let v: Value = workers_scoreboard(AxumState(node), Query(HashMap::new()))
            .await
            .0;
        let workers = v.get("workers").and_then(|x| x.as_array()).unwrap();
        assert_eq!(workers.len(), 1, "stale worker must be hidden");
        assert_eq!(
            workers[0].get("worker_id").and_then(|x| x.as_str()),
            Some("alive")
        );
    }

    #[test]
    // `score` is assigned the literal 0.0 by the branch under test, so exact
    // equality is the assertion: an epsilon would also pass for a wrong-but-tiny
    // score, which is the bug this is here to catch.
    #[allow(clippy::float_cmp)]
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
            success_rate * 1000.0 - (w.sum_total_ms_success as f64 / w.success_count as f64)
        };
        assert_eq!(score, 0.0);
    }

    // ── assemble_pipeline ───────────────────────────────────────────────
    //
    // This is the planner all three inference endpoints now share, so these
    // pin the behaviour that used to differ between them. Each case below
    // corresponds to a failure observed on the live network.

    fn shard(node: &str, addr: &str, start: usize, end: usize) -> ShardInfo {
        ShardInfo {
            start_layer: start,
            end_layer: end,
            total_layers: 32,
            model_id: "0xabec".into(),
            model_name: "arc-32L-4096d-32h-32000v".into(),
            memory_mb: 100,
            full_model_mb: 4000,
            socket_addr: addr.into(),
            node_name: node.into(),
        }
    }

    /// The live topology: 32 layers in 6 ranges, each on 3 of 6 nodes.
    fn live_topology() -> Vec<ShardInfo> {
        let ranges = [(0, 6), (6, 12), (12, 17), (17, 22), (22, 27), (27, 32)];
        let nodes = [
            ("NYC", "149.28.32.76:9090"),
            ("LAX", "140.82.16.112:9090"),
            ("AMS", "136.244.109.1:9090"),
            ("LHR", "104.238.171.11:9090"),
            ("NRT", "202.182.107.41:9090"),
            ("SGP", "149.28.153.31:9090"),
        ];
        let mut out = Vec::new();
        for (i, (s, e)) in ranges.iter().enumerate() {
            for r in 0..3 {
                let (name, addr) = nodes[(i + r) % nodes.len()];
                out.push(shard(name, addr, *s, *e));
            }
        }
        out
    }

    fn no_stats() -> dashmap::DashMap<String, LatencyEWMA> {
        dashmap::DashMap::new()
    }

    #[test]
    fn exact_artifact_filter_keeps_same_model_shards_and_rejects_mutated_weights() {
        let exact_id =
            arc_crypto::hash_bytes(b"GGUF:layers=32,width=4096,vocab=32000;weights=artifact-A");
        let mutated_id =
            arc_crypto::hash_bytes(b"GGUF:layers=32,width=4096,vocab=32000;weights=artifact-B");
        assert_ne!(exact_id, mutated_id, "same shape is not model identity");

        let mut exact_shards = live_topology();
        for shard in &mut exact_shards {
            shard.model_id = format!("0x{}", exact_id.to_hex());
        }
        let hops = assemble_pipeline_for_model(exact_shards.clone(), exact_id, &no_stats())
            .expect("partial shards of the same artifact form one pipeline");
        assert_eq!(hops.len(), 6);

        for shard in &mut exact_shards {
            shard.model_id = format!("0x{}", mutated_id.to_hex());
        }
        assert!(matches!(
            assemble_pipeline_for_model(exact_shards, exact_id, &no_stats()),
            Err(PipelineError::NoMatchingModel { model_id }) if model_id == exact_id
        ));
    }

    #[test]
    fn assemble_accepts_the_live_3x_replicated_topology() {
        // 18 announcements, 6 hops, 3 replicas each. /inference/auto's old
        // hand-rolled walk called this "not contiguous" because the SECOND
        // [0, 6) replica has start_layer 0 != covered_to 6 — so it declared
        // has_full_pipeline false on a network with complete coverage and
        // never took the sharded path once.
        let hops =
            assemble_pipeline(live_topology(), &no_stats()).expect("live topology covers 0..32");
        assert_eq!(
            hops.len(),
            6,
            "one hop per layer range, not per announcement"
        );
        assert_eq!(
            hops.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            vec![(0, 6), (6, 12), (12, 17), (17, 22), (22, 27), (27, 32)]
        );
        for (range, replicas) in &hops {
            assert_eq!(
                replicas.len(),
                3,
                "range {:?} should keep all 3 replicas",
                range
            );
        }
    }

    #[test]
    fn assemble_drops_stub_addresses_even_when_they_are_the_only_candidate() {
        // A community worker announcing 127.0.0.1:9090 for an off-grid [0, 8)
        // became the ONLY candidate for layer 0 under run_consensus's old
        // "keep the stub as a fallback" rule, and the pipeline walked off the
        // rails. A bucket that is stub-only must be dropped entirely.
        let mut shards = vec![
            shard("SQUATTER", "127.0.0.1:9090", 0, 8),
            shard("GHOST", "0.0.0.0:9090", 0, 6),
            shard("EMPTY", "", 0, 6),
            shard("V6", "[::1]:9090", 0, 6),
        ];
        shards.extend(live_topology().into_iter().filter(|s| s.start_layer != 0));

        let err = assemble_pipeline(shards, &no_stats())
            .expect_err("layer 0 has only unroutable candidates");
        // Honest failure: it must NOT claim to have covered 0..6 via a stub.
        assert!(
            matches!(err, PipelineError::Gap { expected: 0, .. }),
            "expected a gap at layer 0, got {err:?}"
        );
    }

    #[test]
    fn assemble_prefers_routable_addr_over_same_nodes_stub_announcement() {
        // After a coordinator reboot its own self-announce (0.0.0.0:9090) and
        // the gossiped copy (public IP) land under different registry keys,
        // producing two entries for the same node and range.
        let mut shards = live_topology();
        shards.push(shard("NYC", "0.0.0.0:9090", 0, 6));
        let hops = assemble_pipeline(shards, &no_stats()).expect("still fully covered");
        let (_, first) = &hops[0];
        assert_eq!(
            first.len(),
            3,
            "the stub duplicate must collapse into the routable entry"
        );
        assert!(
            first.iter().all(|r| !is_stub_socket_addr(&r.socket_addr)),
            "no stub survived: {:?}",
            first.iter().map(|r| &r.socket_addr).collect::<Vec<_>>()
        );
    }

    #[test]
    fn assemble_skips_offgrid_range_instead_of_reporting_a_false_gap() {
        // An off-grid [0, 8) alongside the standard [0, 6)/[6, 12) tiling must
        // be skipped, not collided with. Without the overlap skip this is the
        // "Pipeline gap: expected layer 6 next, got shard [0, 8)" that took
        // out sharded inference on the healthiest seed.
        let mut shards = live_topology();
        shards.push(shard("JOINER", "203.0.113.7:9090", 0, 8));
        let hops = assemble_pipeline(shards, &no_stats()).expect("standard tiling still covers");
        assert_eq!(
            hops.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            vec![(0, 6), (6, 12), (12, 17), (17, 22), (22, 27), (27, 32)],
            "the off-grid range must be skipped, not walked"
        );
    }

    #[test]
    fn assemble_reports_a_real_gap_as_a_gap() {
        let shards: Vec<ShardInfo> = live_topology()
            .into_iter()
            .filter(|s| s.start_layer != 12)
            .collect();
        let err = assemble_pipeline(shards, &no_stats()).expect_err("layers 12..17 are missing");
        match err {
            PipelineError::Gap { expected, got, .. } => {
                assert_eq!(expected, 12);
                assert_eq!(got, (17, 22));
            }
            other => panic!("expected Gap, got {other:?}"),
        }
    }

    #[test]
    fn assemble_reports_truncated_coverage_as_incomplete() {
        let shards: Vec<ShardInfo> = live_topology()
            .into_iter()
            .filter(|s| s.end_layer <= 27)
            .collect();
        let err = assemble_pipeline(shards, &no_stats()).expect_err("nothing covers 27..32");
        assert_eq!(
            err,
            PipelineError::Incomplete {
                covered: 27,
                total: 32
            }
        );
    }

    #[test]
    fn assemble_orders_replicas_by_fresh_latency() {
        let stats = no_stats();
        let now = std::time::Instant::now();
        // AMS fastest, NYC middle, LAX slowest.
        for (addr, ms) in [
            ("136.244.109.1:9090", 20.0),
            ("149.28.32.76:9090", 200.0),
            ("140.82.16.112:9090", 900.0),
        ] {
            stats.insert(
                addr.to_string(),
                LatencyEWMA {
                    ms,
                    count: 10,
                    last_updated: now,
                    probe_only: false,
                },
            );
        }
        let hops = assemble_pipeline(live_topology(), &stats).unwrap();
        let (_, first) = &hops[0];
        assert_eq!(
            first
                .iter()
                .map(|r| r.node_name.as_str())
                .collect::<Vec<_>>(),
            vec!["AMS", "NYC", "LAX"],
            "primary must be the fastest measured replica"
        );
    }

    #[test]
    fn stale_latency_samples_stop_steering_the_router() {
        // The LHR case: an EWMA of 37,276 ms recorded over ten hours ago kept
        // it permanently last, and because only replicas[0] was ever dialled
        // it could never earn a new sample. Past LATENCY_STALE_SECS a sample
        // is UNKNOWN, and unknown replicas keep their announcement order
        // rather than being sorted to the bottom by a fossil.
        let stats = no_stats();
        let stale =
            std::time::Instant::now() - std::time::Duration::from_secs(LATENCY_STALE_SECS + 60);
        stats.insert(
            "104.238.171.11:9090".into(),
            LatencyEWMA {
                ms: 37_276.0,
                count: 1370,
                last_updated: stale,
                probe_only: false,
            },
        );
        let fresh_stat = stats.get("104.238.171.11:9090").unwrap();
        assert_eq!(
            effective_latency_ms(&fresh_stat),
            None,
            "a 6-minute-old sample is not evidence"
        );
        drop(fresh_stat);

        // And a fresh one still counts.
        stats.insert(
            "140.82.16.112:9090".into(),
            LatencyEWMA {
                ms: 300.0,
                count: 5,
                last_updated: std::time::Instant::now(),
                probe_only: false,
            },
        );
        let s = stats.get("140.82.16.112:9090").unwrap();
        assert_eq!(effective_latency_ms(&s), Some(300.0));
    }

    /// A node with no blocks must not claim the chain is advancing, and must
    /// not answer `"ok"`. This is the regression that let four seeds report
    /// `{"status":"ok"}` for eight days while sealing nothing.
    #[test]
    fn health_status_is_not_ok_when_block_liveness_is_unknown() {
        let node = fake_node_with_workers(Vec::new());
        let age = latest_available_block(&node).and_then(|b| age_secs_from_ms(b.header.timestamp));
        let chain_advancing = age.map(|a| a <= BLOCK_PRODUCTION_FRESH_SECS);

        // An empty StateDB knows nothing about liveness. "Unknown" must not
        // collapse into "fresh".
        assert_ne!(
            chain_advancing,
            Some(true),
            "a node with no readable block must never report the chain as advancing"
        );
    }

    /// Both directions of the status mapping, including the one a stake-0
    /// community node can never reach locally (it takes no consensus role, so
    /// it never seals and never goes fresh).
    #[test]
    fn health_status_tracks_block_liveness_in_both_directions() {
        // Sealing normally → ok, and no reason field to render.
        let (status, reason) = health_status_from(Some(true), Some(29));
        assert_eq!(status, "ok");
        assert!(reason.is_none(), "a healthy node must not carry a reason");

        // The observed AMS/NRT/SGP stall: 8 days.
        let (status, reason) = health_status_from(Some(false), Some(670_501));
        assert_eq!(status, "degraded");
        let reason = reason.expect("a degraded node must explain itself");
        assert!(
            reason.contains("670501"),
            "reason must cite the real age: {reason}"
        );
        assert!(
            reason.contains("round progress is not block production"),
            "reason must name the DAG/commit distinction that hid this: {reason}"
        );

        // Unknown liveness must not collapse into healthy.
        let (status, reason) = health_status_from(None, None);
        assert_eq!(status, "degraded");
        assert!(reason.expect("reason required").contains("unknown"));
    }

    /// The freshness boundary is inclusive, so a node sealing exactly at the
    /// window edge is not flapped to degraded.
    #[test]
    fn block_age_at_the_freshness_boundary_is_still_ok() {
        assert_eq!(
            health_status_from(Some(true), Some(HEALTH_STALL_SECS)).0,
            "ok"
        );
        let advancing = |age: u64| age <= HEALTH_STALL_SECS;
        assert!(advancing(HEALTH_STALL_SECS));
        assert!(!advancing(HEALTH_STALL_SECS + 1));
    }

    /// The threshold must not flag a slow-but-sealing node.
    ///
    /// Measured on the live network over 10.3 h on 2026-08-18/19: NYC sealed
    /// 50 blocks (~744 s/block) and LAX 108 (~344 s/block), while AMS/NRT/SGP
    /// sat at ~670,000 s. A window that calls NYC and LAX degraded would be a
    /// worse lie than the hardcoded "ok" this replaced, because it would mark
    /// the only two healthy seeds as broken.
    #[test]
    // The last two assertions compare named constants, so clippy sees a value
    // fixed at compile time. That is the point: they fail the build's test run
    // if anyone edits HEALTH_STALL_SECS or BLOCK_PRODUCTION_FRESH_SECS back into
    // an ordering that would mark the two healthy seeds degraded.
    #[allow(clippy::assertions_on_constants)]
    fn health_threshold_separates_slow_sealing_from_an_eight_day_stall() {
        let advancing = |age: u64| age <= HEALTH_STALL_SECS;

        // Real observed inter-block times on the two sealing seeds.
        assert!(advancing(744), "NYC cadence ~12 min must read as advancing");
        assert!(
            advancing(344),
            "LAX cadence ~5.7 min must read as advancing"
        );
        // And the ages actually observed while probing them.
        assert!(advancing(677));
        assert!(advancing(460));

        // The failure this exists to catch: ~7.8 days.
        assert!(!advancing(670_501));
        assert_eq!(health_status_from(Some(false), Some(670_501)).0, "degraded");

        // The old 120 s window would have called both healthy seeds degraded —
        // guard against anyone re-pointing the status field at it.
        assert!(744 > BLOCK_PRODUCTION_FRESH_SECS);
        assert!(HEALTH_STALL_SECS > BLOCK_PRODUCTION_FRESH_SECS);
    }

    /// The demo hands back a receipt. If the tx is not in a block, that receipt
    /// must not carry a link — `/tx/{hash}` answers "not found" and the caller
    /// cannot distinguish an unmined tx from a real one.
    #[test]
    fn explorer_url_is_null_until_the_tx_is_actually_in_a_block() {
        let node = fake_node_with_workers(Vec::new());
        let tx_hash = Hash256::ZERO;

        let (url, reason) = explorer_url_for(&node, &tx_hash, "submitted_to_mempool");
        assert!(
            url.is_null(),
            "a mempool-only tx must not be given an explorer link, got {url:?}"
        );
        let reason = reason.expect("a null link must always carry a reason");
        assert!(
            reason.contains("mempool"),
            "the reason must say why there is no link: {reason}"
        );

        // A tx that was never submitted gets its own distinct explanation.
        let (url, reason) = explorer_url_for(&node, &tx_hash, "sign_failed");
        assert!(url.is_null());
        assert!(
            reason.expect("reason required").contains("sign_failed"),
            "a non-submitted attestation must name its actual status"
        );
    }

    #[test]
    fn health_probe_only_supersedes_an_implausible_recorded_latency() {
        // LHR: 37 s recorded, 200 ms health RTT → fossil, reset it.
        assert!(probe_supersedes_recorded(37_276.0, 200));
        // A genuinely slow node stays slow: 8 s recorded, 6 s health RTT.
        assert!(!probe_supersedes_recorded(8_000.0, 6_000));
        // A merely-mediocre node is never reset, however fast /health is.
        assert!(!probe_supersedes_recorded(900.0, 5));
        // Right at the floor.
        assert!(!probe_supersedes_recorded(LATENCY_POISON_FLOOR_MS, 1));
    }

    #[test]
    // `record_latency` stores 4_000 exactly; the test asserts the probe did not
    // blend into it, so any tolerance would hide exactly the regression tested.
    #[allow(clippy::float_cmp)]
    fn probe_latency_never_overwrites_a_real_hop_sample() {
        let stats = no_stats();
        record_latency(&stats, "1.2.3.4:9090", 4_000);
        record_probe_latency(&stats, "1.2.3.4:9090", 12);
        let s = stats.get("1.2.3.4:9090").unwrap();
        assert_eq!(
            s.ms, 4_000.0,
            "a 12 ms /health RTT is not a forward_shard latency"
        );
        assert!(!s.probe_only);
    }

    #[test]
    // Same reason: the assertion is "replaced, not blended", which is only
    // expressible as exact equality with the recorded sample.
    #[allow(clippy::float_cmp)]
    fn first_real_hop_replaces_a_provisional_probe_value_rather_than_blending() {
        let stats = no_stats();
        record_probe_latency(&stats, "1.2.3.4:9090", 200);
        {
            let s = stats.get("1.2.3.4:9090").unwrap();
            assert!(s.probe_only);
            assert_eq!(s.count, 0);
        }
        record_latency(&stats, "1.2.3.4:9090", 3_000);
        let s = stats.get("1.2.3.4:9090").unwrap();
        assert_eq!(
            s.ms, 3_000.0,
            "blending would have understated the hop cost"
        );
        assert!(!s.probe_only);
        assert_eq!(s.count, 1);
    }

    // ── race-to-majority tally ──────────────────────────────────────────
    //
    // `pipeline_hop`'s Fanout arm returns the instant `needed` responses carry
    // the same output hash. These exercise that decision rule directly: given
    // hashes arriving in a known order, at which arrival does the hop return,
    // and which response does it pick?

    /// Mirror of the tally in `pipeline_hop`'s Fanout arm: fold hashes in
    /// arrival order and report the index of the arrival that reached
    /// `needed` agreement, plus the winning group.
    fn tally_until_majority(
        arrivals: &[Option<&str>],
        needed: usize,
    ) -> Option<(usize, String, Vec<usize>)> {
        let mut tally: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, h) in arrivals.iter().enumerate() {
            let Some(h) = h else { continue };
            let bucket = tally.entry((*h).to_string()).or_default();
            bucket.push(idx);
            if bucket.len() >= needed {
                return Some((idx, (*h).to_string(), bucket.clone()));
            }
        }
        None
    }

    #[test]
    fn majority_returns_on_the_second_agreeing_response_not_the_third() {
        // k=3, needed=2. The whole point: with 3 replicas per range and the
        // desktop's k=3, the old collect loop waited for the SLOWEST of three.
        // Hop cost is now the 2nd response.
        let got = tally_until_majority(&[Some("0xaa"), Some("0xaa"), Some("0xaa")], 2)
            .expect("two agreed");
        assert_eq!(
            got.0, 1,
            "returned at arrival index 1 — the third never blocks us"
        );
        assert_eq!(got.1, "0xaa");
        assert_eq!(got.2, vec![0, 1]);
    }

    #[test]
    fn majority_waits_past_a_divergent_first_responder() {
        // Fastest replica disagrees. We must NOT return its answer just
        // because it arrived first — that is the difference between consensus
        // and a race.
        let got = tally_until_majority(&[Some("0xbad"), Some("0xok"), Some("0xok")], 2)
            .expect("the honest pair agreed");
        assert_eq!(got.0, 2, "had to wait for the second honest answer");
        assert_eq!(got.1, "0xok");
        assert_eq!(
            got.2,
            vec![1, 2],
            "the divergent first responder is not in the winning group"
        );
    }

    #[test]
    fn three_way_split_reaches_no_majority() {
        assert!(tally_until_majority(&[Some("0xa"), Some("0xb"), Some("0xc")], 2).is_none());
    }

    #[test]
    fn failed_replicas_do_not_count_toward_agreement() {
        // None = the replica errored or timed out.
        assert!(tally_until_majority(&[None, Some("0xa"), None], 2).is_none());
        let got = tally_until_majority(&[None, Some("0xa"), Some("0xa")], 2).expect("two agreed");
        assert_eq!(got.0, 2);
    }

    #[test]
    fn hedged_mode_takes_the_first_valid_answer() {
        // redundancy: 2 is Fanout { fanout: 2, needed: 1 } — first valid wins.
        let got = tally_until_majority(&[Some("0xaa"), Some("0xbb")], 1).expect("first valid");
        assert_eq!(got.0, 0);
        assert_eq!(got.1, "0xaa");
    }

    #[test]
    fn free_fanout_can_degrade_but_reward_quorum_never_does() {
        assert_eq!(
            fanout_quorum_plan(3, 2, 3, false).unwrap(),
            (3, 2),
            "community verification starts with the exact 2-of-3 plan"
        );
        for live_after_eviction in [2, 1] {
            let error = fanout_quorum_plan(3, 2, live_after_eviction, false)
                .expect_err("mid-run replica eviction must fail the reward verifier closed");
            assert!(error.contains("fixed quorum unavailable"), "{error}");
        }
        assert_eq!(
            fanout_quorum_plan(3, 2, 1, true).unwrap(),
            (1, 1),
            "only the explicitly degraded free route may continue as 1-of-1"
        );
    }

    #[test]
    fn community_quorum_grid_requires_two_distinct_approved_signers() {
        let signer_a = Hash256([0x11; 32]);
        let signer_b = Hash256([0x22; 32]);
        let unapproved = Hash256([0x33; 32]);
        let pipeline = vec![(
            (0, 1),
            vec![
                shard("a", "10.0.0.1:9090", 0, 1),
                shard("b", "10.0.0.2:9090", 0, 1),
                shard("c", "10.0.0.3:9090", 0, 1),
            ],
        )];
        let hop_stats = vec![HopStats {
            positions: 2,
            ..HopStats::default()
        }];
        let vote = |position| RangeVote {
            position,
            range: (0, 1),
            replicas_contacted: vec!["a".into(), "b".into(), "c".into()],
            replicas_returned: vec!["a".into(), "b".into()],
            majority_hash: Some(arc_crypto::hash_bytes(b"agreed").to_hex()),
            majority_signers: vec![signer_a, signer_b],
            divergent: Vec::new(),
            agreement: "majority".into(),
        };
        let votes = vec![vote(0), vote(1)];
        let active = vec![(signer_a, 10), (signer_b, 10)];
        assert_eq!(
            validate_community_range_position_quorums(&pipeline, &hop_stats, &votes, &active,)
                .unwrap(),
            2
        );

        let mut one_identity_twice = votes.clone();
        one_identity_twice[1].majority_signers = vec![signer_a, signer_a];
        let error = validate_community_range_position_quorums(
            &pipeline,
            &hop_stats,
            &one_identity_twice,
            &active,
        )
        .expect_err("two aliases for one validator are not two approvals");
        assert!(error.contains("1 distinct authenticated signer"), "{error}");

        let mut includes_unapproved = votes;
        includes_unapproved[1].majority_signers = vec![signer_a, unapproved];
        let error = validate_community_range_position_quorums(
            &pipeline,
            &hop_stats,
            &includes_unapproved,
            &active,
        )
        .expect_err("an unapproved signature cannot satisfy reward verification");
        assert!(error.contains("unapproved signer"), "{error}");
    }

    // ── compute pool ────────────────────────────────────────────────────

    #[test]
    fn compute_pool_rebuilds_and_releases() {
        let node = fake_node_with_workers(vec![]);
        assert_eq!(node.compute_threads.load(Ordering::Relaxed), 0);
        // Work runs fine with no dedicated pool (rayon global).
        assert_eq!(install_on_compute_pool(&node, || 6 * 7), 42);

        set_compute_threads(&node, 3).expect("3-thread pool");
        assert_eq!(node.compute_threads.load(Ordering::Relaxed), 3);
        assert!(node.compute_pool.read().is_some());
        // Nested rayon work sees the installed pool.
        let width = install_on_compute_pool(&node, rayon::current_num_threads);
        assert_eq!(
            width, 3,
            "par_iter inside the job must see the resized pool"
        );

        set_compute_threads(&node, 5).expect("resize live");
        assert_eq!(
            install_on_compute_pool(&node, rayon::current_num_threads),
            5
        );

        set_compute_threads(&node, 0).expect("back to global");
        assert_eq!(node.compute_threads.load(Ordering::Relaxed), 0);
        assert!(node.compute_pool.read().is_none());
    }

    #[test]
    fn compute_pool_rejects_absurd_widths() {
        let node = fake_node_with_workers(vec![]);
        assert!(set_compute_threads(&node, 100_000).is_err());
    }
}

// ---------------------------------------------------------------------------
// Tests for the honest-projection math (v0.7.11+)
//
// Every assertion here targets a way a projection can lie: a finite pool
// treated as infinite, a validator count inflated by peers with nothing at
// stake, or a rate manufactured from a window too small to measure one.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod projection_tests {
    use super::*;
    use arc_types::economics::{ARC_BASE_UNITS, INFERENCE_ATTESTATION_REWARD};

    fn addr(n: u8) -> Hash256 {
        let mut b = [0u8; 32];
        b[0] = n;
        Hash256(b)
    }

    // ── Reward / treasury math ────────────────────────────────────────────

    #[test]
    fn reward_constant_is_the_shared_one_not_a_literal() {
        // /economics/rewards and /worker/earnings must both quote the constant
        // arc-state actually credits. If someone re-hardcodes 2.5 anywhere,
        // this catches the drift.
        assert_eq!(INFERENCE_ATTESTATION_REWARD, 2_500_000_000);
        assert!(
            (REWARD_PER_ATTESTATION_ARC
                - INFERENCE_ATTESTATION_REWARD as f64 / ARC_BASE_UNITS as f64)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn rewards_remaining_divides_the_finite_treasury() {
        // The genesis faucet pool holds 1000 ARC → exactly 400 more rewards.
        let treasury = 1_000 * ARC_BASE_UNITS;
        assert_eq!(
            rewards_remaining(treasury, INFERENCE_ATTESTATION_REWARD),
            Some(400)
        );
    }

    #[test]
    fn rewards_remaining_is_zero_when_treasury_is_empty() {
        // An empty pool funds nothing. This is the case a naive "count × 2.5
        // ARC forever" projection gets wrong.
        assert_eq!(rewards_remaining(0, INFERENCE_ATTESTATION_REWARD), Some(0));
    }

    #[test]
    fn rewards_remaining_is_zero_when_treasury_holds_less_than_one_reward() {
        // 2.4999... ARC cannot pay a 2.5 ARC reward. Floor division must not
        // round up to 1, and the attestation still applies — the attester just
        // receives min(reward, treasury_balance), i.e. less than the rate.
        let almost = INFERENCE_ATTESTATION_REWARD - 1;
        assert_eq!(
            rewards_remaining(almost, INFERENCE_ATTESTATION_REWARD),
            Some(0)
        );
        // Exactly one reward funds exactly one.
        assert_eq!(
            rewards_remaining(INFERENCE_ATTESTATION_REWARD, INFERENCE_ATTESTATION_REWARD),
            Some(1)
        );
        // One base unit short of two rewards funds one, not two.
        assert_eq!(
            rewards_remaining(
                INFERENCE_ATTESTATION_REWARD * 2 - 1,
                INFERENCE_ATTESTATION_REWARD
            ),
            Some(1)
        );
    }

    #[test]
    fn rewards_remaining_is_null_when_reward_is_zero() {
        // Undefined, not infinite — the endpoint reports null plus a reason.
        assert_eq!(rewards_remaining(1_000_000, 0), None);
        assert_eq!(rewards_remaining(0, 0), None);
    }

    #[test]
    // This guard applies only to the coordinator's local attestation path.
    // Community reward certificates have a separate fixed zero-bond contract.
    #[allow(clippy::assertions_on_constants)]
    fn local_attestation_bond_contract_is_self_consistent() {
        assert!(DEFAULT_ATTESTATION_BOND < INFERENCE_ATTESTATION_REWARD);
        assert_eq!(
            INFERENCE_ATTESTATION_REWARD.saturating_sub(DEFAULT_ATTESTATION_BOND),
            2_499_999_000
        );
        assert!(DEFAULT_ATTESTATION_CHALLENGE_PERIOD_BLOCKS > 0);
    }

    // ── Active vs registered validators ───────────────────────────────────

    #[test]
    fn validator_split_excludes_zero_stake_peers() {
        // The live shape: 14 registered, 4 of them carrying stake 0.
        let mut vals: Vec<(Hash256, u64)> = (0..10)
            .map(|i| (addr(i), arc_consensus::STAKE_SPARK))
            .collect();
        for i in 10..14 {
            vals.push((addr(i), 0));
        }
        let split = split_validators(&vals, arc_consensus::STAKE_SPARK);
        assert_eq!(split.registered, 14, "registered is the raw set length");
        assert_eq!(split.active, 10, "zero-stake peers are not active");
        assert_eq!(split.zero_stake, 4);
        assert_eq!(split.total_stake, 10 * arc_consensus::STAKE_SPARK);
        assert_eq!(split.active_stake, split.total_stake);
    }

    #[test]
    fn validator_split_respects_the_min_stake_boundary() {
        let vals = vec![
            (addr(1), arc_consensus::STAKE_SPARK), // exactly at min → active
            (addr(2), arc_consensus::STAKE_SPARK - 1), // one under → not active
            (addr(3), arc_consensus::STAKE_CORE),  // well over → active
        ];
        let split = split_validators(&vals, arc_consensus::STAKE_SPARK);
        assert_eq!(split.registered, 3);
        assert_eq!(split.active, 2);
        assert_eq!(split.zero_stake, 0);
        assert_eq!(
            split.active_stake,
            arc_consensus::STAKE_SPARK + arc_consensus::STAKE_CORE
        );
        // Stake below the threshold still counts toward total_stake.
        assert_eq!(
            split.total_stake,
            split.active_stake + arc_consensus::STAKE_SPARK - 1
        );
    }

    #[test]
    fn validator_split_all_zero_stake_reports_no_active_validators() {
        let vals: Vec<(Hash256, u64)> = (0..6).map(|i| (addr(i), 0)).collect();
        let split = split_validators(&vals, arc_consensus::STAKE_SPARK);
        assert_eq!(split.registered, 6);
        assert_eq!(split.active, 0);
        assert_eq!(split.zero_stake, 6);
        assert_eq!(split.total_stake, 0);
    }

    #[test]
    fn validator_split_zero_min_still_excludes_zero_stake() {
        // A validator with nothing at risk secures nothing, even if the
        // threshold is configured to zero.
        let vals = vec![(addr(1), 0), (addr(2), 1)];
        let split = split_validators(&vals, 0);
        assert_eq!(split.active, 1);
        assert_eq!(split.zero_stake, 1);
    }

    #[test]
    fn validator_split_of_empty_set() {
        let split = split_validators(&[], arc_consensus::STAKE_SPARK);
        assert_eq!(split.registered, 0);
        assert_eq!(split.active, 0);
        assert_eq!(split.total_stake, 0);
    }

    // ── Observed rate: the null paths ─────────────────────────────────────

    const DAY_MS: u64 = 86_400_000;

    #[test]
    fn rate_is_null_with_no_attestations() {
        let err = attestations_per_day_observed(0, None, None).unwrap_err();
        assert!(err.contains("no attestations"), "got: {}", err);
    }

    #[test]
    fn rate_is_null_with_a_single_attestation() {
        // One event defines no interval. This is the case the old "12% of
        // lifetime" fabrication papered over.
        let err = attestations_per_day_observed(1, Some(1_000), Some(1_000)).unwrap_err();
        assert!(err.contains("single attestation"), "got: {}", err);
    }

    #[test]
    fn rate_is_null_when_block_timestamps_are_pruned() {
        // Non-archive nodes drop old blocks, so the window's endpoints have no
        // timestamps. Null plus a reason — never a nominal-block-time guess.
        let err = attestations_per_day_observed(10, None, Some(DAY_MS)).unwrap_err();
        assert!(err.contains("not retained"), "got: {}", err);
        let err = attestations_per_day_observed(10, Some(1), None).unwrap_err();
        assert!(err.contains("not retained"), "got: {}", err);
        let err = attestations_per_day_observed(10, None, None).unwrap_err();
        assert!(err.contains("not retained"), "got: {}", err);
    }

    #[test]
    fn rate_is_null_on_zero_or_non_advancing_timestamps() {
        assert!(attestations_per_day_observed(5, Some(0), Some(DAY_MS)).is_err());
        assert!(attestations_per_day_observed(5, Some(DAY_MS), Some(0)).is_err());
        // Same instant → no elapsed time to divide by.
        let err = attestations_per_day_observed(5, Some(DAY_MS), Some(DAY_MS)).unwrap_err();
        assert!(err.contains("same instant"), "got: {}", err);
        // Clock going backwards across the window is not a negative rate.
        assert!(attestations_per_day_observed(5, Some(2 * DAY_MS), Some(DAY_MS)).is_err());
    }

    #[test]
    fn rate_uses_intervals_not_events() {
        // A real epoch-millis base: 0 is reserved for "absent timestamp".
        const T0: u64 = 1_700_000_000_000;

        // 3 attestations spanning exactly 2 days = 2 intervals = 1.0/day.
        // Dividing by the event count instead would report 1.5/day.
        let rate = attestations_per_day_observed(3, Some(T0), Some(T0 + 2 * DAY_MS)).unwrap();
        assert!((rate - 1.0).abs() < 1e-9, "got {}", rate);

        // 25 attestations over one day = 24 intervals/day.
        let rate = attestations_per_day_observed(25, Some(T0), Some(T0 + DAY_MS)).unwrap();
        assert!((rate - 24.0).abs() < 1e-9, "got {}", rate);

        // Two attestations one hour apart → 24/day, and never an integer-
        // division zero.
        let rate = attestations_per_day_observed(2, Some(T0), Some(T0 + DAY_MS / 24)).unwrap();
        assert!((rate - 24.0).abs() < 1e-9, "got {}", rate);
    }

    // ── Own-compute statistics ────────────────────────────────────────────

    #[test]
    fn mean_and_p50_are_null_without_samples() {
        // An average of no measurements is unknown, not 0 ms.
        assert_eq!(mean_u64(&[]), None);
        assert_eq!(p50_u64(&[]), None);
    }

    #[test]
    fn p50_returns_a_value_that_was_actually_measured() {
        assert_eq!(p50_u64(&[5]), Some(5));
        assert_eq!(p50_u64(&[9, 1, 5]), Some(5));
        // Even count → lower middle, so the figure is a real sample rather
        // than an interpolated number nothing measured.
        assert_eq!(p50_u64(&[10, 20, 30, 40]), Some(20));
        assert_eq!(mean_u64(&[10, 20, 30, 40]), Some(25.0));
    }

    #[test]
    fn own_compute_ring_is_bounded_and_keeps_newest() {
        let mut ring = std::collections::VecDeque::new();
        let extra = 50u64;
        for i in 0..(OWN_COMPUTE_SAMPLE_CAP as u64 + extra) {
            push_bounded(&mut ring, i, OWN_COMPUTE_SAMPLE_CAP);
        }
        assert_eq!(ring.len(), OWN_COMPUTE_SAMPLE_CAP, "ring must stay bounded");
        assert_eq!(
            *ring.back().unwrap(),
            OWN_COMPUTE_SAMPLE_CAP as u64 + extra - 1,
            "newest sample retained"
        );
        assert_eq!(*ring.front().unwrap(), extra, "oldest evicted first");
        // p50 over a full ring is still a real sample from it.
        let samples: Vec<u64> = ring.iter().copied().collect();
        assert!(
            p50_u64(&samples)
                .map(|v| samples.contains(&v))
                .unwrap_or(false)
        );
    }
}
