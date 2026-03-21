// ARC Node Desktop — Tauri backend
//
// MVP scaffold: manages the arc-node child process and exposes
// status/stats to the frontend via Tauri commands.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use serde::{Deserialize, Serialize};
use std::process::{Child, Command};
use std::sync::Mutex;
use tauri::State;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct NodeProcess(Mutex<Option<Child>>);

// ---------------------------------------------------------------------------
// Types returned to the frontend
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
struct NodeStatus {
    running: bool,
    version: String,
    uptime_secs: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct NodeStats {
    block_height: u64,
    network_tps: u64,
    your_tps: u64,
    peers: u32,
    finality_secs: f64,
    total_validators: u32,
    total_staked: u64,
    shards: u32,
}

#[derive(Serialize, Deserialize, Clone)]
struct ValidatorInfo {
    address: String,
    role: String,
    stake: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct Earnings {
    session_earned: f64,
    total_earned: f64,
}

// ---------------------------------------------------------------------------
// Helper — locate the arc-node binary
// ---------------------------------------------------------------------------

fn arc_node_bin() -> String {
    // In a release build the binary would be bundled alongside the app.
    // During development, look for it relative to the workspace root.
    if let Ok(p) = std::env::var("ARC_NODE_BIN") {
        return p;
    }
    // Fallback: assume it's been built in the parent workspace.
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("target").join("release").join("arc-node"))
        .unwrap_or_default();
    workspace.to_string_lossy().to_string()
}

// ---------------------------------------------------------------------------
// Helper — HTTP GET against the local node API (localhost:9090)
// ---------------------------------------------------------------------------

async fn node_get<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, String> {
    let url = format!("http://127.0.0.1:9090{}", path);
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("Node unreachable: {e}"))?;
    resp.json::<T>()
        .await
        .map_err(|e| format!("Bad response: {e}"))
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
async fn start_node(state: State<'_, NodeProcess>) -> Result<String, String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Ok("Node is already running".into());
    }

    let bin = arc_node_bin();
    let child = Command::new(&bin)
        .args(["--rpc-port", "9090"])
        .spawn()
        .map_err(|e| format!("Failed to start arc-node at {bin}: {e}"))?;

    *guard = Some(child);
    Ok("Node started".into())
}

#[tauri::command]
async fn stop_node(state: State<'_, NodeProcess>) -> Result<String, String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if let Some(ref mut child) = *guard {
        child.kill().map_err(|e| format!("Failed to stop node: {e}"))?;
        child.wait().ok();
        *guard = None;
        Ok("Node stopped".into())
    } else {
        Ok("Node is not running".into())
    }
}

#[tauri::command]
async fn get_node_status(state: State<'_, NodeProcess>) -> Result<NodeStatus, String> {
    // Check if WE started the node, OR if one is already running externally.
    // This allows the app to connect to a node started via CLI too.
    match node_get::<serde_json::Value>("/health").await {
        Ok(v) => Ok(NodeStatus {
            running: true,
            version: v["version"].as_str().unwrap_or("0.1.0").to_string(),
            uptime_secs: v["uptime_secs"].as_u64().unwrap_or(0),
        }),
        Err(_) => {
            // Node not reachable — check if we have a child process
            let guard = state.0.lock().map_err(|e| e.to_string())?;
            Ok(NodeStatus {
                running: guard.is_some(),
                version: String::new(),
                uptime_secs: 0,
            })
        }
    }
}

#[tauri::command]
async fn get_node_stats() -> Result<NodeStats, String> {
    // Fetch both /stats and /health to get all the data we need.
    let stats = node_get::<serde_json::Value>("/stats").await.ok();
    let health = node_get::<serde_json::Value>("/health").await.ok();

    let block_height = stats.as_ref()
        .and_then(|s| s["block_height"].as_u64())
        .or_else(|| health.as_ref().and_then(|h| h["height"].as_u64()))
        .unwrap_or(0);

    let peers = health.as_ref()
        .and_then(|h| h["peers"].as_u64())
        .unwrap_or(0) as u32;

    let total_txs = stats.as_ref()
        .and_then(|s| s["total_transactions"].as_u64())
        .unwrap_or(0);

    let total_accounts = stats.as_ref()
        .and_then(|s| s["total_accounts"].as_u64())
        .unwrap_or(0);

    let uptime = health.as_ref()
        .and_then(|h| h["uptime_secs"].as_u64())
        .unwrap_or(1);

    // Compute TPS from total_transactions / uptime
    let network_tps = if uptime > 0 { total_txs / uptime } else { 0 };

    Ok(NodeStats {
        block_height,
        network_tps,
        your_tps: network_tps, // Single node = you ARE the network
        peers,
        finality_secs: if block_height > 0 { 4.2 } else { 0.0 },
        total_validators: (peers + 1) as u32, // You + peers
        total_staked: total_accounts * 5_000_000, // Estimate
        shards: 1,
    })
}

#[tauri::command]
async fn get_validator_address() -> Result<ValidatorInfo, String> {
    // In production this reads from a keypair file in the user's data dir.
    // For the MVP scaffold, return a placeholder.
    let data_dir = dirs::data_dir()
        .unwrap_or_default()
        .join("arc-chain")
        .join("validator.key");

    let address = if data_dir.exists() {
        // Read first line of the key file as the address.
        std::fs::read_to_string(&data_dir)
            .map(|s| s.lines().next().unwrap_or("").to_string())
            .unwrap_or_else(|_| "not-found".into())
    } else {
        "not-generated".into()
    };

    Ok(ValidatorInfo {
        address,
        role: "Observer".into(),
        stake: 0,
    })
}

#[tauri::command]
async fn get_peers() -> Result<u32, String> {
    match node_get::<serde_json::Value>("/peers").await {
        Ok(v) => Ok(v["count"].as_u64().unwrap_or(0) as u32),
        Err(_) => Ok(0),
    }
}

#[tauri::command]
async fn get_earnings() -> Result<Earnings, String> {
    match node_get::<Earnings>("/earnings").await {
        Ok(e) => Ok(e),
        Err(_) => Ok(Earnings {
            session_earned: 0.0,
            total_earned: 0.0,
        }),
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    tauri::Builder::default()
        .manage(NodeProcess(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            start_node,
            stop_node,
            get_node_status,
            get_node_stats,
            get_validator_address,
            get_peers,
            get_earnings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ARC Node desktop app");
}
