use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareInfo {
    pub platform: String,
    pub arch: String,
    pub cpu_model: String,
    pub cpu_cores: u32,
    pub ram_gb: u64,
    pub gpu_name: Option<String>,
    pub gpu_vram_gb: Option<u64>,
    pub recommended_model: String,
    pub recommended_role: String,
    pub estimated_daily_arc: f64,
}

/// On-disk identity. Carries the BIP-39 phrase because the phrase is what
/// derives the validator keypair arc-node is launched with.
///
/// This type must NOT cross the IPC boundary — see [`IdentityPublic`]. The
/// WebView is a browser: anything handed to it can be read by DevTools, by
/// any script that gets injected, and (once the frontend persisted it) by
/// anything that can read the WebView profile directory. The phrase stays
/// Rust-side and is handed out exactly once, on explicit request, by
/// `commands::reveal_seed_phrase`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub address: String,
    pub public_key: String,
    pub seed_phrase: String,
    pub created_at: i64,
}

/// The half of [`Identity`] that is safe to show the UI. Everything the
/// frontend actually renders — the address, the public key, the creation
/// date — with the signing material left behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityPublic {
    pub address: String,
    pub public_key: String,
    pub created_at: i64,
}

impl From<&Identity> for IdentityPublic {
    fn from(id: &Identity) -> Self {
        Self {
            address: id.address.clone(),
            public_key: id.public_key.clone(),
            created_at: id.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfig {
    pub role: String,
    pub model_path: Option<String>,
    pub rpc_port: u16,
    pub p2p_port: u16,
    pub auto_start: bool,
    pub auto_update: bool,
    pub data_dir: String,
    /// How many CPU cores the node may use for parallel work (rayon's global
    /// pool). `None` = let rayon size itself, which means every logical core.
    /// Surfaced as the Settings "Compute contribution" slider.
    ///
    /// `#[serde(default)]` so a `store.json` written by an older build still
    /// deserializes instead of resetting the whole config to defaults.
    #[serde(default)]
    pub worker_threads: Option<u32>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        // Default to port 9090 - matches the community installer script
        // (`arc-node --rpc 0.0.0.0:9090`) so the app auto-detects an
        // already-running community node on first launch.
        Self {
            role: "worker".into(),
            model_path: None,
            rpc_port: 9090,
            p2p_port: 9091,
            auto_start: true,
            auto_update: true,
            data_dir: "~/.arc".into(),
            worker_threads: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub running: bool,
    pub pid: Option<u32>,
    /// `live` (local peers ≥ 1), `lite` (local has no peers but a public
    /// coordinator's /health responded — the user is fully usable via
    /// HTTPS RPC even if their network blocks UDP P2P), `syncing` (local
    /// is up but no peers and no coordinator yet), `offline` (neither).
    pub health: String,
    pub version: String,
    pub peers: u32,
    pub round: u64,
    pub committed: u64,
    pub height: u64,
    pub uptime_seconds: u64,
    pub address: Option<String>,
    pub rpc_port: u16,
    pub last_error: Option<String>,
    /// HTTPS RPC origin of a reachable public seed coordinator (e.g.
    /// `http://140.82.16.112:9090`). Set whenever any `COORDINATOR_HOSTS`
    /// entry returned 200 on `/health` during the last poll. Lets the UI
    /// show "Lite mode (via NYC)" instead of a hard "offline" when local
    /// P2P fails — common on residential ISPs that drop outbound UDP on
    /// non-standard ports.
    #[serde(default)]
    pub coordinator_url: Option<String>,

    // ── Network-wide numbers ────────────────────────────────────────────
    // Everything above this line describes the user's OWN node. The three
    // fields below describe the public chain, read from whichever seed is
    // currently freshest. They are deliberately separate: rendering a
    // datacenter's block height in a tile labelled "your node" is the exact
    // dishonesty this split exists to prevent.
    /// Origin of the seed these chain numbers came from, for attribution in
    /// the UI ("Network · via LAX"). `None` when no seed answered.
    #[serde(default)]
    pub chain_host: Option<String>,
    #[serde(default)]
    pub chain_height: Option<u64>,
    #[serde(default)]
    pub chain_round: Option<u64>,
    /// Age in seconds of the freshest block the chosen seed knows about.
    /// Large values mean block production has stalled network-wide, which
    /// is worth showing rather than hiding behind a green "Live" pill.
    #[serde(default)]
    pub chain_block_age_seconds: Option<u64>,

    /// Cores the running node was launched with (`worker_threads` from the
    /// config that started it). `None` = unconstrained / not started by us.
    #[serde(default)]
    pub worker_threads: Option<u32>,
    /// Logical cores on this machine — the upper bound for the Settings
    /// slider, carried here so the Dashboard can render "6 of 12 cores"
    /// without a second IPC round trip.
    #[serde(default)]
    pub cpu_cores: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Earnings {
    pub total_arc: f64,
    /// Earned since 00:00 UTC. `None` when the chain does not report it —
    /// which is not the same as zero, and must not be rendered as "0.00".
    pub today_arc: Option<f64>,
    /// Submitted but not yet released. `None` until the chain exposes the
    /// distinction; previously this was invented client-side.
    pub pending_arc: Option<f64>,
    pub rank: Option<u32>,
    pub attestations: u64,
    /// Epoch millis of the last payout. Only ever a real timestamp.
    pub last_payout_at: Option<i64>,
    /// Block height of the last attestation. Kept apart from
    /// `last_payout_at` because feeding a block height (~123,462) into a
    /// relative-time formatter renders "20770d ago".
    pub last_payout_block: Option<u64>,
    /// True when these numbers came from the chain's `/worker/earnings`
    /// endpoint. False means they were synthesized locally and should be
    /// labelled as an estimate.
    pub from_chain: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attestation {
    pub tx_hash: String,
    pub input_preview: String,
    pub output_hash: String,
    pub model_hash: String,
    /// `None` when the record carries no token count, so the UI can omit
    /// the meta line instead of printing a confident "0 tokens".
    pub tokens: Option<u32>,
    pub latency_ms: Option<u32>,
    /// Only set for attestations credited to THIS user. Showing another
    /// validator's work as "+2.50" in the user's own earnings feed is the
    /// single most misleading thing this screen could do.
    pub reward_arc: Option<f64>,
    /// Real epoch millis when the record carries one. `None` means "recent,
    /// exact time unknown" — previously this was a fabricated
    /// `now - i * 30s` series that looked like real telemetry.
    pub timestamp: Option<i64>,
    pub block_height: Option<u64>,
    /// Submitting address, when present.
    pub from: Option<String>,
    /// `from` matches the user's address.
    pub mine: bool,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub id: String,
    pub timestamp: i64,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStats {
    pub total_nodes: u64,
    pub total_inferences: u64,
    pub avg_tps: u64,
    pub latest_block: u64,
}

// `UpdateCheck` was removed along with the `check_for_update` command it
// served. Update state now comes solely from the Tauri updater plugin, which
// reads the signed release manifest — the only source that can actually
// install anything.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryStatus {
    /// Absolute path to the resolved / downloaded arc-node binary.
    pub path: String,
    /// Bytes downloaded this call (0 if already installed).
    pub downloaded_bytes: u64,
    /// Total bytes announced by the server (0 if unknown).
    pub total_bytes: u64,
    /// True if the binary was already on disk when this was called.
    pub already_installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountBalance {
    pub address: String,
    pub balance: u64,
    pub nonce: u64,
    pub staked_balance: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaucetResult {
    pub tx_hash: String,
    pub amount: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceConsensus {
    pub k: u32,
    pub votes_total: u32,
    pub unanimous: u32,
    pub majority: u32,
    pub split: u32,
    pub divergent_replica_count: u32,
}

/// One shard in the pipeline that served an inference. The chain returns
/// these as `shard_trace`; surfacing them is what turns "the network
/// answered" into "these six machines each ran their slice of the model".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceHop {
    pub hop: u32,
    /// Human label for the machine that ran this slice.
    pub node: String,
    /// Layer range, e.g. `"0..6"`.
    pub layers: String,
    pub compute_ms: u64,
    pub wall_ms: u64,
    pub is_terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceResult {
    pub input: String,
    pub output: String,
    pub output_hash: String,
    pub model_hash: String,
    pub tokens_generated: u32,
    pub inference_ms: u32,
    pub tx_hash: String,
    pub deterministic: bool,
    pub engine: String,
    pub explorer_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus: Option<InferenceConsensus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<String>,
    /// Per-shard pipeline trace, when the serving node reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<Vec<InferenceHop>>,
    /// True when the request was served by the arc-node on this machine
    /// rather than a remote seed. Lets the UI say so plainly.
    #[serde(default)]
    pub served_locally: bool,
}

/// Outcome of a compute-contribution change. The chain is growing a
/// `POST /node/threads` endpoint that reconfigures rayon's pool in place;
/// until every node has it, a 404 means we fall back to a node restart.
/// `restarted` tells the UI which of the two happened so it can say
/// "applied live" versus "restarted with 6 cores".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadsApplied {
    pub worker_threads: u32,
    pub restarted: bool,
    pub message: String,
}

/// Where `save_logs` wrote the log file, or `None` if the user cancelled
/// the save dialog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedLogs {
    pub path: Option<String>,
    pub lines: usize,
}

/// One entry in the desktop's model-tier picker. The frontend renders this
/// in onboarding (and the Settings → "Switch model" flow) so the user can
/// pick what to download. `size_bytes` is the canonical HF-reported size, used
/// both for the human-readable size label ("~4.1 GB") and for the
/// already-downloaded check (`existing_model_for_tier`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTierInfo {
    pub id: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub url: String,
}

/// Streamed progress event for an in-flight model download. Frontend listens
/// on the `model-download-progress` Tauri channel and renders a progress bar
/// from `downloaded_bytes / total_bytes`. `done = true` is the terminal event
/// (file fully written + atomically renamed into place).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDownloadProgress {
    pub tier: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub done: bool,
}

/// Result of `reset_peer_state` — wipes `<data_dir>/known_peers.json`
/// and restarts the node so it bootstraps from the bundled testnet
/// seed list. The dashboard surfaces `message` as a toast and
/// `was_present` to distinguish "we cleaned up stale state" from "the
/// problem wasn't a stale peer cache."
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetPeerStateResult {
    pub removed_path: String,
    pub was_present: bool,
    pub message: String,
}

/// Milestone B (#36): paid-inference response - carries the InferenceResult
/// fields plus the on-chain receipts (open tx hash, release tx hash, the
/// payer's address, the max_fee that was escrowed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaidInferenceResult {
    pub input: String,
    pub output: String,
    pub output_hash: String,
    pub tokens_generated: u32,
    pub inference_ms: u32,
    pub coordinator: String,
    pub consensus: InferenceConsensus,
    pub payer_address: String,
    pub max_fee: u64,
    pub open_tx_hash: String,
    pub release_tx_hash: String,
}
