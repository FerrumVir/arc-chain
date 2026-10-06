mod auto_update;
mod commands;
mod hardware;
mod identity;
mod native_paid;
mod network_live;
mod node_manager;
mod paths;
mod production_acceptance;
mod rpc_client;
mod store;
mod tray;
mod types;
mod updater_channel;
mod wallet;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Manager, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as AutostartManagerExt};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

const AUTOSTART_RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const AUTOSTART_RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

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
    delay: std::time::Duration,
) -> bool {
    tokio::select! {
        biased;
        _ = cancel.changed() => false,
        _ = tokio::time::sleep(delay) => true,
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
    /// Maps an in-flight Tier 1 request_id to the seed VPS that accepted the
    /// submit. Each seed runs its own chain, so the poll must hit the same
    /// host. In-memory only — survives only for the lifetime of the process,
    /// which is fine because Tier 1 requests finalize in seconds.
    pub tier1_routes: Arc<Mutex<HashMap<String, String>>>,
    /// Exact receipt identity + origin captured natively from a successful
    /// inference response. Receipt polling must match this entry; an IPC caller
    /// cannot select a different allowlisted seed after the fact.
    pub community_receipt_routes: Arc<Mutex<HashMap<String, CommunityReceiptRoute>>>,
    /// Once a community inference returns a settlement identity, all chain
    /// reads in this process stay on that exact coordinator.  Production
    /// validators are intentionally independent chains; electing a different,
    /// merely fresher seed for Earnings or Explorer can make a just-confirmed
    /// payment disappear or cross-bind it to unrelated block history.
    pub community_chain_host: Arc<Mutex<Option<String>>>,
    /// Serializes community inference writes so two IPC requests cannot race
    /// the session's first immutable receipt-origin selection.
    pub community_inference_write: Arc<Mutex<()>>,
    /// The immutable seed elected by the first session chain read, plus when
    /// it was elected. An unavailable source remains unavailable rather than
    /// silently switching wallets or reward history to an independent chain.
    pub chain_host: Arc<Mutex<Option<(commands::ChainHostChoice, std::time::Instant)>>>,
    /// Serializes wallet writes so two UI clicks cannot sign the same account
    /// nonce concurrently. This lock never contains the recovery phrase.
    pub wallet_write: Arc<Mutex<()>>,
    pub auto_update_requests: Arc<Mutex<Option<auto_update::RequestFence>>>,
    /// Whether a system tray icon was actually created. Gates hide-to-tray:
    /// on a desktop with no tray, hiding the window makes the app
    /// unreachable.
    pub has_tray: Arc<std::sync::atomic::AtomicBool>,
    /// Authoritative native fail-closed fence for an unresolved data
    /// migration preflight. Every command that can start arc-node consults
    /// this state; a WebView Start/Restart click cannot bypass a startup
    /// failure and replay an ambiguous legacy WAL.
    pub data_migration_error: Arc<Mutex<Option<String>>>,
    /// Cancels the single per-launch auto-start transaction and its bounded-
    /// rate network retry loop when Stop, Quit, update, or settings disable it.
    pub startup_retry_cancel: tokio::sync::watch::Sender<()>,
}

impl AppState {
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

fn configured_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        // A signed transaction is bound to the elected origin. Never allow a
        // gateway redirect to move that POST to another scheme or host.
        .redirect(reqwest::redirect::Policy::none())
        // Interactive launches may use an explicitly configured proxy, but
        // reqwest must never replay a wallet or inference write by itself.
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

    // Acceptance is a shipped-executable native-core proof, not a UI test.
    // Branch before constructing Tauri so no WebView page, plugin, tray,
    // updater, autostart hook, or IPC handler can run alongside its one-shot
    // inference. The separately sealed AppImage gate covers real UI → IPC.
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
    // Store starts empty; `setup()` resolves the per-platform writable
    // data dir via Tauri's PathResolver and loads from there.
    let store = Arc::new(Mutex::new(store::Store::default()));
    let data_dir = Arc::new(Mutex::new(PathBuf::new()));
    let http = match configured_http_client() {
        Ok(http) => http,
        Err(error) => {
            // Falling back to a default client here would silently re-enable
            // redirects for wallet and inference traffic. If the constrained
            // transport cannot be constructed, refuse to start instead.
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
            let node_shared = app.state::<AppState>().node.clone();
            let configured_rpc_port = start_config.rpc_port;
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
                node_shared
                    .lock()
                    .await
                    .configure_rpc_port_if_stopped(configured_rpc_port);
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

            // Keep managed-node crash supervision alive while the window is
            // hidden. It reuses NodeManager's exact launch plan and lifecycle
            // lock, and has no effect on an intentional Stop/Quit.
            let supervisor_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let Some(state) = supervisor_handle.try_state::<AppState>() else {
                        break;
                    };
                    state.node.lock().await.supervise_managed_node().await;
                }
            });

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
                let mut startup_cancel = startup_retry_cancel_receiver.clone();

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

                let mut retry_delay = AUTOSTART_RETRY_BASE_DELAY;
                loop {
                    if startup_cancel.has_changed().unwrap_or(true) {
                        tracing::info!("auto-start cancelled by an explicit lifecycle or settings action");
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
                            // A user who opted in to contributing compute but
                            // is not a worker yet (an interrupted model
                            // download, or consent given while offline) is
                            // finished here, so the install takes jobs without
                            // another click. No-op without consent.
                            commands::promote_consented_install_on_startup(&handle, &state).await;
                            break;
                        }
                        Err(error) if error.is_transient() => {
                            tracing::warn!(
                                %error,
                                delay_secs = retry_delay.as_secs(),
                                "auto-start hit a transient release-network failure; retrying"
                            );
                            if !wait_autostart_retry(&mut startup_cancel, retry_delay).await {
                                tracing::info!("auto-start retry cancelled by an explicit lifecycle or settings action");
                                break;
                            }
                            retry_delay = next_autostart_retry_delay(retry_delay);
                        }
                        Err(error) => {
                            tracing::error!(%error, "auto-start failed permanently");
                            break;
                        }
                    }
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
                                state.cancel_startup_retry();
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
            commands::try_prepare_auto_update_relaunch,
            commands::begin_update_handoff,
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
            commands::fetch_worker_status,
            network_live::network_live_read,
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
            updater_channel::check_arc_update,
            commands::list_model_tiers,
            commands::recommended_tier,
            commands::existing_model_for_tier,
            commands::download_model,
            commands::remove_model,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ARC desktop");
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

    #[tokio::test]
    async fn disabling_autostart_cancels_the_retry_delay() {
        let (cancel, mut receiver) = tokio::sync::watch::channel(());
        receiver.borrow_and_update();
        let task = tokio::spawn(async move {
            wait_autostart_retry(&mut receiver, std::time::Duration::from_secs(60)).await
        });
        tokio::task::yield_now().await;
        cancel.send_replace(());
        assert!(!task.await.unwrap());
    }
}
