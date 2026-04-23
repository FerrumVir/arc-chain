mod commands;
mod hardware;
mod identity;
mod node_manager;
mod rpc_client;
mod store;
mod tray;
mod types;

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
        .build()
        .unwrap_or_default();

    let state = AppState {
        node,
        store: store.clone(),
        data_dir: data_dir.clone(),
        http,
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
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
            // Load existing store from the resolved dir, if any.
            let loaded_store = store::Store::load_from(&resolved);
            let autostart_desired = loaded_store
                .config
                .as_ref()
                .map(|c| c.auto_start)
                .unwrap_or(true);

            let store_shared = store.clone();
            let data_dir_shared = data_dir.clone();
            tauri::async_runtime::block_on(async move {
                *store_shared.lock().await = loaded_store;
                *data_dir_shared.lock().await = resolved;
            });

            // Sync the autostart plugin with what the user chose during
            // onboarding (default: on). Errors are non-fatal — tray and
            // window still work without it.
            let autostart = app.autolaunch();
            match (autostart_desired, autostart.is_enabled().unwrap_or(false)) {
                (true, false) => { let _ = autostart.enable(); }
                (false, true) => { let _ = autostart.disable(); }
                _ => {}
            }

            // Build the system tray icon. Gives the user a way to open the
            // window after hide-to-tray, and a real Quit so arc-node can
            // be stopped explicitly.
            tray::install(app.handle())?;

            // If the app was launched with `--minimized` (set by the
            // autostart plugin on login), keep the window hidden and
            // let the tray be the only surface until the user clicks it.
            let launched_minimized = std::env::args().any(|a| a == "--minimized");
            if launched_minimized {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.hide();
                }
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
                    let _ = window.hide();
                    api.prevent_close();
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
            commands::node_status,
            commands::fetch_earnings,
            commands::fetch_attestations,
            commands::fetch_logs,
            commands::fetch_network_stats,
            commands::open_external,
            commands::check_for_update,
            commands::fetch_balance,
            commands::faucet_claim,
            commands::run_inference,
            commands::clear_crash,
            commands::ensure_binary,
            commands::get_autostart,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ARC desktop");
}
