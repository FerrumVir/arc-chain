mod auto_update;
mod background_promotion;
mod commands;
mod desktop_instance;
mod hardware;
mod identity;
#[allow(dead_code)]
#[path = "../../../shared/keep_awake.rs"]
mod keep_awake;
mod launch_visibility;
mod legacy_upgrade;
mod native_paid;
mod node_care;
mod node_manager;
mod paths;
mod previous_install;
mod production_acceptance;
mod rpc_client;
#[cfg_attr(test, allow(dead_code))]
mod sharing_awake;
mod start_fence;
mod store;
mod tray;
mod types;
mod update_location;
mod updater_channel;
mod wake;
mod wallet;
mod worker_network;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Manager, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as AutostartManagerExt};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

const AUTOSTART_RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const AUTOSTART_RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

const AUTOSTART_SLOW_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const AUTOSTART_SLOW_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

fn next_slow_autostart_retry_delay(current: std::time::Duration) -> std::time::Duration {
    current.saturating_mul(2).min(AUTOSTART_SLOW_MAX_DELAY)
}

fn autostart_allowed(auto_start: bool, has_identity: bool, fenced: bool) -> bool {
    auto_start && has_identity && !fenced
}

fn autostart_wake_deadline(
    due: tokio::time::Instant,
    now: tokio::time::Instant,
) -> tokio::time::Instant {
    due.min(now + background_promotion::AFTER_WAKE_DELAY)
}

fn next_autostart_retry_delay(current: std::time::Duration) -> std::time::Duration {
    current.saturating_mul(2).min(AUTOSTART_RETRY_MAX_DELAY)
}

async fn cancellable_autostart_attempt<F>(
    cancel: &mut tokio::sync::watch::Receiver<()>,
    attempt: F,
) -> Option<Result<(), commands::StartupFailure>>
where
    F: std::future::Future<Output = Result<(), commands::StartupFailure>>,
{
    tokio::select! {
        biased;
        changed = cancel.changed() => {
            changed.ok()?;
            None
        }
        result = attempt => Some(result),
    }
}

async fn wait_autostart_retry(
    cancel: &mut tokio::sync::watch::Receiver<()>,
    wakes: &mut tokio::sync::watch::Receiver<u64>,
    delay: std::time::Duration,
) -> bool {
    let mut due = tokio::time::Instant::now() + delay;
    let mut wakes_open = true;
    loop {
        tokio::select! {
            biased;
            _ = cancel.changed() => return false,
            _ = tokio::time::sleep_until(due) => return true,
            changed = wakes.changed(), if wakes_open => {
                if changed.is_ok() {
                    due = autostart_wake_deadline(due, tokio::time::Instant::now());
                } else {
                    wakes_open = false;
                }
            }
        }
    }
}

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
    pub tier1_routes: Arc<Mutex<HashMap<String, String>>>,
    pub community_receipt_routes: Arc<Mutex<HashMap<String, CommunityReceiptRoute>>>,
    pub community_chain_host: Arc<Mutex<Option<String>>>,
    pub community_inference_write: Arc<Mutex<()>>,
    pub chain_host: Arc<Mutex<Option<(commands::ChainHostChoice, std::time::Instant)>>>,
    pub wallet_write: Arc<Mutex<()>>,
    pub auto_update_requests: Arc<Mutex<Option<auto_update::RequestFence>>>,
    pub has_tray: Arc<std::sync::atomic::AtomicBool>,
    pub data_migration_error: Arc<Mutex<Option<start_fence::StartFence>>>,
    pub startup_retry_cancel: tokio::sync::watch::Sender<()>,
}

impl AppState {
    pub async fn another_desktop_owns_node(&self) -> bool {
        matches!(*self.data_migration_error.lock().await, Some(start_fence::StartFence::OtherDesktop))
    }

    pub fn cancel_startup_retry(&self) {
        self.startup_retry_cancel.send_replace(());
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommunityReceiptRoute {
    pub source_host: String,
    pub job_id: String,
    pub worker: String,
    pub receipt_url: String,
}

async fn set_aside_previous_install_data(
    node: &mut node_manager::NodeManager,
    configured_data_dir: &str,
    app_data_absent: bool,
) -> Option<types::PreviousInstallNotice> {
    if !app_data_absent {
        return None;
    }
    let data_dir = node_manager::resolve_data_dir(configured_data_dir);
    let probe_dir = data_dir.clone();
    let decision = tokio::task::spawn_blocking(move || {
        previous_install::set_aside_if_previous_install(
            &probe_dir,
            app_data_absent,
            std::time::SystemTime::now(),
        )
    })
    .await;
    match decision {
        Ok(previous_install::Decision::Keep) => None,
        Ok(previous_install::Decision::Held(reason)) => {
            tracing::warn!(
                %reason,
                "node data from an earlier install stays in place; launch reconciliation runs as before"
            );
            None
        }
        Ok(previous_install::Decision::SetAside { from, to }) => {
            let shown_from = paths::display_with_home_redacted(&from);
            let shown_to = paths::display_with_home_redacted(&to);
            tracing::warn!(
                from = %shown_from,
                to = %shown_to,
                "set aside node data left by an earlier ARC install whose app data is gone"
            );
            node_manager::push_log(
                &node.logs,
                "warn",
                format!(
                    "ARC found node data from an earlier install at {shown_from} and set it aside at {shown_to}; nothing was deleted, and this install starts with a fresh data directory"
                ),
            )
            .await;
            Some(types::PreviousInstallNotice {
                set_aside_data_dir: to.display().to_string(),
                active_data_dir: data_dir.display().to_string(),
                set_aside_at: chrono::Utc::now().timestamp_millis(),
            })
        }
        Err(error) => {
            tracing::error!(
                %error,
                "the previous-install check did not complete; node data stays in place"
            );
            None
        }
    }
}

async fn record_previous_install_notice(state: &AppState, notice: types::PreviousInstallNotice) {
    let mut store = state.store.lock().await;
    store.previous_install_notice = Some(notice);
    let dir = state.data_dir.lock().await.clone();
    if let Err(error) = store.save_to(&dir) {
        tracing::error!(
            %error,
            "could not persist the previous-install notice; it shows for this session only"
        );
    }
}

async fn reconcile_previous_managed_node(
    handle: &tauri::AppHandle,
    state: &AppState,
    start_config: &types::NodeConfig,
    app_data_absent_at_launch: bool,
    legacy_migration_block: Option<String>,
    legacy_windows_stop: Option<(types::DataMigrationNotice, Zeroizing<String>)>,
) -> Result<(), String> {
    let set_aside_before_reconciliation = {
        let mut node = state.node.lock().await;
        set_aside_previous_install_data(
            &mut node,
            &start_config.data_dir,
            app_data_absent_at_launch,
        )
        .await
    };
    if let Some(notice) = set_aside_before_reconciliation.as_ref() {
        record_previous_install_notice(state, notice.clone()).await;
    }

    let mut managed_recovery_required = false;
    let mut set_aside_after_drain = None;
    let mut repoint = Ok(());
    {
        let legacy_resources = commands::resolve_testnet_resources(handle);
        let mut node = state.node.lock().await;
        if let Err(error) = node.configure_managed_data_dir(&start_config.data_dir) {
            tracing::error!(
                %error,
                "managed-node durability receipt is invalid; blocking node start"
            );
            return Err(format!(
                "managed-node startup reconciliation failed: {error}"
            ));
        }
        if let Some(reason) = legacy_migration_block {
            node.block_legacy_windows_reconciliation(reason.clone());
            tracing::error!(
                %reason,
                "legacy desktop migration is not durable; leaving the old node running and blocking startup/update"
            );
            return Err(reason);
        }
        if let Some((notice, validator_seed)) = legacy_windows_stop {
            if let Err(error) = node.configure_legacy_windows_stop_context(
                start_config,
                &notice,
                validator_seed,
                &legacy_resources,
            ) {
                tracing::error!(
                    %error,
                    "one-time tokenless legacy node context is invalid; blocking startup/update"
                );
                return Err(format!(
                    "one-time legacy node reconciliation context is invalid: {error}"
                ));
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
                tracing::error!(
                    %error,
                    "could not stop stale managed arc-node; suppressing auto-start"
                );
                if error
                    .downcast_ref::<node_manager::ManagedLifecycleContention>()
                    .is_some()
                {
                    return Err(start_fence::ALREADY_OPEN.into());
                }
                return Err(paths::redact_home_text(&format!(
                    "managed-node startup reconciliation failed: {error}"
                )));
            }
        }
        if set_aside_before_reconciliation.is_none() {
            set_aside_after_drain = set_aside_previous_install_data(
                &mut node,
                &start_config.data_dir,
                app_data_absent_at_launch,
            )
            .await;
            if set_aside_after_drain.is_some() {
                managed_recovery_required = false;
                repoint = node.configure_managed_data_dir(&start_config.data_dir);
            }
        }
    }
    if let Some(notice) = set_aside_after_drain {
        record_previous_install_notice(state, notice).await;
    }
    if let Err(error) = repoint {
        return Err(format!(
            "managed-node startup reconciliation failed: {error}"
        ));
    }

    if managed_recovery_required {
        if let Err(error) = commands::recover_managed_shutdown_inner(handle, state).await {
            tracing::error!(
                %error,
                "quarantined managed-node replay/WAL recovery failed; blocking node start"
            );
            return Err(format!("managed-node durability recovery failed: {error}"));
        }
        tracing::info!(
            "recovered the previous managed node's unacknowledged shutdown; its durability receipt is clear"
        );
    }
    Ok(())
}

fn configured_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|error| format!("build redirect-fenced desktop HTTP client: {error}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let production_acceptance = match production_acceptance::request_from_args(std::env::args_os())
    {
        Ok(request) => request,
        Err(error) => {
            eprintln!("ARC packaged production acceptance refused: {error}");
            std::process::exit(64);
        }
    };
    if production_acceptance.is_some() {
        if let Err(error) = production_acceptance::reject_ambient_network_authority() {
            eprintln!("ARC packaged production acceptance refused before Tauri setup: {error}");
            std::process::exit(64);
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,arc_desktop_lib=debug".into()),
        )
        .init();

    if let Some(request) = production_acceptance {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("ARC packaged production acceptance could not start its isolated async runtime: {error}");
                std::process::exit(70);
            }
        };
        match runtime.block_on(production_acceptance::run_standalone(&request)) {
            Ok(()) => {
                println!(
                    "VERIFIED ARC packaged native production acceptance {}",
                    request.output.display()
                );
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("ARC packaged native production acceptance failed: {error}");
                std::process::exit(70);
            }
        }
    }

    let node = Arc::new(Mutex::new(node_manager::NodeManager::new()));
    let store = Arc::new(Mutex::new(store::Store::default()));
    let data_dir = Arc::new(Mutex::new(PathBuf::new()));
    let http = match configured_http_client() {
        Ok(http) => http,
        Err(error) => {
            eprintln!("ARC desktop refused an unconstrained HTTP fallback: {error}");
            std::process::exit(70);
        }
    };

    let has_tray = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let data_migration_error = Arc::new(Mutex::new(None));
    let (startup_retry_cancel, _) = tokio::sync::watch::channel(());
    let mut startup_retry_cancel_receiver = startup_retry_cancel.subscribe();
    startup_retry_cancel_receiver.borrow_and_update();
    let state = AppState {
        node,
        store: store.clone(),
        data_dir: data_dir.clone(),
        http,
        tier1_routes: Arc::new(Mutex::new(HashMap::new())),
        community_receipt_routes: Arc::new(Mutex::new(HashMap::new())),
        community_chain_host: Arc::new(Mutex::new(None)),
        community_inference_write: Arc::new(Mutex::new(())),
        chain_host: Arc::new(Mutex::new(None)),
        wallet_write: Arc::new(Mutex::new(())),
        auto_update_requests: Arc::new(Mutex::new(None)),
        has_tray: has_tray.clone(),
        data_migration_error: data_migration_error.clone(),
        startup_retry_cancel,
    };

    tauri::Builder::default()
        .plugin(desktop_instance::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(
            tauri_plugin_autostart::init(
                MacosLauncher::LaunchAgent,
                Some(vec!["--minimized"]),
            ),
        )
        .manage(state)
        .setup(move |app| {
            if desktop_instance::show_startup_failure(app.handle()) {
                return Ok(());
            }
            let resolver = app.path();
            let resolved = resolver
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir());
            tracing::info!("app data dir: {}", resolved.display());

            if let Ok(home) = resolver.home_dir() {
                paths::set_home_fallback(home);
            }

            if let Some(existing) = store::Store::load_for_owner_probe(&resolved) {
                if let Some(config) = existing.config.as_ref() {
                    if !legacy_upgrade::config_uses_v07_layout(config, &paths::home_dir()) {
                        if let Err(error) = node_manager::probe_existing_managed_lifecycle_owner(&config.data_dir) {
                            if error.downcast_ref::<node_manager::ManagedLifecycleContention>().is_some() {
                                tauri::async_runtime::block_on(async {
                                    *store.lock().await = existing;
                                    *data_dir.lock().await = resolved;
                                    *data_migration_error.lock().await = Some(start_fence::StartFence::OtherDesktop);
                                });
                                return Ok(());
                            }
                        }
                    }
                }
            }

            let (mut loaded_store, store_load) =
                store::Store::load_from_with_outcome(&resolved);
            let app_data_absent_at_launch = previous_install::app_data_was_absent(store_load);
            if store_load == store::StoreLoadOutcome::Failed {
                tracing::error!(
                    "the identity store is present but could not be read; node data stays in place and the store is not rewritten by launch"
                );
            }
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
            let config_uses_v07_layout = loaded_store.config.as_ref().is_some_and(|config| {
                legacy_upgrade::config_uses_v07_layout(config, &paths::home_dir())
            });
            let migration_result = match legacy_process_data_dir {
                Ok(Some(path)) => loaded_store.protect_running_legacy_v07_data_at(&path),
                Ok(None) => loaded_store.protect_legacy_v07_data(),
                Err(error) => Err(error),
            };
            let migration_created_notice = matches!(migration_result, Ok(Some(_)));
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
            let startup_hold = legacy_upgrade::hold_at_startup(
                &mut loaded_store,
                &resolved,
                &paths::home_dir(),
                migration_created_notice,
                config_uses_v07_layout,
                legacy_upgrade::now_unix_ms(),
            );
            if startup_hold.hold.held {
                tracing::warn!(
                    "upgraded from v0.7: compute contribution is held until the user answers the first-launch question"
                );
            }
            if let Some(error) = startup_hold.hold.record_error {
                tracing::error!(%error, "could not record the v0.7 upgrade compute question; the node is held as an observer anyway");
            }
            if let Some(error) = startup_hold.persist_error {
                tracing::error!(%error, "could not persist the held observer config");
            }
            if let Err(error) =
                auto_update::apply_unattended_install_default(&mut loaded_store, &resolved)
            {
                tracing::error!(%error, "could not persist the unattended-update default");
            }
            if !startup_hold.hold.held && loaded_store.identity.is_some() && loaded_store.config.is_some() {
                let (ram_gb, cpu_cores) = hardware::compute_capacity();
                let before = loaded_store.config.clone();
                if commands::apply_compute_default(&mut loaded_store, ram_gb, cpu_cores, false) {
                    if let Err(error) = loaded_store.save_to(&resolved) {
                        loaded_store.config = before;
                        tracing::error!(%error, "could not persist sharing default; leaving the previous choice intact");
                    }
                }
            }
            if let Some(config) = &loaded_store.config {
                sharing_awake::set_enabled(config.keep_awake_while_sharing == Some(true)
                    && commands::compute_contribution_enabled(config));
            }
            let autostart_desired = loaded_store
                .config
                .as_ref()
                .map(|c| c.auto_start)
                .unwrap_or(true);
            let start_config = loaded_store.config.clone().unwrap_or_default();
            let has_identity = loaded_store.identity.is_some();
            let has_config = loaded_store.config.is_some();
            let legacy_windows_stop = durable_legacy_stop_material(
                migration_notice_is_durable,
                loaded_store.data_migration_notice.clone(),
                legacy_identity_seed,
            );
            let legacy_migration_block = migration_failure_reason.clone();

            let store_shared = store.clone();
            let data_dir_shared = data_dir.clone();
            let migration_error_shared = data_migration_error.clone();
            let node_shared = app.state::<AppState>().node.clone();
            let configured_rpc_port = start_config.rpc_port;
            let startup_fence = start_fence::at_launch(migration_failure_reason.clone());
            tauri::async_runtime::block_on(async move {
                *store_shared.lock().await = loaded_store;
                *data_dir_shared.lock().await = resolved;
                *migration_error_shared.lock().await = Some(startup_fence);
                node_shared
                    .lock()
                    .await
                    .configure_rpc_port_if_stopped(configured_rpc_port);
            });

            let autostart = app.autolaunch();
            let enabled_now = autostart.is_enabled().unwrap_or(false);
            if autostart_desired {
                if let Err(e) = autostart.enable() {
                    tracing::warn!("could not register the login item: {}", e);
                }
            } else if enabled_now {
                if let Err(e) = autostart.disable() {
                    tracing::warn!("could not remove the login item: {}", e);
                }
            }

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

            let supervisor_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let Some(state) = supervisor_handle.try_state::<AppState>() else {
                        break;
                    };
                    let auto_start = state.store.lock().await.config.as_ref()
                        .is_some_and(|config| config.auto_start);
                    let fenced = state.data_migration_error.lock().await.is_some();
                    let mut node = state.node.lock().await;
                    node.set_crash_autostart_enabled(auto_start && !fenced);
                    node.supervise_managed_node().await;
                }
            });

            wake::spawn(app.handle().clone());

            let launched_minimized = std::env::args().any(|a| a == "--minimized");
            if launch_visibility::hide_on_launch(
                launched_minimized,
                has_tray.load(std::sync::atomic::Ordering::SeqCst),
                has_identity,
                has_config,
            ) {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.hide();
                }
            }

            let should_start =
                start_config.auto_start && has_identity && migration_allows_autostart;
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let Some(state) = handle.try_state::<AppState>() else {
                    return;
                };
                let mut startup_cancel = startup_retry_cancel_receiver.clone();

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

                let reconciliation = reconcile_previous_managed_node(
                    &handle,
                    &state,
                    &start_config,
                    app_data_absent_at_launch,
                    legacy_migration_block,
                    legacy_windows_stop,
                )
                .await;
                let reconciled = reconciliation.is_ok();
                let mut fence = state.data_migration_error.lock().await;
                if !matches!(*fence, Some(start_fence::StartFence::MigrationRefused(_))) {
                    *fence = start_fence::after_startup_reconciliation(reconciliation);
                }
                let other_desktop = matches!(*fence, Some(start_fence::StartFence::OtherDesktop));
                drop(fence);
                if other_desktop {
                    state.has_tray.store(false, std::sync::atomic::Ordering::SeqCst);
                    let app = handle.clone();
                    let _ = handle.run_on_main_thread(move || {
                        app.remove_tray_by_id("main");
                        desktop_instance::show_main(&app);
                    });
                }
                if !reconciled {
                    return;
                }

                tauri::async_runtime::spawn(background_promotion::run_retry_loop(handle.clone()));

                if !should_start {
                    if start_config.auto_start && !migration_allows_autostart {
                        tracing::error!(
                            "node auto-start remains suppressed because legacy-data migration was not durably persisted"
                        );
                    }
                    return;
                }

                let mut retry_delay = AUTOSTART_RETRY_BASE_DELAY;
                let mut slow_retry_delay = AUTOSTART_SLOW_BASE_DELAY;
                let mut wakes = wake::subscribe();
                loop {
                    if startup_cancel.has_changed().unwrap_or(true) {
                        tracing::info!("auto-start cancelled by an explicit lifecycle or settings action");
                        break;
                    }
                    let (auto_start, has_identity) = {
                        let store = state.store.lock().await;
                        (store.config.as_ref().is_some_and(|config| config.auto_start),
                            store.identity.is_some())
                    };
                    let fenced = state.data_migration_error.lock().await.is_some();
                    if !autostart_allowed(auto_start, has_identity, fenced) {
                        break;
                    }
                    let Some(outcome) = cancellable_autostart_attempt(
                        &mut startup_cancel,
                        commands::autostart_node_inner(&handle, &state),
                    )
                    .await else {
                        tracing::info!("auto-start cancelled by an explicit lifecycle or settings action");
                        break;
                    };
                    match outcome {
                        Ok(()) => {
                            tracing::info!("auto-started arc-node on launch");
                            commands::promote_consented_install_on_startup(&handle, &state).await;
                            break;
                        }
                        Err(error) if error.is_transient() => {
                            tracing::warn!(
                                %error,
                                delay_secs = retry_delay.as_secs(),
                                "auto-start hit a transient release-network failure; retrying"
                            );
                            if !wait_autostart_retry(&mut startup_cancel, &mut wakes, retry_delay).await {
                                tracing::info!("auto-start retry cancelled by an explicit lifecycle or settings action");
                                break;
                            }
                            retry_delay = next_autostart_retry_delay(retry_delay);
                        }
                        Err(error) => {
                            tracing::warn!(%error, delay_secs = slow_retry_delay.as_secs(),
                                "auto-start failed; waiting before another attempt");
                            if !wait_autostart_retry(&mut startup_cancel, &mut wakes, slow_retry_delay).await {
                                break;
                            }
                            slow_retry_delay = next_slow_autostart_retry_delay(slow_retry_delay);
                        }
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Focused(true) = event {
                if window.label() == "main" {
                    wake::emit_signal(window.app_handle(), wake::SignalReason::Focus);
                }
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let tray_present = window
                        .app_handle()
                        .try_state::<AppState>()
                        .map(|s| s.has_tray.load(std::sync::atomic::Ordering::SeqCst))
                        .unwrap_or(false);
                    if tray_present {
                        let _ = window.hide();
                        api.prevent_close();
                    } else {
                        api.prevent_close();
                        desktop_instance::begin_closing(window.app_handle());
                        sharing_awake::set_enabled(false);
                        let handle = window.app_handle().clone();
                        tauri::async_runtime::spawn(async move {
                            if let Some(state) = handle.try_state::<AppState>() {
                                state.cancel_startup_retry();
                                if !state.another_desktop_owns_node().await {
                                    let mut node = state.node.lock().await;
                                    let _ = node.stop().await;
                                }
                            }
                            crate::sharing_awake::set_enabled(false);
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
            commands::complete_onboarding,
            commands::load_config,
            commands::load_data_migration_notice,
            commands::dismiss_data_migration_notice,
            commands::load_previous_install_notice,
            commands::dismiss_previous_install_notice,
            legacy_upgrade::load_legacy_compute_question,
            legacy_upgrade::answer_legacy_compute_question,
            commands::claim_compute_prompt,
            commands::acknowledge_compute_notice,
            commands::set_keep_awake_while_sharing,
            commands::accept_compute_prompt,
            commands::start_node,
            commands::stop_node,
            commands::prepare_update_relaunch,
            commands::try_prepare_auto_update_relaunch,
            commands::begin_update_handoff,
            commands::abort_update_relaunch,
            commands::restart_node,
            commands::reset_peer_state,
            commands::node_status,
            commands::node_start_block,
            commands::fetch_earnings,
            commands::fetch_attestations,
            commands::fetch_logs,
            commands::fetch_network_stats,
            commands::fetch_reward_economics,
            commands::fetch_earnings_projection,
            commands::fetch_node_contribution,
            commands::fetch_worker_status,
            commands::fetch_worker_network,
            commands::set_compute_contribution,
            commands::promote_consented_install,
            commands::set_prevent_sleep_during_jobs,
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
            commands::fetch_community_reward_receipt,
            commands::tier1_submit,
            commands::tier1_result,
            commands::run_paid_inference,
            native_paid::native_context,
            native_paid::native_prepare,
            native_paid::native_submit,
            native_paid::native_receipt,
            native_paid::native_refund,
            native_paid::native_resubmit,
            native_paid::native_journal,
            commands::clear_crash,
            commands::ensure_binary,
            commands::get_autostart,
            commands::update_install_policy,
            commands::update_location_block,
            updater_channel::check_arc_update,
            commands::list_model_tiers,
            commands::recommended_tier,
            commands::existing_model_for_tier,
            commands::download_model,
            commands::remove_model,
            node_care::node_care_info,
            node_care::node_care_load_state,
            node_care::node_care_save_state,
            node_care::node_care_send,
        ])
        .build(tauri::generate_context!())
        .expect("error while building ARC desktop")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Ready) {
                desktop_instance::finish_startup(app);
            }
            #[cfg(target_os = "macos")]
            if matches!(event, tauri::RunEvent::Reopen { .. }) {
                desktop_instance::show_main(app);
            }
            if matches!(event, tauri::RunEvent::Exit) {
                sharing_awake::set_enabled(false);
            }
        });
}

#[cfg(test)]
mod startup_retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn autostart_backoff_doubles_and_caps_at_one_minute() {
        let delays = [1, 2, 4, 8, 16, 32, 60, 60];
        let mut delay = AUTOSTART_RETRY_BASE_DELAY;
        for seconds in delays {
            assert_eq!(delay, std::time::Duration::from_secs(seconds));
            delay = next_autostart_retry_delay(delay);
        }
    }

    #[test]
    fn lasting_failures_retry_slowly_and_cap_at_two_hours() {
        let mut delay = AUTOSTART_SLOW_BASE_DELAY;
        for minutes in [15, 30, 60, 120, 120] {
            assert_eq!(delay.as_secs(), minutes * 60);
            delay = next_slow_autostart_retry_delay(delay);
        }
    }

    #[test]
    fn wake_advances_a_slow_retry_but_never_delays_a_due_attempt() {
        let now = tokio::time::Instant::now();
        assert_eq!(
            autostart_wake_deadline(now + AUTOSTART_SLOW_BASE_DELAY, now),
            now + std::time::Duration::from_secs(90)
        );
        assert_eq!(autostart_wake_deadline(now, now), now);
    }

    #[test]
    fn disabling_autostart_missing_setup_and_start_fences_forbid_retries() {
        assert!(autostart_allowed(true, true, false));
        assert!(!autostart_allowed(false, true, false));
        assert!(!autostart_allowed(true, false, false));
        assert!(!autostart_allowed(true, true, true));
    }

    #[tokio::test]
    async fn stop_cancels_pending_autostart_attempt_and_drops_its_future() {
        struct DropFlag(Arc<AtomicUsize>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let (cancel, mut receiver) = tokio::sync::watch::channel(());
        receiver.borrow_and_update();
        let dropped = Arc::new(AtomicUsize::new(0));
        let future_dropped = dropped.clone();
        let attempt = async move {
            let _drop_flag = DropFlag(future_dropped);
            std::future::pending::<Result<(), commands::StartupFailure>>().await
        };
        let task =
            tokio::spawn(
                async move { cancellable_autostart_attempt(&mut receiver, attempt).await },
            );
        tokio::task::yield_now().await;
        cancel.send_replace(());
        assert!(task.await.unwrap().is_none());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn successful_autostart_attempt_is_delivered_once() {
        let (_cancel, mut receiver) = tokio::sync::watch::channel(());
        receiver.borrow_and_update();
        let starts = Arc::new(AtomicUsize::new(0));
        let started = starts.clone();
        let result = cancellable_autostart_attempt(&mut receiver, async move {
            started.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_native_wake_interrupts_the_slow_wait_after_network_grace() {
        let (_cancel, mut cancel_rx) = tokio::sync::watch::channel(());
        let (wakes, mut wake_rx) = tokio::sync::watch::channel(0);
        let task = tokio::spawn(async move {
            wait_autostart_retry(&mut cancel_rx, &mut wake_rx, AUTOSTART_SLOW_BASE_DELAY).await
        });
        tokio::task::yield_now().await;
        wakes.send_replace(1);
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(89)).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(task.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn stop_also_cancels_the_wake_grace_period() {
        let (cancel, mut cancel_rx) = tokio::sync::watch::channel(());
        let (wakes, mut wake_rx) = tokio::sync::watch::channel(0);
        let task = tokio::spawn(async move {
            wait_autostart_retry(&mut cancel_rx, &mut wake_rx, AUTOSTART_SLOW_BASE_DELAY).await
        });
        tokio::task::yield_now().await;
        wakes.send_replace(1);
        tokio::task::yield_now().await;
        cancel.send_replace(());
        assert!(!task.await.unwrap());
    }

    #[tokio::test]
    async fn disabling_autostart_cancels_the_retry_delay() {
        let (cancel, mut receiver) = tokio::sync::watch::channel(());
        receiver.borrow_and_update();
        let task = tokio::spawn(async move {
            wait_autostart_retry(
                &mut receiver,
                &mut wake::subscribe(),
                std::time::Duration::from_secs(60),
            )
            .await
        });
        tokio::task::yield_now().await;
        cancel.send_replace(());
        assert!(!task.await.unwrap());
    }
}

#[cfg(test)]
mod startup_reconciliation_tests {
    #[test]
    fn every_reconciliation_exit_resolves_the_launch_fence() {
        let source = include_str!("lib.rs");
        let start = source
            .find("async fn reconcile_previous_managed_node(")
            .expect("reconciliation function");
        let body = &source[start..];
        let body = &body[..body
            .find("fn configured_http_client()")
            .expect("end of reconciliation function")];

        let fence_write = [
            "let mut fence = state.",
            "data_migration_error.lock().await;",
        ]
        .concat();
        let resolve = ["start_fence::", "after_startup_reconciliation("].concat();
        let recover = ["commands::", "recover_managed_shutdown_inner("].concat();

        assert!(!body.contains("data_migration_error"));
        assert!(body.contains(&recover));
        assert_eq!(source.matches(&recover).count(), 1);
        assert_eq!(source.matches(&fence_write).count(), 1);
        assert_eq!(source.matches(&resolve).count(), 1);
        let task_write: String = source[source.find(&fence_write).unwrap()..]
            .chars()
            .take(400)
            .collect();
        assert!(task_write.contains(&resolve));
    }
}
