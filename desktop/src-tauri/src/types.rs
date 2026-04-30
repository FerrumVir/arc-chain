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
pub struct NodeConfig {
    pub role: String,
    pub model_path: Option<String>,
    pub rpc_port: u16,
    pub p2p_port: u16,
    pub auto_start: bool,
    pub auto_update: bool,
    pub data_dir: String,
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub running: bool,
    pub pid: Option<u32>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Earnings {
    pub total_arc: f64,
    pub today_arc: f64,
    pub pending_arc: f64,
    pub rank: Option<u32>,
    pub attestations: u64,
    pub last_payout_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attestation {
    pub tx_hash: String,
    pub input_preview: String,
    pub output_hash: String,
    pub model_hash: String,
    pub tokens: u32,
    pub latency_ms: u32,
    pub reward_arc: f64,
    pub timestamp: i64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    pub has_update: bool,
    pub version: String,
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
