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
            // Keep protocol-v3 state separate from the managed binary, models,
            // and any v0.7 WAL that older desktop builds wrote in ~/.arc.
            data_dir: "~/.arc/data-v3".into(),
            worker_threads: None,
        }
    }
}

/// Persistent, dismissible evidence that the desktop fenced chain data that
/// cannot be safely replayed after the updater/relaunch boundary (an unbound
/// v0.7 WAL or a malformed v3 binding). The old path is never deleted or
/// rewritten; only NodeConfig::data_dir is switched to a fresh v3 child before
/// arc-node can auto-start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataMigrationNotice {
    pub legacy_data_dir: String,
    pub active_data_dir: String,
    pub migrated_at: i64,
    pub reason: String,
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
    /// `https://140.82.16.112`). Set whenever any `COORDINATOR_HOSTS`
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
pub struct ConfirmedRewardReceipt {
    pub tx_hash: String,
    pub job_id: String,
    pub block_height: u64,
    pub block_hash: String,
    pub reward_base: u64,
    pub reward_arc: f64,
    pub recovery_epoch: Option<u64>,
    pub validator_set_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Earnings {
    /// Gross rewards visible in the selected host's current retained receipt
    /// window. This is deliberately not a lifetime ledger: a non-archive host
    /// can prune old rows and restart with an empty in-memory index.
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
    /// Auditable rows that alone contribute to `total_arc`.
    pub confirmed_receipts: Vec<ConfirmedRewardReceipt>,
    /// Server projection is present only when an active explicit reward
    /// policy, confirmed-receipt rate, and funded treasury all exist.
    pub projected_daily_arc: Option<f64>,
    pub projected_daily_unavailable_reason: Option<String>,
    pub recovery_epoch: Option<u64>,
    pub validator_set_id: Option<u64>,
    /// Exact reason the selected host's receipt index could not be used.
    /// `None` for both a positive result and a confirmed zero; populated only
    /// when `from_chain` is false so an outage or malformed response never
    /// becomes "you earned zero."
    pub unavailable_reason: Option<String>,
    /// Backend-declared source of the retained receipt window.
    pub receipt_source: Option<String>,
    /// Whether the selected host reports archival state.
    pub archive_mode: Option<bool>,
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
    /// Real epoch millis when the record carries one. `None` means "recent,
    /// exact time unknown" — previously this was a fabricated
    /// `now - i * 30s` series that looked like real telemetry.
    pub timestamp: Option<i64>,
    pub block_height: Option<u64>,
    /// Transaction type as the host labelled it.
    ///
    /// Current builds emit `"Inference"` on every row. Older deployed seeds
    /// padded `/inference/attestations` with unrelated transactions tagged
    /// `"Other"` once real rows ran out — at `limit=500` some seeds returned
    /// 500 padding rows and zero real ones. The Network screen filters on this
    /// so a chain view is not half transfers presented as inference evidence.
    pub tx_type: Option<String>,
    /// Protocol-v3 discriminator from the typed inference activity contract.
    pub record_kind: Option<String>,
    /// Successful computation evidence (0x16 or 0x25).
    pub computed: bool,
    /// Successful mined 0x25 payment evidence only.
    pub paid: bool,
    /// Receipt-backed earning evidence only; never inferred from raw 0x16.
    pub earned: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInstallPolicy {
    /// True only when Tauri can safely replace this distribution in place.
    pub can_install: bool,
    /// `appimage`, `native`, or `package-manager`.
    pub channel: String,
    /// User-facing next step when in-app installation is unavailable.
    pub instructions: String,
}

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
    /// Exact base-unit integer. Serialized as a string so values above
    /// JavaScript's 53-bit safe-integer ceiling cannot be rounded in IPC.
    pub balance_base: String,
    /// Exact 9-decimal ARC representation derived from `balance_base`.
    pub balance_arc: String,
    pub nonce: u64,
    pub staked_balance_base: String,
    pub staked_balance_arc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletTxResult {
    pub tx_hash: String,
    /// Exact base units submitted, kept as a string across IPC.
    pub amount_base: String,
    /// Exact human ARC amount (at most nine fractional digits).
    pub amount_arc: String,
    /// `pending`, `mined_success`, `mined_failed`, or `receipt_unavailable`.
    pub receipt_status: String,
    /// True only when `GET /tx/{hash}` returned a mined receipt.
    pub mined: bool,
    /// The mined receipt's execution result; absent until a receipt exists.
    pub success: Option<bool>,
    pub block_height: Option<u64>,
    pub block_hash: Option<String>,
    pub source_host: String,
    pub unavailable: Option<String>,
    pub message: String,
}

pub type FaucetResult = WalletTxResult;

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

/// Reward-settlement state returned alongside a community-routed inference.
/// A transaction hash here is a CommunityInferenceReward (0x25), not the
/// unpaid InferenceAttestation (0x16) carried by `InferenceResult::tx_hash`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceSettlement {
    pub status: String,
    #[serde(default)]
    pub tx_type: String,
    #[serde(default)]
    pub tx_hash: String,
    #[serde(default)]
    pub job_id: String,
    #[serde(default)]
    pub submitted: bool,
    #[serde(default)]
    pub included: bool,
    #[serde(default)]
    pub confirmed: bool,
    #[serde(default)]
    pub reward_arc: Option<f64>,
    #[serde(default)]
    pub receipt_url: String,
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
    /// True only when every shard hop was selected under one exact execution
    /// profile identity.
    #[serde(default)]
    pub profile_bound: bool,
    /// True only when the serving coordinator reports a complete authenticated
    /// same-profile quorum grid for the generated positions.
    #[serde(default)]
    pub quorum_verified: bool,
    #[serde(default)]
    pub execution_profile: String,
    pub engine: String,
    pub explorer_url: String,
    /// Exact server routing declaration (`community:<worker>`, `local`, ...).
    #[serde(default)]
    pub routed_via: String,
    /// Present only when the server reported community reward settlement.
    /// Pending submission is deliberately distinct from a confirmed receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement: Option<InferenceSettlement>,
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

/// Legacy paid-inference response shape, retained for IPC compatibility.
/// The v0.8.0 recovery candidate rejects new escrow writes before signing.
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

// ── Chain-visibility + projection types (v0.8.0) ─────────────────────────
//
// Every struct below carries `unavailable: Option<String>` and
// `source_host: String`.
//
// `unavailable` is a human-readable reason the data could not be read: a 404
// from a seed predating the endpoint, a connection failure, an unrecognised
// shape. When it is set the numeric fields are `None`, which the UI renders as
// a stated reason rather than a figure. It is never zero — a host that cannot
// answer is not a host reporting zero, and the difference is the whole point.
//
// `source_host` is the pinned chain host (CLAUDE.md rule 4: chain reads stay
// on ONE elected seed for the session). It is shown next to anything derived
// from it, so no number in the UI is unattributable.

/// `GET /economics/rewards` — the finite testnet reward treasury.
///
/// The ceiling is the reason this type exists. A per-day earnings projection
/// with no stated ceiling implies an unlimited payout, which is the dishonest
/// version of a projection.
///
/// Two field names on the wire are easy to misread, and both were misread once:
///
/// - **`rewards_remaining` is a COUNT of fundable reward receipts, not an ARC
///   amount.** It is the treasury balance divided by the per-receipt reward.
///   Rendering it as currency is wrong by nine orders of magnitude *and* wrong
///   in kind.
///   It is carried here as [`Self::attestations_remaining`] so the name cannot
///   be confused at the call site.
/// - The treasury balance is `treasury_balance_arc` / `treasury_balance_base`.
///   There is no `treasury_total` or `rewards_paid`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardEconomics {
    pub source_host: String,
    pub unavailable: Option<String>,
    /// ARC paid for one successful mined community-reward receipt.
    pub reward_per_attestation: Option<f64>,
    /// ARC left in the reward treasury.
    pub treasury_balance_arc: Option<f64>,
    /// Why the treasury balance is absent, in the host's own words.
    pub treasury_balance_unavailable_reason: Option<String>,
    /// How many MORE successful reward receipts the treasury can still fund.
    ///
    /// A count, not currency — see the type docs. This is the honest form of
    /// "how much is left": it is denominated in the thing a worker actually
    /// produces.
    pub attestations_remaining: Option<u64>,
    pub attestations_remaining_unavailable_reason: Option<String>,
    /// The host states outright that the treasury is bounded.
    pub treasury_is_finite: Option<bool>,
    /// ARC bonded by a community worker reward certificate. The v1
    /// certificate contract reports zero; this deliberately does not expose
    /// the unrelated bond used by the coordinator's local attestation path.
    pub bond_per_attestation: Option<f64>,
    /// Reserved for a future community-certificate challenge period.
    pub challenge_period_blocks: Option<u64>,
    /// Reserved for a future community-certificate bond refund contract.
    pub bond_refunded_after_challenge_period: Option<bool>,
    /// Where the money comes from, in the host's own words. Used to label the
    /// funding rather than inventing a description of it.
    pub funding_detail: Option<String>,
}

/// `GET /worker/earnings/{addr}` — the inputs a projection needs.
///
/// Kept apart from [`Earnings`] (the selected host's retained receipt window)
/// because a projection has a different honesty burden: it is the only number
/// in the app describing something that has not happened yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EarningsProjection {
    pub source_host: String,
    pub unavailable: Option<String>,
    /// ARC per successful mined community-reward receipt.
    pub reward_per_attestation: Option<f64>,
    /// `"chain"` = the host reported the rate; `"constant"` = the named local
    /// constant; `"unknown"` = neither. Never `"assumed"`.
    pub reward_rate_source: String,
    /// Exact rollout gate reported by the selected coordinator. Only `Some(true)`
    /// permits the UI to show a forward-looking community reward projection.
    pub community_rewards_enabled: Option<bool>,
    /// Backend-authoritative forecast after readiness/budget/treasury policy.
    pub projected_daily_arc: Option<f64>,
    /// Populated exactly when `projected_daily_arc` is unavailable.
    pub projected_daily_unavailable_reason: Option<String>,
    /// Consensus-sealed promotional issuance policy commitment.
    pub reward_policy_hash: Option<String>,
    pub reward_budget_epoch: Option<u64>,
    pub rewards_remaining_this_epoch: Option<u64>,
    pub worker_rewards_remaining_this_epoch: Option<u64>,
    pub coordinator_rewards_remaining_this_epoch: Option<u64>,
    pub issuance_ready_for_worker: Option<bool>,
    pub reward_program: Option<String>,
    pub reward_is_customer_demand: Option<bool>,
    pub attestations_total: u64,
    pub first_attestation_block: Option<u64>,
    /// Reward receipts per day, MEASURED over this address's retained history.
    ///
    /// `None` with `rate_unavailable_reason` set whenever there is no history
    /// to measure — the common case. Never extrapolated from zero: an account
    /// with no retained receipts has no rate, not a rate of zero.
    pub attestations_per_day: Option<f64>,
    pub rate_unavailable_reason: Option<String>,
    /// Blocks the rate was observed across (`blocks_observed` on the wire).
    /// Named in the assumptions line so the rate can be judged.
    pub observed_over_blocks: Option<u64>,
    /// The host's own caveat about how the rate was derived, shown verbatim.
    pub rate_caveat: Option<String>,
}

/// What this machine is actually contributing, as opposed to what the slider
/// is set to. Read from the LOCAL node, not a seed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeContribution {
    pub source_host: String,
    pub unavailable: Option<String>,
    /// `"contribution"` = the dedicated endpoint answered; `"composed"` =
    /// built from `/node/threads` + `/stats`; `"none"` = neither answered.
    pub source: String,
    /// Threads the node is actually working with (`threads.in_use`).
    pub threads_in_use: Option<u32>,
    /// Logical cores the node can see (`threads.available_parallelism`).
    pub threads_available: Option<u32>,
    /// Layer ranges rendered for display, e.g. "0..6, 12..18".
    pub layers_held: Option<String>,
    /// Distinct layers held — a UNION, not a sum over replicas.
    pub layer_count: Option<u32>,
    /// Layers in the whole model, for "6 of 32".
    pub total_layers: Option<u32>,
    /// Real sharded pipeline walks served. Deliberately NOT summed with cache
    /// hits — the node counts those separately and a cache hit is not work.
    pub runs_served: Option<u64>,
    pub cache_hits: Option<u64>,
    /// Measured mean of this node's OWN compute per hop
    /// (`own_compute_ms.mean_ms`). `None` = never measured.
    pub hop_ms_mean: Option<f64>,
    /// How many samples the mean above rests on. A mean over 2 samples and a
    /// mean over 200 are different claims.
    pub hop_samples: Option<u64>,
    /// The host's reason for having no timing, shown verbatim.
    pub hop_unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorInfo {
    pub address: String,
    pub stake: u64,
    /// Stake > 0. Zero-stake entries are still counted by `/health`, which is
    /// what inflates the displayed validator set.
    pub active: bool,
}

/// The Network screen's chain view, entirely from the ONE pinned host.
///
/// Nothing here reads a second host. The seeds are independent chains with
/// different hashes at the same height, so a side-by-side would present a
/// structural disagreement as if it were a fault.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkOverview {
    pub source_host: String,
    pub unavailable: Option<String>,
    /// Network name as `/network/info` reports it.
    ///
    /// `None` when that endpoint is absent, and then the UI says the name is
    /// unknown. `/info` is NOT used as a substitute: its `chain` field is the
    /// hardcoded string "ARC Chain" on every deployment, so it cannot tell a
    /// testnet from a mainnet and would be a fabricated answer.
    pub network_name: Option<String>,
    /// The host's reason for not naming its network, shown verbatim.
    pub network_name_unavailable_reason: Option<String>,
    pub chain_id: Option<String>,
    /// Whether the host's genesis DECLARES itself mainnet.
    ///
    /// `None` means the host did not say, and the UI then says nothing about
    /// mainnet either. This is the only input allowed to make the app describe
    /// a network as mainnet; `/info`'s `chain` field is the constant string
    /// "ARC Chain" everywhere and cannot distinguish one network from another.
    pub declares_mainnet: Option<bool>,
    /// The host's own verdict on whether it is producing blocks, with the basis
    /// it used. Shown alongside the app's own block-age reading rather than
    /// replacing it, because the two answer slightly different questions.
    pub is_block_producing: Option<bool>,
    pub is_block_producing_basis: Option<String>,
    /// arc-node version the pinned host runs.
    pub host_version: Option<String>,
    pub height: Option<u64>,
    /// Age of the newest block this host knows about — the one number that
    /// separates a live chain from a stalled one. `/health` reports `ok`
    /// either way, because DAG rounds keep advancing after blocks stop.
    pub last_block_age_secs: Option<u64>,
    pub dag_round: Option<u64>,
    pub dag_committed: Option<u64>,
    pub peers: Option<u32>,
    /// Validators that can actually lead a round.
    ///
    /// Taken from `/network/info` when it answers, which applies the real
    /// `min_active_stake` threshold. Otherwise DERIVED by counting stake > 0
    /// from `/validators`, which is an approximation of the same thing —
    /// [`Self::validator_split_derived`] says which happened.
    pub validators_active: Option<u32>,
    /// Every validator in the set, zero-stake entries included.
    pub validators_registered: Option<u32>,
    /// Minimum stake for an active validator, when the host reports it.
    pub min_active_stake: Option<u64>,
    /// True when the active/registered split was counted locally rather than
    /// reported. The UI says so, so a threshold mismatch is never passed off as
    /// the host's own number.
    pub validator_split_derived: bool,
    pub validators: Vec<ValidatorInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockSummary {
    pub height: u64,
    pub hash: String,
    pub timestamp_ms: Option<u64>,
    pub tx_count: Option<u32>,
    pub proposer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentBlocks {
    pub source_host: String,
    pub unavailable: Option<String>,
    pub blocks: Vec<BlockSummary>,
}

/// A `GET /tx/{hash}` result.
///
/// Deliberately narrow: the endpoint returns a `TxReceipt`, which carries no
/// `tx_type` and no `from` (those live on `/tx/{hash}/full`). Rather than
/// fetch two endpoints and risk rendering half a record, this reports what a
/// receipt actually proves — in a block, at what height, and whether it
/// succeeded.
///
/// `status` is one of:
/// - `"mined"` — a receipt exists.
/// - `"not_found"` — HTTP 404. This is ALSO exactly what a pending
///   attestation looks like, because `/tx/{hash}` is a receipt lookup and a
///   mempool tx has no receipt. Rendered "not in a block yet", never
///   "invalid".
/// - `"invalid_hash"` — HTTP 400, not 64 hex chars. A genuinely different
///   answer from `not_found`, and worth saying: this one really is a bad
///   paste, so the user isn't left waiting for a tx nobody submitted.
/// - `"error"` — the lookup itself failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxLookup {
    pub source_host: String,
    pub unavailable: Option<String>,
    pub hash: String,
    pub status: String,
    pub block_height: Option<u64>,
    pub block_hash: Option<String>,
    pub tx_index: Option<u32>,
    pub success: Option<bool>,
    pub gas_used: Option<u64>,
}

/// One transaction inside a block, as `GET /block/{h}/txs` reports it.
///
/// Only `index` and `hash` are guaranteed. Normal blocks return exactly those
/// two; the extra fields appear only on reconstructed benchmark blocks, so
/// they stay optional rather than being defaulted into existence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockTx {
    pub index: u32,
    pub hash: String,
    pub tx_type: Option<String>,
    pub from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockTxs {
    pub source_host: String,
    pub unavailable: Option<String>,
    pub height: u64,
    /// Total in the block, which can exceed `txs.len()` when paginated.
    pub tx_count: Option<u32>,
    pub txs: Vec<BlockTx>,
}
