use crate::node_manager::{managed_binary_path, TestnetResources};
use crate::types::*;
use crate::{hardware, identity, rpc_client, AppState};
use std::io::Write as _;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;

type CmdResult<T> = Result<T, String>;

fn map_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
pub async fn detect_hardware() -> CmdResult<HardwareInfo> {
    Ok(hardware::detect())
}

#[tauri::command]
pub async fn generate_identity(state: State<'_, AppState>) -> CmdResult<Identity> {
    let id = identity::generate();
    {
        let mut store = state.store.lock().await;
        store.identity = Some(id.clone());
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
    }
    Ok(id)
}

#[tauri::command]
pub async fn import_identity(
    state: State<'_, AppState>,
    phrase: String,
) -> CmdResult<Identity> {
    // Restoration path: user types their 12-word phrase on a new device
    // and gets back the exact same address + signing keys.
    identity::validate_bip39(&phrase)?;
    let id = identity::derive(&phrase)?;
    {
        let mut store = state.store.lock().await;
        store.identity = Some(id.clone());
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
    }
    Ok(id)
}

#[tauri::command]
pub async fn load_identity(state: State<'_, AppState>) -> CmdResult<Option<Identity>> {
    let store = state.store.lock().await;
    Ok(store.identity.clone())
}

#[tauri::command]
pub async fn save_config(
    app: AppHandle,
    state: State<'_, AppState>,
    config: NodeConfig,
) -> CmdResult<()> {
    let auto_start = config.auto_start;
    {
        let mut store = state.store.lock().await;
        store.config = Some(config);
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
    }
    // Keep the OS-level login item in sync with the user's stored
    // preference. Errors here don't block the save — tray still works.
    let autostart = app.autolaunch();
    let current = autostart.is_enabled().unwrap_or(false);
    match (auto_start, current) {
        (true, false) => {
            let _ = autostart.enable();
        }
        (false, true) => {
            let _ = autostart.disable();
        }
        _ => {}
    }
    Ok(())
}

#[tauri::command]
pub async fn get_autostart(app: AppHandle) -> CmdResult<bool> {
    Ok(app.autolaunch().is_enabled().unwrap_or(false))
}

#[tauri::command]
pub async fn load_config(state: State<'_, AppState>) -> CmdResult<Option<NodeConfig>> {
    let store = state.store.lock().await;
    Ok(store.config.clone())
}

#[tauri::command]
pub async fn start_node(
    app: AppHandle,
    state: State<'_, AppState>,
    config: NodeConfig,
) -> CmdResult<()> {
    let validator_seed = {
        let store = state.store.lock().await;
        store
            .identity
            .as_ref()
            .map(|i| i.seed_phrase.clone())
            .ok_or_else(|| {
                "no identity — run onboarding so we can derive an on-chain validator address before starting arc-node".to_string()
            })?
    };
    let resources = resolve_testnet_resources(&app);
    let mut node = state.node.lock().await;
    node.start(&config, &validator_seed, &resources)
        .await
        .map_err(map_err)
}

#[tauri::command]
pub async fn stop_node(state: State<'_, AppState>) -> CmdResult<()> {
    let mut node = state.node.lock().await;
    node.stop().await.map_err(map_err)
}

#[tauri::command]
pub async fn restart_node(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    let (cfg, validator_seed) = {
        let store = state.store.lock().await;
        let cfg = store.config.clone().unwrap_or_default();
        let seed = store
            .identity
            .as_ref()
            .map(|i| i.seed_phrase.clone())
            .ok_or_else(|| "no identity — cannot restart arc-node".to_string())?;
        (cfg, seed)
    };
    let resources = resolve_testnet_resources(&app);
    let mut node = state.node.lock().await;
    node.restart(&cfg, &validator_seed, &resources)
        .await
        .map_err(map_err)
}

#[tauri::command]
pub async fn node_status(state: State<'_, AppState>) -> CmdResult<NodeStatus> {
    let (port, pid, address, crash) = {
        let mut node = state.node.lock().await;
        // Opportunistic crash detection — checks if our child process exited
        // unexpectedly since the last poll.
        node.try_reap_if_crashed().await;
        let pid = if node.is_running() { node.pid() } else { None };
        let port = node.rpc_port;
        let crash = node
            .crash_info
            .lock()
            .await
            .as_ref()
            .map(|c| c.message.clone());
        drop(node);
        let address = {
            let store = state.store.lock().await;
            store.identity.as_ref().map(|i| i.address.clone())
        };
        (port, pid, address, crash)
    };
    Ok(rpc_client::fetch_status(&state.http, port, pid, address, crash).await)
}

#[tauri::command]
pub async fn clear_crash(state: State<'_, AppState>) -> CmdResult<()> {
    state.node.lock().await.clear_crash().await;
    Ok(())
}

#[tauri::command]
pub async fn fetch_earnings(state: State<'_, AppState>) -> CmdResult<Earnings> {
    let port = state.node.lock().await.rpc_port;
    Ok(rpc_client::fetch_earnings(&state.http, port).await)
}

#[tauri::command]
pub async fn fetch_attestations(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<Vec<Attestation>> {
    let port = state.node.lock().await.rpc_port;
    Ok(rpc_client::fetch_attestations(&state.http, port, limit.unwrap_or(20)).await)
}

#[tauri::command]
pub async fn fetch_logs(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<Vec<LogEntry>> {
    let node = state.node.lock().await;
    Ok(node
        .logs_snapshot(limit.unwrap_or(200) as usize)
        .await)
}

#[tauri::command]
pub async fn fetch_network_stats(state: State<'_, AppState>) -> CmdResult<NetworkStats> {
    let port = state.node.lock().await.rpc_port;
    Ok(rpc_client::fetch_network_stats(&state.http, port).await)
}

#[tauri::command]
pub async fn open_external(app: AppHandle, url: String) -> CmdResult<()> {
    use tauri_plugin_opener::OpenerExt;
    app.opener().open_url(url, None::<&str>).map_err(map_err)
}

#[tauri::command]
pub async fn fetch_balance(state: State<'_, AppState>) -> CmdResult<AccountBalance> {
    let (port, addr) = {
        let node = state.node.lock().await;
        let store = state.store.lock().await;
        let addr = store.identity.as_ref().map(|i| i.address.clone());
        (node.rpc_port, addr)
    };
    let addr = addr.ok_or_else(|| "no identity".to_string())?;
    rpc_client::fetch_balance(&state.http, port, &addr).await
}

#[tauri::command]
pub async fn faucet_claim(state: State<'_, AppState>) -> CmdResult<FaucetResult> {
    let (port, addr) = {
        let node = state.node.lock().await;
        let store = state.store.lock().await;
        let addr = store.identity.as_ref().map(|i| i.address.clone());
        (node.rpc_port, addr)
    };
    let addr = addr.ok_or_else(|| "no identity".to_string())?;
    rpc_client::faucet_claim(&state.http, port, &addr).await
}

#[tauri::command]
pub async fn run_inference(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
) -> CmdResult<InferenceResult> {
    let port = state.node.lock().await.rpc_port;
    rpc_client::run_inference(&state.http, port, &prompt, max_tokens.unwrap_or(32)).await
}

#[tauri::command]
pub async fn check_for_update() -> CmdResult<UpdateCheck> {
    // Query the public GitHub releases API for the latest tag.
    let client = reqwest::Client::builder()
        .user_agent("arc-desktop/0.1")
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(map_err)?;
    let resp = client
        .get("https://api.github.com/repos/FerrumVir/arc-chain/releases/latest")
        .send()
        .await
        .map_err(map_err)?;
    let v: serde_json::Value = resp.json().await.map_err(map_err)?;
    let version = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .map(|s| s.trim_start_matches('v').to_string())
        .unwrap_or_else(|| "unknown".into());
    let current = env!("CARGO_PKG_VERSION");
    Ok(UpdateCheck {
        has_update: version != current && version != "unknown",
        version,
    })
}

/// First-launch readiness check. Confirms the bundled testnet resources are
/// resolvable AND the arc-node binary is present; if the binary isn't
/// installed, downloads it from the latest GitHub release for this platform.
/// The onboarding screen calls this before transitioning to the "launch"
/// step so the user can see download progress (or fail fast with a clear
/// error rather than a cryptic "failed to start arc-node" later).
#[tauri::command]
pub async fn ensure_binary(app: AppHandle) -> CmdResult<BinaryStatus> {
    let target = managed_binary_path();
    if target.exists() {
        return Ok(BinaryStatus {
            path: target.to_string_lossy().into_owned(),
            downloaded_bytes: 0,
            total_bytes: 0,
            already_installed: true,
        });
    }

    let asset = platform_release_asset().ok_or_else(|| {
        format!(
            "no prebuilt arc-node binary for platform {}-{} — build from source or open an issue",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let url = format!(
        "https://github.com/FerrumVir/arc-chain/releases/latest/download/{}",
        asset
    );

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(map_err)?;
    }

    let client = reqwest::Client::builder()
        .user_agent("arc-desktop/0.1")
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(map_err)?;

    let resp = client.get(&url).send().await.map_err(map_err)?;
    if !resp.status().is_success() {
        return Err(format!(
            "release asset {} returned HTTP {}",
            asset,
            resp.status()
        ));
    }
    let total_bytes = resp.content_length().unwrap_or(0);
    let tmp = target.with_extension("download");
    {
        let mut file = std::fs::File::create(&tmp).map_err(map_err)?;
        let bytes = resp.bytes().await.map_err(map_err)?;
        file.write_all(&bytes).map_err(map_err)?;
        file.sync_all().ok();
    }
    std::fs::rename(&tmp, &target).map_err(map_err)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&target, perms).map_err(map_err)?;
    }

    // Best-effort: strip any macOS quarantine flag on our own download.
    // User still needs to allow the desktop .app itself past Gatekeeper.
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(&target)
            .output();
    }

    let _ = app; // reserved for future progress events via app.emit(...)

    Ok(BinaryStatus {
        path: target.to_string_lossy().into_owned(),
        downloaded_bytes: total_bytes,
        total_bytes,
        already_installed: false,
    })
}

fn platform_release_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("arc-node-macos-arm64"),
        ("macos", "x86_64") => Some("arc-node-macos-x86_64"),
        ("linux", "x86_64") => Some("arc-node-linux-x86_64"),
        _ => None,
    }
}

fn resolve_testnet_resources(app: &AppHandle) -> TestnetResources {
    let resolver = app.path();
    let seeds = resolver
        .resolve("resources/testnet-seeds.txt", tauri::path::BaseDirectory::Resource)
        .ok()
        .filter(|p: &PathBuf| p.exists());
    let genesis = resolver
        .resolve("resources/genesis.toml", tauri::path::BaseDirectory::Resource)
        .ok()
        .filter(|p: &PathBuf| p.exists());
    TestnetResources {
        seeds_file: seeds,
        genesis_file: genesis,
    }
}
