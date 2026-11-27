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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub address: String,
    pub public_key: String,
    pub seed_phrase: String,
    pub created_at: i64,
}

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
    #[serde(default)]
    pub auto_install_updates: bool,
    pub data_dir: String,
    #[serde(default)]
    pub worker_threads: Option<u32>,
    #[serde(default)]
    pub compute_consent: Option<bool>,
    #[serde(default)]
    pub prevent_sleep_during_jobs: Option<bool>,
    #[serde(default)]
    pub keep_awake_while_sharing: Option<bool>,
    #[serde(default)]
    pub compute_default_notice_pending: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            role: "worker".into(),
            model_path: None,
            rpc_port: 9090,
            p2p_port: 9091,
            auto_start: true,
            auto_update: true,
            auto_install_updates: true,
            data_dir: "~/.arc/data-v3".into(),
            worker_threads: None,
            compute_consent: None,
            prevent_sleep_during_jobs: None,
            keep_awake_while_sharing: None,
            compute_default_notice_pending: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataMigrationNotice {
    pub legacy_data_dir: String,
    pub active_data_dir: String,
    pub migrated_at: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviousInstallNotice {
    pub set_aside_data_dir: String,
    pub active_data_dir: String,
    pub set_aside_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialShardRangeStatus {
    pub start_layer: u64,
    pub end_layer: u64,
    pub total_layers: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialShardWorkerStatus {
    pub phase: Option<String>,
    pub model_name: Option<String>,
    pub model_id: Option<String>,
    pub lease_seq: Option<u64>,
    pub lease_epoch: Option<u64>,
    pub assigned_range: Option<PartialShardRangeStatus>,
    pub ready_expires_at_unix_ms: Option<u64>,
    pub lease_live: bool,
    pub steps_executed: Option<u64>,
    pub steps_submitted: Option<u64>,
    pub submit_retries: Option<u64>,
    pub submit_stale_conflicts: Option<u64>,
    pub cancellations_applied: Option<u64>,
    pub rebalances_completed: Option<u64>,
    pub reconnect_attempts: Option<u64>,
    pub last_heartbeat_unix_ms: Option<u64>,
    pub last_step_submitted_unix_ms: Option<u64>,
    pub last_error: Option<String>,
    pub rewards_enabled: bool,
    pub reward_eligible: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub health: String,
    pub version: String,
    pub peers: u32,
    #[serde(default)]
    pub chain_participation_enabled: Option<bool>,
    pub round: u64,
    pub committed: u64,
    pub height: u64,
    pub uptime_seconds: u64,
    pub address: Option<String>,
    pub rpc_port: u16,
    pub last_error: Option<String>,
    #[serde(default)]
    pub coordinator_url: Option<String>,

    #[serde(default)]
    pub chain_host: Option<String>,
    #[serde(default)]
    pub chain_height: Option<u64>,
    #[serde(default)]
    pub chain_round: Option<u64>,
    #[serde(default)]
    pub chain_block_age_seconds: Option<u64>,

    #[serde(default)]
    pub worker_threads: Option<u32>,
    #[serde(default)]
    pub cpu_cores: Option<u32>,
    #[serde(default)]
    pub model_unavailable: Option<ModelUnavailableReason>,
    #[serde(default)]
    pub crash_retry_in_secs: Option<u64>,
    #[serde(default)]
    pub model_download_wait: Option<ModelDownloadWait>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "reason",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ModelDownloadWait {
    OnBattery,
    MeteredNetwork,
    DiskFull { needed_bytes: u64 },
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelUnavailableReason {
    NotFound,
    Unreadable,
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
    pub receipt_url: String,
    pub recovery_epoch: Option<u64>,
    pub validator_set_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Earnings {
    pub total_arc: f64,
    pub today_arc: Option<f64>,
    pub pending_arc: Option<f64>,
    pub rank: Option<u32>,
    pub attestations: u64,
    pub last_payout_at: Option<i64>,
    pub last_payout_block: Option<u64>,
    pub confirmed_receipts: Vec<ConfirmedRewardReceipt>,
    pub projected_daily_arc: Option<f64>,
    pub projected_daily_unavailable_reason: Option<String>,
    pub recovery_epoch: Option<u64>,
    pub validator_set_id: Option<u64>,
    pub unavailable_reason: Option<String>,
    pub receipt_source: Option<String>,
    pub archive_mode: Option<bool>,
    pub history_complete_since_recovery: Option<bool>,
    pub history_scope: Option<String>,
    pub from_chain: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attestation {
    pub tx_hash: String,
    pub input_preview: String,
    pub output_hash: String,
    pub model_hash: String,
    pub tokens: Option<u32>,
    pub latency_ms: Option<u32>,
    pub timestamp: Option<i64>,
    pub block_height: Option<u64>,
    pub tx_type: Option<String>,
    pub record_kind: Option<String>,
    pub computed: bool,
    pub paid: bool,
    pub earned: bool,
    pub from: Option<String>,
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
    pub latest_block: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInstallPolicy {
    pub can_install: bool,
    pub channel: String,
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryStatus {
    pub path: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub already_installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountBalance {
    pub address: String,
    pub balance_base: String,
    pub balance_arc: String,
    pub nonce: u64,
    pub staked_balance_base: String,
    pub staked_balance_arc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletTxResult {
    pub tx_hash: String,
    pub amount_base: String,
    pub amount_arc: String,
    pub receipt_status: String,
    pub mined: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceHop {
    pub hop: u32,
    pub node: String,
    pub layers: String,
    pub compute_ms: u64,
    pub wall_ms: u64,
    pub is_terminal: bool,
}

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
    pub worker: String,
    #[serde(default)]
    pub submitted: bool,
    #[serde(default)]
    pub included: bool,
    #[serde(default)]
    pub confirmed: bool,
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub block_height: Option<u64>,
    #[serde(default)]
    pub block_hash: Option<String>,
    #[serde(default)]
    pub index: Option<u64>,
    #[serde(default)]
    pub reward_base: Option<u64>,
    #[serde(default)]
    pub reward_arc: Option<f64>,
    #[serde(default)]
    pub receipt_url: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub input_hash: String,
    #[serde(default)]
    pub output_hash: String,
    #[serde(default)]
    pub assignment_epoch: String,
    #[serde(default)]
    pub transaction_domain: String,
    #[serde(default)]
    pub recovery_epoch: Option<u64>,
    #[serde(default)]
    pub validator_set_id: Option<u64>,
    #[serde(default)]
    pub validator_set_commitment: String,
    #[serde(default)]
    pub validator_approvals: Option<u64>,
    #[serde(default)]
    pub evidence_source: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation_status: Option<String>,
    #[serde(default)]
    pub attestation_hash: String,
    pub tx_hash: String,
    pub deterministic: bool,
    #[serde(default)]
    pub profile_bound: bool,
    #[serde(default)]
    pub quorum_verified: bool,
    #[serde(default)]
    pub execution_profile: String,
    pub engine: String,
    pub explorer_url: String,
    #[serde(default)]
    pub routed_via: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement: Option<InferenceSettlement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus: Option<InferenceConsensus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<Vec<InferenceHop>>,
    #[serde(default)]
    pub served_locally: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadsApplied {
    pub worker_threads: u32,
    pub restarted: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedLogs {
    pub path: Option<String>,
    pub lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTierInfo {
    pub id: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDownloadProgress {
    pub tier: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub done: bool,
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub resumed_from_bytes: u64,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub retry_in_secs: Option<u64>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetPeerStateResult {
    pub removed_path: String,
    pub was_present: bool,
    pub message: String,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardEconomics {
    pub source_host: String,
    pub unavailable: Option<String>,
    pub reward_per_attestation: Option<f64>,
    pub treasury_balance_arc: Option<f64>,
    pub treasury_balance_unavailable_reason: Option<String>,
    pub attestations_remaining: Option<u64>,
    pub attestations_remaining_unavailable_reason: Option<String>,
    pub treasury_is_finite: Option<bool>,
    pub bond_per_attestation: Option<f64>,
    pub challenge_period_blocks: Option<u64>,
    pub bond_refunded_after_challenge_period: Option<bool>,
    pub funding_detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EarningsProjection {
    pub source_host: String,
    pub unavailable: Option<String>,
    pub reward_per_attestation: Option<f64>,
    pub reward_rate_source: String,
    pub community_rewards_enabled: Option<bool>,
    pub projected_daily_arc: Option<f64>,
    pub projected_daily_unavailable_reason: Option<String>,
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
    pub attestations_per_day: Option<f64>,
    pub rate_unavailable_reason: Option<String>,
    pub observed_over_blocks: Option<u64>,
    pub rate_caveat: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeContribution {
    pub source_host: String,
    pub unavailable: Option<String>,
    pub source: String,
    pub threads_in_use: Option<u32>,
    pub threads_available: Option<u32>,
    pub layers_held: Option<String>,
    pub layer_count: Option<u32>,
    pub total_layers: Option<u32>,
    pub runs_served: Option<u64>,
    pub cache_hits: Option<u64>,
    pub hop_ms_mean: Option<f64>,
    pub hop_samples: Option<u64>,
    pub hop_unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorInfo {
    pub address: String,
    pub stake: u64,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkOverview {
    pub source_host: String,
    pub unavailable: Option<String>,
    pub network_name: Option<String>,
    pub network_name_unavailable_reason: Option<String>,
    pub chain_id: Option<String>,
    pub declares_mainnet: Option<bool>,
    pub is_block_producing: Option<bool>,
    pub is_block_producing_basis: Option<String>,
    pub host_version: Option<String>,
    pub height: Option<u64>,
    pub last_block_age_secs: Option<u64>,
    pub dag_round: Option<u64>,
    pub dag_committed: Option<u64>,
    pub peers: Option<u32>,
    pub validators_active: Option<u32>,
    pub validators_registered: Option<u32>,
    pub min_active_stake: Option<u64>,
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
    pub tx_count: Option<u32>,
    pub txs: Vec<BlockTx>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerStatus {
    pub running: bool,
    pub unavailable: Option<String>,
    pub state: Option<String>,
    pub public_name: Option<String>,
    pub coordinators_registered: Option<u32>,
    pub coordinators_total: Option<u32>,
    pub jobs_claimed: Option<u64>,
    pub jobs_completed: Option<u64>,
    pub jobs_verified: Option<u64>,
    pub jobs_failed: Option<u64>,
    pub jobs_declined: Option<u64>,
    pub last_job_completed_unix_ms: Option<u64>,
    pub started_unix_ms: Option<u64>,
    pub prevent_sleep_during_jobs: Option<bool>,
    pub mode: Option<String>,
    pub shard: Option<PartialShardWorkerStatus>,
}
