mod commands;
mod hardware;
mod identity;
mod node_manager;
mod paths;
mod rpc_client;
mod store;
mod tray;
mod types;
mod wallet;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Manager, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as AutostartManagerExt};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

fn durable_legacy_stop_material(
    migration_notice_is_durable: bool,
    notice: Option<types::DataMigrationNotice>,
    seed: Option<Zeroizing<String>>,
) -> Option<(types::DataMigrationNotice, Zeroizing<String>)> {
    if migration_notice_is_durable {
        notice.zip(seed)
    } else {
        None
    }
}

pub struct AppState {
    pub node: Arc<Mutex<node_manager::NodeManager>>,
    pub store: Arc<Mutex<store::Store>>,
    pub data_dir: Arc<Mutex<PathBuf>>,
    pub http: reqwest::Client,
    /// Maps an in-flight Tier 1 request_id to the seed VPS that accepted the
    /// submit. Each seed runs its own chain, so the poll must hit the same
    /// host. In-memory only — survives only for the lifetime of the process,
    /// which is fine because Tier 1 requests finalize in seconds.
    pub tier1_routes: Arc<Mutex<HashMap<String, String>>>,
    /// The seed currently elected for chain reads, plus when it was elected.
    /// Re-probed on a TTL rather than per request — `node_status` polls every
    /// 1.5s and probing six seeds that often would be pointless load on a
    /// live production network.
    pub chain_host: Arc<Mutex<Option<(commands::ChainHostChoice, std::time::Instant)>>>,
    /// Serializes wallet writes so two UI clicks cannot sign the same account
    /// nonce concurrently. This lock never contains the recovery phrase.
    pub wallet_write: Arc<Mutex<()>>,
    /// Whether a system tray icon was actually created. Gates hide-to-tray:
    /// on a desktop with no tray, hiding the window makes the app
    /// unreachable.
    pub has_tray: Arc<std::sync::atomic::AtomicBool>,
    /// Authoritative native fail-closed fence for an unresolved data
    /// migration preflight. Every command that can start arc-node consults
    /// this state; a WebView Start/Restart click cannot bypass a startup
    /// failure and replay an ambiguous legacy WAL.
    pub data_migration_error: Arc<Mutex<Option<String>>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,arc_desktop_lib=debug".into()),
        )
        .init();

    let node = Arc::new(Mutex::new(node_manager::NodeManager::new()));
    // Store starts empty; `setup()` resolves the per-platform writable
    // data dir via Tauri's PathResolver and loads from there.
    let store = Arc::new(Mutex::new(store::Store::default()));
    let data_dir = Arc::new(Mutex::new(PathBuf::new()));
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        // A signed transaction is bound to the elected origin. Never allow a
        // gateway redirect to move that POST to another scheme or host.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default();

    let has_tray = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let data_migration_error = Arc::new(Mutex::new(None));
    let state = AppState {
        node,
        store: store.clone(),
        data_dir: data_dir.clone(),
        http,
        tier1_routes: Arc::new(Mutex::new(HashMap::new())),
        chain_host: Arc::new(Mutex::new(None)),
        wallet_write: Arc::new(Mutex::new(())),
        has_tray: has_tray.clone(),
        data_migration_error: data_migration_error.clone(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Needed so the frontend can call `relaunch()` from
        // `@tauri-apps/plugin-process` after `update.downloadAndInstall()`
        // finishes. Without this, the new installer runs but the app stays
        // dead until the user manually relaunches.
        .plugin(tauri_plugin_process::init())
        // Auto-launch on OS login. LaunchAgent = macOS launchd user-scoped
        // LoginItem, Linux XDG autostart, Windows Run key. `--minimized`
        // tells the app to start with the window hidden to the tray so
        // the user doesn't get a window on every reboot.
        .plugin(
            tauri_plugin_autostart::init(
                MacosLauncher::LaunchAgent,
                Some(vec!["--minimized"]),
            ),
        )
        .manage(state)
        .setup(move |app| {
            // Resolve the per-platform writable dir NOW (AppHandle available).
            let resolver = app.path();
            let resolved = resolver
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir());
            tracing::info!("app data dir: {}", resolved.display());

            // Give paths::home_dir() a last-resort value for the case where
            // neither HOME nor USERPROFILE is set. Must happen before
            // anything resolves ~/.arc.
            if let Ok(home) = resolver.home_dir() {
                paths::set_home_fallback(home);
            }

            // Load existing store from the resolved dir, if any.
            let mut loaded_store = store::Store::load_from(&resolved);
            let had_durable_migration_notice = loaded_store.data_migration_notice.is_some();
            let legacy_identity_seed = loaded_store
                .identity
                .as_ref()
                .map(|identity| Zeroizing::new(identity.seed_phrase.clone()));
            let legacy_process_data_dir = match (
                loaded_store.config.as_ref(),
                legacy_identity_seed.as_ref(),
            ) {
                (Some(config), Some(seed)) => {
                    let resources = commands::resolve_testnet_resources(app.handle());
                    node_manager::detect_running_legacy_v07_data_dir(config, seed, &resources)
                }
                _ => Ok(None),
            };
            // A v0.7 desktop stored unbound chain state in the same ~/.arc
            // root as binaries and models. Fence that WAL before deriving the
            // auto-start config: old bytes stay untouched while only the
            // persisted data-dir pointer moves to a fresh protocol-v3 child.
            let migration_result = match legacy_process_data_dir {
                Ok(Some(path)) => loaded_store.protect_running_legacy_v07_data_at(&path),
                Ok(None) => loaded_store.protect_legacy_v07_data(),
                Err(error) => Err(error),
            };
            let (
                migration_allows_autostart,
                migration_failure_reason,
                migration_notice_is_durable,
            ) =
                match migration_result {
                Ok(Some(notice)) => match loaded_store.save_to(&resolved) {
                    Ok(()) => {
                        tracing::warn!(
                            legacy = %notice.legacy_data_dir,
                            active = %notice.active_data_dir,
                            "preserved legacy ARC data and selected a fresh protocol-v3 directory"
                        );
                        (true, None, true)
                    }
                    Err(error) => {
                        tracing::error!(
                            %error,
                            "legacy data was detected but the protected v3 config could not be persisted; suppressing node auto-start"
                        );
                        (
                            false,
                            Some(format!(
                                "the protected protocol-v3 data pointer could not be persisted: {error}"
                            )),
                            false,
                        )
                    }
                },
                Ok(None) => (true, None, had_durable_migration_notice),
                Err(error) => {
                    tracing::error!(
                        %error,
                        "legacy data migration preflight failed; suppressing node auto-start"
                    );
                    (
                        false,
                        Some(format!("legacy-data migration preflight failed: {error}")),
                        false,
                    )
                }
            };
            let autostart_desired = loaded_store
                .config
                .as_ref()
                .map(|c| c.auto_start)
                .unwrap_or(true);
            // Capture what we need for the auto-start decision before the
            // store moves into the shared mutex.
            let start_config = loaded_store.config.clone().unwrap_or_default();
            let has_identity = loaded_store.identity.is_some();
            let legacy_windows_stop = durable_legacy_stop_material(
                migration_notice_is_durable,
                loaded_store.data_migration_notice.clone(),
                legacy_identity_seed,
            );
            let legacy_migration_block = migration_failure_reason.clone();

            let store_shared = store.clone();
            let data_dir_shared = data_dir.clone();
            let migration_error_shared = data_migration_error.clone();
            let startup_boundary_reason = migration_failure_reason.clone().or_else(|| {
                Some(
                    "managed-node startup reconciliation is still in progress; binary replacement and node start are temporarily blocked"
                        .to_string(),
                )
            });
            tauri::async_runtime::block_on(async move {
                *store_shared.lock().await = loaded_store;
                *data_dir_shared.lock().await = resolved;
                *migration_error_shared.lock().await = startup_boundary_reason;
            });

            // Sync the autostart plugin with what the user chose during
            // onboarding (default: on).
            //
            // enable() is re-run on every launch even when the OS already
            // reports it enabled, because the stored login item embeds an
            // absolute path: the macOS LaunchAgent plist names the .app
            // (dangling as soon as the user drags it from ~/Downloads to
            // /Applications — the single most common install flow), and the
            // Linux XDG .desktop names the executable, which for an AppImage
            // is a /tmp/.mount_XXXX path that changes every run. Re-enabling
            // rewrites the entry with the current location, so it self-heals.
            let autostart = app.autolaunch();
            let enabled_now = autostart.is_enabled().unwrap_or(false);
            if autostart_desired {
                if let Err(e) = autostart.enable() {
                    // No longer swallowed: a login item that silently failed
                    // to register looks identical to one that worked.
                    tracing::warn!("could not register the login item: {}", e);
                }
            } else if enabled_now {
                if let Err(e) = autostart.disable() {
                    tracing::warn!("could not remove the login item: {}", e);
                }
            }

            // Build the system tray icon. Gives the user a way to open the
            // window after hide-to-tray, and a real Quit so arc-node can
            // be stopped explicitly.
            //
            // Failure is NOT fatal, and is recorded: stock GNOME ships no
            // AppIndicator host, so the tray silently does not appear. Hiding
            // the window to a tray that isn't there left the app with no way
            // to reopen it and no way to quit except `pkill`.
            match tray::install(app.handle()) {
                Ok(()) => has_tray.store(true, std::sync::atomic::Ordering::SeqCst),
                Err(e) => {
                    has_tray.store(false, std::sync::atomic::Ordering::SeqCst);
                    tracing::warn!(
                        "no system tray ({}) - the window will close normally instead of hiding",
                        e
                    );
                }
            }

            // If the app was launched with `--minimized` (set by the
            // autostart plugin on login), keep the window hidden and
            // let the tray be the only surface until the user clicks it.
            // Without a tray there is no other surface, so ignore the flag.
            let launched_minimized = std::env::args().any(|a| a == "--minimized");
            if launched_minimized && has_tray.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.hide();
                }
            }

            // ── Start the node on launch ────────────────────────────────
            //
            // `auto_start` previously drove only the OS login item, so the
            // Settings copy — "Automatically launch the node whenever ARC
            // opens" — was simply untrue. Nothing in the app started
            // arc-node after onboarding finished, and because the Dashboard's
            // Start button was unreachable (it read a remote seed and
            // therefore always believed the node was running), quitting and
            // reopening left the user with no way to run their node at all.
            let should_start =
                start_config.auto_start && has_identity && migration_allows_autostart;
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let Some(state) = handle.try_state::<AppState>() else {
                    return;
                };

                // Adopt an already-running local node only when it is the
                // exact matched version. A v0.7 child deliberately survives
                // the old GUI's Tauri relaunch on some platforms; treating
                // any HTTP 200 as compatible left the v0.8 desktop driving
                // that stale process and its unbound WAL indefinitely.
                let mut managed_recovery_required = false;
                match commands::probe_local_node_compatibility(
                    &state.http,
                    start_config.rpc_port,
                )
                .await
                {
                    commands::LocalNodeCompatibility::Exact if should_start => {
                        tracing::info!(
                            version = commands::EXPECTED_NODE_VERSION,
                            port = start_config.rpc_port,
                            "exact-version local node detected; proving its private shutdown receipt before restart/adoption"
                        );
                    }
                    commands::LocalNodeCompatibility::Exact => {
                        // `auto_start=false` must also clean up a desktop child
                        // left by an older updater. NodeManager only targets
                        // ARC-managed executable paths, so a separately
                        // installed system service remains operator-owned.
                        tracing::info!(
                            version = commands::EXPECTED_NODE_VERSION,
                            port = start_config.rpc_port,
                            "desktop auto-start is disabled; draining any managed leftover node"
                        );
                    }
                    commands::LocalNodeCompatibility::Absent => {}
                    commands::LocalNodeCompatibility::Incompatible(reason) => {
                        tracing::warn!(
                            %reason,
                            port = start_config.rpc_port,
                            "refusing to adopt incompatible local node"
                        );
                    }
                }

                // Drain any desktop-managed child left by an older app
                // process, including one listening on a fallback port. This
                // reconciliation runs even when auto-start is off or migration
                // persistence failed: those states forbid starting a node but
                // must not leave the pre-update child alive. A stop failure is
                // a hard updater/startup boundary; do not race two versions
                // against one data directory.
                {
                    let legacy_resources = commands::resolve_testnet_resources(&handle);
                    let mut node = state.node.lock().await;
                    if let Err(error) =
                        node.configure_managed_data_dir(&start_config.data_dir)
                    {
                        tracing::error!(
                            %error,
                            "managed-node durability receipt is invalid; blocking startup/update"
                        );
                        return;
                    }
                    if let Some(reason) = legacy_migration_block {
                        node.block_legacy_windows_reconciliation(reason.clone());
                        *state.data_migration_error.lock().await = Some(reason.clone());
                        tracing::error!(
                            %reason,
                            "legacy desktop migration is not durable; leaving the old node running and blocking startup/update"
                        );
                        return;
                    }
                    if let Some((notice, validator_seed)) = legacy_windows_stop {
                        if let Err(error) = node.configure_legacy_windows_stop_context(
                            &start_config,
                            &notice,
                            validator_seed,
                            &legacy_resources,
                        ) {
                            *state.data_migration_error.lock().await = Some(format!(
                                "one-time legacy node reconciliation context is invalid: {error}"
                            ));
                            tracing::error!(
                                %error,
                                "one-time tokenless legacy node context is invalid; blocking startup/update"
                            );
                            return;
                        }
                    }
                    if let Err(error) = node.stop().await {
                        if node_manager::is_managed_durability_recovery_required(&error) {
                            managed_recovery_required = true;
                            tracing::warn!(
                                %error,
                                "an inherited managed-node durability fence requires a quarantined recovery cycle"
                            );
                        } else {
                            *state.data_migration_error.lock().await = Some(format!(
                                "managed-node startup reconciliation failed: {error}"
                            ));
                            tracing::error!(
                                %error,
                                "could not stop stale managed arc-node; suppressing auto-start"
                            );
                            return;
                        }
                    }
                }

                if managed_recovery_required {
                    if let Err(error) =
                        commands::recover_managed_shutdown_inner(&handle, &state).await
                    {
                        *state.data_migration_error.lock().await = Some(format!(
                            "managed-node durability recovery failed: {error}"
                        ));
                        tracing::error!(
                            %error,
                            "quarantined managed-node replay/WAL recovery failed; blocking startup/update"
                        );
                        return;
                    }
                }

                // Only after the exact old/detached process boundary is clear
                // may WebView Start/Ensure/Update entrypoints mutate or spawn.
                *state.data_migration_error.lock().await = None;

                if !should_start {
                    if start_config.auto_start && !migration_allows_autostart {
                        tracing::error!(
                            "node auto-start remains suppressed because legacy-data migration was not durably persisted"
                        );
                    }
                    return;
                }

                match commands::start_node_inner(&handle, &state).await {
                    Ok(()) => tracing::info!("auto-started arc-node on launch"),
                    // Surfaced in the log ring and the Dashboard's error
                    // state rather than thrown away; a failed auto-start
                    // is exactly what the user needs told.
                    Err(e) => tracing::error!("auto-start failed: {}", e),
                }
            });

            Ok(())
        })
        // Window-close hides to tray instead of exiting. arc-node
        // (spawned as our child) keeps running. Real exit is via the
        // tray → Quit menu item, which calls app.exit() after stopping
        // arc-node cleanly.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    // Only hide-to-tray if there is a tray to hide to.
                    // Otherwise let the close proceed, stopping arc-node
                    // first so we don't strand an orphaned child process.
                    let tray_present = window
                        .app_handle()
                        .try_state::<AppState>()
                        .map(|s| s.has_tray.load(std::sync::atomic::Ordering::SeqCst))
                        .unwrap_or(false);
                    if tray_present {
                        let _ = window.hide();
                        api.prevent_close();
                    } else {
                        let handle = window.app_handle().clone();
                        tauri::async_runtime::spawn(async move {
                            if let Some(state) = handle.try_state::<AppState>() {
                                let mut node = state.node.lock().await;
                                let _ = node.stop().await;
                            }
                            handle.exit(0);
                        });
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::detect_hardware,
            commands::generate_identity,
            commands::import_identity,
            commands::load_identity,
            commands::save_config,
            commands::load_config,
            commands::load_data_migration_notice,
            commands::dismiss_data_migration_notice,
            commands::start_node,
            commands::stop_node,
            commands::prepare_update_relaunch,
            commands::abort_update_relaunch,
            commands::restart_node,
            commands::reset_peer_state,
            commands::node_status,
            commands::fetch_earnings,
            commands::fetch_attestations,
            commands::fetch_logs,
            commands::fetch_network_stats,
            commands::fetch_reward_economics,
            commands::fetch_earnings_projection,
            commands::fetch_node_contribution,
            commands::fetch_network_overview,
            commands::fetch_recent_blocks,
            commands::fetch_block_txs,
            commands::lookup_tx,
            commands::open_external,
            commands::save_logs,
            commands::set_worker_threads,
            commands::reveal_seed_phrase,
            commands::fetch_balance,
            commands::faucet_claim,
            commands::send_arc,
            commands::run_inference,
            commands::run_inference_via_coordinator,
            commands::run_inference_via_coordinator_direct,
            commands::tier1_submit,
            commands::tier1_result,
            commands::run_paid_inference,
            commands::clear_crash,
            commands::ensure_binary,
            commands::get_autostart,
            commands::update_install_policy,
            commands::list_model_tiers,
            commands::recommended_tier,
            commands::existing_model_for_tier,
            commands::download_model,
            commands::remove_model,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ARC desktop");
}
