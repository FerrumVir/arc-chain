use crate::node_manager::{managed_binary_path, TestnetResources};
use crate::types::*;
use crate::{hardware, identity, paths, rpc_client, AppState, CommunityReceiptRoute};
use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};
use ssh_key::{PublicKey, SshSig};
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroize as _;

type CmdResult<T> = Result<T, String>;

fn map_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Hardware detection is expensive — `System::new_all()` plus, on macOS, a
/// `system_profiler` call that routinely takes 1-3s on a cold cache. It was
/// being run twice per onboarding (once here, once inside `recommended_tier`)
/// on a tokio worker with no `spawn_blocking`. The result never changes
/// during a session, so detect once and hand out clones.
fn cached_hardware() -> HardwareInfo {
    use std::sync::OnceLock;
    static HW: OnceLock<HardwareInfo> = OnceLock::new();
    HW.get_or_init(hardware::detect).clone()
}

#[tauri::command]
pub async fn detect_hardware() -> CmdResult<HardwareInfo> {
    // Off the async worker: the first call does real blocking I/O.
    tokio::task::spawn_blocking(cached_hardware)
        .await
        .map_err(map_err)
}

// ── Identity / IPC boundary ────────────────────────────────────────────────
//
// These three commands return `IdentityPublic`, never `Identity`. The BIP-39
// phrase is the user's signing key; handing it to the WebView put it within
// reach of DevTools, of any injected script, and — because the frontend
// persisted whatever it received — of anything able to read the WebView
// profile directory, where it sat in plaintext localStorage.
//
// The phrase stays Rust-side. `reveal_seed_phrase` hands it out exactly once,
// on an explicit user action, for the backup screen.

#[tauri::command]
pub async fn generate_identity(state: State<'_, AppState>) -> CmdResult<IdentityPublic> {
    let id = identity::generate();
    let public = IdentityPublic::from(&id);
    {
        let mut store = state.store.lock().await;
        store.identity = Some(id);
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
    }
    Ok(public)
}

#[tauri::command]
pub async fn import_identity(
    state: State<'_, AppState>,
    phrase: String,
) -> CmdResult<IdentityPublic> {
    // Restoration path: user types their 12-word phrase on a new device
    // and gets back the exact same address + signing keys.
    identity::validate_bip39(&phrase)?;
    let id = identity::derive(&phrase)?;
    let public = IdentityPublic::from(&id);
    {
        let mut store = state.store.lock().await;
        store.identity = Some(id);
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
    }
    Ok(public)
}

#[tauri::command]
pub async fn load_identity(state: State<'_, AppState>) -> CmdResult<Option<IdentityPublic>> {
    let store = state.store.lock().await;
    Ok(store.identity.as_ref().map(IdentityPublic::from))
}

/// Hand the recovery phrase to the UI for the "write this down" screen.
///
/// Deliberately a separate, explicit call rather than a field on the identity
/// object: it makes every place the phrase reaches the WebView a single
/// greppable call site, and it means the phrase is only in WebView memory
/// while the backup screen is actually open. The frontend must never persist
/// what this returns.
#[tauri::command]
pub async fn reveal_seed_phrase(state: State<'_, AppState>) -> CmdResult<String> {
    let store = state.store.lock().await;
    store
        .identity
        .as_ref()
        .map(|i| i.seed_phrase.clone())
        .ok_or_else(|| "no identity on this device".to_string())
}

#[tauri::command]
pub async fn save_config(
    app: AppHandle,
    state: State<'_, AppState>,
    mut config: NodeConfig,
) -> CmdResult<()> {
    let auto_start;
    {
        let mut store = state.store.lock().await;
        // `data_dir` is a native chain-history boundary, not a WebView
        // preference. Once one is persisted (including a freshly fenced v3
        // directory), generic config saves may update ports, role, model and
        // lifecycle flags but can never repoint the node at preserved v0.7
        // history. A future data-move feature needs its own verified native
        // transaction instead of widening this IPC surface.
        preserve_authoritative_data_dir(&mut config, store.config.as_ref());
        auto_start = config.auto_start;
        store.config = Some(config);
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
    }
    // Keep the OS-level login item in sync with the user's stored
    // preference. Errors here don't block the save - tray still works.
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

fn preserve_authoritative_data_dir(config: &mut NodeConfig, persisted: Option<&NodeConfig>) {
    if let Some(persisted) = persisted {
        config.data_dir.clone_from(&persisted.data_dir);
    }
}

#[tauri::command]
pub async fn get_autostart(app: AppHandle) -> CmdResult<bool> {
    Ok(app.autolaunch().is_enabled().unwrap_or(false))
}

fn update_install_policy_for(os: &str, appimage: Option<&Path>) -> UpdateInstallPolicy {
    if os == "linux" {
        let appimage_ready = appimage
            .filter(|path| path.is_absolute() && path.is_file())
            .is_some();
        if appimage_ready {
            return UpdateInstallPolicy {
                can_install: true,
                channel: "appimage".into(),
                instructions: "ARC can install this signed AppImage update in place.".into(),
            };
        }
        return UpdateInstallPolicy {
            can_install: false,
            channel: "package-manager".into(),
            instructions: "A signed update is available. Install the new .deb or .rpm with the same package manager used for this ARC installation.".into(),
        };
    }

    UpdateInstallPolicy {
        can_install: true,
        channel: "native".into(),
        instructions: "ARC can install this signed update in place.".into(),
    }
}

/// Report whether this distribution can consume Tauri's updater payload.
/// Linux package installs must remain owned by apt/dnf/rpm; only an actual
/// AppImage launch receives in-app replacement.
#[tauri::command]
pub async fn update_install_policy() -> CmdResult<UpdateInstallPolicy> {
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    Ok(update_install_policy_for(
        std::env::consts::OS,
        appimage.as_deref(),
    ))
}

#[tauri::command]
pub async fn load_config(state: State<'_, AppState>) -> CmdResult<Option<NodeConfig>> {
    let store = state.store.lock().await;
    Ok(store.config.clone())
}

#[tauri::command]
pub async fn load_data_migration_notice(
    state: State<'_, AppState>,
) -> CmdResult<Option<DataMigrationNotice>> {
    let store = state.store.lock().await;
    Ok(store.data_migration_notice.clone())
}

#[tauri::command]
pub async fn dismiss_data_migration_notice(state: State<'_, AppState>) -> CmdResult<()> {
    let mut store = state.store.lock().await;
    store.data_migration_notice = None;
    let dir = state.data_dir.lock().await.clone();
    store.save_to(&dir).map_err(map_err)
}

/// The one path that actually starts arc-node.
///
/// Factored out of the `start_node` command so `lib.rs` `setup()` can launch
/// the node on app start through exactly the same code — previously
/// `auto_start` only toggled the OS login item, and nothing in the app ever
/// spawned arc-node after onboarding finished.
///
/// Takes `&AppState` rather than `State<'_, AppState>` so it is callable both
/// from a command and from a background task holding an `AppHandle`.
async fn start_node_transaction(
    app: &AppHandle,
    state: &AppState,
    start_after_recovery: bool,
) -> Result<(), String> {
    require_data_migration_ready(state).await?;
    let (config, mut recovery_phrase, persisted_address) = {
        let store = state.store.lock().await;
        let config = store.config.clone().unwrap_or_default();
        let identity = store
            .identity
            .as_ref()
            .ok_or_else(|| {
                "no identity - run onboarding so we can derive an on-chain validator address before starting arc-node".to_string()
            })?;
        (
            config,
            identity.seed_phrase.clone(),
            identity.address.clone(),
        )
    };
    let resources = resolve_testnet_resources(app);
    {
        let mut node = state.node.lock().await;
        if node.is_running() {
            return Ok(());
        }
    }
    let lock_data_dir = config.data_dir.clone();
    let lifecycle_lock = tokio::task::spawn_blocking(move || {
        crate::node_manager::acquire_managed_lifecycle_lock(&lock_data_dir)
    })
    .await
    .map_err(map_err)?
    .map_err(map_err)?;
    // Recheck only after owning the cross-process data lifecycle. The lock is
    // held through binary mutation, stable-resource materialization, receipt
    // arm and the spawn outcome, closing check→replace races between GUIs.
    let recovery_launch = crate::node_manager::managed_shutdown_recovery_required(&config.data_dir)
        .map_err(map_err)?;
    if !recovery_launch {
        if !start_after_recovery {
            return Ok(());
        }
        // Make sure we have a runnable binary only after proving there is no
        // stale receipt bound to the currently installed bytes. Replacing an
        // executable first would strand the only safe recovery identity.
        ensure_binary_inner(app).await?;
    }
    let app_data_dir = state.data_dir.lock().await.clone();
    let keyfile_result =
        identity::ensure_validator_keyfile(&app_data_dir, &recovery_phrase, &persisted_address);
    recovery_phrase.zeroize();
    let validator_keyfile = keyfile_result?;
    let mut node = state.node.lock().await;
    node.start(&config, &validator_keyfile, &resources, lifecycle_lock)
        .await
        .map_err(map_err)?;
    if !recovery_launch {
        return Ok(());
    }

    // A stale marker is a quarantined recovery transaction, not permission to
    // expose the old receipt-bound node to the normal dashboard/RPC flow. The
    // node's early authenticated request path defers exit until StateDB has
    // opened/replayed, all writers join, and the final WAL fsync publishes the
    // positive ACK. Only after exact death + ACK consumption may we replace
    // the binary/stable network identity and launch the requested current node.
    node.stop()
        .await
        .map_err(|error| format!("managed-node durability recovery failed: {error}"))?;
    drop(node);

    if !start_after_recovery {
        return Ok(());
    }

    let lock_data_dir = config.data_dir.clone();
    let lifecycle_lock = tokio::task::spawn_blocking(move || {
        crate::node_manager::acquire_managed_lifecycle_lock(&lock_data_dir)
    })
    .await
    .map_err(map_err)?
    .map_err(map_err)?;
    if crate::node_manager::managed_shutdown_recovery_required(&config.data_dir).map_err(map_err)? {
        return Err(
            "recovery node exited but its authenticated shutdown boundary remains unresolved"
                .into(),
        );
    }
    ensure_binary_inner(app).await?;
    let mut node = state.node.lock().await;
    node.start(&config, &validator_keyfile, &resources, lifecycle_lock)
        .await
        .map_err(map_err)
}

pub async fn start_node_inner(app: &AppHandle, state: &AppState) -> Result<(), String> {
    start_node_transaction(app, state, true).await
}

pub(crate) async fn recover_managed_shutdown_inner(
    app: &AppHandle,
    state: &AppState,
) -> Result<(), String> {
    start_node_transaction(app, state, false).await
}

fn data_migration_start_gate(reason: Option<&str>) -> Result<(), String> {
    match reason {
        None => Ok(()),
        Some(reason) => Err(format!(
            "ARC refused to start the node because chain-data migration is not safely resolved: {reason}. Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory."
        )),
    }
}

async fn require_data_migration_ready(state: &AppState) -> Result<(), String> {
    let reason = state.data_migration_error.lock().await.clone();
    data_migration_start_gate(reason.as_deref())
}

#[tauri::command]
pub async fn start_node(
    app: AppHandle,
    state: State<'_, AppState>,
    config: NodeConfig,
) -> CmdResult<()> {
    // Retain the command argument for IPC compatibility with older WebViews,
    // but never trust it for chain-state selection. Onboarding persists its
    // candidate first; the native store is the sole launch authority.
    let _ = config;
    start_node_inner(&app, &state).await
}

#[tauri::command]
pub async fn stop_node(state: State<'_, AppState>) -> CmdResult<()> {
    let mut node = state.node.lock().await;
    node.stop().await.map_err(map_err)
}

/// Establish the native updater/relaunch boundary.
///
/// Tauri replaces and relaunches the desktop process, but `arc-node` is a
/// separate child (and is deliberately placed in a new process group on
/// Windows). Without an explicit stop it survives the GUI update, so the new
/// desktop can see a healthy old node and silently keep using an incompatible
/// protocol. A failed stop blocks download/install instead of pretending the
/// boundary is safe.
#[tauri::command]
pub async fn prepare_update_relaunch(state: State<'_, AppState>) -> CmdResult<()> {
    require_data_migration_ready(&state).await?;
    let mut node = state.node.lock().await;
    node.prepare_update_relaunch()
        .await
        .map_err(|error| format!("could not establish the native update lifecycle fence: {error}"))
}

/// Seal the one-way native updater boundary immediately before invoking the
/// signed installer. From this point the old node cannot be resumed by an
/// abort command, even if installer IPC later rejects or disconnects.
#[tauri::command]
pub async fn begin_update_handoff(state: State<'_, AppState>) -> CmdResult<()> {
    let mut node = state.node.lock().await;
    node.begin_update_handoff()
        .map_err(|error| format!("could not commit the native updater handoff: {error}"))
}

/// Release a prepared native update fence only when the signed installer
/// rejects/cancels before accepting bundle mutation. If Prepare stopped one
/// exact owned node, this same native transaction resumes that exact launch
/// while continuously retaining its lifecycle lock. Successful installation
/// deliberately has no release path in the old GUI: relaunch or manual quit
/// must end that process before another node can start.
#[tauri::command]
pub async fn abort_update_relaunch(state: State<'_, AppState>) -> CmdResult<()> {
    let mut node = state.node.lock().await;
    node.abort_update_relaunch().await.map_err(|error| {
        format!("could not safely abort the update and restore the prior node state: {error}")
    })
}

#[tauri::command]
pub async fn restart_node(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    require_data_migration_ready(&state).await?;
    let (cfg, mut recovery_phrase, persisted_address) = {
        let store = state.store.lock().await;
        let cfg = store.config.clone().unwrap_or_default();
        let identity = store
            .identity
            .as_ref()
            .ok_or_else(|| "no identity - cannot restart arc-node".to_string())?;
        (cfg, identity.seed_phrase.clone(), identity.address.clone())
    };
    let app_data_dir = state.data_dir.lock().await.clone();
    let keyfile_result =
        identity::ensure_validator_keyfile(&app_data_dir, &recovery_phrase, &persisted_address);
    recovery_phrase.zeroize();
    let validator_keyfile = keyfile_result?;

    // ORDER MATTERS: stop the child BEFORE ensure_binary may rename over the
    // executable.
    //
    // ensure_binary installs a download by renaming it over
    // ~/.arc/bin/arc-node(.exe). On POSIX that succeeds against the old
    // inode even while the process runs. Windows locks a running
    // executable's image file, so MoveFileEx returns ERROR_ACCESS_DENIED —
    // which made Restart fail on Windows only, and only when a version
    // mismatch pushed it down the download path. Doing this in the old
    // order also meant the same failure hit the observer→worker upgrade
    // flow, which restarts immediately after switching roles.
    {
        let mut node = state.node.lock().await;
        node.stop().await.map_err(map_err)?;
    }

    let lock_data_dir = cfg.data_dir.clone();
    let lifecycle_lock = tokio::task::spawn_blocking(move || {
        crate::node_manager::acquire_managed_lifecycle_lock(&lock_data_dir)
    })
    .await
    .map_err(map_err)?
    .map_err(map_err)?;
    if crate::node_manager::managed_shutdown_recovery_required(&cfg.data_dir).map_err(map_err)? {
        return Err(
            "restart stopped the node but its durable shutdown receipt remains unresolved".into(),
        );
    }

    // A restart is a good moment to pick up a newer arc-node, since the user
    // is already paying the restart cost. Now safe: nothing holds the file.
    ensure_binary_inner(&app).await?;

    let resources = resolve_testnet_resources(&app);
    let mut node = state.node.lock().await;
    node.start(&cfg, &validator_keyfile, &resources, lifecycle_lock)
        .await
        .map_err(map_err)
}

/// Stops the node, wipes the cached peer dial list (`known_peers.json` in
/// the data directory), and restarts. This is the recovery button for
/// "I had peers, then I restarted, now I'm stuck at 0 peers / Lite
/// mode" — the most common cause is a stale peer cache pinning to dead
/// or unreachable seeds. After wiping, the node falls back to the
/// bundled testnet-seeds.txt and re-bootstraps.
///
/// All other state (WAL, blocks, identity, config) is preserved. Only
/// the peer dial cache is removed.
#[tauri::command]
pub async fn reset_peer_state(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ResetPeerStateResult> {
    // This command mutates the configured data directory before restarting.
    // Apply the same native migration fence first: when legacy selection is
    // ambiguous, even deleting its peer cache would violate the promise that
    // every preserved v0.7 byte remains untouched.
    require_data_migration_ready(&state).await?;
    // Resolve the data dir through the SAME helper node_manager uses.
    // Duplicating the expansion here (HOME-only) meant this deleted
    // known_peers.json from a different directory than the node actually
    // uses on Windows, then reported success.
    let (cfg, mut recovery_phrase, persisted_address) = {
        let store = state.store.lock().await;
        let cfg = store.config.clone().unwrap_or_default();
        let identity = store
            .identity
            .as_ref()
            .ok_or_else(|| "no identity - cannot reset peer state".to_string())?;
        (cfg, identity.seed_phrase.clone(), identity.address.clone())
    };
    let data_dir = crate::node_manager::resolve_data_dir(&cfg.data_dir);
    let peers_path = data_dir.join("known_peers.json");
    let app_data_dir = state.data_dir.lock().await.clone();
    let keyfile_result =
        identity::ensure_validator_keyfile(&app_data_dir, &recovery_phrase, &persisted_address);
    recovery_phrase.zeroize();
    let validator_keyfile = keyfile_result?;
    let resources = resolve_testnet_resources(&app);

    // Stop first and retain the exact cross-process data-directory guard
    // through deletion, binary readiness, receipt arm, and replacement spawn.
    // Releasing it after Stop used to let a second GUI start a writer while
    // this command removed that writer's peer cache.
    let lifecycle_lock = {
        let mut node = state.node.lock().await;
        node.stop_for_local_mutation().await.map_err(|error| {
            format!(
                "refusing to mutate peer state because the managed node did not prove a clean shutdown: {error}"
            )
        })?
    };

    let removed = match std::fs::remove_file(&peers_path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("failed to remove {}: {}", peers_path.display(), e)),
    };

    ensure_binary_inner(&app).await?;
    let mut node = state.node.lock().await;
    node.start(&cfg, &validator_keyfile, &resources, lifecycle_lock)
        .await
        .map_err(map_err)?;

    Ok(ResetPeerStateResult {
        removed_path: peers_path.display().to_string(),
        was_present: removed,
        message: if removed {
            "Cleared cached peer list. Rebootstrapping from testnet seeds.".into()
        } else {
            "No cached peer file existed. Restarted with the bundled seeds.".into()
        },
    })
}

/// Health and progress of the node running on THIS machine.
///
/// The single most damaging bug in the app was that this read a remote seed:
/// `wallet_host()` discarded its port argument and returned the LAX seed, so
/// the Dashboard reported a datacenter's peers, uptime, version and height as
/// the user's own. Consequences cascaded — `running` was always true, so the
/// Start button was never rendered, Stop appeared to do nothing, and the
/// entire lite/syncing UI was unreachable. The tray, which had always polled
/// 127.0.0.1 correctly, contradicted the window on the same screen.
///
/// Local state now comes from the local node. Chain-wide numbers are still
/// returned, but in clearly separate `chain*` fields.
#[tauri::command]
pub async fn node_status(state: State<'_, AppState>) -> CmdResult<NodeStatus> {
    let (port, pid, crash, worker_threads) = {
        let mut node = state.node.lock().await;
        // Opportunistic crash detection - checks if our child process exited
        // unexpectedly since the last poll.
        node.try_reap_if_crashed().await;
        let pid = if node.is_running() { node.pid() } else { None };
        let port = node.rpc_port;
        let worker_threads = node.active_worker_threads;
        let crash = node
            .crash_info
            .lock()
            .await
            .as_ref()
            .map(|c| c.message.clone());
        (port, pid, crash, worker_threads)
    };
    let address = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    };

    let local = paths::local_host(port);
    let chain = chain_host(&state).await;
    let chain_choice = cached_chain_choice(&state).await;

    let mut status =
        rpc_client::fetch_status(&state.http, &local, &chain, port, pid, address, crash).await;

    status.chain_host = Some(chain);
    status.chain_height = chain_choice.as_ref().map(|c| c.height);
    status.chain_block_age_seconds = chain_choice.as_ref().map(|c| c.block_age_seconds());
    status.worker_threads = worker_threads;
    status.cpu_cores = Some(cached_hardware().cpu_cores);
    Ok(status)
}

#[tauri::command]
pub async fn clear_crash(state: State<'_, AppState>) -> CmdResult<()> {
    state.node.lock().await.clear_crash().await;
    Ok(())
}

#[tauri::command]
pub async fn fetch_earnings(state: State<'_, AppState>) -> CmdResult<Earnings> {
    let address = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    };
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_earnings(&state.http, &host, address.as_deref()).await)
}

#[tauri::command]
pub async fn fetch_attestations(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<Vec<Attestation>> {
    // The user's address is needed here, not just for display: it decides
    // which attestations are credited as theirs. Without it the feed showed
    // every validator's work as "+2.50 ARC" in the user's own earnings view.
    let address = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    };
    let host = chain_host(&state).await;
    Ok(
        rpc_client::fetch_attestations(&state.http, &host, limit.unwrap_or(20), address.as_deref())
            .await,
    )
}

#[tauri::command]
pub async fn fetch_logs(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<Vec<LogEntry>> {
    let node = state.node.lock().await;
    Ok(node.logs_snapshot(limit.unwrap_or(200) as usize).await)
}

#[tauri::command]
pub async fn fetch_network_stats(state: State<'_, AppState>) -> CmdResult<NetworkStats> {
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_network_stats(&state.http, &host).await)
}

/// Enforce, natively, the same scheme policy the desktop capability declares.
///
/// `capabilities/default.json` scopes `shell:allow-open` to `http://**` and
/// `https://**`, but this command reaches the OS handler through
/// `OpenerExt::open_url`, which is a direct Rust call and therefore never
/// consults that scope. Without this check an IPC caller could hand the
/// platform handler a `file:`, `smb:`, or Windows shell URL that the app has
/// no reason to open. Every in-app caller already passes an `http(s)` URL.
fn external_web_url(url: &str) -> CmdResult<()> {
    let parsed = tauri::Url::parse(url)
        .map_err(|_| format!("refusing to open a malformed external URL: {url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "refusing to open an external URL with unsupported scheme '{}'",
            parsed.scheme()
        ));
    }
    Ok(())
}

#[tauri::command]
pub async fn open_external(app: AppHandle, url: String) -> CmdResult<()> {
    use tauri_plugin_opener::OpenerExt;
    external_web_url(&url)?;
    app.opener().open_url(url, None::<&str>).map_err(map_err)
}

// ── Chain visibility + projection (v0.8.0) ──────────────────────────────
//
// Every command here reads the pinned chain host from `chain_host()`, except
// `fetch_node_contribution`, which describes the user's own machine and so
// reads 127.0.0.1. None of them read a second seed: the seeds are independent
// chains (CLAUDE.md rule 4), so comparing two would report a structural
// disagreement as if it were a fault.
//
// Several of the endpoints behind these are newer than the deployed seed
// binaries. That is expected, and each returns a struct carrying an
// `unavailable` reason rather than an error — a 404 is information about the
// host, not a failure of the app, and the UI states it.

/// The finite reward treasury. Feeds the "how much is left" line that keeps a
/// projection from implying an unlimited payout.
#[tauri::command]
pub async fn fetch_reward_economics(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::RewardEconomics> {
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_reward_economics(&state.http, &host).await)
}

/// Projection inputs for this device's address.
#[tauri::command]
pub async fn fetch_earnings_projection(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::EarningsProjection> {
    let address = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    };
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_earnings_projection(&state.http, &host, address.as_deref()).await)
}

/// What the node on THIS machine is contributing. Local read by design — the
/// whole bug class this app has been unwinding was showing a datacenter's
/// numbers as the user's own.
#[tauri::command]
pub async fn fetch_node_contribution(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::NodeContribution> {
    let port = state.node.lock().await.rpc_port;
    let local = paths::local_host(port);
    let cores = Some(cached_hardware().cpu_cores);
    Ok(rpc_client::fetch_node_contribution(&state.http, &local, cores).await)
}

/// Height, block age, validator split, peers and DAG round for the pinned host.
#[tauri::command]
pub async fn fetch_network_overview(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::NetworkOverview> {
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_network_overview(&state.http, &host).await)
}

#[tauri::command]
pub async fn fetch_recent_blocks(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<crate::types::RecentBlocks> {
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_recent_blocks(&state.http, &host, limit.unwrap_or(10)).await)
}

/// Transactions inside one block. Called on expand, never on the poll path.
#[tauri::command]
pub async fn fetch_block_txs(
    state: State<'_, AppState>,
    height: u64,
    limit: Option<u32>,
) -> CmdResult<crate::types::BlockTxs> {
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_block_txs(&state.http, &host, height, limit.unwrap_or(50)).await)
}

/// Look one hash up on the pinned host. Replaces an `openExternal` to a
/// hardcoded LAX IP serving a page that is not a block explorer.
#[tauri::command]
pub async fn lookup_tx(
    state: State<'_, AppState>,
    hash: String,
) -> CmdResult<crate::types::TxLookup> {
    let host = chain_host(&state).await;
    Ok(rpc_client::lookup_tx(&state.http, &host, &hash).await)
}

#[tauri::command]
pub async fn fetch_balance(state: State<'_, AppState>) -> CmdResult<AccountBalance> {
    let addr = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    }
    .ok_or_else(|| "no identity".to_string())?;
    let host = chain_host(&state).await;
    rpc_client::fetch_balance(&state.http, &host, &addr).await
}

#[tauri::command]
pub async fn faucet_claim(state: State<'_, AppState>) -> CmdResult<FaucetResult> {
    let addr = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    }
    .ok_or_else(|| "no identity".to_string())?;
    let host = crate::wallet::validate_rpc_origin(&chain_host(&state).await)?;
    rpc_client::faucet_claim(&state.http, &host, &addr).await
}

/// Sign and submit an ARC transfer without ever handing recovery material to
/// the WebView. IPC contains only the recipient and a decimal amount string.
#[tauri::command]
pub async fn send_arc(
    state: State<'_, AppState>,
    to: String,
    amount_arc: String,
) -> CmdResult<WalletTxResult> {
    let amount_base = crate::wallet::parse_arc_amount(&amount_arc)?;
    if amount_base == 0 {
        return Err("amount must be greater than zero".to_string());
    }

    // Prevent concurrent clicks from reading and signing the same nonce.
    let _write_guard = state.wallet_write.lock().await;
    let address = {
        let store = state.store.lock().await;
        store
            .identity
            .as_ref()
            .map(|identity| identity.address.clone())
    }
    .ok_or_else(|| "no identity".to_string())?;

    let host = crate::wallet::validate_rpc_origin(&chain_host(&state).await)?;
    let account = rpc_client::fetch_balance(&state.http, &host, &address).await?;
    let available = account
        .balance_base
        .parse::<u64>()
        .map_err(|_| "selected host returned an invalid wallet balance".to_string())?;
    // Read the domain from the exact pinned origin before touching the
    // recovery phrase. Missing v3 recovery metadata fails closed. The v3
    // minimum fee is part of the signed transaction and therefore part of
    // the available-balance decision too.
    let domain = rpc_client::transaction_signing_domain(&state.http, &host).await?;
    let fee_base = crate::wallet::transfer_fee_base(domain);
    let required = amount_base
        .checked_add(fee_base)
        .ok_or_else(|| "amount plus transaction fee exceeds ARC's base-unit limit".to_string())?;
    if required > available {
        return Err(format!(
            "insufficient balance: available {} ARC, requested {} ARC plus {} ARC network fee",
            account.balance_arc,
            crate::wallet::format_arc_amount(amount_base),
            crate::wallet::format_arc_amount(fee_base),
        ));
    }
    let tx = {
        let store = state.store.lock().await;
        let identity = store
            .identity
            .as_ref()
            .ok_or_else(|| "no identity".to_string())?;
        crate::wallet::signed_transfer(identity, &to, amount_base, account.nonce, domain)?
    };

    rpc_client::submit_signed_transfer(&state.http, &host, &tx, amount_base).await
}

// The chain host is elected ONCE and then pinned for the life of the
// process. It is deliberately NOT re-elected on a timer.
//
// The seeds are not one chain. They share a DAG round but not state:
// `/block/43000` returns a different hash on each, heights span 51k-135k, and
// a faucet credit on LAX never appears on AMS. Silently migrating the wallet
// to a different seed mid-session would make the user's balance change for no
// visible reason, or make a faucet claim they just watched succeed vanish.
// (See CLAUDE.md rule 4.)
//
// So: pick the freshest seed at startup — which is the part the old code got
// wrong, hard-pinning LAX even when it was six days stale — then stay there.
// Re-election happens only if the pinned host stops answering, since a dead
// host is worse than an inconsistent one.

/// The seed whose chain view we read balances, earnings, attestations and
/// network stats from.
///
/// Chosen dynamically rather than pinned. The previous code hard-pinned LAX,
/// which was a reasonable choice when it was written and a bad one now: four
/// of the six seeds have not produced a block in roughly six days, and which
/// of the remaining two is ahead changes over the course of a day. A pin
/// means the wallet silently reads a stalled chain.
///
/// Selection is by freshest `/block/latest` header timestamp — the direct
/// measure of "is this host still producing?", where `/health` alone is not
/// (a stalled seed still reports `status: ok` and a healthy peer count, and
/// its DAG round keeps advancing even while block height stands still).
///
/// All candidates are probed concurrently. Sequential probing with a 2s
/// timeout each is up to 12s of dead air on a screen that repaints every
/// 1.5s.
async fn probe_chain_host(http: &reqwest::Client) -> Option<ChainHostChoice> {
    let mut set = tokio::task::JoinSet::new();
    for host in WALLET_HOSTS {
        let http = http.clone();
        let host = host.to_string();
        set.spawn(async move {
            let url = format!("{}/block/latest", host);
            let resp =
                tokio::time::timeout(std::time::Duration::from_secs(3), http.get(&url).send())
                    .await
                    .ok()?
                    .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let v: serde_json::Value = resp.json().await.ok()?;
            let header = v.get("header")?;
            let timestamp = header.get("timestamp").and_then(|t| t.as_u64())?;
            let height = header.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
            Some(ChainHostChoice {
                host,
                block_timestamp_ms: timestamp,
                height,
            })
        });
    }

    // Drain every probe rather than taking the first to answer: we want the
    // FRESHEST host, not the FASTEST one. The quickest responder is often a
    // stalled seed that simply has lower latency.
    let mut best: Option<ChainHostChoice> = None;
    while let Some(joined) = set.join_next().await {
        if let Ok(Some(choice)) = joined {
            if best
                .as_ref()
                .map(|b| choice.block_timestamp_ms > b.block_timestamp_ms)
                .unwrap_or(true)
            {
                best = Some(choice);
            }
        }
    }
    best
}

#[derive(Clone, Debug)]
pub struct ChainHostChoice {
    pub host: String,
    pub block_timestamp_ms: u64,
    pub height: u64,
}

impl ChainHostChoice {
    /// Age of this host's newest block. Surfaced so the UI can say "network
    /// last produced a block 6 days ago" instead of implying everything is
    /// fine.
    pub fn block_age_seconds(&self) -> u64 {
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        now.saturating_sub(self.block_timestamp_ms) / 1000
    }
}

/// Resolve (and cache) the chain host for read-only chain queries.
///
/// `ARC_WALLET_HOST` pins it explicitly — the documented override for
/// pointing the app at a local devnet or a specific seed. `ARC_TIER1_RPC` is
/// still honored for backward compatibility with existing dev shells, but it
/// is deliberately no longer the *first* thing checked and no longer silently
/// redirects tier 1 alone: it redirects chain reads, which is what it always
/// actually did.
async fn chain_host(state: &AppState) -> String {
    for key in ["ARC_WALLET_HOST", "ARC_TIER1_RPC"] {
        if let Ok(env) = std::env::var(key) {
            let trimmed = env.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    // Stay on the pinned host as long as it still answers.
    let pinned = {
        let cached = state.chain_host.lock().await;
        cached.as_ref().map(|(c, _)| c.host.clone())
    };
    if let Some(host) = pinned {
        if health_ok(&state.http, &host, std::time::Duration::from_secs(3)).await {
            return host;
        }
        tracing::warn!("pinned chain host {} stopped answering - re-electing", host);
    }

    match probe_chain_host(&state.http).await {
        Some(choice) => {
            let host = choice.host.clone();
            tracing::info!(
                "chain host pinned to {} (height {}, newest block {}s old) for this session",
                host,
                choice.height,
                choice.block_age_seconds()
            );
            *state.chain_host.lock().await = Some((choice, std::time::Instant::now()));
            host
        }
        None => {
            // Every seed refused or timed out. Fall back to the first
            // candidate so the caller still produces a well-formed error
            // against a real host rather than panicking on an empty string.
            tracing::warn!(
                "no seed answered /block/latest; falling back to {}",
                WALLET_HOSTS[0]
            );
            WALLET_HOSTS[0].to_string()
        }
    }
}

/// The cached chain choice, without triggering a probe. Used by
/// `node_status` to attach chain height/round/age after `chain_host` has
/// already run.
async fn cached_chain_choice(state: &AppState) -> Option<ChainHostChoice> {
    state
        .chain_host
        .lock()
        .await
        .as_ref()
        .map(|(c, _)| c.clone())
}

/// A conservative upper bound for the number of tokens produced by ARC's
/// raw-input tokenizer.
///
/// `CachedIntegerModel::encode` prepends one three-byte SentencePiece marker,
/// replaces every ASCII space with that same three-byte marker, and then emits
/// at most one token per transformed UTF-8 byte. Keeping the bound here in
/// bytes means the desktop does not need the model vocabulary merely to avoid
/// cancelling a valid coordinator request too early.
fn inference_prompt_token_upper_bound(prompt: &str) -> u64 {
    const SENTENCEPIECE_MARKER_BYTES: u64 = 3;

    prompt
        .as_bytes()
        .iter()
        .fold(SENTENCEPIECE_MARKER_BYTES, |transformed_bytes, byte| {
            transformed_bytes.saturating_add(if *byte == b' ' {
                SENTENCEPIECE_MARKER_BYTES
            } else {
                1
            })
        })
}

/// Mirror the coordinator's protocol budget: worker inference, independent
/// validator recomputation, and remote reward approvals can span three model
/// passes. The server budgets the complete generation context -- one internal
/// BOS position, the tokenized prompt, and requested output -- rather than
/// output tokens alone. Add 60 seconds beyond its capped deadline so a valid
/// settled response is never cancelled by the desktop first.
fn inference_timeout(prompt: &str, max_tokens: u32) -> std::time::Duration {
    const MIN_SERVER_SECS: u64 = 45;
    const MAX_SERVER_SECS: u64 = 3_900;
    const CLAIM_WINDOW_SECS: u64 = 30;
    const CLIENT_HEADROOM_SECS: u64 = 60;
    const INTERNAL_BOS_POSITIONS: u64 = 1;
    // One generation is budgeted at 3.3s/token. Dispatch can include the
    // worker pass, coordinator verification, and validator approval pass,
    // each with 50% headroom: ceil(14.85s * positions) + claim window.
    let required_positions = INTERNAL_BOS_POSITIONS
        .saturating_add(inference_prompt_token_upper_bound(prompt))
        .saturating_add(u64::from(max_tokens));
    let estimated_ms = required_positions.saturating_mul(14_850);
    let estimated_secs = estimated_ms.saturating_add(999) / 1_000;
    let server_secs = estimated_secs
        .saturating_add(CLAIM_WINDOW_SECS)
        .clamp(MIN_SERVER_SECS, MAX_SERVER_SECS);
    std::time::Duration::from_secs(server_secs + CLIENT_HEADROOM_SECS)
}

/// How long a coordinator gets to answer `/health` before we skip it.
const COORDINATOR_HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

fn inference_client(prompt: &str, max_tokens: u32) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(inference_timeout(prompt, max_tokens))
        .build()
        .map_err(map_err)
}

async fn health_ok(http: &reqwest::Client, host: &str, timeout: std::time::Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, http.get(format!("{}/health", host)).send()).await,
        Ok(Ok(r)) if r.status().is_success()
    )
}

/// Coordinators to try, best first.
///
/// Two changes from the original, both of which matter on this network:
///
/// 1. **The local node goes first when it is up.** It is the only node
///    running the current build — the public seeds are still on v0.7.9,
///    whose coordinator is markedly slower — and routing through a
///    datacenter to compute something the user's own machine can compute is
///    both slower and a worse story to tell.
/// 2. **Remotes are probed concurrently**, and only reachable ones are
///    returned, ordered by how fast they answered. The previous code walked
///    a fixed list sequentially with a per-host inference timeout, so an
///    unreachable host cost a full timeout before the next was tried.
async fn coordinator_candidates(state: &AppState) -> Vec<String> {
    let mut ordered = Vec::new();

    let port = state.node.lock().await.rpc_port;
    let local = paths::local_host(port);
    if health_ok(&state.http, &local, COORDINATOR_HEALTH_TIMEOUT).await {
        ordered.push(local);
    }

    let mut set = tokio::task::JoinSet::new();
    for host in COORDINATOR_HOSTS {
        let http = state.http.clone();
        let host = host.to_string();
        set.spawn(async move {
            let started = std::time::Instant::now();
            if health_ok(&http, &host, COORDINATOR_HEALTH_TIMEOUT).await {
                Some((host, started.elapsed()))
            } else {
                None
            }
        });
    }
    let mut remotes: Vec<(String, std::time::Duration)> = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(Some(hit)) = joined {
            remotes.push(hit);
        }
    }
    remotes.sort_by_key(|(_, elapsed)| *elapsed);
    ordered.extend(remotes.into_iter().map(|(h, _)| h));
    ordered
}

fn community_receipt_source_is_pinned(source_host: &str, local_host: &str) -> bool {
    source_host == local_host || COORDINATOR_HOSTS.contains(&source_host)
}

async fn pin_community_receipt_route(
    state: &AppState,
    source_host: &str,
    result: &InferenceResult,
) {
    let Some(settlement) = result.settlement.as_ref() else {
        return;
    };
    let mut routes = state.community_receipt_routes.lock().await;
    // Keep the in-memory pin set bounded without evicting the receipt just
    // returned. This state is session-local and only supports visible results.
    if routes.len() >= 512 && !routes.contains_key(&settlement.tx_hash) {
        if let Some(oldest_arbitrary_key) = routes.keys().next().cloned() {
            routes.remove(&oldest_arbitrary_key);
        }
    }
    routes.insert(
        settlement.tx_hash.clone(),
        CommunityReceiptRoute {
            source_host: source_host.to_string(),
            job_id: settlement.job_id.clone(),
            worker: settlement.worker.clone(),
            receipt_url: settlement.receipt_url.clone(),
        },
    );
}

/// Independently read one canonical 0x25 receipt from the exact host that
/// served the inference. The WebView cannot turn this into an arbitrary URL
/// fetch or silently migrate a receipt lookup to another seed: only the exact
/// current loopback node or a compiled-in coordinator origin is accepted, and
/// `rpc_client` performs one identity-bound GET with redirects disabled.
#[tauri::command]
pub async fn fetch_community_reward_receipt(
    state: State<'_, AppState>,
    source_host: String,
    tx_hash: String,
    job_id: String,
    worker: String,
    receipt_url: String,
) -> CmdResult<InferenceSettlement> {
    let local_host = paths::local_host(state.node.lock().await.rpc_port);
    if !community_receipt_source_is_pinned(&source_host, &local_host) {
        return Err(
            "reward receipt unavailable: source host is not the exact local node or a compiled-in ARC coordinator"
                .to_string(),
        );
    }
    let pinned = state
        .community_receipt_routes
        .lock()
        .await
        .get(&tx_hash)
        .cloned()
        .ok_or_else(|| {
            "reward receipt unavailable: no native inference route is pinned for this transaction"
                .to_string()
        })?;
    if pinned.source_host != source_host
        || pinned.job_id != job_id
        || pinned.worker != worker
        || pinned.receipt_url != receipt_url
    {
        return Err(
            "reward receipt unavailable: requested identity differs from the native inference route pin"
                .to_string(),
        );
    }
    rpc_client::fetch_community_reward_receipt(
        &state.http,
        &source_host,
        &tx_hash,
        &job_id,
        &worker,
        &receipt_url,
    )
    .await
}

/// Run inference on the local node.
///
/// Kept as its own command so the UI can show "served by your machine"
/// truthfully. Previously this went to the LAX seed via `wallet_host`, while
/// the Inference screen's help text claimed "your prompt goes to the local
/// node" — it did not.
#[tauri::command]
pub async fn run_inference(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    chat_template: Option<bool>,
) -> CmdResult<InferenceResult> {
    let port = state.node.lock().await.rpc_port;
    let host = paths::local_host(port);
    let max_tokens = max_tokens.unwrap_or(32);
    let client = inference_client(&prompt, max_tokens)?;
    let mut result = rpc_client::run_inference(
        &client,
        &host,
        &prompt,
        max_tokens,
        chat_template.unwrap_or(true),
    )
    .await?;
    result.served_locally = true;
    pin_community_receipt_route(&state, &host, &result).await;
    Ok(result)
}

/// Milestone A (#35): observer / no-model nodes route inference through a
/// testnet seed coordinator's `/inference/run_consensus` endpoint.
///
/// Iterates the built-in `COORDINATOR_HOSTS` list until one seed responds
/// with success. Each request uses the same token-scaled deadline as the
/// coordinator plus client headroom, so reward verification can settle
/// without the desktop cancelling first.
///
/// This command is intentionally separate from `run_inference` so the UI
/// can try the local node first (fast path when the user is a validator
/// with --model loaded) and fall back here on 503 / network error without
/// the Rust side having to know about the local node's role.
#[tauri::command]
pub async fn run_inference_via_coordinator(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    k: Option<u32>,
    chat_template: Option<bool>,
) -> CmdResult<InferenceResult> {
    let max_tokens = max_tokens.unwrap_or(32);
    let client = inference_client(&prompt, max_tokens)?;
    let k = k.unwrap_or(3);
    let chat_template = chat_template.unwrap_or(true);

    let candidates = coordinator_candidates(&state).await;
    if candidates.is_empty() {
        return Err("no coordinator answered /health - check your internet connection".into());
    }
    let local_prefix = paths::local_host(state.node.lock().await.rpc_port);

    let mut last_err = String::new();
    for host in &candidates {
        match rpc_client::run_inference_consensus(
            &client,
            host,
            &prompt,
            max_tokens,
            k,
            chat_template,
        )
        .await
        {
            Ok(mut r) => {
                r.served_locally = *host == local_prefix;
                pin_community_receipt_route(&state, host, &r).await;
                return Ok(r);
            }
            Err(e) => last_err = e,
        }
    }
    Err(format!(
        "all {} reachable coordinators failed; last: {}",
        candidates.len(),
        last_err
    ))
}

fn direct_inference_error_must_not_retry(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("may still settle")
        || normalized.contains("refusing a second late")
        || normalized.contains("query its job status")
        || [400, 401, 403, 409, 413, 422, 504]
            .iter()
            .any(|status| normalized.contains(&format!("http {status}")))
}

/// Exact prefix of the aggregate "every reachable coordinator declined the
/// direct community path" error.
///
/// `Inference.tsx` switches to the standalone `/run_consensus` fallback only on
/// this literal substring, so it must stay verbatim in the produced message.
/// It previously read `all {n} reachable coordinators failed (direct path)`,
/// which never matched, so the fallback was unreachable in the packaged app.
const DIRECT_PATH_EXHAUSTED_SENTINEL: &str = "all coordinators failed (direct path)";

fn direct_path_exhausted_error(reachable: usize, last_error: &str) -> String {
    format!("{DIRECT_PATH_EXHAUSTED_SENTINEL}; {reachable} reachable, last: {last_error}")
}

#[cfg(test)]
mod inference_retry_tests {
    use super::{
        community_receipt_source_is_pinned, direct_inference_error_must_not_retry,
        direct_path_exhausted_error, COORDINATOR_HOSTS, DIRECT_PATH_EXHAUSTED_SENTINEL,
    };

    #[test]
    fn exhausted_direct_path_error_carries_the_ui_consensus_fallback_sentinel() {
        let message = direct_path_exhausted_error(3, "HTTP 503: no eligible workers");
        assert!(message.contains(DIRECT_PATH_EXHAUSTED_SENTINEL));
        assert!(message.contains("3 reachable"));
        assert!(message.contains("HTTP 503: no eligible workers"));

        // The Inference screen only falls through to `/run_consensus` when it
        // recognizes this exact substring. An aggregate error that does not
        // contain it silently removes the sharded-consensus fallback in the
        // packaged app, where the browser mock's wording is never used.
        let ui = include_str!("../../src/screens/Inference.tsx");
        assert!(
            ui.contains(&format!(
                "message.includes(\"{DIRECT_PATH_EXHAUSTED_SENTINEL}\")"
            )),
            "Inference.tsx no longer gates its consensus fallback on the native sentinel"
        );
    }

    #[test]
    fn claimed_or_terminal_direct_failures_never_retry_elsewhere() {
        assert!(direct_inference_error_must_not_retry(
            "HTTP 504: assignment may still settle; query its job status"
        ));
        assert!(direct_inference_error_must_not_retry(
            "HTTP 422: invalid input"
        ));
        assert!(!direct_inference_error_must_not_retry(
            "HTTP 503: no eligible workers or shard topology"
        ));
        assert!(!direct_inference_error_must_not_retry(
            "connection refused before dispatch"
        ));
    }

    #[test]
    fn reward_receipt_source_is_exactly_allowlisted_without_prefix_or_path_matches() {
        let local = "http://127.0.0.1:9090";
        assert!(community_receipt_source_is_pinned(local, local));
        assert!(community_receipt_source_is_pinned(
            COORDINATOR_HOSTS[0],
            local
        ));
        assert!(!community_receipt_source_is_pinned(
            "https://149.28.32.76.evil.example",
            local
        ));
        assert!(!community_receipt_source_is_pinned(
            "https://149.28.32.76/community/reward_receipt/anything",
            local
        ));
        assert!(!community_receipt_source_is_pinned(
            "http://127.0.0.1:9091",
            local
        ));
    }
}

/// Community-first coordinator inference. `/inference/run` dispatches to an
/// eligible registered community worker before it considers local or sharded
/// seed execution. The UI calls this before standalone `/run_consensus` so
/// normal desktop traffic can actually reach community nodes.
///
/// Errors proving that a community assignment may still settle, plus terminal
/// client/request errors, stop immediately. Retrying another coordinator in
/// those cases could duplicate compute or a reward.
#[tauri::command]
pub async fn run_inference_via_coordinator_direct(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    chat_template: Option<bool>,
) -> CmdResult<InferenceResult> {
    let max_tokens = max_tokens.unwrap_or(32);
    let client = inference_client(&prompt, max_tokens)?;
    let chat_template = chat_template.unwrap_or(true);

    let candidates = coordinator_candidates(&state).await;
    if candidates.is_empty() {
        return Err("no coordinator answered /health - check your internet connection".into());
    }
    let local_prefix = paths::local_host(state.node.lock().await.rpc_port);

    let mut last_err = String::new();
    for host in &candidates {
        match rpc_client::run_inference_remote(&client, host, &prompt, max_tokens, chat_template)
            .await
        {
            Ok(mut r) => {
                r.served_locally = *host == local_prefix;
                pin_community_receipt_route(&state, host, &r).await;
                return Ok(r);
            }
            Err(e) if direct_inference_error_must_not_retry(&e) => return Err(e),
            Err(e) => last_err = e,
        }
    }
    Err(direct_path_exhausted_error(candidates.len(), &last_err))
}

const SETTLEMENT_WRITE_UNAVAILABLE: &str = "is unavailable in the v0.8.0 recovery candidate before any transaction is signed or submitted: exact model-artifact binding, validator-authenticated authorization, and settlement are not production-ready. VRF selection and server-derived replica labels are not validator approval. Free/community inference remains available.";

fn settlement_write_unavailable<T>(flow: &str) -> CmdResult<T> {
    Err(format!("{} {}", flow, SETTLEMENT_WRITE_UNAVAILABLE))
}

/// Tier 1 settlement is intentionally unavailable in this recovery candidate.
///
/// This command used to derive a model ID from a static shape label, use VRF
/// selection as though it authorized spend, and submit an
/// `InferenceRequest` before the validator-approval protocol was complete.
/// Its body now returns a typed error without probing a host, reading a nonce,
/// signing a transaction, or performing any network write.
#[tauri::command]
pub async fn tier1_submit(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    max_reward: Option<u64>,
    deadline_blocks: Option<u64>,
    committee_size: Option<u8>,
) -> CmdResult<rpc_client::Tier1Submitted> {
    let _ = (
        state,
        prompt,
        max_tokens,
        max_reward,
        deadline_blocks,
        committee_size,
    );
    settlement_write_unavailable("Tier 1 on-chain inference")
}

/// Read the on-chain state of a Tier 1 request created by an older build.
/// The current desktop does not submit or poll new requests. This read-only
/// compatibility path looks up the host that accepted the original submit from
/// `state.tier1_routes`; if missing (e.g. app restart between submit and
/// poll), falls back to scanning every host.
#[tauri::command]
pub async fn tier1_result(
    state: State<'_, AppState>,
    request_id: String,
) -> CmdResult<rpc_client::Tier1Result> {
    let pinned = state.tier1_routes.lock().await.get(&request_id).cloned();
    if let Some(host) = pinned {
        return rpc_client::tier1_result(&state.http, &host, &request_id).await;
    }
    let mut last_err = String::from("no tier1 hosts configured");
    for host in tier1_candidate_hosts() {
        match rpc_client::tier1_result(&state.http, &host, &request_id).await {
            Ok(r) => {
                state
                    .tier1_routes
                    .lock()
                    .await
                    .insert(request_id.clone(), host);
                return Ok(r);
            }
            Err(e) => last_err = format!("{}: {}", host, e),
        }
    }
    Err(format!(
        "tier1_result not found on any host; last: {}",
        last_err
    ))
}

/// Tier 1 RPC host candidates in the order to try them. Honors
/// `ARC_TIER1_RPC` (single host, for local dev). Otherwise shuffles
/// `COORDINATOR_HOSTS` so load spreads across the 6 testnet seeds and a
/// dead host (e.g. NYC = 149.28.32.76 was unreachable as of 2026-05-22)
/// just causes one extra hop instead of a permanent failure.
fn tier1_candidate_hosts() -> Vec<String> {
    if let Ok(env) = std::env::var("ARC_TIER1_RPC") {
        let trimmed = env.trim();
        if !trimmed.is_empty() {
            return vec![trimmed.to_string()];
        }
    }
    use rand::seq::SliceRandom;
    let mut hosts: Vec<String> = COORDINATOR_HOSTS.iter().map(|s| s.to_string()).collect();
    hosts.shuffle(&mut rand::thread_rng());
    hosts
}

/// Candidate hosts for free coordinator inference and read-only compatibility
/// queries. New paid/Tier 1 request writes are disabled above.
const COORDINATOR_HOSTS: [&str; 6] = rpc_client::PRODUCTION_RPC_ORIGINS;

/// The public testnet seeds, as candidates for chain reads.
///
/// No longer a priority list with a pinned `[0]` — `chain_host()` elects
/// among these by block freshness on every TTL expiry. Order is
/// presentational only.
const WALLET_HOSTS: [&str; 6] = rpc_client::PRODUCTION_RPC_ORIGINS;

/// Paid inference escrow is intentionally unavailable in this recovery
/// candidate.
///
/// The removed implementation opened escrow using a label-derived model ID
/// before asking the coordinator to run the exact artifact. A candidate
/// coordinator then rejected the mismatch, leaving funds locked until timeout.
/// This command now returns an error before identity access, host probing,
/// signing, nonce reads, transaction submission, or any other network write.
#[tauri::command]
pub async fn run_paid_inference(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    max_fee: Option<u64>,
    k: Option<u32>,
) -> CmdResult<PaidInferenceResult> {
    let _ = (state, prompt, max_tokens, max_fee, k);
    settlement_write_unavailable("Paid inference escrow")
}

// `check_for_update` (GitHub releases API) was deleted here deliberately.
//
// It was a second, independent notion of "is there an update" that
// disagreed with the one the Install button actually used. Settings rendered
// the button from this command's `tag_name != CARGO_PKG_VERSION`, but
// clicking it called the Tauri updater's `check()`, which reads the signed
// `latest.json`. Any tag that ships arc-node binaries without a desktop
// bundle — exactly what a normal tag push produces — advanced `tag_name`
// while publishing no manifest, so the app offered an update and then
// reported "No update available." It was also an unauthenticated
// api.github.com call subject to a 60/hr rate limit and the first thing to
// fail behind a corporate proxy.
//
// Both the badge and the button now come from the updater plugin, which
// reads the signed manifest and is the only source that can actually
// install anything. The version string for display comes off the `Update`
// object. See `Settings.tsx`.

/// Write the in-memory log ring to a file the user picks.
///
/// The Download button built a `Blob`, made an `<a download>` and clicked it.
/// WKWebView — the macOS webview — does not implement the `download`
/// attribute for `blob:` URLs without a host-side download delegate, so the
/// click was a silent no-op on macOS while appearing to work on Windows and
/// Linux. Handing logs to support is the whole point of the button, so the
/// failure was both invisible and consequential. Doing the write in Rust
/// works identically everywhere.
#[tauri::command]
pub async fn save_logs(app: AppHandle, state: State<'_, AppState>) -> CmdResult<SavedLogs> {
    use tauri_plugin_dialog::DialogExt;

    let entries = {
        let node = state.node.lock().await;
        node.logs_snapshot(5000).await
    };
    let body = entries
        .iter()
        .map(|l| {
            let ts = chrono::DateTime::from_timestamp_millis(l.timestamp)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| l.timestamp.to_string());
            format!("[{}] {:<5} {}", ts, l.level.to_uppercase(), l.message)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let default_name = format!(
        "arc-node-{}.log",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );

    // The dialog plugin's blocking picker would deadlock the async runtime;
    // hop it onto a oneshot instead.
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(&default_name)
        .add_filter("Log file", &["log", "txt"])
        .save_file(move |path| {
            let _ = tx.send(path);
        });
    let picked = rx.await.map_err(map_err)?;

    let Some(path) = picked else {
        // User cancelled - not an error.
        return Ok(SavedLogs {
            path: None,
            lines: entries.len(),
        });
    };
    let path: PathBuf = path
        .into_path()
        .map_err(|e| format!("could not resolve the chosen path: {}", e))?;

    std::fs::write(&path, body).map_err(|e| format!("write {}: {}", path.display(), e))?;

    Ok(SavedLogs {
        path: Some(path.to_string_lossy().into_owned()),
        lines: entries.len(),
    })
}

/// Change how many cores the node contributes.
///
/// Tries the cheap path first: `POST /node/threads` on the local node
/// resizes rayon's pool in place, so "add two cores" takes effect without
/// dropping the node off the network. That endpoint is being added
/// chain-side and does not exist on shipped nodes yet, so a 404 (or any
/// other refusal) falls back to a graceful restart with the new width. Both
/// outcomes are reported honestly via `ThreadsApplied.restarted`, because
/// "applied live" and "restarted your node" are very different things to
/// have just done to someone.
#[tauri::command]
pub async fn set_worker_threads(
    app: AppHandle,
    state: State<'_, AppState>,
    threads: u32,
) -> CmdResult<ThreadsApplied> {
    let cores = cached_hardware().cpu_cores.max(1);
    if threads == 0 || threads > cores {
        return Err(format!(
            "core count must be between 1 and {} on this machine",
            cores
        ));
    }

    // Persist first, so whichever path we take below (live reconfigure or
    // restart) the new width survives — including if the user quits before
    // it is applied. `restart_node` re-reads the config from the store, so
    // this write is what it will pick up.
    let was_running = {
        let mut store = state.store.lock().await;
        let mut cfg = store.config.clone().unwrap_or_default();
        cfg.worker_threads = Some(threads);
        store.config = Some(cfg);
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
        state.node.lock().await.is_running()
    };

    if !was_running {
        return Ok(ThreadsApplied {
            worker_threads: threads,
            restarted: false,
            message: format!("Saved. The node will use {} cores when it starts.", threads),
        });
    }

    // Attempt the live reconfigure.
    let port = state.node.lock().await.rpc_port;
    let url = format!("{}/node/threads", paths::local_host(port));
    let live = state
        .http
        .post(&url)
        .json(&serde_json::json!({ "threads": threads }))
        .send()
        .await;
    match live {
        Ok(r) if r.status().is_success() => {
            let mut node = state.node.lock().await;
            node.active_worker_threads = Some(threads);
            return Ok(ThreadsApplied {
                worker_threads: threads,
                restarted: false,
                message: format!("Now contributing {} cores (applied live).", threads),
            });
        }
        Ok(r) => tracing::info!(
            "POST /node/threads returned {} - falling back to a restart",
            r.status()
        ),
        Err(e) => tracing::info!(
            "POST /node/threads failed ({}) - falling back to a restart",
            e
        ),
    }

    restart_node(app, state).await?;
    Ok(ThreadsApplied {
        worker_threads: threads,
        restarted: true,
        message: format!("Restarted the node with {} cores.", threads),
    })
}

/// Desktop and arc-node ship as a matched pair - the desktop's CARGO_PKG_VERSION
/// is the same string arc-node prints from `--version` (both inherit from the
/// release tag's workspace version). Mismatch → we have a stale arc-node from
/// a previous release sitting in ~/.arc/bin and must redownload, otherwise
/// chain bug fixes never reach existing users on auto-update.
pub(crate) const EXPECTED_NODE_VERSION: &str = env!("CARGO_PKG_VERSION");
const ARC_RELEASE_DOWNLOAD_ROOT: &str = "https://github.com/FerrumVir/arc-chain/releases/download";
const ARC_RELEASE_REPOSITORY: &str = "FerrumVir/arc-chain";
const ARC_RELEASE_MANIFEST_NAMESPACE: &str = "arc-release-manifest-v1";
const ARC_RELEASE_ALLOWED_SIGNERS: &str =
    include_str!("../../../release/arc-release-allowed-signers");
const MAX_NODE_BINARY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CHECKSUM_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_MANIFEST_SIGNATURE_BYTES: usize = 128 * 1024;
const MAX_LOCAL_HEALTH_BYTES: usize = 64 * 1024;
const BINARY_INSTALL_TRANSACTION_SCHEMA: &str = "arc-node-install-transaction-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalNodeCompatibility {
    Absent,
    Exact,
    Incompatible(String),
}

fn classify_local_health(
    value: &serde_json::Value,
    expected_version: &str,
) -> LocalNodeCompatibility {
    match value.get("version").and_then(serde_json::Value::as_str) {
        Some(version) if version == expected_version => LocalNodeCompatibility::Exact,
        Some(version) => LocalNodeCompatibility::Incompatible(format!(
            "local /health reports arc-node v{version}; desktop requires v{expected_version}"
        )),
        None => LocalNodeCompatibility::Incompatible(
            "local /health has no parseable arc-node version".to_string(),
        ),
    }
}

/// Probe the configured local RPC port before startup adopts a process it did
/// not spawn. Only an exact matched-pair version is adoptable. Connection
/// failure means no process is present; any successful but malformed, stale, or
/// future response is incompatible and the desktop must start its own node.
pub(crate) async fn probe_local_node_compatibility(
    http: &reqwest::Client,
    port: u16,
) -> LocalNodeCompatibility {
    let url = format!("{}/health", paths::local_host(port));
    let response = match http.get(&url).send().await {
        Ok(response) => response,
        Err(_) => return LocalNodeCompatibility::Absent,
    };
    if !response.status().is_success() {
        return LocalNodeCompatibility::Incompatible(format!(
            "local /health returned HTTP {}",
            response.status()
        ));
    }
    let bytes =
        match read_bounded_release_body(response, MAX_LOCAL_HEALTH_BYTES, "local /health").await {
            Ok(bytes) => bytes,
            Err(error) => return LocalNodeCompatibility::Incompatible(error),
        };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => classify_local_health(&value, EXPECTED_NODE_VERSION),
        Err(_) => LocalNodeCompatibility::Incompatible(
            "local /health did not return a JSON object".to_string(),
        ),
    }
}

fn exact_release_asset_url(asset: &str) -> String {
    format!(
        "{}/v{}/{}",
        ARC_RELEASE_DOWNLOAD_ROOT, EXPECTED_NODE_VERSION, asset
    )
}

/// Read one GNU/BSD-style SHA256SUMS entry without accepting an ambiguous or
/// path-substituted match. The release assembler emits exactly this shape.
fn expected_release_sha256(manifest: &str, asset: &str) -> Result<[u8; 32], String> {
    let mut matches = manifest.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let filename = fields.next()?;
        if fields.next().is_some() || filename.trim_start_matches('*') != asset {
            return None;
        }
        Some(digest)
    });

    let digest = matches
        .next()
        .ok_or_else(|| format!("SHA256SUMS has no entry for {}", asset))?;
    if matches.next().is_some() {
        return Err(format!("SHA256SUMS has more than one entry for {}", asset));
    }
    let decoded =
        hex::decode(digest).map_err(|_| format!("SHA256SUMS has invalid hex for {}", asset))?;
    decoded
        .try_into()
        .map_err(|_| format!("SHA256SUMS has a non-SHA-256 digest for {}", asset))
}

fn release_manifest_public_key(allowed_signers: &str) -> Result<PublicKey, String> {
    let mut records = allowed_signers
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let record = records
        .next()
        .ok_or_else(|| "embedded ARC release signer list is empty".to_string())?;
    if records.next().is_some() {
        return Err("embedded ARC release signer list must contain exactly one key".to_string());
    }
    let fields: Vec<_> = record.split_whitespace().collect();
    if fields.len() < 4
        || fields[0] != "arc-release"
        || fields[1] != "namespaces=\"arc-release-manifest-v1\""
        || fields[2] != "ssh-ed25519"
    {
        return Err("embedded ARC release signer policy is malformed".to_string());
    }
    PublicKey::from_openssh(&format!("{} {}", fields[2], fields[3]))
        .map_err(|error| format!("embedded ARC release public key is invalid: {error}"))
}

fn validate_release_manifest_binding(manifest: &str, expected_version: &str) -> Result<(), String> {
    let mut lines = manifest.lines();
    if lines.next() != Some("# ARC release manifest v1") {
        return Err("release checksum manifest has no supported ARC schema header".to_string());
    }
    let expected_repository = format!("# repository={ARC_RELEASE_REPOSITORY}");
    if lines.next() != Some(expected_repository.as_str()) {
        return Err("release checksum manifest is bound to a different repository".to_string());
    }
    let expected_tag = format!("# tag=v{expected_version}");
    if lines.next() != Some(expected_tag.as_str()) {
        return Err(format!(
            "release checksum manifest is not bound to exact tag v{expected_version}"
        ));
    }
    let commit = lines
        .next()
        .and_then(|line| line.strip_prefix("# commit="))
        .ok_or_else(|| "release checksum manifest has no commit binding".to_string())?;
    if commit.len() != 40
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("release checksum manifest commit is not lowercase 40-byte hex".to_string());
    }
    Ok(())
}

fn verify_release_manifest_signature_with_signers(
    manifest: &[u8],
    signature: &[u8],
    allowed_signers: &str,
    expected_version: &str,
) -> Result<(), String> {
    let manifest_text = std::str::from_utf8(manifest)
        .map_err(|_| "release checksum manifest is not UTF-8".to_string())?;
    validate_release_manifest_binding(manifest_text, expected_version)?;
    let public_key = release_manifest_public_key(allowed_signers)?;
    let ssh_signature = SshSig::from_pem(signature)
        .map_err(|error| format!("release SHA256SUMS.sig is malformed: {error}"))?;
    public_key
        .verify(ARC_RELEASE_MANIFEST_NAMESPACE, manifest, &ssh_signature)
        .map_err(|_| "release SHA256SUMS signature is invalid or not owner-authorized".to_string())
}

fn verify_release_manifest_signature(manifest: &[u8], signature: &[u8]) -> Result<(), String> {
    verify_release_manifest_signature_with_signers(
        manifest,
        signature,
        ARC_RELEASE_ALLOWED_SIGNERS,
        EXPECTED_NODE_VERSION,
    )
}

async fn read_bounded_release_body(
    mut response: reqwest::Response,
    maximum: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(format!("{label} exceeds its {}-byte safety limit", maximum));
    }
    let capacity = response
        .content_length()
        .unwrap_or_default()
        .min(maximum as u64) as usize;
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await.map_err(map_err)? {
        if chunk.len() > maximum.saturating_sub(body.len()) {
            return Err(format!("{label} exceeds its {}-byte safety limit", maximum));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn binary_download_sidecar(target: &Path, nonce: u64) -> PathBuf {
    let suffix = format!("download-{}-{nonce:016x}", std::process::id());
    match target.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => target.with_extension(format!("{suffix}.{extension}")),
        None => target.with_extension(suffix),
    }
}

/// One independently created download. `create_new` prevents a concurrent
/// desktop instance (or a stale symbolic link) from sharing/truncating the
/// file whose signed digest this invocation is verifying. Drop is a cleanup
/// backstop for every network, checksum, version, and install error path.
struct PendingVerifiedDownload {
    path: PathBuf,
    file: Option<tokio::fs::File>,
}

impl PendingVerifiedDownload {
    fn file_mut(&mut self) -> Result<&mut tokio::fs::File, String> {
        self.file
            .as_mut()
            .ok_or_else(|| "arc-node download sidecar is already closed".to_string())
    }

    fn close(&mut self) {
        self.file.take();
    }
}

impl Drop for PendingVerifiedDownload {
    fn drop(&mut self) {
        // Close before unlinking: Windows refuses removal of an open file.
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn create_binary_download_sidecar(target: &Path) -> Result<PendingVerifiedDownload, String> {
    for _ in 0..32 {
        let path = binary_download_sidecar(target, rand::random::<u64>());
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&path).await {
            Ok(file) => {
                return Ok(PendingVerifiedDownload {
                    path,
                    file: Some(file),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create {}: {}", path.display(), error)),
        }
    }
    Err(format!(
        "could not allocate an isolated arc-node download beside {}",
        target.display()
    ))
}

fn binary_install_lock_path(target: &Path) -> PathBuf {
    target.with_extension("install.lock")
}

fn model_download_lock_path(target: &Path) -> PathBuf {
    target.with_extension("download.lock")
}

async fn acquire_exclusive_download_lock(
    lock_path: PathBuf,
    label: &'static str,
) -> Result<std::fs::File, String> {
    tokio::task::spawn_blocking(move || {
        match std::fs::symlink_metadata(&lock_path) {
            Ok(metadata)
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() =>
            {
                return Err(format!(
                    "refusing {label} lock that is not a regular file: {}",
                    lock_path.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("inspect {}: {}", lock_path.display(), error)),
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(&lock_path)
            .map_err(|error| format!("open {}: {}", lock_path.display(), error))?;
        file.lock_exclusive()
            .map_err(|error| format!("lock {}: {}", lock_path.display(), error))?;
        Ok(file)
    })
    .await
    .map_err(map_err)?
}

/// Serialize the complete inspect/download/verify/replace sequence across
/// both tasks and desktop processes. The OS releases this advisory lock if a
/// process crashes, so a stale lock file never strands future updates.
async fn acquire_binary_install_lock(target: &Path) -> Result<std::fs::File, String> {
    acquire_exclusive_download_lock(binary_install_lock_path(target), "arc-node install").await
}

async fn acquire_model_download_lock(target: &Path) -> Result<std::fs::File, String> {
    acquire_exclusive_download_lock(model_download_lock_path(target), "model download").await
}

/// First-launch readiness check. Confirms the bundled testnet resources are
/// resolvable AND the arc-node binary is present at the version this desktop
/// was built against. If the binary is missing OR its `--version` doesn't
/// match this desktop's `CARGO_PKG_VERSION`, downloads the matching arc-node
/// binary from that exact immutable release for this platform. The onboarding
/// screen calls this before launching the node, and `start_node` also calls
/// it on every start so existing users picked up by the desktop auto-updater
/// always get the matching arc-node binary instead of running a stale one.
#[tauri::command]
pub async fn ensure_binary(app: AppHandle, state: State<'_, AppState>) -> CmdResult<BinaryStatus> {
    require_data_migration_ready(&state).await?;
    let configured_data_dir = state
        .store
        .lock()
        .await
        .config
        .clone()
        .unwrap_or_default()
        .data_dir;
    let lock_data_dir = configured_data_dir.clone();
    let _lifecycle_lock = tokio::task::spawn_blocking(move || {
        crate::node_manager::acquire_managed_lifecycle_lock(&lock_data_dir)
    })
    .await
    .map_err(map_err)?
    .map_err(map_err)?;
    if crate::node_manager::managed_shutdown_recovery_required(&configured_data_dir)
        .map_err(map_err)?
    {
        return Err(
            "cannot replace arc-node while a prior shutdown receipt is unresolved; start the exact installed node and complete one authenticated clean shutdown first"
                .into(),
        );
    }
    ensure_binary_inner(&app).await
}

fn installed(path: &Path) -> BinaryStatus {
    BinaryStatus {
        path: path.to_string_lossy().into_owned(),
        downloaded_bytes: 0,
        total_bytes: 0,
        already_installed: true,
    }
}

/// Make sure a runnable arc-node exists at the exact desktop version.
///
/// v0.7.10 and v0.7.11 were published without `arc-node-*` assets, which made
/// every refresh return 404. Continuing with an older managed binary looked
/// friendlier but silently paired incompatible protocols after upgrades. The
/// unified release now makes a missing exact asset/checksum a publication
/// failure; the desktop therefore fails closed instead of pretending a stale
/// node is current. Operators who intentionally maintain a custom binary can
/// select it explicitly with `ARC_NODE_BIN`.
///
/// Resolution order mirrors `node_manager::resolve_binary` so the thing this
/// function blesses is the thing that actually gets spawned.
async fn ensure_binary_inner(app: &AppHandle) -> Result<BinaryStatus, String> {
    // 1. An explicitly configured binary is the operator's decision. Never
    //    version-check it, never overwrite it.
    if let Some(p) = crate::node_manager::env_binary_override() {
        if p.exists() {
            tracing::info!("using arc-node from env override: {}", p.display());
            return Ok(installed(&p));
        }
        return Err(format!(
            "ARC_NODE_BIN points at {}, which does not exist",
            p.display()
        ));
    }

    let target = managed_binary_path();
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(map_err)?;
    }

    static ENSURE_BINARY_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    let _task_guard = ENSURE_BINARY_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _process_guard = acquire_binary_install_lock(&target).await?;

    // Complete or roll back a journaled Windows replacement before deciding
    // whether the managed binary is missing/stale. This also restores `.old`
    // files left by pre-journal desktop versions, so a power cut cannot turn
    // a complete prior executable into an opaque "missing binary" state.
    let recovery_target = target.clone();
    tokio::task::spawn_blocking(move || {
        recover_interrupted_binary_install(&recovery_target, EXPECTED_NODE_VERSION)
    })
    .await
    .map_err(map_err)??;

    // 2. A binary already in the managed location. Exact matches are reused;
    //    older copies are replaced, and newer/unparseable copies stop with a
    //    clear mismatch instead of silently coupling incompatible versions.
    if target.exists() {
        match read_arc_node_version(&target) {
            Some(ref v) if v == EXPECTED_NODE_VERSION => return Ok(installed(&target)),
            Some(v) => {
                if semver_gt(EXPECTED_NODE_VERSION, &v) {
                    tracing::info!(
                        "arc-node {} at {} is older than this desktop's {} - attempting refresh",
                        v,
                        target.display(),
                        EXPECTED_NODE_VERSION
                    );
                    // Fall through to the download attempt below.
                } else if semver_gt(&v, EXPECTED_NODE_VERSION) {
                    return Err(format!(
                        "managed arc-node v{} is newer than desktop v{}. Upgrade the desktop, or set ARC_NODE_BIN explicitly if this pairing is intentional",
                        v, EXPECTED_NODE_VERSION
                    ));
                } else {
                    return Err(format!(
                        "managed arc-node at {} reports unrecognized version '{}'; expected v{}",
                        target.display(),
                        v,
                        EXPECTED_NODE_VERSION
                    ));
                }
            }
            None => {
                tracing::warn!(
                    "arc-node at {} did not report a parseable --version - attempting refresh",
                    target.display()
                );
            }
        }
    } else if let Some(dev) = crate::node_manager::dev_build_binary() {
        // 3. A release build in this repo checkout. This is how the demo
        //    machine runs: the checkout has a matching arc-node while the
        //    published release has none.
        tracing::info!("using locally built arc-node: {}", dev.display());
        return Ok(installed(&dev));
    }

    // 4. Download the exact release. An older or corrupt managed binary is not
    //    a valid fallback across a protocol-version boundary.
    match download_arc_node(&target).await {
        Ok(total_bytes) => {
            let _ = app; // reserved for progress events via app.emit(...)
            Ok(BinaryStatus {
                path: target.to_string_lossy().into_owned(),
                downloaded_bytes: total_bytes,
                total_bytes,
                already_installed: false,
            })
        }
        Err(e) => {
            // Last chance: a dev build we skipped earlier because the
            // managed path existed but turned out unusable.
            if let Some(dev) = crate::node_manager::dev_build_binary() {
                tracing::warn!(
                    "arc-node download failed ({}) - falling back to {}",
                    e,
                    dev.display()
                );
                return Ok(installed(&dev));
            }
            Err(format!(
                "{}. No arc-node is available to run. Build one with \
                 `cargo build --release -p arc-node` in the arc-chain checkout, or set \
                 ARC_NODE_BIN to an existing binary.",
                e
            ))
        }
    }
}

/// Fetch the platform's arc-node release asset and install it at `target`.
/// Returns the byte count on success.
async fn download_arc_node(target: &Path) -> Result<u64, String> {
    let asset = platform_release_asset().ok_or_else(|| {
        format!(
            "no prebuilt arc-node binary for platform {}-{}; build from source with \
             `cargo build --release -p arc-node`",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let url = exact_release_asset_url(asset);
    let checksum_url = exact_release_asset_url("SHA256SUMS");
    let checksum_signature_url = exact_release_asset_url("SHA256SUMS.sig");

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(map_err)?;
    }

    let client = reqwest::Client::builder()
        .user_agent(format!("arc-desktop/{}", EXPECTED_NODE_VERSION))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(map_err)?;

    let checksum_resp = client.get(&checksum_url).send().await.map_err(map_err)?;
    if !checksum_resp.status().is_success() {
        return Err(format!(
            "release checksum manifest returned HTTP {} for v{}",
            checksum_resp.status(),
            EXPECTED_NODE_VERSION
        ));
    }
    let checksum_bytes = read_bounded_release_body(
        checksum_resp,
        MAX_CHECKSUM_MANIFEST_BYTES,
        "release checksum manifest",
    )
    .await?;
    let signature_resp = client
        .get(&checksum_signature_url)
        .send()
        .await
        .map_err(map_err)?;
    if !signature_resp.status().is_success() {
        return Err(format!(
            "release checksum signature returned HTTP {} for v{}",
            signature_resp.status(),
            EXPECTED_NODE_VERSION
        ));
    }
    let signature_bytes = read_bounded_release_body(
        signature_resp,
        MAX_MANIFEST_SIGNATURE_BYTES,
        "release checksum signature",
    )
    .await?;
    // The checksum text is attacker-controlled until this succeeds. Verify the
    // namespaced owner signature and exact repo/tag/commit header before using
    // even one digest from it to authenticate an executable child process.
    verify_release_manifest_signature(&checksum_bytes, &signature_bytes)?;
    let checksum_manifest = std::str::from_utf8(&checksum_bytes)
        .map_err(|_| "release checksum manifest is not UTF-8".to_string())?;
    let expected_sha256 = expected_release_sha256(checksum_manifest, asset)?;

    let mut resp = client.get(&url).send().await.map_err(map_err)?;
    if !resp.status().is_success() {
        return Err(format!(
            "release asset {} returned HTTP {}",
            asset,
            resp.status()
        ));
    }
    if resp
        .content_length()
        .is_some_and(|length| length > MAX_NODE_BINARY_BYTES)
    {
        return Err(format!(
            "release asset {} exceeds the 512 MiB safety limit",
            asset
        ));
    }
    let mut pending = create_binary_download_sidecar(target).await?;
    let mut hasher = Sha256::new();
    let mut total_bytes = 0u64;
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                return Err(format!(
                    "release asset {} failed after {} bytes: {}",
                    asset, total_bytes, error
                ));
            }
        };
        total_bytes = total_bytes.saturating_add(chunk.len() as u64);
        if total_bytes > MAX_NODE_BINARY_BYTES {
            return Err(format!(
                "release asset {} exceeds the 512 MiB safety limit",
                asset
            ));
        }
        hasher.update(&chunk);
        if let Err(error) = pending.file_mut()?.write_all(&chunk).await {
            return Err(format!("write {}: {}", pending.path.display(), error));
        }
    }
    pending.file_mut()?.flush().await.map_err(map_err)?;
    pending.file_mut()?.sync_all().await.map_err(map_err)?;
    pending.close();

    let actual_sha256: [u8; 32] = hasher.finalize().into();
    if actual_sha256 != expected_sha256 {
        return Err(format!(
            "checksum verification failed for {} (expected {}, got {})",
            asset,
            hex::encode(expected_sha256),
            hex::encode(actual_sha256)
        ));
    }

    // Verify the durable file, not only the network byte stream. The unique
    // create-new sidecar means this digest belongs solely to this invocation.
    let persisted_path = pending.path.clone();
    let persisted_sha256 =
        tokio::task::spawn_blocking(move || sha256_regular_file(&persisted_path))
            .await
            .map_err(map_err)??;
    if persisted_sha256 != Some(expected_sha256) {
        return Err(format!(
            "durable checksum verification failed for {}",
            asset
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&pending.path, perms).map_err(map_err)?;
    }
    let downloaded_version = read_arc_node_version(&pending.path)
        .ok_or_else(|| format!("downloaded {} did not report a parseable version", asset))?;
    if downloaded_version != EXPECTED_NODE_VERSION {
        return Err(format!(
            "downloaded {} reports v{}, expected v{}",
            asset, downloaded_version, EXPECTED_NODE_VERSION
        ));
    }

    install_over(&pending.path, target, expected_sha256)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(target, perms).map_err(map_err)?;
    }

    // Best-effort: strip any macOS quarantine flag on our own download.
    // User still needs to allow the desktop .app itself past Gatekeeper.
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(target)
            .output();
    }

    Ok(total_bytes)
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct BinaryInstallTransaction {
    schema: String,
    version: String,
    sha256: String,
    sidecar: String,
}

fn binary_install_journal_path(target: &Path) -> PathBuf {
    target.with_extension("install-transaction.json")
}

fn binary_install_rollback_path(target: &Path) -> PathBuf {
    target.with_extension("old")
}

fn sync_parent_best_effort(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
    }
}

fn sha256_regular_file(path: &Path) -> Result<Option<[u8; 32]>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect {}: {}", path.display(), error)),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!(
            "refusing arc-node install candidate that is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_NODE_BINARY_BYTES {
        return Err(format!(
            "arc-node install candidate exceeds the 512 MiB safety limit: {}",
            path.display()
        ));
    }

    let mut file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {}", path.display(), error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {}", path.display(), error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(Some(hasher.finalize().into()))
}

fn require_replaceable_binary_target(target: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(format!(
            "refusing to replace managed arc-node target that is not a regular file: {}",
            target.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect {}: {}", target.display(), error)),
    }
}

fn write_binary_install_transaction(
    tmp: &Path,
    target: &Path,
    expected_sha256: [u8; 32],
) -> Result<PathBuf, String> {
    let target_parent = target
        .parent()
        .ok_or_else(|| format!("{} has no install directory", target.display()))?;
    if tmp.parent() != Some(target_parent) {
        return Err("arc-node install sidecar escaped the managed binary directory".to_string());
    }
    let sidecar = tmp
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "arc-node install sidecar name is not UTF-8".to_string())?;
    let expected_prefix = format!(
        "{}{}",
        target
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "managed arc-node filename is not UTF-8".to_string())?,
        ".download-"
    );
    if !sidecar.starts_with(&expected_prefix) {
        return Err("arc-node install sidecar has an unexpected name".to_string());
    }

    let transaction = BinaryInstallTransaction {
        schema: BINARY_INSTALL_TRANSACTION_SCHEMA.to_string(),
        version: EXPECTED_NODE_VERSION.to_string(),
        sha256: hex::encode(expected_sha256),
        sidecar: sidecar.to_string(),
    };
    let journal = binary_install_journal_path(target);
    if std::fs::symlink_metadata(&journal).is_ok() {
        return Err(format!(
            "refusing to overwrite unresolved arc-node install transaction {}",
            journal.display()
        ));
    }
    let journal_tmp = journal.with_extension(format!(
        "json.tmp-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&journal_tmp)
        .map_err(|error| format!("create {}: {}", journal_tmp.display(), error))?;
    let bytes = serde_json::to_vec_pretty(&transaction).map_err(map_err)?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&journal_tmp);
        return Err(format!("persist {}: {}", journal_tmp.display(), error));
    }
    drop(file);
    if let Err(error) = std::fs::rename(&journal_tmp, &journal) {
        let _ = std::fs::remove_file(&journal_tmp);
        return Err(format!("commit {}: {}", journal.display(), error));
    }
    sync_parent_best_effort(&journal);
    Ok(journal)
}

fn transaction_sidecar(
    target: &Path,
    transaction: &BinaryInstallTransaction,
) -> Result<PathBuf, String> {
    let sidecar = Path::new(&transaction.sidecar);
    if sidecar.file_name().and_then(|name| name.to_str()) != Some(transaction.sidecar.as_str()) {
        return Err("arc-node install transaction contains a non-local sidecar path".to_string());
    }
    let target_stem = target
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "managed arc-node filename is not UTF-8".to_string())?;
    if !transaction
        .sidecar
        .starts_with(&format!("{target_stem}.download-"))
    {
        return Err("arc-node install transaction names an unrelated sidecar".to_string());
    }
    Ok(target
        .parent()
        .ok_or_else(|| format!("{} has no install directory", target.display()))?
        .join(sidecar))
}

fn decode_transaction_digest(transaction: &BinaryInstallTransaction) -> Result<[u8; 32], String> {
    let decoded = hex::decode(&transaction.sha256)
        .map_err(|_| "arc-node install transaction has an invalid digest".to_string())?;
    decoded
        .try_into()
        .map_err(|_| "arc-node install transaction has a non-SHA-256 digest".to_string())
}

fn restore_binary_rollback_if_missing(target: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_file() => return Ok(false),
        Ok(_) => {
            return Err(format!(
                "refusing arc-node recovery through non-regular target {}",
                target.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect {}: {}", target.display(), error)),
    }
    let rollback = binary_install_rollback_path(target);
    let metadata = match std::fs::symlink_metadata(&rollback) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("inspect {}: {}", rollback.display(), error)),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!(
            "refusing non-regular arc-node rollback file {}",
            rollback.display()
        ));
    }
    std::fs::rename(&rollback, target).map_err(|error| {
        format!(
            "restore interrupted arc-node install from {}: {}",
            rollback.display(),
            error
        )
    })?;
    sync_parent_best_effort(target);
    tracing::warn!(
        rollback = %rollback.display(),
        target = %target.display(),
        "restored the previous arc-node after an interrupted executable replacement"
    );
    Ok(true)
}

fn discard_invalid_binary_transaction(
    target: &Path,
    journal: &Path,
    reason: impl std::fmt::Display,
) -> Result<(), String> {
    restore_binary_rollback_if_missing(target)?;
    let _ = std::fs::remove_file(journal);
    sync_parent_best_effort(journal);
    tracing::warn!(
        %reason,
        journal = %journal.display(),
        "discarded an invalid arc-node install transaction after preserving the last complete executable"
    );
    Ok(())
}

/// Complete or roll back the small journaled window required by Windows,
/// where `rename` cannot atomically replace an existing executable. The old
/// complete image is never deleted until the signed new image is at `target`.
fn recover_interrupted_binary_install(target: &Path, expected_version: &str) -> Result<(), String> {
    let journal = binary_install_journal_path(target);
    let journal_metadata = match std::fs::symlink_metadata(&journal) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("inspect {}: {}", journal.display(), error)),
    };

    let Some(metadata) = journal_metadata else {
        // Compatibility recovery for an interruption in the old, unjournaled
        // Windows replacement sequence.
        restore_binary_rollback_if_missing(target)?;
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return discard_invalid_binary_transaction(
            target,
            &journal,
            "transaction path was not a regular file",
        );
    }
    if metadata.len() > 64 * 1024 {
        return discard_invalid_binary_transaction(
            target,
            &journal,
            "transaction exceeded its 64 KiB safety limit",
        );
    }

    let transaction: BinaryInstallTransaction = match std::fs::read(&journal)
        .map_err(map_err)
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(map_err))
    {
        Ok(transaction) => transaction,
        Err(error) => {
            return discard_invalid_binary_transaction(target, &journal, error);
        }
    };
    if transaction.schema != BINARY_INSTALL_TRANSACTION_SCHEMA
        || transaction.version != expected_version
    {
        return discard_invalid_binary_transaction(
            target,
            &journal,
            "transaction schema or release version did not match this desktop",
        );
    }
    let expected_sha256 = match decode_transaction_digest(&transaction) {
        Ok(digest) => digest,
        Err(error) => return discard_invalid_binary_transaction(target, &journal, error),
    };
    let sidecar = match transaction_sidecar(target, &transaction) {
        Ok(sidecar) => sidecar,
        Err(error) => return discard_invalid_binary_transaction(target, &journal, error),
    };

    if sha256_regular_file(target)? == Some(expected_sha256) {
        let _ = std::fs::remove_file(&sidecar);
        let _ = std::fs::remove_file(binary_install_rollback_path(target));
        let _ = std::fs::remove_file(&journal);
        sync_parent_best_effort(target);
        return Ok(());
    }

    let sidecar_sha256 = match sha256_regular_file(&sidecar) {
        Ok(digest) => digest,
        Err(error) => return discard_invalid_binary_transaction(target, &journal, error),
    };
    if sidecar_sha256 == Some(expected_sha256) {
        let rollback = binary_install_rollback_path(target);
        if std::fs::symlink_metadata(target).is_ok() {
            if std::fs::symlink_metadata(&rollback).is_ok() {
                std::fs::remove_file(&rollback).map_err(map_err)?;
            }
            std::fs::rename(target, &rollback).map_err(|error| {
                format!(
                    "preserve {} before resuming install: {}",
                    target.display(),
                    error
                )
            })?;
            sync_parent_best_effort(target);
        }
        if let Err(error) = std::fs::rename(&sidecar, target) {
            restore_binary_rollback_if_missing(target)?;
            return Err(format!(
                "resume verified arc-node install at {}: {}",
                target.display(),
                error
            ));
        }
        sync_parent_best_effort(target);
        if sha256_regular_file(target)? != Some(expected_sha256) {
            let _ = std::fs::remove_file(target);
            restore_binary_rollback_if_missing(target)?;
            return Err("resumed arc-node install failed its durable digest check".to_string());
        }
        let _ = std::fs::remove_file(&rollback);
        let _ = std::fs::remove_file(&journal);
        sync_parent_best_effort(target);
        tracing::warn!(
            target = %target.display(),
            "completed a verified arc-node executable replacement after an interrupted update"
        );
        return Ok(());
    }

    // The new image is incomplete or missing. Restore the last complete image
    // and let normal exact-version logic fetch a fresh signed artifact.
    restore_binary_rollback_if_missing(target)?;
    let _ = std::fs::remove_file(&sidecar);
    let _ = std::fs::remove_file(&journal);
    sync_parent_best_effort(target);
    Ok(())
}

fn install_over_transactional(
    tmp: &Path,
    target: &Path,
    expected_sha256: [u8; 32],
) -> Result<(), String> {
    let journal = write_binary_install_transaction(tmp, target, expected_sha256)?;
    let rollback = binary_install_rollback_path(target);
    if std::fs::symlink_metadata(&rollback).is_ok() {
        std::fs::remove_file(&rollback).map_err(map_err)?;
    }
    if let Err(error) = std::fs::rename(target, &rollback) {
        let _ = std::fs::remove_file(&journal);
        sync_parent_best_effort(target);
        return Err(format!(
            "could not preserve {} before replacement: {}",
            target.display(),
            error
        ));
    }
    sync_parent_best_effort(target);

    if let Err(error) = std::fs::rename(tmp, target) {
        let restored = restore_binary_rollback_if_missing(target).unwrap_or(false);
        if restored {
            let _ = std::fs::remove_file(&journal);
            sync_parent_best_effort(target);
        }
        return Err(format!(
            "could not install new arc-node at {}: {}",
            target.display(),
            error
        ));
    }
    sync_parent_best_effort(target);
    if sha256_regular_file(target)? != Some(expected_sha256) {
        let _ = std::fs::remove_file(target);
        restore_binary_rollback_if_missing(target)?;
        return Err("installed arc-node failed its durable digest check".to_string());
    }

    let _ = std::fs::remove_file(&rollback);
    let _ = std::fs::remove_file(&journal);
    sync_parent_best_effort(target);
    Ok(())
}

/// Move `tmp` onto `target`. Unix gets a single atomic rename. Windows cannot
/// replace an existing executable with `rename`, so its fallback writes and
/// fsyncs a recovery journal before moving the complete old image aside.
fn install_over(tmp: &Path, target: &Path, expected_sha256: [u8; 32]) -> Result<(), String> {
    if sha256_regular_file(tmp)? != Some(expected_sha256) {
        return Err(format!(
            "refusing to install arc-node candidate with an unexpected durable digest: {}",
            tmp.display()
        ));
    }
    require_replaceable_binary_target(target)?;
    if std::fs::rename(tmp, target).is_ok() {
        let _ = std::fs::remove_file(binary_install_rollback_path(target));
        let _ = std::fs::remove_file(binary_install_journal_path(target));
        sync_parent_best_effort(target);
        return Ok(());
    }
    install_over_transactional(tmp, target, expected_sha256)
}

/// Run `arc-node --version` and return the version token (e.g. "0.5.7").
/// Returns None if the binary fails to launch (corrupt, wrong arch, missing
/// Returns true if semver string `a` is strictly greater than `b`.
/// Compares major.minor.patch numerically. Falls back to false on parse error.
fn semver_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Option<(u64, u64, u64)> {
        let mut parts = s.trim().split('.');
        let maj = parts.next()?.parse().ok()?;
        let min = parts.next()?.parse().ok()?;
        let pat = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some((maj, min, pat))
    };
    match (parse(a), parse(b)) {
        (Some(av), Some(bv)) => av > bv,
        _ => false,
    }
}

/// shared lib) or prints something we can't parse - in either case the caller
/// should redownload to recover.
fn read_arc_node_version(binary: &std::path::Path) -> Option<String> {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("--version");
    // Windows: suppress the console flash that would otherwise appear
    // for ~50 ms on every Start/Restart click. Same CREATE_NO_WINDOW
    // flag as in node_manager::start; see the comment there for the
    // full rationale (this probe is short-lived so the crash risk is
    // smaller, but the flicker is user-visible).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Expected format: "arc-node 0.5.7"
    stdout.split_whitespace().nth(1).map(|s| s.to_string())
}

fn platform_release_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("arc-node-macos-arm64"),
        ("macos", "x86_64") => Some("arc-node-macos-x86_64"),
        ("windows", "x86_64") => Some("arc-node-windows-x86_64.exe"),
        ("linux", "x86_64") => Some("arc-node-linux-x86_64"),
        _ => None,
    }
}

pub(crate) fn resolve_testnet_resources(app: &AppHandle) -> TestnetResources {
    let resolver = app.path();
    let seeds = resolver
        .resolve(
            "resources/testnet-seeds.txt",
            tauri::path::BaseDirectory::Resource,
        )
        .ok()
        .filter(|p: &PathBuf| p.exists());
    let genesis = resolver
        .resolve(
            "resources/genesis.toml",
            tauri::path::BaseDirectory::Resource,
        )
        .ok()
        .filter(|p: &PathBuf| p.exists());
    TestnetResources {
        seeds_file: seeds,
        genesis_file: genesis,
    }
}

// ─── Model download (community inference worker setup) ─────────────────────
//
// The desktop's #1 user complaint with v0.5.x: "I joined, I have peers, but
// I have 0 attestations and 0 earnings." Root cause: onboarding defaulted to
// `role: "observer"` with `model_path: None`, so node_manager never passed
// `--community-mode` to arc-node, so the coordinator never dispatched
// inference to the user. They were a passive validator forever.
//
// v0.8.0 downloads only the exact model artifact accepted by the recovered
// production network. Offering hardware-sized alternatives here is actively
// harmful: a TinyLlama or 13B worker can load successfully but its model ID can
// never match a 7B production assignment, leaving the user with a multi-GB
// download and zero eligible jobs. Other GGUFs may still be selected manually
// for local inference, but the earning-compatible onboarding path is singular.
struct ModelTierSpec {
    id: &'static str,
    display_name: &'static str,
    url: &'static str,
    size_bytes: u64,
    /// SHA-256 from the repository's immutable LFS object ID. URLs may move,
    /// but a desktop-selected tier must always resolve to these exact bytes.
    sha256: &'static str,
}

const MODEL_TIERS: &[ModelTierSpec] = &[ModelTierSpec {
    id: "standard",
    display_name: "Llama-2 7B Chat (Q4_K_M) — ARC compatible",
    url: "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/191239b3e26b2882fb562ffccdd1cf0f65402adb/llama-2-7b-chat.Q4_K_M.gguf",
    size_bytes: 4_081_004_224,
    sha256: "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa",
}];

fn model_digest(spec: &ModelTierSpec) -> Result<[u8; 32], String> {
    let decoded = hex::decode(spec.sha256)
        .map_err(|_| format!("invalid built-in SHA-256 for model tier {}", spec.id))?;
    decoded
        .try_into()
        .map_err(|_| format!("invalid built-in SHA-256 length for model tier {}", spec.id))
}

fn verify_model_file(path: &Path, spec: &ModelTierSpec) -> Result<bool, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("inspect {}: {}", path.display(), error)),
    };
    if !metadata.is_file() || metadata.len() != spec.size_bytes {
        return Ok(false);
    }

    let mut file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {}", path.display(), error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {}", path.display(), error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual: [u8; 32] = hasher.finalize().into();
    Ok(actual == model_digest(spec)?)
}

fn tier_spec(id: &str) -> Option<&'static ModelTierSpec> {
    MODEL_TIERS.iter().find(|t| t.id == id)
}

fn models_dir() -> PathBuf {
    paths::arc_home().join("models")
}

fn model_path_for(tier: &str) -> PathBuf {
    models_dir().join(format!("{}.gguf", tier))
}

fn model_download_sidecar(target: &Path, nonce: u64) -> PathBuf {
    let suffix = format!("download-{}-{nonce:016x}", std::process::id());
    match target.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => target.with_extension(format!("{suffix}.{extension}")),
        None => target.with_extension(suffix),
    }
}

async fn create_model_download_sidecar(target: &Path) -> Result<PendingVerifiedDownload, String> {
    for _ in 0..32 {
        let path = model_download_sidecar(target, rand::random::<u64>());
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&path).await {
            Ok(file) => {
                return Ok(PendingVerifiedDownload {
                    path,
                    file: Some(file),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create {}: {}", path.display(), error)),
        }
    }
    Err(format!(
        "could not allocate an isolated model download beside {}",
        target.display()
    ))
}

fn cleanup_model_download_sidecars(target: &Path) -> Result<(), String> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    let stem = target
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "model filename is not UTF-8".to_string())?;
    let extension = target
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let prefix = format!("{stem}.download-");
    let suffix = if extension.is_empty() {
        String::new()
    } else {
        format!(".{extension}")
    };
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read {}: {}", parent.display(), error)),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(&suffix) {
            continue;
        }
        let path = entry.path();
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file()) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

/// List the tiers the desktop knows how to auto-download. Frontend uses this
/// to render the picker.
#[tauri::command]
pub async fn list_model_tiers() -> CmdResult<Vec<ModelTierInfo>> {
    Ok(MODEL_TIERS
        .iter()
        .map(|t| ModelTierInfo {
            id: t.id.into(),
            display_name: t.display_name.into(),
            size_bytes: t.size_bytes,
            url: t.url.into(),
        })
        .collect())
}

/// Recommend the one production-compatible tier only when the machine has
/// enough RAM. A larger GPU does not make a different model ID eligible.
///
/// Returns "none" when the machine isn't strong enough to run any tier
/// usefully — frontend should offer "verifier-only" mode instead.
#[tauri::command]
pub async fn recommended_tier() -> CmdResult<String> {
    let hw = hardware::detect();
    let tier = if hw.ram_gb >= 16 { "standard" } else { "none" };
    Ok(tier.into())
}

/// Returns `Some(path)` only when the matching tier's GGUF is byte-for-byte
/// the pinned artifact. Hashing runs off the async worker because these files
/// are multi-gigabyte. A same-size mutation must never be treated as ready.
#[tauri::command]
pub async fn existing_model_for_tier(tier: String) -> CmdResult<Option<String>> {
    let Some(spec) = tier_spec(&tier) else {
        return Ok(None);
    };
    let p = model_path_for(&tier);
    let verify_path = p.clone();
    let valid = tokio::task::spawn_blocking(move || verify_model_file(&verify_path, spec))
        .await
        .map_err(map_err)??;
    Ok(valid.then(|| p.to_string_lossy().into_owned()))
}

/// Download the GGUF for `tier` to ~/.arc/models/<tier>.gguf, streaming
/// progress events on the `model-download-progress` channel so the UI can
/// render a real progress bar.
///
/// Idempotent only for an exact pinned artifact. The stream is checked against
/// both its exact LFS size and SHA-256 before an atomic rename from the
/// `.download` sidecar, so a crash or mirror mutation cannot replace a known
/// good model.
#[tauri::command]
pub async fn download_model(app: AppHandle, tier: String) -> CmdResult<String> {
    let spec = tier_spec(&tier).ok_or_else(|| format!("unknown model tier: {}", tier))?;
    let target = model_path_for(&tier);

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(map_err)?;
    }
    // Onboarding and the existing-observer banner can request the same tier
    // concurrently. Hold one OS-backed per-target lock across recheck,
    // download, digest verification, fsync, and promotion; a waiter rechecks
    // the completed target instead of opening/truncating the first stream.
    let _download_guard = acquire_model_download_lock(&target).await?;
    cleanup_model_download_sidecars(&target)?;

    // Already downloaded and hash-verified → done.
    let verify_path = target.clone();
    if tokio::task::spawn_blocking(move || verify_model_file(&verify_path, spec))
        .await
        .map_err(map_err)??
    {
        let _ = app.emit(
            "model-download-progress",
            ModelDownloadProgress {
                tier: tier.clone(),
                downloaded_bytes: spec.size_bytes,
                total_bytes: spec.size_bytes,
                done: true,
            },
        );
        return Ok(target.to_string_lossy().into_owned());
    }

    // The production 7B artifact is ~4.1 GB; four hours keeps slow residential
    // connections viable without allowing an unbounded request.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4 * 60 * 60))
        .build()
        .map_err(map_err)?;

    let resp = client
        .get(spec.url)
        .send()
        .await
        .map_err(|e| format!("HF fetch failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!(
            "GGUF mirror returned HTTP {} for tier {}",
            resp.status(),
            tier
        ));
    }
    if let Some(length) = resp.content_length() {
        if length != spec.size_bytes {
            return Err(format!(
                "GGUF mirror reported {} bytes for tier {}, expected {}",
                length, tier, spec.size_bytes
            ));
        }
    }
    let total_bytes = spec.size_bytes;

    let mut pending = create_model_download_sidecar(&target).await?;

    let mut stream = resp;
    let mut downloaded: u64 = 0;
    let mut hasher = Sha256::new();
    let mut last_emit = std::time::Instant::now();
    // Emit progress at most every 250ms. HF chunks tend to land in 8-64 KB
    // units; emitting on every chunk would flood the IPC channel and pin
    // the UI thread re-rendering progress.
    let emit_every = std::time::Duration::from_millis(250);

    loop {
        let chunk = match stream.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(error) => {
                return Err(format!(
                    "chunk read failed at {} bytes: {}",
                    downloaded, error
                ));
            }
        };
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > spec.size_bytes {
            return Err(format!(
                "model tier {} exceeded its pinned size of {} bytes",
                tier, spec.size_bytes
            ));
        }
        hasher.update(&chunk);
        if let Err(error) = pending.file_mut()?.write_all(&chunk).await {
            return Err(format!("write to temp file: {}", error));
        }

        if last_emit.elapsed() >= emit_every {
            let _ = app.emit(
                "model-download-progress",
                ModelDownloadProgress {
                    tier: tier.clone(),
                    downloaded_bytes: downloaded,
                    total_bytes,
                    done: false,
                },
            );
            last_emit = std::time::Instant::now();
        }
    }

    pending.file_mut()?.flush().await.map_err(map_err)?;
    pending.file_mut()?.sync_all().await.map_err(map_err)?;
    pending.close();

    if downloaded != spec.size_bytes {
        return Err(format!(
            "model tier {} ended at {} bytes, expected {}",
            tier, downloaded, spec.size_bytes
        ));
    }
    let actual_sha256: [u8; 32] = hasher.finalize().into();
    let expected_sha256 = model_digest(spec)?;
    if actual_sha256 != expected_sha256 {
        return Err(format!(
            "SHA-256 verification failed for model tier {}",
            tier
        ));
    }

    // Re-read the fsynced unique sidecar and require the same exact
    // size+digest contract before promotion. This catches disk faults and
    // proves the file being renamed, not merely the received byte stream.
    let verify_path = pending.path.clone();
    if !tokio::task::spawn_blocking(move || verify_model_file(&verify_path, spec))
        .await
        .map_err(map_err)??
    {
        return Err(format!(
            "durable model verification failed for tier {}",
            tier
        ));
    }

    // Atomic rename over any existing target. std::fs::rename uses
    // MoveFileEx(REPLACE_EXISTING) on Windows since Rust 1.62, so this
    // works cross-platform.
    std::fs::rename(&pending.path, &target)
        .map_err(|e| format!("rename to {}: {}", target.display(), e))?;
    sync_parent_best_effort(&target);

    let _ = app.emit(
        "model-download-progress",
        ModelDownloadProgress {
            tier: tier.clone(),
            downloaded_bytes: downloaded,
            total_bytes,
            done: true,
        },
    );

    Ok(target.to_string_lossy().into_owned())
}

/// Delete a previously-downloaded model. Frontend uses this when the user
/// switches tiers (e.g., from `tiny` → `standard`) so we don't leave 600 MB
/// of dead weight on a laptop with limited disk.
#[tauri::command]
pub async fn remove_model(tier: String) -> CmdResult<()> {
    let p = model_path_for(&tier);
    if p.parent().is_none_or(|parent| !parent.exists()) {
        return Ok(());
    }
    let _download_guard = acquire_model_download_lock(&p).await?;
    if p.exists() {
        std::fs::remove_file(&p).map_err(map_err)?;
    }
    // Also clean sidecars from both the legacy deterministic scheme and the
    // unique create-new scheme after holding the same per-target lock.
    let tmp = p.with_extension("download");
    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }
    cleanup_model_download_sidecars(&p)?;
    Ok(())
}

#[allow(dead_code)]
fn _path_helper(_: &Path) {} // keep `Path` import used if `model_path_for` returns inline

#[cfg(test)]
mod release_binary_tests {
    use super::*;

    fn source_between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        let start_at = source.find(start).expect("command start marker");
        let tail = &source[start_at..];
        let end_at = tail.find(end).expect("command end marker");
        &tail[..end_at]
    }

    #[test]
    fn settlement_write_gate_explains_why_it_is_closed() {
        let error = settlement_write_unavailable::<()>("Paid inference escrow")
            .expect_err("recovery candidate must reject settlement writes");
        assert!(error.contains("before any transaction is signed or submitted"));
        assert!(error.contains("exact model-artifact binding"));
        assert!(error.contains("validator-authenticated authorization"));
        assert!(error.contains("Free/community inference remains available"));
    }

    #[test]
    fn unresolved_data_migration_blocks_manual_start_and_restart() {
        assert!(data_migration_start_gate(None).is_ok());
        let error = data_migration_start_gate(Some("legacy data directory is a symbolic link"))
            .expect_err("an unresolved native migration fence must block every start path");
        assert!(error.contains("refused to start"));
        assert!(error.contains("migration is not safely resolved"));
        assert!(error.contains("preserved legacy directory"));
    }

    #[test]
    fn generic_config_save_preserves_the_native_data_directory() {
        let persisted = NodeConfig {
            data_dir: "/safe/fenced/data-v3".to_string(),
            ..NodeConfig::default()
        };
        let mut webview_candidate = NodeConfig {
            data_dir: "/preserved/v0.7-history".to_string(),
            rpc_port: 10_001,
            ..NodeConfig::default()
        };

        preserve_authoritative_data_dir(&mut webview_candidate, Some(&persisted));

        assert_eq!(webview_candidate.data_dir, persisted.data_dir);
        assert_eq!(webview_candidate.rpc_port, 10_001);
    }

    #[test]
    fn paid_and_tier1_commands_have_no_network_write_body() {
        let source = include_str!("commands.rs");
        let tier1 = source_between(
            source,
            "pub async fn tier1_submit(",
            "/// Read the on-chain state",
        );
        let paid = source_between(
            source,
            "pub async fn run_paid_inference(",
            "// `check_for_update`",
        );

        for (name, body) in [("tier1_submit", tier1), ("run_paid_inference", paid)] {
            assert!(body.contains("settlement_write_unavailable"), "{name}");
            for forbidden in [
                ".post(",
                ".send()",
                "submit_signed",
                "Transaction {",
                "hash_bytes(",
                ".sign(",
            ] {
                assert!(!body.contains(forbidden), "{name} contains {forbidden}");
            }
        }
        let legacy_shape_label = ["arc", "32L", "test"].join("-");
        let legacy_model_label = ["arc", "testnet", "llama", "2", "7b", "chat", "q4"].join("-");
        assert!(!source.contains(&legacy_shape_label));
        assert!(!source.contains(&legacy_model_label));
    }

    #[test]
    fn external_open_enforces_the_declared_http_only_capability_scope() {
        for allowed in [
            "https://github.com/FerrumVir/arc-chain",
            "http://140.82.16.112:9090/block/latest",
        ] {
            external_web_url(allowed).unwrap_or_else(|error| panic!("{allowed}: {error}"));
        }
        for refused in [
            "file:///etc/passwd",
            "file://C:/Windows/System32/cmd.exe",
            "smb://attacker.example/share",
            "javascript:alert(1)",
            "not a url",
        ] {
            assert!(
                external_web_url(refused).is_err(),
                "native opener accepted {refused}"
            );
        }

        // The native command must never be looser than the scope the shipped
        // capability declares for the WebView's own opener.
        let capability = include_str!("../capabilities/default.json");
        assert!(capability.contains("\"url\": \"https://**\""));
        assert!(capability.contains("\"url\": \"http://**\""));
    }

    #[test]
    fn node_asset_urls_are_version_pinned() {
        let url = exact_release_asset_url("arc-node-linux-x86_64");
        assert!(url.contains(&format!("/v{}/", EXPECTED_NODE_VERSION)));
        assert!(!url.contains("/latest/"));
    }

    #[test]
    fn startup_adopts_only_an_exact_node_version() {
        let exact = serde_json::json!({ "status": "ok", "version": EXPECTED_NODE_VERSION });
        assert_eq!(
            classify_local_health(&exact, EXPECTED_NODE_VERSION),
            LocalNodeCompatibility::Exact
        );

        let stale = serde_json::json!({ "status": "ok", "version": "0.7.11" });
        let stale_reason = match classify_local_health(&stale, EXPECTED_NODE_VERSION) {
            LocalNodeCompatibility::Incompatible(reason) => reason,
            other => panic!("stale local node was classified as {other:?}"),
        };
        assert!(stale_reason.contains("0.7.11"));
        assert!(stale_reason.contains(EXPECTED_NODE_VERSION));

        let malformed = serde_json::json!({ "status": "ok" });
        assert!(matches!(
            classify_local_health(&malformed, EXPECTED_NODE_VERSION),
            LocalNodeCompatibility::Incompatible(_)
        ));
    }

    #[test]
    fn desktop_inference_deadline_outlives_the_server_protocol_budget() {
        fn admitted_coordinator_budget(required_positions: u64) -> Option<u64> {
            let estimated = required_positions
                .saturating_mul(14_850)
                .div_ceil(1_000)
                .saturating_add(30);
            (estimated <= 3_900).then_some(estimated.max(45))
        }

        // The UTF-8 byte bound accounts for the tokenizer's leading marker
        // and its expansion of an ASCII space into a three-byte marker.
        let short_prompt = "ARC node";
        assert_eq!(inference_prompt_token_upper_bound(short_prompt), 13);
        assert_eq!(inference_prompt_token_upper_bound("🧪 "), 10);
        let short_positions = 1 + 13 + 16;
        let short_timeout = inference_timeout(short_prompt, 16).as_secs();
        assert_eq!(short_timeout, 536);
        assert_eq!(
            short_timeout,
            admitted_coordinator_budget(short_positions).unwrap() + 60
        );
        assert!(short_timeout < 10 * 60, "short prompts stay bounded");

        // At one output token, a long prompt alone can consume almost the
        // complete coordinator budget. The old output-only calculation gave
        // this request 105 seconds and cancelled it thousands of seconds too
        // early. The conservative prompt bound now covers that budget plus
        // client headroom.
        let long_prompt = "x".repeat(255);
        let long_positions = 1 + inference_prompt_token_upper_bound(&long_prompt) + 1;
        assert_eq!(long_positions, 260);
        assert_eq!(inference_timeout(&long_prompt, 1).as_secs(), 3_951);
        assert_eq!(
            inference_timeout(&long_prompt, 1).as_secs(),
            admitted_coordinator_budget(long_positions).unwrap() + 60
        );

        // One more raw byte crosses the coordinator's admitted deadline. The
        // desktop saturates at the full 3,900s server cap plus 60s headroom,
        // including for arithmetic-overflow-scale output requests.
        assert_eq!(inference_timeout(&"x".repeat(256), 1).as_secs(), 3_960);
        assert_eq!(inference_timeout("", u32::MAX).as_secs(), 3_960);
    }

    #[test]
    fn checksum_manifest_requires_one_exact_asset() {
        let digest = "11".repeat(32);
        let manifest = format!(
            "{}  arc-node-linux-x86_64\n{} *arc-node-macos-arm64\n",
            digest, digest
        );
        assert_eq!(
            expected_release_sha256(&manifest, "arc-node-linux-x86_64").unwrap(),
            [0x11; 32]
        );
        assert!(expected_release_sha256(&manifest, "arc-node-linux-arm64").is_err());

        let duplicate = format!(
            "{}  arc-node-linux-x86_64\n{} *arc-node-linux-x86_64\n",
            digest, digest
        );
        assert!(expected_release_sha256(&duplicate, "arc-node-linux-x86_64").is_err());
    }

    #[test]
    fn checksum_manifest_rejects_wrong_digest_shape() {
        let manifest = "abcd  arc-node-linux-x86_64\n";
        assert!(expected_release_sha256(manifest, "arc-node-linux-x86_64").is_err());
    }

    fn signed_manifest_fixture() -> (String, String, String) {
        use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey};

        let mut rng = rand::rngs::OsRng;
        let private_key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
            .expect("generate ephemeral release test key");
        let public_key = private_key
            .public_key()
            .to_openssh()
            .expect("encode ephemeral public key");
        let allowed_signers = format!(
            "arc-release namespaces=\"{}\" {} arc-release-test\n",
            ARC_RELEASE_MANIFEST_NAMESPACE, public_key
        );
        let manifest = format!(
            "# ARC release manifest v1\n# repository={}\n# tag=v{}\n# commit={}\n{}  arc-node-linux-x86_64\n",
            ARC_RELEASE_REPOSITORY,
            EXPECTED_NODE_VERSION,
            "a".repeat(40),
            "11".repeat(32),
        );
        let signature = private_key
            .sign(
                ARC_RELEASE_MANIFEST_NAMESPACE,
                HashAlg::Sha512,
                manifest.as_bytes(),
            )
            .expect("sign manifest")
            .to_pem(LineEnding::LF)
            .expect("armor signature");
        (manifest, signature, allowed_signers)
    }

    #[test]
    fn child_node_manifest_requires_owner_signature_and_exact_binding() {
        let (manifest, signature, allowed_signers) = signed_manifest_fixture();
        verify_release_manifest_signature_with_signers(
            manifest.as_bytes(),
            signature.as_bytes(),
            &allowed_signers,
            EXPECTED_NODE_VERSION,
        )
        .expect("valid exact-tag owner signature");

        let tampered = manifest.replace(&"11".repeat(32), &"22".repeat(32));
        assert!(
            verify_release_manifest_signature_with_signers(
                tampered.as_bytes(),
                signature.as_bytes(),
                &allowed_signers,
                EXPECTED_NODE_VERSION,
            )
            .is_err(),
            "a checksum edit must invalidate the owner signature"
        );

        assert!(
            verify_release_manifest_signature_with_signers(
                manifest.as_bytes(),
                signature.as_bytes(),
                &allowed_signers,
                "0.8.1",
            )
            .is_err(),
            "a valid signature must not be replayable across release tags"
        );
    }

    #[test]
    fn download_sidecar_preserves_windows_executable_suffix() {
        let pid = std::process::id();
        assert_eq!(
            binary_download_sidecar(Path::new("arc-node.exe"), 1),
            PathBuf::from(format!("arc-node.download-{pid}-0000000000000001.exe"))
        );
        assert_eq!(
            binary_download_sidecar(Path::new("arc-node"), 2),
            PathBuf::from(format!("arc-node.download-{pid}-0000000000000002"))
        );
    }

    fn binary_install_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "arc-desktop-{label}-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    #[tokio::test]
    async fn concurrent_downloads_get_exclusive_sidecars() {
        let dir = binary_install_test_dir("isolated-downloads");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("arc-node.exe");
        let first = create_binary_download_sidecar(&target).await.unwrap();
        let second = create_binary_download_sidecar(&target).await.unwrap();
        assert_ne!(first.path, second.path);
        for path in [&first.path, &second.path] {
            let metadata = std::fs::symlink_metadata(path).unwrap();
            assert!(metadata.file_type().is_file());
            assert!(!metadata.file_type().is_symlink());
            assert_eq!(
                path.extension().and_then(|value| value.to_str()),
                Some("exe")
            );
        }
        drop(first);
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binary_install_lock_serializes_concurrent_ensure_sequences() {
        let dir = binary_install_test_dir("binary-install-lock");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("arc-node.exe");
        let first = acquire_binary_install_lock(&target).await.unwrap();
        let waiter_target = target.clone();
        let waiter = tokio::spawn(async move { acquire_binary_install_lock(&waiter_target).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "a second ensure sequence must wait for the first install lock"
        );
        drop(first);
        let second = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("second ensure sequence should resume after unlock")
            .expect("install-lock task should not panic")
            .expect("second install lock");
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_binary_replacement_completes_verified_sidecar() {
        let dir = binary_install_test_dir("binary-install-resume");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("arc-node.exe");
        let rollback = binary_install_rollback_path(&target);
        let sidecar = binary_download_sidecar(&target, 7);
        let old = b"complete old executable";
        let new = b"owner-signed new executable";
        let digest: [u8; 32] = Sha256::digest(new).into();
        std::fs::write(&target, old).unwrap();
        std::fs::write(&sidecar, new).unwrap();
        let journal = write_binary_install_transaction(&sidecar, &target, digest).unwrap();

        // Power loss after Windows moved the old image aside but before the
        // verified sidecar reached the canonical path.
        std::fs::rename(&target, &rollback).unwrap();
        recover_interrupted_binary_install(&target, EXPECTED_NODE_VERSION).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), new);
        assert!(!rollback.exists());
        assert!(!sidecar.exists());
        assert!(!journal.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_binary_replacement_restores_old_if_sidecar_is_torn() {
        let dir = binary_install_test_dir("binary-install-rollback");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("arc-node.exe");
        let rollback = binary_install_rollback_path(&target);
        let sidecar = binary_download_sidecar(&target, 8);
        let old = b"last complete executable";
        let expected_new = b"expected complete executable";
        let digest: [u8; 32] = Sha256::digest(expected_new).into();
        std::fs::write(&target, old).unwrap();
        std::fs::write(&sidecar, expected_new).unwrap();
        let journal = write_binary_install_transaction(&sidecar, &target, digest).unwrap();
        std::fs::rename(&target, &rollback).unwrap();
        std::fs::write(&sidecar, b"torn").unwrap();

        recover_interrupted_binary_install(&target, EXPECTED_NODE_VERSION).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), old);
        assert!(!rollback.exists());
        assert!(!sidecar.exists());
        assert!(!journal.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn torn_install_journal_still_restores_the_complete_old_binary() {
        let dir = binary_install_test_dir("binary-install-torn-journal");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("arc-node.exe");
        let rollback = binary_install_rollback_path(&target);
        let journal = binary_install_journal_path(&target);
        let old = b"recoverable pre-update executable";
        std::fs::write(&rollback, old).unwrap();
        std::fs::write(&journal, b"{torn").unwrap();

        recover_interrupted_binary_install(&target, EXPECTED_NODE_VERSION).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), old);
        assert!(!rollback.exists());
        assert!(!journal.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transactional_binary_replacement_keeps_a_rollback_until_commit() {
        let dir = binary_install_test_dir("binary-install-transaction");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("arc-node.exe");
        let sidecar = binary_download_sidecar(&target, 9);
        let new = b"verified transaction executable";
        let digest: [u8; 32] = Sha256::digest(new).into();
        std::fs::write(&target, b"old executable").unwrap();
        std::fs::write(&sidecar, new).unwrap();

        install_over_transactional(&sidecar, &target, digest).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), new);
        assert!(!binary_install_rollback_path(&target).exists());
        assert!(!binary_install_journal_path(&target).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn executable_replacement_never_moves_aside_a_non_regular_target() {
        let dir = binary_install_test_dir("binary-install-non-regular-target");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("arc-node.exe");
        let sidecar = binary_download_sidecar(&target, 10);
        let new = b"verified replacement executable";
        let digest: [u8; 32] = Sha256::digest(new).into();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(&sidecar, new).unwrap();

        let error = install_over(&sidecar, &target, digest)
            .expect_err("a directory at the executable path must fail closed");

        assert!(error.contains("not a regular file"));
        assert!(target.is_dir());
        assert_eq!(std::fs::read(&sidecar).unwrap(), new);
        assert!(!binary_install_rollback_path(&target).exists());
        assert!(!binary_install_journal_path(&target).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn executable_replacement_never_follows_or_replaces_a_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = binary_install_test_dir("binary-install-symlink-target");
        std::fs::create_dir_all(&dir).unwrap();
        let actual = dir.join("operator-owned-node");
        let target = dir.join("arc-node");
        let sidecar = binary_download_sidecar(&target, 11);
        let original = b"operator-owned executable";
        let new = b"verified replacement executable";
        let digest: [u8; 32] = Sha256::digest(new).into();
        std::fs::write(&actual, original).unwrap();
        symlink(&actual, &target).unwrap();
        std::fs::write(&sidecar, new).unwrap();

        let error = install_over(&sidecar, &target, digest)
            .expect_err("a symlink at the managed executable path must fail closed");

        assert!(error.contains("not a regular file"));
        assert!(std::fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&actual).unwrap(), original);
        assert_eq!(std::fs::read(&sidecar).unwrap(), new);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn version_comparison_rejects_non_strict_values() {
        assert!(semver_gt("0.8.0", "0.7.11"));
        assert!(!semver_gt("0.8.0-beta", "0.7.11"));
        assert!(!semver_gt("0.7", "0.6.99"));
        assert!(!semver_gt("0.8.0.1", "0.8.0"));
    }

    #[test]
    fn every_builtin_model_has_a_fixed_sha256() {
        assert_eq!(MODEL_TIERS.len(), 1);
        for spec in MODEL_TIERS {
            assert_eq!(model_digest(spec).unwrap().len(), 32, "{}", spec.id);
            assert_eq!(spec.sha256.len(), 64, "{}", spec.id);
        }
        let production = &MODEL_TIERS[0];
        assert_eq!(production.id, "standard");
        assert_eq!(production.size_bytes, 4_081_004_224);
        assert_eq!(
            production.sha256,
            "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
        );
        assert!(production
            .url
            .contains("/resolve/191239b3e26b2882fb562ffccdd1cf0f65402adb/"));
        assert!(!production.url.contains("/resolve/main/"));
    }

    #[tokio::test]
    async fn concurrent_model_invocations_get_independent_create_new_sidecars() {
        let dir = binary_install_test_dir("isolated-model-downloads");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("standard.gguf");
        let mut first = create_model_download_sidecar(&target).await.unwrap();
        let mut second = create_model_download_sidecar(&target).await.unwrap();
        assert_ne!(first.path, second.path);
        first.file_mut().unwrap().write_all(b"first").await.unwrap();
        second
            .file_mut()
            .unwrap()
            .write_all(b"second")
            .await
            .unwrap();
        let first_path = first.path.clone();
        let second_path = second.path.clone();
        drop(first);
        assert!(!first_path.exists());
        assert!(
            second_path.exists(),
            "one failed/cancelled model download must not remove its concurrent peer"
        );
        drop(second);
        assert!(!second_path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_cleanup_removes_only_stale_model_sidecars() {
        let dir = binary_install_test_dir("stale-model-sidecar-cleanup");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("standard.gguf");
        let lock = model_download_lock_path(&target);
        let stale = model_download_sidecar(&target, 7);
        std::fs::write(&target, b"already verified canonical target").unwrap();
        std::fs::write(&lock, b"").unwrap();
        std::fs::write(&stale, b"interrupted stream").unwrap();

        cleanup_model_download_sidecars(&target).unwrap();

        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"already verified canonical target"
        );
        assert!(lock.exists(), "the durable lock inode remains reusable");
        assert!(!stale.exists(), "an abandoned unique sidecar is removable");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn linux_native_packages_cannot_call_in_app_install() {
        let policy = update_install_policy_for("linux", None);
        assert!(!policy.can_install);
        assert_eq!(policy.channel, "package-manager");
        assert!(policy.instructions.contains(".deb or .rpm"));
    }

    #[test]
    fn only_a_real_appimage_path_enables_linux_in_app_install() {
        let missing = std::env::temp_dir().join("arc-missing-appimage");
        assert!(!update_install_policy_for("linux", Some(&missing)).can_install);

        let path = std::env::temp_dir().join(format!(
            "arc-updater-policy-{}.AppImage",
            std::process::id()
        ));
        std::fs::write(&path, b"appimage-test").unwrap();
        let policy = update_install_policy_for("linux", Some(&path));
        assert!(policy.can_install);
        assert_eq!(policy.channel, "appimage");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn macos_and_windows_keep_native_signed_updates() {
        for os in ["macos", "windows"] {
            let policy = update_install_policy_for(os, None);
            assert!(policy.can_install, "{os}");
            assert_eq!(policy.channel, "native", "{os}");
        }
    }

    #[test]
    fn same_size_model_mutation_is_rejected() {
        let good = b"good";
        let digest = Box::leak(hex::encode(Sha256::digest(good)).into_boxed_str());
        let spec = ModelTierSpec {
            id: "test",
            display_name: "test",
            url: "https://example.invalid/model.gguf",
            size_bytes: good.len() as u64,
            sha256: digest,
        };
        let path =
            std::env::temp_dir().join(format!("arc-model-check-{}-same-size", std::process::id()));
        std::fs::write(&path, good).unwrap();
        assert!(verify_model_file(&path, &spec).unwrap());
        std::fs::write(&path, b"evil").unwrap();
        assert!(!verify_model_file(&path, &spec).unwrap());
        let _ = std::fs::remove_file(path);
    }
}
