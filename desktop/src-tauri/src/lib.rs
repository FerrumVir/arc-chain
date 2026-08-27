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
    let state = AppState {
        node,
        store: store.clone(),
        data_dir: data_dir.clone(),
        http,
        tier1_routes: Arc::new(Mutex::new(HashMap::new())),
        chain_host: Arc::new(Mutex::new(None)),
        wallet_write: Arc::new(Mutex::new(())),
        has_tray: has_tray.clone(),
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
            let loaded_store = store::Store::load_from(&resolved);
            let autostart_desired = loaded_store
                .config
                .as_ref()
                .map(|c| c.auto_start)
                .unwrap_or(true);
            // Capture what we need for the auto-start decision before the
            // store moves into the shared mutex.
            let start_config = loaded_store.config.clone().unwrap_or_default();
            let has_identity = loaded_store.identity.is_some();

            let store_shared = store.clone();
            let data_dir_shared = data_dir.clone();
            tauri::async_runtime::block_on(async move {
                *store_shared.lock().await = loaded_store;
                *data_dir_shared.lock().await = resolved;
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
            if start_config.auto_start && has_identity {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let Some(state) = handle.try_state::<AppState>() else { return };

                    // Don't fight an already-running node: a community
                    // installer daemon, or our own child surviving a dev
                    // rebuild, already owns this port.
                    let probe = format!(
                        "{}/health",
                        paths::local_host(start_config.rpc_port)
                    );
                    if let Ok(r) = state.http.get(&probe).send().await {
                        if r.status().is_success() {
                            tracing::info!("a node already answers on {} - not starting another", probe);
                            return;
                        }
                    }

                    match commands::start_node_inner(&handle, &state, &start_config).await {
                        Ok(()) => tracing::info!("auto-started arc-node on launch"),
                        // Surfaced in the log ring and the Dashboard's error
                        // state rather than thrown away; a failed auto-start
                        // is exactly what the user needs told.
                        Err(e) => tracing::error!("auto-start failed: {}", e),
                    }
                });
            }

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
            commands::start_node,
            commands::stop_node,
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
            commands::list_model_tiers,
            commands::recommended_tier,
            commands::existing_model_for_tier,
            commands::download_model,
            commands::remove_model,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ARC desktop");
}
