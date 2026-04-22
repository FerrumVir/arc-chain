mod commands;
mod hardware;
mod identity;
mod node_manager;
mod rpc_client;
mod store;
mod types;

use std::path::PathBuf;
use std::sync::Arc;
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
        // Updater plugin disabled for the testnet release: a signing keypair
        // needs to be generated and paid for before auto-updates can be
        // trusted. Re-enable once tauri.conf.json > plugins.updater.pubkey
        // is populated.
        .manage(state)
        .setup(move |app| {
            // Resolve the per-platform writable dir NOW (AppHandle available).
            use tauri::Manager;
            let resolver = app.path();
            let resolved = resolver
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir());
            tracing::info!("app data dir: {}", resolved.display());
            // Load existing store from the resolved dir, if any.
            let loaded = store::Store::load_from(&resolved);
            let store = store.clone();
            let data_dir = data_dir.clone();
            tauri::async_runtime::block_on(async move {
                *store.lock().await = loaded;
                *data_dir.lock().await = resolved;
            });
            Ok(())
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running ARC desktop");
}
