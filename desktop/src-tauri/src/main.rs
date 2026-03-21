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
    let running = {
        let guard = state.0.lock().map_err(|e| e.to_string())?;
        guard.is_some()
    };

    if !running {
        return Ok(NodeStatus {
            running: false,
            version: String::new(),
            uptime_secs: 0,
        });
    }

    // Try the health endpoint; fall back to a simple "running" response.
    match node_get::<NodeStatus>("/health").await {
        Ok(s) => Ok(NodeStatus { running: true, ..s }),
        Err(_) => Ok(NodeStatus {
            running: true,
            version: "0.1.0".into(),
            uptime_secs: 0,
        }),
    }
}

#[tauri::command]
async fn get_node_stats() -> Result<NodeStats, String> {
    match node_get::<NodeStats>("/stats").await {
        Ok(s) => Ok(s),
        // Return placeholder data so the UI has something to display
        // while the node is still initialising.
        Err(_) => Ok(NodeStats {
            block_height: 0,
            network_tps: 0,
            your_tps: 0,
            peers: 0,
            finality_secs: 0.0,
            total_validators: 0,
            total_staked: 0,
            shards: 0,
        }),
    }
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
