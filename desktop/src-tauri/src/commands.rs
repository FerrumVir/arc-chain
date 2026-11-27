use crate::node_manager::{managed_binary_path, TestnetResources};
use crate::start_fence::{AppUpdateBoundary, StartAccess};
use crate::types::*;
use crate::{hardware, identity, paths, rpc_client, store::Store, AppState, CommunityReceiptRoute};
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
    paths::redact_home_text(&e.to_string())
}

pub(crate) fn cached_hardware() -> HardwareInfo {
    use std::sync::OnceLock;
    static HW: OnceLock<HardwareInfo> = OnceLock::new();
    HW.get_or_init(hardware::detect).clone()
}

#[tauri::command]
pub async fn detect_hardware() -> CmdResult<HardwareInfo> {
    tokio::task::spawn_blocking(cached_hardware)
        .await
        .map_err(map_err)
}

#[tauri::command]
pub async fn generate_identity(state: State<'_, AppState>) -> CmdResult<IdentityPublic> {
    require_settings_owner(&state).await?;
    let mut store = state.store.lock().await;
    let dir = state.data_dir.lock().await.clone();
    generate_identity_if_missing(&mut store, &dir)
}

fn generate_identity_if_missing(store: &mut Store, dir: &Path) -> CmdResult<IdentityPublic> {
    if store.identity.is_none() {
        store.identity = Some(identity::generate());
    }

    let public = IdentityPublic::from(store.identity.as_ref().unwrap());
    store.save_to(dir).map_err(map_err)?;
    Ok(public)
}

#[cfg(test)]
mod identity_generation_tests {
    use super::*;
    use crate::types::Identity;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn saved_identity() -> Identity {
        Identity {
            address: format!("0x{}", "11".repeat(32)),
            public_key: format!("0x{}", "22".repeat(32)),
            seed_phrase: "saved recovery phrase remains native".into(),
            created_at: 1_758_000_000,
        }
    }

    #[test]
    fn onboarding_reuses_saved_identity_even_without_config() {
        let dir = tempfile::tempdir().unwrap();
        let identity = saved_identity();
        let expected = IdentityPublic::from(&identity);
        let mut store = Store {
            identity: Some(identity),
            config: None,
            data_migration_notice: None,
            previous_install_notice: None,
        };
        store.save_to(dir.path()).unwrap();
        let original_bytes = std::fs::read(dir.path().join("store.json")).unwrap();

        let actual = generate_identity_if_missing(&mut store, dir.path()).unwrap();
        let repeated = generate_identity_if_missing(&mut store, dir.path()).unwrap();
        let saved_bytes = std::fs::read(dir.path().join("store.json")).unwrap();

        assert_eq!(actual.address, expected.address);
        assert_eq!(actual.public_key, expected.public_key);
        assert_eq!(actual.created_at, expected.created_at);
        assert_eq!(repeated.address, expected.address);
        assert_eq!(repeated.public_key, expected.public_key);
        assert!(store.config.is_none());
        assert_eq!(saved_bytes, original_bytes);
        let reopened = Store::load_from(dir.path());
        let reopened = IdentityPublic::from(reopened.identity.as_ref().unwrap());
        assert_eq!(reopened.address, expected.address);
        assert_eq!(reopened.public_key, expected.public_key);
    }

    #[tokio::test]
    async fn concurrent_first_requests_persist_and_return_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(Store::default()));
        let mut requests = Vec::new();
        for _ in 0..8 {
            let store = Arc::clone(&store);
            let dir = dir.path().to_path_buf();
            requests.push(tokio::spawn(async move {
                let mut store = store.lock().await;
                generate_identity_if_missing(&mut store, &dir)
            }));
        }

        let mut results = Vec::new();
        for request in requests {
            results.push(request.await.unwrap().unwrap());
        }
        assert!(results
            .iter()
            .all(|identity| identity.address == results[0].address));
        assert!(results
            .iter()
            .all(|identity| identity.public_key == results[0].public_key));

        let persisted = Store::load_from(dir.path());
        let persisted = IdentityPublic::from(persisted.identity.as_ref().unwrap());
        assert_eq!(persisted.address, results[0].address);
        assert_eq!(persisted.public_key, results[0].public_key);
    }

    #[test]
    fn failed_first_save_retains_the_same_identity_for_a_durable_retry() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"file blocks the app-data directory").unwrap();
        let mut store = Store::default();

        assert!(generate_identity_if_missing(&mut store, &blocker.join("child")).is_err());
        let pending = IdentityPublic::from(store.identity.as_ref().unwrap());

        let retry_dir = tempfile::tempdir().unwrap();
        let retry = generate_identity_if_missing(&mut store, retry_dir.path()).unwrap();
        let reopened = Store::load_from(retry_dir.path());
        let reopened = IdentityPublic::from(reopened.identity.as_ref().unwrap());

        assert_eq!(retry.address, pending.address);
        assert_eq!(retry.public_key, pending.public_key);
        assert_eq!(reopened.address, pending.address);
        assert_eq!(reopened.public_key, pending.public_key);
    }
}

#[tauri::command]
pub async fn import_identity(
    state: State<'_, AppState>,
    phrase: String,
) -> CmdResult<IdentityPublic> {
    require_settings_owner(&state).await?;
    state.cancel_startup_retry();
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
    config: NodeConfig,
) -> CmdResult<()> {
    save_webview_config(&app, &state, config, ConfigSave::Preferences).await
}

#[tauri::command]
pub async fn complete_onboarding(
    app: AppHandle,
    state: State<'_, AppState>,
    config: NodeConfig,
) -> CmdResult<()> {
    require_settings_owner(&state).await?;
    let mut config = config;
    if takes_setup_core_default(&config) {
        let logical_cores = tokio::task::spawn_blocking(|| cached_hardware().cpu_cores)
            .await
            .unwrap_or(0);
        apply_setup_core_default(&mut config, logical_cores);
    }
    save_webview_config(&app, &state, config, ConfigSave::Onboarding).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigSave {
    Onboarding,
    Preferences,
}

fn apply_webview_config_save(store: &mut Store, mut config: NodeConfig, save: ConfigSave) {
    preserve_authoritative_data_dir(&mut config, store.config.as_ref());
    match save {
        ConfigSave::Onboarding => {
            let explicitly_answered = config.compute_consent.is_some();
            preserve_unsent_contribution_choices(&mut config, store.config.as_ref());
            if explicitly_answered {
                config.compute_default_notice_pending = false;
            }
            if let Some(persisted) = store.config.as_ref() {
                config.auto_install_updates = persisted.auto_install_updates;
                if persisted.worker_threads.is_some() {
                    config.worker_threads = persisted.worker_threads;
                }
            }
        }
        ConfigSave::Preferences => {
            preserve_native_contribution_state(&mut config, store.config.as_ref())
        }
    }
    store.config = Some(config);
}

async fn save_webview_config(
    app: &AppHandle,
    state: &AppState,
    config: NodeConfig,
    save: ConfigSave,
) -> CmdResult<()> {
    require_settings_owner(state).await?;
    let auto_start;
    {
        let mut store = state.store.lock().await;
        apply_webview_config_save(&mut store, config, save);
        auto_start = store
            .config
            .as_ref()
            .is_some_and(|config| config.auto_start);
        let dir = state.data_dir.lock().await.clone();
        persist_webview_save(&store, &dir, save).map_err(map_err)?;
    }
    if !auto_start {
        state.cancel_startup_retry();
    }
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

fn persist_webview_config(store: &Store, dir: &Path) -> anyhow::Result<()> {
    store.save_to(dir)?;
    if let Err(error) = crate::auto_update::record_unattended_install_default(dir) {
        tracing::warn!(%error, "could not record the unattended-update default after a config save");
    }
    Ok(())
}

fn persist_webview_save(store: &Store, dir: &Path, save: ConfigSave) -> anyhow::Result<()> {
    persist_webview_config(store, dir)?;
    if save == ConfigSave::Onboarding {
        note_compute_question_answered(dir);
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

#[tauri::command]
pub async fn update_install_policy() -> CmdResult<UpdateInstallPolicy> {
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    Ok(update_install_policy_for(
        std::env::consts::OS,
        appimage.as_deref(),
    ))
}

#[tauri::command]
pub async fn update_location_block() -> CmdResult<Option<String>> {
    Ok(crate::update_location::refusal().map(str::to_owned))
}

#[tauri::command]
pub async fn load_config(state: State<'_, AppState>) -> CmdResult<Option<NodeConfig>> {
    Ok(load_config_inner(&state).await)
}

async fn load_config_inner(state: &AppState) -> Option<NodeConfig> {
    state.store.lock().await.config.clone()
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
    require_settings_owner(&state).await?;
    let mut store = state.store.lock().await;
    store.data_migration_notice = None;
    let dir = state.data_dir.lock().await.clone();
    store.save_to(&dir).map_err(map_err)
}

#[tauri::command]
pub async fn load_previous_install_notice(
    state: State<'_, AppState>,
) -> CmdResult<Option<PreviousInstallNotice>> {
    let store = state.store.lock().await;
    Ok(store.previous_install_notice.clone())
}

#[tauri::command]
pub async fn dismiss_previous_install_notice(state: State<'_, AppState>) -> CmdResult<()> {
    require_settings_owner(&state).await?;
    let mut store = state.store.lock().await;
    store.previous_install_notice = None;
    let dir = state.data_dir.lock().await.clone();
    store.save_to(&dir).map_err(map_err)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartupFailure {
    Transient(String),
    Terminal(String),
}

impl StartupFailure {
    pub(crate) fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }

    fn with_context(self, context: &str) -> Self {
        match self {
            Self::Transient(message) => Self::Transient(format!("{message}. {context}")),
            Self::Terminal(message) => Self::Terminal(format!("{message}. {context}")),
        }
    }
}

impl std::fmt::Display for StartupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Transient(message) | Self::Terminal(message) => message,
        })
    }
}

impl From<String> for StartupFailure {
    fn from(message: String) -> Self {
        Self::Terminal(message)
    }
}

impl From<&str> for StartupFailure {
    fn from(message: &str) -> Self {
        Self::Terminal(message.to_owned())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartTransaction {
    Start,
    StartupRecoveryOnly,
}

impl StartTransaction {
    fn fence_access(self) -> StartAccess {
        match self {
            Self::Start => StartAccess::Ordinary,
            Self::StartupRecoveryOnly => StartAccess::StartupRecovery,
        }
    }

    fn starts_current_node(self) -> bool {
        matches!(self, Self::Start)
    }
}

fn ensure_launch_not_superseded(
    cancel_watch: &tokio::sync::watch::Receiver<()>,
) -> Result<(), StartupFailure> {
    if cancel_watch.has_changed().unwrap_or(true) {
        return Err(StartupFailure::Terminal(
            "node launch superseded by a newer lifecycle action".into(),
        ));
    }
    Ok(())
}

async fn with_current_launch_config<T, F, Fut>(
    store: &tokio::sync::Mutex<crate::store::Store>,
    cancel_watch: &tokio::sync::watch::Receiver<()>,
    locked_data_dir: &str,
    expected_address: &str,
    launch: F,
) -> Result<T, StartupFailure>
where
    F: FnOnce(NodeConfig) -> Fut,
    Fut: std::future::Future<Output = Result<T, StartupFailure>>,
{
    let store_guard = store.lock().await;
    ensure_launch_not_superseded(cancel_watch)?;
    let current_address = store_guard
        .identity
        .as_ref()
        .map(|identity| identity.address.as_str());
    if current_address != Some(expected_address) {
        return Err(StartupFailure::Terminal(
            "saved validator identity changed while preparing to start arc-node".into(),
        ));
    }
    let config = store_guard.config.clone().unwrap_or_default();
    if config.data_dir != locked_data_dir {
        return Err(StartupFailure::Terminal(
            "managed node data directory changed while preparing to start arc-node".into(),
        ));
    }
    let result = launch(config).await;
    drop(store_guard);
    result
}

async fn start_node_transaction(
    app: &AppHandle,
    state: &AppState,
    transaction: StartTransaction,
    cancel_watch: tokio::sync::watch::Receiver<()>,
) -> Result<(), StartupFailure> {
    require_start_fence(state, transaction.fence_access()).await?;
    let start_after_recovery = transaction.starts_current_node();
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
        ensure_launch_not_superseded(&cancel_watch)?;
        node.refuse_start_during_update()?;
        if node.requires_completed_stop_before_start() {
            node.stop().await.map_err(map_err)?;
        } else if node.is_running() {
            return Ok(());
        }
        node.require_no_pending_crash_recovery().map_err(map_err)?;
    }
    let lock_data_dir = config.data_dir.clone();
    let lifecycle_lock = tokio::task::spawn_blocking(move || {
        crate::node_manager::acquire_managed_lifecycle_lock(&lock_data_dir)
    })
    .await
    .map_err(map_err)?
    .map_err(map_err)?;
    let recovery_launch = crate::node_manager::managed_shutdown_recovery_required(&config.data_dir)
        .map_err(map_err)?;
    if !recovery_launch {
        if !start_after_recovery {
            return Ok(());
        }
        ensure_binary_inner_classified(app).await?;
    }
    let app_data_dir = state.data_dir.lock().await.clone();
    let keyfile_result =
        identity::ensure_validator_keyfile(&app_data_dir, &recovery_phrase, &persisted_address);
    recovery_phrase.zeroize();
    let validator_keyfile = keyfile_result?;
    if !recovery_launch {
        let mut node = state.node.lock().await;
        let node_ref = &mut *node;
        let validator_keyfile_ref = &validator_keyfile;
        let resources_ref = &resources;
        return with_current_launch_config(
            &state.store,
            &cancel_watch,
            &config.data_dir,
            &persisted_address,
            |launch_config| async move {
                node_ref
                    .start(
                        &launch_config,
                        validator_keyfile_ref,
                        resources_ref,
                        lifecycle_lock,
                    )
                    .await
                    .map_err(map_err)?;
                Ok(())
            },
        )
        .await;
    }
    let mut node = state.node.lock().await;
    ensure_launch_not_superseded(&cancel_watch)?;
    node.start(&config, &validator_keyfile, &resources, lifecycle_lock)
        .await
        .map_err(map_err)?;

    node.stop()
        .await
        .map_err(|error| format!("managed-node durability recovery failed: {error}"))?;
    drop(node);

    if !start_after_recovery {
        return Ok(());
    }
    ensure_launch_not_superseded(&cancel_watch)?;

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
    ensure_binary_inner_classified(app).await?;
    let mut node = state.node.lock().await;
    let node_ref = &mut *node;
    let validator_keyfile_ref = &validator_keyfile;
    let resources_ref = &resources;
    with_current_launch_config(
        &state.store,
        &cancel_watch,
        &config.data_dir,
        &persisted_address,
        |launch_config| async move {
            node_ref
                .start(
                    &launch_config,
                    validator_keyfile_ref,
                    resources_ref,
                    lifecycle_lock,
                )
                .await
                .map_err(map_err)?;
            Ok(())
        },
    )
    .await
}

pub async fn start_node_inner(app: &AppHandle, state: &AppState) -> Result<(), String> {
    state.startup_retry_cancel.send_replace(());
    let cancel_watch = state.startup_retry_cancel.subscribe();
    require_start_fence(state, StartAccess::Ordinary).await?;
    state.node.lock().await.cancel_slow_crash_retry();
    start_node_transaction(app, state, StartTransaction::Start, cancel_watch)
        .await
        .map_err(|failure| paths::redact_home_text(&failure.to_string()))
}

pub(crate) async fn autostart_node_inner(
    app: &AppHandle,
    state: &AppState,
) -> Result<(), StartupFailure> {
    let cancel_watch = state.startup_retry_cancel.subscribe();
    {
        let store = state.store.lock().await;
        if !store
            .config
            .as_ref()
            .is_some_and(|config| config.auto_start)
            || store.identity.is_none()
        {
            return Err(StartupFailure::Terminal(
                "automatic node start is no longer enabled or has no saved identity".into(),
            ));
        }
    }
    start_node_transaction(app, state, StartTransaction::Start, cancel_watch).await
}

pub(crate) async fn recover_managed_shutdown_inner(
    app: &AppHandle,
    state: &AppState,
) -> Result<(), String> {
    let cancel_watch = state.startup_retry_cancel.subscribe();
    start_node_transaction(
        app,
        state,
        StartTransaction::StartupRecoveryOnly,
        cancel_watch,
    )
    .await
    .map_err(|failure| paths::redact_home_text(&failure.to_string()))
}

async fn require_settings_owner(state: &AppState) -> Result<(), String> {
    let fence = state.data_migration_error.lock().await;
    crate::start_fence::check_settings_write(fence.as_ref(), crate::desktop_instance::closing())
}

async fn require_start_fence(state: &AppState, access: StartAccess) -> Result<(), String> {
    let fence = state.data_migration_error.lock().await.clone();
    crate::start_fence::check(fence.as_ref(), access)
        .map_err(|error| paths::redact_home_text(&error))
}

async fn app_update_boundary(state: &AppState) -> Result<AppUpdateBoundary, String> {
    let fence = state.data_migration_error.lock().await.clone();
    crate::start_fence::app_update_boundary(fence.as_ref())
}

fn refuse_update_from_wrong_location() -> CmdResult<()> {
    match crate::update_location::refusal() {
        Some(refusal) => Err(refusal.to_owned()),
        None => Ok(()),
    }
}

async fn require_data_migration_ready(state: &AppState) -> Result<(), String> {
    require_start_fence(state, StartAccess::Ordinary).await
}

#[tauri::command]
pub async fn start_node(
    app: AppHandle,
    state: State<'_, AppState>,
    config: NodeConfig,
) -> CmdResult<()> {
    let _ = config;
    start_node_inner(&app, &state).await
}

#[tauri::command]
pub async fn stop_node(state: State<'_, AppState>) -> CmdResult<()> {
    if state.another_desktop_owns_node().await {
        return Err(crate::start_fence::ALREADY_OPEN.into());
    }
    state.cancel_startup_retry();
    let mut node = state.node.lock().await;
    node.stop().await.map_err(map_err)
}

#[tauri::command]
pub async fn prepare_update_relaunch(state: State<'_, AppState>) -> CmdResult<()> {
    refuse_update_from_wrong_location()?;
    let boundary = app_update_boundary(&state).await?;
    prepare_update_relaunch_inner(&state, boundary).await
}

pub(crate) async fn prepare_update_relaunch_inner(
    state: &AppState,
    boundary: AppUpdateBoundary,
) -> CmdResult<()> {
    if boundary == AppUpdateBoundary::Ordinary {
        state.cancel_startup_retry();
    }
    let mut node = state.node.lock().await;
    let fenced = boundary == AppUpdateBoundary::Fenced || node.ordinary_update_boundary_is_fenced();
    let result = if fenced {
        let prepared = node.prepare_fenced_update_relaunch();
        if prepared.is_ok() {
            state.cancel_startup_retry();
        }
        prepared
    } else {
        node.prepare_update_relaunch().await
    };
    result
        .map_err(|error| format!("could not establish the native update lifecycle fence: {error}"))
}

#[tauri::command]
pub async fn try_prepare_auto_update_relaunch(
    state: State<'_, AppState>,
) -> CmdResult<crate::auto_update::PrepareResult> {
    refuse_update_from_wrong_location()?;
    let boundary = app_update_boundary(&state).await?;
    try_prepare_auto_update_relaunch_inner(&state, boundary).await
}

pub(crate) async fn try_prepare_auto_update_relaunch_inner(
    state: &AppState,
    boundary: AppUpdateBoundary,
) -> CmdResult<crate::auto_update::PrepareResult> {
    use crate::auto_update::{PrepareResult, RequestFence};
    if !update_install_policy().await?.can_install {
        return Ok(PrepareResult::busy(
            "This installation is updated by its package manager",
            0,
        ));
    }
    let mut held = state.auto_update_requests.lock().await;
    if held.is_some() {
        return Ok(PrepareResult::busy(
            "An update preparation is already pending",
            0,
        ));
    }
    {
        let store = state.store.lock().await;
        if !store
            .config
            .as_ref()
            .is_some_and(|c| c.auto_update && c.auto_install_updates)
        {
            return Ok(PrepareResult::busy("Automatic installation is disabled", 0));
        }
    }
    let Some(fence) =
        RequestFence::try_acquire(&state.community_inference_write, &state.wallet_write)
    else {
        return Ok(PrepareResult::busy(
            "An inference or wallet operation is in progress",
            1,
        ));
    };
    *held = Some(fence);
    let Ok(mut node) = state.node.try_lock() else {
        *held = None;
        return Ok(PrepareResult::busy(
            "A node lifecycle operation is in progress",
            0,
        ));
    };
    if boundary == AppUpdateBoundary::Fenced || node.ordinary_update_boundary_is_fenced() {
        let prepared = node.prepare_fenced_update_relaunch();
        if prepared.is_ok() {
            state.cancel_startup_retry();
        }
        return prepared.map(|()| PrepareResult::Prepared).map_err(|error| {
            format!("automatic update preparation requires a safe abort: {error}")
        });
    }
    if node.pid().is_some() {
        state.cancel_startup_retry();
    }
    let result = node.prepare_auto_update_relaunch().await;
    if matches!(result, Ok(PrepareResult::Busy { .. })) {
        *held = None;
    }
    result.map_err(|error| format!("automatic update preparation requires a safe abort: {error}"))
}

#[tauri::command]
pub async fn begin_update_handoff(state: State<'_, AppState>) -> CmdResult<()> {
    let _requests = state.auto_update_requests.lock().await;
    let mut node = state.node.lock().await;
    node.begin_update_handoff()
        .map_err(|error| format!("could not commit the native updater handoff: {error}"))
}

#[tauri::command]
pub async fn abort_update_relaunch(state: State<'_, AppState>) -> CmdResult<()> {
    let mut requests = state.auto_update_requests.lock().await;
    let mut node = state.node.lock().await;
    node.abort_update_relaunch().await.map_err(|error| {
        format!("could not safely abort the update and restore the prior node state: {error}")
    })?;
    *requests = None;
    Ok(())
}

#[tauri::command]
pub async fn restart_node(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    require_data_migration_ready(&state).await?;
    restart_node_inner(&app, &state).await
}

pub(crate) async fn restart_node_inner(app: &AppHandle, state: &AppState) -> Result<(), String> {
    state.cancel_startup_retry();
    let cancel_watch = state.startup_retry_cancel.subscribe();
    restart_node_transaction(app, state, cancel_watch, None).await
}

async fn restart_node_transaction(
    app: &AppHandle,
    state: &AppState,
    cancel_watch: tokio::sync::watch::Receiver<()>,
    required_running_pid: Option<u32>,
) -> Result<(), String> {
    require_data_migration_ready(state).await?;
    state.node.lock().await.refuse_start_during_update()?;
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

    {
        let mut node = state.node.lock().await;
        ensure_launch_not_superseded(&cancel_watch).map_err(|failure| failure.to_string())?;
        if required_running_pid.is_some_and(|pid| {
            node.pid() != Some(pid) || node.requires_completed_stop_before_start()
        }) {
            return Ok(());
        }
        node.stop().await.map_err(map_err)?;
    }
    ensure_launch_not_superseded(&cancel_watch).map_err(|failure| failure.to_string())?;

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

    ensure_binary_inner(app).await?;

    let resources = resolve_testnet_resources(app);
    let mut node = state.node.lock().await;
    let node_ref = &mut *node;
    let validator_keyfile_ref = &validator_keyfile;
    let resources_ref = &resources;
    with_current_launch_config(
        &state.store,
        &cancel_watch,
        &cfg.data_dir,
        &persisted_address,
        |launch_cfg| async move {
            node_ref
                .start(
                    &launch_cfg,
                    validator_keyfile_ref,
                    resources_ref,
                    lifecycle_lock,
                )
                .await
                .map_err(map_err)?;
            Ok(())
        },
    )
    .await
    .map_err(|failure| failure.to_string())
}

pub(crate) const COMPUTE_CONSENT_REQUIRED: &str =
    "Compute contribution is off. Turn it on in Settings to download the model and take ARC jobs on this computer.";

pub(crate) const COMPUTE_INELIGIBLE: &str =
    "This computer has less than 16 GB of memory, so it cannot run the ARC model. It stays an observer, which still relays and verifies.";

pub(crate) fn compute_contribution_enabled(config: &NodeConfig) -> bool {
    config
        .compute_consent
        .unwrap_or(config.role == "worker" && config.model_path.is_some())
}

pub(crate) fn apply_compute_default(
    store: &mut Store,
    ram_gb: u64,
    cores: u32,
    legacy_pending: bool,
) -> bool {
    if store.identity.is_none() || legacy_pending {
        return false;
    }
    let Some(config) = store.config.as_mut() else {
        return false;
    };
    if tier_for_ram_gb(ram_gb) == "none" {
        let changed = config.role != "observer";
        config.role = "observer".into();
        return changed;
    }
    if config.compute_consent.is_some() {
        return false;
    }
    let already_sharing = config.role == "worker" && config.model_path.is_some();
    if already_sharing {
        config.compute_consent = Some(true);
    } else {
        turn_on_compute(config, cores);
    }
    config.compute_default_notice_pending = !already_sharing;
    true
}

pub(crate) fn default_worker_threads(logical_cores: u32) -> u32 {
    (logical_cores / 4).max(1)
}

fn takes_setup_core_default(config: &NodeConfig) -> bool {
    config.compute_consent == Some(true) && config.worker_threads.is_none()
}

fn apply_setup_core_default(config: &mut NodeConfig, logical_cores: u32) {
    if takes_setup_core_default(config) {
        config.worker_threads = Some(default_worker_threads(logical_cores));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromotionNeed {
    NoConsent,
    Ineligible,
    Ready,
    Promote,
}

pub(crate) fn promotion_need(config: &NodeConfig, ram_gb: u64) -> PromotionNeed {
    if !compute_contribution_enabled(config) {
        PromotionNeed::NoConsent
    } else if tier_for_ram_gb(ram_gb) == "none" {
        PromotionNeed::Ineligible
    } else if config.role == "worker" && config.model_path.is_some() {
        PromotionNeed::Ready
    } else {
        PromotionNeed::Promote
    }
}

fn preserve_unsent_contribution_choices(config: &mut NodeConfig, persisted: Option<&NodeConfig>) {
    if let Some(persisted) = persisted {
        if config.compute_consent.is_none() {
            config.compute_consent = persisted.compute_consent;
        }
        if config.keep_awake_while_sharing.is_none() {
            config.keep_awake_while_sharing = persisted.keep_awake_while_sharing;
        }
        config.compute_default_notice_pending = persisted.compute_default_notice_pending;
        if config.prevent_sleep_during_jobs.is_none() {
            config.prevent_sleep_during_jobs = persisted.prevent_sleep_during_jobs;
        }
    }
}

fn preserve_native_contribution_state(config: &mut NodeConfig, persisted: Option<&NodeConfig>) {
    let Some(persisted) = persisted else {
        return;
    };
    config.compute_consent = persisted.compute_consent;
    config.keep_awake_while_sharing = persisted.keep_awake_while_sharing;
    config.compute_default_notice_pending = persisted.compute_default_notice_pending;
    config.prevent_sleep_during_jobs = persisted.prevent_sleep_during_jobs;
    config.role.clone_from(&persisted.role);
    config.model_path.clone_from(&persisted.model_path);
    config.worker_threads = persisted.worker_threads;
}

async fn current_config(state: &AppState) -> NodeConfig {
    state.store.lock().await.config.clone().unwrap_or_default()
}

async fn update_stored_config(
    state: &AppState,
    update: impl FnOnce(&mut NodeConfig) + Send,
) -> Result<NodeConfig, String> {
    require_settings_owner(state).await?;
    let mut store = state.store.lock().await;
    let config = update_stored_config_in(&mut store, update);
    let dir = state.data_dir.lock().await.clone();
    store.save_to(&dir).map_err(map_err)?;
    crate::sharing_awake::set_enabled(
        config.keep_awake_while_sharing == Some(true) && compute_contribution_enabled(&config),
    );
    Ok(config)
}

fn update_stored_config_in(store: &mut Store, update: impl FnOnce(&mut NodeConfig)) -> NodeConfig {
    let mut config = store.config.clone().unwrap_or_default();
    update(&mut config);
    store.config = Some(config.clone());
    config
}

async fn persist_promotion_if_still_consented(
    state: &AppState,
    model_path: String,
) -> Result<Option<NodeConfig>, String> {
    require_settings_owner(state).await?;
    let mut store = state.store.lock().await;
    let latest = store.config.clone().unwrap_or_default();
    if !compute_contribution_enabled(&latest) {
        return Ok(None);
    }
    let mut promoted = NodeConfig {
        role: "worker".into(),
        model_path: Some(model_path),
        compute_consent: Some(true),
        ..latest
    };
    preserve_authoritative_data_dir(&mut promoted, store.config.as_ref());
    store.config = Some(promoted.clone());
    let dir = state.data_dir.lock().await.clone();
    store.save_to(&dir).map_err(map_err)?;
    Ok(Some(promoted))
}

fn withdraw_compute_consent(config: &mut NodeConfig) {
    config.compute_consent = Some(false);
    config.compute_default_notice_pending = false;
    config.role = "observer".into();
    config.model_path = None;
}

#[tauri::command]
pub async fn set_compute_contribution(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> CmdResult<NodeConfig> {
    require_settings_owner(&state).await?;
    if !enabled {
        state.cancel_startup_retry();
        let cancel_watch = state.startup_retry_cancel.subscribe();
        crate::sharing_awake::set_enabled(false);
        let config = update_stored_config(&state, withdraw_compute_consent).await?;
        reconcile_running_compute(&app, &state, cancel_watch, false).await?;
        return Ok(config);
    }
    update_stored_config(&state, record_explicit_compute_consent).await?;
    promote_consented_install_inner(&app, &state).await
}

#[tauri::command]
pub async fn promote_consented_install(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<NodeConfig> {
    promote_consented_install_inner(&app, &state).await
}

static PROMOTION_FLIGHT: crate::background_promotion::SingleFlight<Result<NodeConfig, String>> =
    crate::background_promotion::SingleFlight::new();

const PROMOTION_INTERRUPTED: &str =
    "Enabling compute contribution was interrupted. Try again; anything already downloaded is kept.";

pub(crate) async fn promote_consented_install_inner(
    app: &AppHandle,
    state: &AppState,
) -> Result<NodeConfig, String> {
    let result = PROMOTION_FLIGHT
        .run_or_join(
            || promote_consented_install_once(app, state),
            || Err(PROMOTION_INTERRUPTED.to_string()),
        )
        .await;
    let outcome = match &result {
        Ok(_) => crate::background_promotion::TickOutcome::Promoted,
        Err(error) => crate::background_promotion::TickOutcome::Failed(error.clone()),
    };
    crate::background_promotion::record_outcome(&outcome);
    result
}

async fn promote_consented_install_once(
    app: &AppHandle,
    state: &AppState,
) -> Result<NodeConfig, String> {
    let cancel_watch = state.startup_retry_cancel.subscribe();
    let config = current_config(state).await;
    let ram_gb = tokio::task::spawn_blocking(|| cached_hardware().ram_gb)
        .await
        .map_err(map_err)?;
    let tier = tier_for_ram_gb(ram_gb);
    match promotion_need(&config, ram_gb) {
        PromotionNeed::NoConsent => return Err(COMPUTE_CONSENT_REQUIRED.to_string()),
        PromotionNeed::Ineligible => return Err(COMPUTE_INELIGIBLE.to_string()),
        PromotionNeed::Ready
            if crate::node_manager::configured_model_unavailable(&config).is_none() =>
        {
            reconcile_running_compute(app, state, cancel_watch, true).await?;
            return Ok(current_config(state).await);
        }
        PromotionNeed::Ready if tier == "none" => return Err(COMPUTE_INELIGIBLE.to_string()),
        PromotionNeed::Ready | PromotionNeed::Promote => {}
    }
    let model_path = match existing_model_for_tier(tier.to_string()).await? {
        Some(path) => path,
        None => download_model(app.clone(), tier.to_string()).await?,
    };
    let Some(promoted) = persist_promotion_if_still_consented(state, model_path).await? else {
        return Ok(current_config(state).await);
    };
    reconcile_running_compute(app, state, cancel_watch, false).await?;
    Ok(promoted)
}

async fn reconcile_running_compute(
    app: &AppHandle,
    state: &AppState,
    cancel_watch: tokio::sync::watch::Receiver<()>,
    observer_only: bool,
) -> Result<(), String> {
    let pid = {
        let node = state.node.lock().await;
        ensure_launch_not_superseded(&cancel_watch).map_err(|failure| failure.to_string())?;
        if node.requires_completed_stop_before_start()
            || (observer_only && !node.running_as_observer())
        {
            return Ok(());
        }
        node.pid()
    };
    if let Some(pid) = pid {
        restart_node_transaction(app, state, cancel_watch, Some(pid)).await?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComputeReconcileAction {
    None,
    RestartRunningObserver,
    PromoteModel,
}

pub(crate) fn compute_reconcile_action(
    config: &NodeConfig,
    ram_gb: u64,
    running_as_observer: bool,
) -> ComputeReconcileAction {
    match promotion_need(config, ram_gb) {
        PromotionNeed::NoConsent | PromotionNeed::Ineligible => ComputeReconcileAction::None,
        PromotionNeed::Promote => ComputeReconcileAction::PromoteModel,
        PromotionNeed::Ready => {
            if crate::node_manager::configured_model_unavailable(config).is_some() {
                ComputeReconcileAction::PromoteModel
            } else if running_as_observer {
                ComputeReconcileAction::RestartRunningObserver
            } else {
                ComputeReconcileAction::None
            }
        }
    }
}

fn needs_promotion(config: &NodeConfig, ram_gb: u64) -> bool {
    matches!(
        compute_reconcile_action(config, ram_gb, false),
        ComputeReconcileAction::PromoteModel
    )
}

fn startup_promotion_skip_reason(
    config: &NodeConfig,
    conditions: &impl crate::background_promotion::DownloadConditions,
    disk_space_needed: Option<u64>,
    cached_model_ready: bool,
) -> Option<crate::background_promotion::SkipReason> {
    if cached_model_ready {
        return None;
    }
    if let Err(reason) = startup_download_gate(config, conditions) {
        return Some(reason);
    }
    disk_space_needed
        .map(|needed_bytes| crate::background_promotion::SkipReason::DiskFull { needed_bytes })
}

async fn cached_model_verified_for_tier(tier: &str) -> bool {
    existing_model_for_tier(tier.to_string())
        .await
        .ok()
        .flatten()
        .is_some()
}

pub(crate) async fn promote_consented_install_on_startup(app: &AppHandle, state: &AppState) {
    let config = current_config(state).await;
    let ram_gb = tokio::task::spawn_blocking(|| cached_hardware().ram_gb)
        .await
        .unwrap_or(0);
    let running_as_observer = state.node.lock().await.running_as_observer();
    match compute_reconcile_action(&config, ram_gb, running_as_observer) {
        ComputeReconcileAction::None => return,
        ComputeReconcileAction::RestartRunningObserver => {}
        ComputeReconcileAction::PromoteModel => {
            let tier = tier_for_ram_gb(ram_gb);
            let disk_space_needed = background_download_space_needed(tier).await;
            if let Some(reason) = startup_promotion_skip_reason(
                &config,
                &crate::background_promotion::SystemConditions,
                disk_space_needed,
                false,
            ) {
                if !cached_model_verified_for_tier(tier).await {
                    crate::background_promotion::record_outcome(
                        &crate::background_promotion::TickOutcome::Skipped(reason),
                    );
                    tracing::info!(?reason, "default-enabled startup promotion deferred");
                    return;
                }
            }
        }
    }
    match promote_consented_install_inner(app, state).await {
        Ok(_) => tracing::info!("compute contribution enabled: worker mode with a verified model"),
        Err(error) => {
            tracing::warn!(%error, "could not finish enabling compute contribution; will retry")
        }
    }
}

fn startup_download_gate(
    config: &NodeConfig,
    conditions: &impl crate::background_promotion::DownloadConditions,
) -> Result<(), crate::background_promotion::SkipReason> {
    use crate::background_promotion::{download_gate, Trigger};
    let trigger = if config.compute_default_notice_pending {
        Trigger::Background
    } else {
        Trigger::User
    };
    download_gate(trigger, conditions)
}

const COMPUTE_PROMPT_MARKER: &str = "compute-prompt-shown-at.v1";

fn record_compute_prompt(dir: &Path) -> std::io::Result<bool> {
    let now = chrono::Utc::now().timestamp_millis().max(0);
    arc_crypto::secret_file::durably_publish_new_private(
        &dir.join(COMPUTE_PROMPT_MARKER),
        format!("{now}\n").as_bytes(),
    )
}

pub(crate) fn note_compute_question_answered(dir: &Path) {
    if let Err(error) = record_compute_prompt(dir) {
        tracing::warn!(%error, "could not record that the compute question was already asked");
    }
}

const COMPUTE_NOTICE_MARKER: &str = "compute-default-notice-shown-at.v2";

fn compute_notice_due(store: &Store, dir: &Path, ram_gb: u64) -> bool {
    tier_for_ram_gb(ram_gb) != "none"
        && store.identity.is_some()
        && store
            .config
            .as_ref()
            .is_some_and(|c| c.compute_default_notice_pending && c.compute_consent == Some(true))
        && !dir.join(COMPUTE_NOTICE_MARKER).exists()
}

#[tauri::command]
pub async fn claim_compute_prompt(state: State<'_, AppState>) -> CmdResult<bool> {
    let store = state.store.lock().await;
    let dir = state.data_dir.lock().await;
    Ok(compute_notice_due(
        &store,
        &dir,
        crate::hardware::compute_capacity().0,
    ))
}

#[tauri::command]
pub async fn acknowledge_compute_notice(state: State<'_, AppState>) -> CmdResult<()> {
    let store = state.store.lock().await;
    let dir = state.data_dir.lock().await;
    if compute_notice_due(&store, &dir, crate::hardware::compute_capacity().0) {
        arc_crypto::secret_file::durably_publish_new_private(
            &dir.join(COMPUTE_NOTICE_MARKER),
            b"shown\n",
        )
        .map_err(map_err)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn set_keep_awake_while_sharing(
    state: State<'_, AppState>,
    enabled: bool,
) -> CmdResult<NodeConfig> {
    require_settings_owner(&state).await?;
    if !enabled {
        crate::sharing_awake::set_enabled(false);
    }
    let config =
        update_stored_config(&state, |c| c.keep_awake_while_sharing = Some(enabled)).await?;
    Ok(config)
}

fn record_explicit_compute_consent(config: &mut NodeConfig) {
    config.compute_consent = Some(true);
    config.compute_default_notice_pending = false;
}

fn turn_on_compute(config: &mut NodeConfig, logical_cores: u32) {
    record_explicit_compute_consent(config);
    apply_setup_core_default(config, logical_cores);
}

#[tauri::command]
pub async fn accept_compute_prompt(state: State<'_, AppState>) -> CmdResult<NodeConfig> {
    let logical_cores = tokio::task::spawn_blocking(|| cached_hardware().cpu_cores)
        .await
        .unwrap_or(0);
    update_stored_config(&state, |config| turn_on_compute(config, logical_cores)).await
}

#[tauri::command]
pub async fn set_prevent_sleep_during_jobs(
    state: State<'_, AppState>,
    enabled: bool,
) -> CmdResult<NodeConfig> {
    require_settings_owner(&state).await?;
    update_stored_config(&state, |config| {
        config.prevent_sleep_during_jobs = Some(enabled)
    })
    .await
}

#[tauri::command]
pub async fn fetch_worker_network(
    state: State<'_, AppState>,
) -> CmdResult<Option<crate::worker_network::WorkerNetwork>> {
    let host = chain_host(&state).await;
    Ok(crate::worker_network::fetch(&state.http, &host).await)
}

#[tauri::command]
pub async fn fetch_worker_status(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::WorkerStatus> {
    let port = state.node.lock().await.rpc_port;
    let local = paths::local_host(port);
    Ok(rpc_client::fetch_worker_status(&state.http, &local).await)
}

#[tauri::command]
pub async fn reset_peer_state(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ResetPeerStateResult> {
    state.cancel_startup_retry();
    require_data_migration_ready(&state).await?;
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

#[tauri::command]
pub async fn node_status(state: State<'_, AppState>) -> CmdResult<NodeStatus> {
    node_status_inner(&state, crate::desktop_instance::closing()).await
}

async fn node_status_inner(state: &AppState, closing: bool) -> CmdResult<NodeStatus> {
    if closing {
        return Ok(NodeStatus {
            last_error: Some("ARC is closing".into()),
            ..NodeStatus::default()
        });
    }
    let fence = state.data_migration_error.lock().await.clone();
    if matches!(fence, Some(crate::start_fence::StartFence::OtherDesktop)) {
        return Ok(NodeStatus {
            last_error: crate::start_fence::check(fence.as_ref(), StartAccess::Ordinary).err(),
            ..NodeStatus::default()
        });
    }
    let (port, pid, crash, worker_threads, model_unavailable, crash_retry_in_secs) = {
        let mut node = state.node.lock().await;
        let pid = if node.is_running() { node.pid() } else { None };
        let port = node.rpc_port;
        let worker_threads = node.active_worker_threads;
        let model_unavailable = node.active_model_unavailable;
        let crash = node
            .crash_info
            .lock()
            .await
            .as_ref()
            .map(|c| c.message.clone());
        (
            port,
            pid,
            crash,
            worker_threads,
            model_unavailable,
            node.crash_retry_in_secs(),
        )
    };
    let address = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    };

    let local = paths::local_host(port);
    let chain = chain_host(state).await;
    let chain_choice = cached_chain_choice(state).await;

    let mut status =
        rpc_client::fetch_status(&state.http, &local, &chain, port, pid, address, crash).await;

    status.chain_host = Some(chain);
    status.chain_height = chain_choice.as_ref().map(|c| c.height);
    status.chain_block_age_seconds = chain_choice.as_ref().map(|c| c.block_age_seconds());
    status.worker_threads = worker_threads;
    status.cpu_cores = Some(cached_hardware().cpu_cores);
    status.model_unavailable = model_unavailable;
    status.crash_retry_in_secs = crash_retry_in_secs;
    let config = current_config(state).await;
    if needs_promotion(&config, cached_hardware().ram_gb) {
        status.model_download_wait = crate::background_promotion::waiting_status();
    }
    Ok(status)
}

#[tauri::command]
pub async fn node_start_block(state: State<'_, AppState>) -> CmdResult<Option<String>> {
    Ok(start_block(&state).await)
}

async fn start_block(state: &AppState) -> Option<String> {
    if crate::desktop_instance::closing() {
        return Some("ARC is closing".into());
    }
    require_data_migration_ready(state).await.err()
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
    Ok(fetch_earnings_inner(&state, address.as_deref()).await)
}

pub(crate) async fn fetch_earnings_inner(state: &AppState, address: Option<&str>) -> Earnings {
    let host = chain_host(state).await;
    rpc_client::fetch_earnings(&state.http, &host, address).await
}

#[tauri::command]
pub async fn fetch_attestations(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<Vec<Attestation>> {
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

#[tauri::command]
pub async fn fetch_reward_economics(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::RewardEconomics> {
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_reward_economics(&state.http, &host).await)
}

#[tauri::command]
pub async fn fetch_earnings_projection(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::EarningsProjection> {
    let address = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    };
    Ok(fetch_earnings_projection_inner(&state, address.as_deref()).await)
}

pub(crate) async fn fetch_earnings_projection_inner(
    state: &AppState,
    address: Option<&str>,
) -> crate::types::EarningsProjection {
    let host = chain_host(state).await;
    rpc_client::fetch_earnings_projection(&state.http, &host, address).await
}

#[tauri::command]
pub async fn fetch_node_contribution(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::NodeContribution> {
    let port = state.node.lock().await.rpc_port;
    let local = paths::local_host(port);
    let cores = Some(cached_hardware().cpu_cores);
    Ok(rpc_client::fetch_node_contribution(&state.http, &local, cores).await)
}

#[tauri::command]
pub async fn fetch_network_overview(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::NetworkOverview> {
    Ok(fetch_network_overview_inner(&state).await)
}

pub(crate) async fn fetch_network_overview_inner(
    state: &AppState,
) -> crate::types::NetworkOverview {
    let host = chain_host(state).await;
    fetch_network_overview_from_host_inner(state, &host).await
}

pub(crate) async fn fetch_network_overview_from_host_inner(
    state: &AppState,
    host: &str,
) -> crate::types::NetworkOverview {
    rpc_client::fetch_network_overview(&state.http, host).await
}

pub(crate) async fn fetch_production_model_identity_from_host_inner(
    state: &AppState,
    host: &str,
    expected_model_id: &str,
) -> Result<String, String> {
    rpc_client::fetch_production_model_identity(&state.http, host, expected_model_id).await
}

#[tauri::command]
pub async fn fetch_recent_blocks(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<crate::types::RecentBlocks> {
    Ok(fetch_recent_blocks_inner(&state, limit.unwrap_or(10)).await)
}

pub(crate) async fn fetch_recent_blocks_inner(
    state: &AppState,
    limit: u32,
) -> crate::types::RecentBlocks {
    let host = chain_host(state).await;
    rpc_client::fetch_recent_blocks(&state.http, &host, limit).await
}

#[tauri::command]
pub async fn fetch_block_txs(
    state: State<'_, AppState>,
    height: u64,
    limit: Option<u32>,
) -> CmdResult<crate::types::BlockTxs> {
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_block_txs(&state.http, &host, height, limit.unwrap_or(50)).await)
}

#[tauri::command]
pub async fn lookup_tx(
    state: State<'_, AppState>,
    hash: String,
) -> CmdResult<crate::types::TxLookup> {
    Ok(lookup_tx_inner(&state, &hash).await)
}

pub(crate) async fn lookup_tx_inner(state: &AppState, hash: &str) -> crate::types::TxLookup {
    let host = chain_host(state).await;
    rpc_client::lookup_tx(&state.http, &host, hash).await
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

#[tauri::command]
pub async fn send_arc(
    state: State<'_, AppState>,
    to: String,
    amount_arc: String,
) -> CmdResult<WalletTxResult> {
    send_arc_inner(&state, to, amount_arc).await
}

pub(crate) async fn send_arc_inner(
    state: &AppState,
    to: String,
    amount_arc: String,
) -> CmdResult<WalletTxResult> {
    let amount_base = crate::wallet::parse_arc_amount(&amount_arc)?;
    if amount_base == 0 {
        return Err("amount must be greater than zero".to_string());
    }

    let _write_guard = state.wallet_write.lock().await;
    let address = {
        let store = state.store.lock().await;
        store
            .identity
            .as_ref()
            .map(|identity| identity.address.clone())
    }
    .ok_or_else(|| "no identity".to_string())?;

    let host = crate::wallet::validate_rpc_origin(&chain_host(state).await)?;
    if crate::native_paid::host_carries_only_native_transactions(state, &host).await {
        return Err(
            "this chain carries only native paid-inference transactions, so a transfer can \
             never be included; nothing was signed"
                .to_string(),
        );
    }
    let account = rpc_client::fetch_balance(&state.http, &host, &address).await?;
    let available = account
        .balance_base
        .parse::<u64>()
        .map_err(|_| "selected host returned an invalid wallet balance".to_string())?;
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
    pub fn block_age_seconds(&self) -> u64 {
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        now.saturating_sub(self.block_timestamp_ms) / 1000
    }
}

fn explicit_chain_origin_with(mut lookup: impl FnMut(&str) -> Option<String>) -> Option<String> {
    ["ARC_WALLET_HOST", "ARC_TIER1_RPC"]
        .into_iter()
        .find_map(|key| {
            lookup(key)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
}

fn explicit_chain_origin() -> Option<String> {
    explicit_chain_origin_with(|key| std::env::var(key).ok())
}
pub(crate) async fn chain_host(state: &AppState) -> String {
    if let Some(origin) = explicit_chain_origin() {
        return origin;
    }

    if let Some(host) = state.community_chain_host.lock().await.clone() {
        return host;
    }

    let pinned = {
        let cached = state.chain_host.lock().await;
        cached.as_ref().map(|(c, _)| c.host.clone())
    };
    if let Some(host) = pinned {
        return host;
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
            tracing::warn!(
                "no seed answered /block/latest; falling back to {}",
                WALLET_HOSTS[0]
            );
            WALLET_HOSTS[0].to_string()
        }
    }
}

async fn cached_chain_choice(state: &AppState) -> Option<ChainHostChoice> {
    state
        .chain_host
        .lock()
        .await
        .as_ref()
        .map(|(c, _)| c.clone())
}

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

fn inference_timeout(prompt: &str, max_tokens: u32) -> std::time::Duration {
    const MIN_SERVER_SECS: u64 = 45;
    const MAX_SERVER_SECS: u64 = 3_900;
    const CLAIM_WINDOW_SECS: u64 = 30;
    const CLIENT_HEADROOM_SECS: u64 = 60;
    const INTERNAL_BOS_POSITIONS: u64 = 1;
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

const COORDINATOR_HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InferenceProxyPolicy {
    Configured,
    AcceptanceDirect,
}

fn inference_client(
    prompt: &str,
    max_tokens: u32,
    proxy_policy: InferenceProxyPolicy,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(inference_timeout(prompt, max_tokens))
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none());
    if proxy_policy == InferenceProxyPolicy::AcceptanceDirect {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(map_err)
}

async fn health_ok(http: &reqwest::Client, host: &str, timeout: std::time::Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, http.get(format!("{}/health", host)).send()).await,
        Ok(Ok(r)) if r.status().is_success()
    )
}

pub(crate) const INFERENCE_PRE_DISPATCH_UNAVAILABLE: &str =
    "ARC_INFERENCE_PRE_DISPATCH_UNAVAILABLE";
pub(crate) const INFERENCE_POST_OUTCOME_AMBIGUOUS: &str =
    "ARC_INFERENCE_POST_OUTCOME_AMBIGUOUS";

fn inference_pre_dispatch_unavailable(detail: impl std::fmt::Display) -> String {
    format!("{INFERENCE_PRE_DISPATCH_UNAVAILABLE}: {detail}")
}

fn try_local_rpc_port(state: &AppState) -> Option<u16> {
    state.node.try_lock().ok().map(|node| node.rpc_port)
}

async fn local_rpc_port(state: &AppState) -> u16 {
    if let Some(port) = try_local_rpc_port(state) {
        return port;
    }
    state
        .store
        .lock()
        .await
        .config
        .as_ref()
        .map_or(NodeConfig::default().rpc_port, |config| config.rpc_port)
}

fn inference_post_outcome_ambiguous(detail: impl std::fmt::Display) -> String {
    format!(
        "{INFERENCE_POST_OUTCOME_AMBIGUOUS}: an inference POST was attempted and may have created an assignment or reward; do not retry another route ({detail})"
    )
}

async fn inference_readiness_ok(http: &reqwest::Client, host: &str) -> Result<bool, String> {
    tokio::time::timeout(
        COORDINATOR_HEALTH_TIMEOUT,
        rpc_client::inference_readiness(http, host),
    )
    .await
    .map_err(|_| format!("{host} readiness timed out"))?
}

async fn coordinator_candidates(state: &AppState) -> Vec<String> {
    let mut ordered = Vec::new();

    if let Some(origin) = established_chain_origin(state).await {
        if health_ok(&state.http, &origin, COORDINATOR_HEALTH_TIMEOUT).await {
            ordered.push(origin);
        }
        return ordered;
    }

    if let Some(port) = try_local_rpc_port(state) {
        let local = paths::local_host(port);
        if health_ok(&state.http, &local, COORDINATOR_HEALTH_TIMEOUT).await {
            ordered.push(local);
        }
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

async fn established_chain_origin(state: &AppState) -> Option<String> {
    if let Some(origin) = state.community_chain_host.lock().await.clone() {
        return Some(origin);
    }
    state
        .chain_host
        .lock()
        .await
        .as_ref()
        .map(|(choice, _)| choice.host.clone())
}

fn community_receipt_source_is_pinned(source_host: &str, local_host: &str) -> bool {
    source_host == local_host || COORDINATOR_HOSTS.contains(&source_host)
}

pub(crate) const RECEIPT_SOURCE_REJECTED: &str = "reward receipt unavailable: source host is not the exact local node or a compiled-in ARC coordinator";
pub(crate) const RECEIPT_ROUTE_MISSING: &str =
    "reward receipt unavailable: no native inference route is pinned for this transaction";
pub(crate) const RECEIPT_ROUTE_MISMATCH: &str =
    "reward receipt unavailable: requested identity differs from the native inference route pin";

async fn pin_community_receipt_route(
    state: &AppState,
    source_host: &str,
    result: &InferenceResult,
) {
    let Some(settlement) = result.settlement.as_ref() else {
        return;
    };
    let local_host = paths::local_host(local_rpc_port(state).await);
    if !settlement.submitted
        || settlement.tx_type != "0x25"
        || !matches!(
            settlement.status.as_str(),
            "pending_mined_receipt" | "mined_success"
        )
        || !community_receipt_source_is_pinned(source_host, &local_host)
        || result.routed_via != format!("community:{}", settlement.worker)
        || rpc_client::validate_community_receipt_expectation(
            &settlement.tx_hash,
            &settlement.job_id,
            &settlement.worker,
            &settlement.receipt_url,
        )
        .is_err()
    {
        tracing::warn!(
            source_host,
            "refusing to pin a malformed or unsubmitted community settlement"
        );
        return;
    }
    if let Some(existing) = state
        .chain_host
        .lock()
        .await
        .as_ref()
        .map(|(choice, _)| choice.host.clone())
    {
        if existing != source_host {
            tracing::error!(
                existing,
                rejected = source_host,
                "refusing to move the already displayed structural chain for a settlement"
            );
            return;
        }
    }
    {
        let mut session_origin = state.community_chain_host.lock().await;
        if let Some(existing) = session_origin.as_deref() {
            if existing != source_host {
                tracing::error!(
                    existing,
                    rejected = source_host,
                    "refusing to move an immutable community receipt-origin session pin"
                );
                return;
            }
        } else {
            *session_origin = Some(source_host.to_string());
        }
    }
    let mut routes = state.community_receipt_routes.lock().await;
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

#[tauri::command]
pub async fn fetch_community_reward_receipt(
    state: State<'_, AppState>,
    source_host: String,
    tx_hash: String,
    job_id: String,
    worker: String,
    receipt_url: String,
) -> CmdResult<InferenceSettlement> {
    fetch_community_reward_receipt_inner(
        &state,
        &source_host,
        &tx_hash,
        &job_id,
        &worker,
        &receipt_url,
    )
    .await
}

pub(crate) async fn fetch_community_reward_receipt_inner(
    state: &AppState,
    source_host: &str,
    tx_hash: &str,
    job_id: &str,
    worker: &str,
    receipt_url: &str,
) -> CmdResult<InferenceSettlement> {
    let local_host = paths::local_host(local_rpc_port(state).await);
    if !community_receipt_source_is_pinned(source_host, &local_host) {
        return Err(RECEIPT_SOURCE_REJECTED.to_string());
    }
    let pinned = state
        .community_receipt_routes
        .lock()
        .await
        .get(tx_hash)
        .cloned()
        .ok_or_else(|| RECEIPT_ROUTE_MISSING.to_string())?;
    if pinned.source_host != source_host
        || pinned.job_id != job_id
        || pinned.worker != worker
        || pinned.receipt_url != receipt_url
    {
        return Err(RECEIPT_ROUTE_MISMATCH.to_string());
    }
    rpc_client::fetch_community_reward_receipt(
        &state.http,
        source_host,
        tx_hash,
        job_id,
        worker,
        receipt_url,
    )
    .await
}

#[tauri::command]
pub async fn run_inference(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    chat_template: Option<bool>,
) -> CmdResult<InferenceResult> {
    run_inference_local_inner(
        &state,
        &prompt,
        max_tokens.unwrap_or(32),
        chat_template.unwrap_or(true),
    )
    .await
}

async fn run_inference_local_inner(
    state: &AppState,
    prompt: &str,
    max_tokens: u32,
    chat_template: bool,
) -> CmdResult<InferenceResult> {
    let _write_guard = state.community_inference_write.lock().await;
    let port = match state.node.try_lock() {
        Ok(node) => node.rpc_port,
        Err(_) => return Err(inference_pre_dispatch_unavailable(
            "the local node is busy starting, stopping or recovering; local inference was skipped",
        )),
    };
    let host = paths::local_host(port);
    if let Some(origin) = established_chain_origin(state).await {
        if origin != host {
            return Err(inference_pre_dispatch_unavailable(
                "immutable session chain origin is remote; local inference write was skipped",
            ));
        }
    }
    match inference_readiness_ok(&state.http, &host).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(inference_pre_dispatch_unavailable(format!(
                "{host} reported no eligible inference route"
            )))
        }
        Err(error) => return Err(inference_pre_dispatch_unavailable(error)),
    }
    let client = inference_client(prompt, max_tokens, InferenceProxyPolicy::Configured)
        .map_err(inference_pre_dispatch_unavailable)?;
    let mut result = rpc_client::run_inference(
        &client,
        &host,
        prompt,
        max_tokens,
        chat_template,
    )
    .await
    .map_err(inference_post_outcome_ambiguous)?;
    result.served_locally = true;
    pin_community_receipt_route(state, &host, &result).await;
    Ok(result)
}

#[tauri::command]
pub async fn run_inference_via_coordinator(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    k: Option<u32>,
    chat_template: Option<bool>,
) -> CmdResult<InferenceResult> {
    let max_tokens = max_tokens.unwrap_or(32);
    let k = k.unwrap_or(3);
    let chat_template = chat_template.unwrap_or(true);

    let candidates = coordinator_candidates(&state).await;
    run_inference_via_coordinator_inner(
        &state,
        &prompt,
        max_tokens,
        k,
        chat_template,
        candidates,
        InferenceProxyPolicy::Configured,
    )
    .await
}

async fn run_inference_via_coordinator_inner(
    state: &AppState,
    prompt: &str,
    max_tokens: u32,
    k: u32,
    chat_template: bool,
    candidates: Vec<String>,
    proxy_policy: InferenceProxyPolicy,
) -> CmdResult<InferenceResult> {
    let _write_guard = state.community_inference_write.lock().await;
    if candidates.is_empty() {
        return Err(inference_pre_dispatch_unavailable(
            "no coordinator answered /health - check your internet connection",
        ));
    }
    let client = inference_client(prompt, max_tokens, proxy_policy)
        .map_err(inference_pre_dispatch_unavailable)?;
    let local_prefix = paths::local_host(local_rpc_port(state).await);
    let host = candidates
        .first()
        .expect("non-empty candidates established above");
    let mut result = rpc_client::run_inference_consensus(
        &client,
        host,
        prompt,
        max_tokens,
        k,
        chat_template,
    )
    .await
    .map_err(inference_post_outcome_ambiguous)?;
    result.served_locally = *host == local_prefix;
    pin_community_receipt_route(state, host, &result).await;
    Ok(result)
}

#[cfg(test)]
mod inference_retry_tests {
    use super::{
        chain_host, community_receipt_source_is_pinned, inference_client,
        pin_community_receipt_route, run_inference_local_inner,
        run_inference_via_coordinator_direct_inner, run_inference_via_coordinator_inner,
        ChainHostChoice, InferenceProxyPolicy, COORDINATOR_HOSTS,
        INFERENCE_POST_OUTCOME_AMBIGUOUS, INFERENCE_PRE_DISPATCH_UNAVAILABLE,
    };
    use crate::types::{InferenceResult, InferenceSettlement};
    use crate::AppState;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;

    pub(super) fn test_state() -> AppState {
        AppState {
            node: Arc::new(Mutex::new(crate::node_manager::NodeManager::new())),
            store: Arc::new(Mutex::new(crate::store::Store::default())),
            data_dir: Arc::new(Mutex::new(PathBuf::new())),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            tier1_routes: Arc::new(Mutex::new(HashMap::new())),
            community_receipt_routes: Arc::new(Mutex::new(HashMap::new())),
            community_chain_host: Arc::new(Mutex::new(None)),
            community_inference_write: Arc::new(Mutex::new(())),
            chain_host: Arc::new(Mutex::new(None)),
            wallet_write: Arc::new(Mutex::new(())),
            auto_update_requests: Arc::new(Mutex::new(None)),
            has_tray: Arc::new(AtomicBool::new(false)),
            data_migration_error: Arc::new(Mutex::new(None)),
            startup_retry_cancel: tokio::sync::watch::channel(()).0,
        }
    }

    fn valid_community_result() -> InferenceResult {
        let worker = format!("0x{}", "11".repeat(32));
        let tx_hash = format!("0x{}", "22".repeat(32));
        InferenceResult {
            input: "acceptance".to_string(),
            output: "ok".to_string(),
            output_hash: format!("0x{}", "33".repeat(32)),
            model_hash: format!("0x{}", "44".repeat(32)),
            tokens_generated: 1,
            inference_ms: 1,
            attestation_status: None,
            attestation_hash: String::new(),
            tx_hash: String::new(),
            deterministic: true,
            profile_bound: true,
            quorum_verified: true,
            execution_profile: "test-profile".to_string(),
            engine: "test".to_string(),
            explorer_url: String::new(),
            routed_via: format!("community:{worker}"),
            settlement: Some(InferenceSettlement {
                status: "pending_mined_receipt".to_string(),
                tx_type: "0x25".to_string(),
                tx_hash: tx_hash.clone(),
                job_id: format!("0x{}", "55".repeat(32)),
                worker,
                submitted: true,
                included: false,
                confirmed: false,
                success: None,
                block_height: None,
                block_hash: None,
                index: None,
                reward_base: None,
                reward_arc: None,
                receipt_url: format!("/community/reward_receipt/{tx_hash}"),
                model_id: String::new(),
                input_hash: String::new(),
                output_hash: String::new(),
                assignment_epoch: String::new(),
                transaction_domain: String::new(),
                recovery_epoch: None,
                validator_set_id: None,
                validator_set_commitment: String::new(),
                validator_approvals: None,
                evidence_source: String::new(),
            }),
            consensus: None,
            coordinator: Some(COORDINATOR_HOSTS[0].to_string()),
            trace: None,
            served_locally: false,
        }
    }

    #[test]
    fn ui_fallback_is_bound_only_to_the_exact_pre_dispatch_sentinel() {
        let ui = include_str!("../../src/screens/Inference.tsx");
        assert!(ui.contains(INFERENCE_PRE_DISPATCH_UNAVAILABLE));
        assert!(!ui.contains("all coordinators failed (direct path)"));
        assert!(!ui.contains("msg.includes(\"503\")"));
        assert!(!ui.contains("msg.includes(\"fetch\")"));
        assert_ne!(
            INFERENCE_PRE_DISPATCH_UNAVAILABLE,
            INFERENCE_POST_OUTCOME_AMBIGUOUS
        );
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

    #[tokio::test]
    async fn a_valid_settlement_pins_all_session_chain_reads_to_its_exact_origin() {
        let state = test_state();
        let result = valid_community_result();
        pin_community_receipt_route(&state, COORDINATOR_HOSTS[0], &result).await;
        *state.chain_host.lock().await = Some((
            ChainHostChoice {
                host: COORDINATOR_HOSTS[1].to_string(),
                block_timestamp_ms: u64::MAX,
                height: u64::MAX,
            },
            std::time::Instant::now(),
        ));
        assert_eq!(chain_host(&state).await, COORDINATOR_HOSTS[0]);
        assert_eq!(
            state.community_chain_host.lock().await.as_deref(),
            Some(COORDINATOR_HOSTS[0])
        );

        let mut second = valid_community_result();
        let second_settlement = second.settlement.as_mut().unwrap();
        second_settlement.tx_hash = format!("0x{}", "66".repeat(32));
        second_settlement.receipt_url =
            format!("/community/reward_receipt/{}", second_settlement.tx_hash);
        let second_tx_hash = second_settlement.tx_hash.clone();
        pin_community_receipt_route(&state, COORDINATOR_HOSTS[1], &second).await;
        assert_eq!(
            state.community_chain_host.lock().await.as_deref(),
            Some(COORDINATOR_HOSTS[0]),
            "a second independent seed must not move the first receipt origin"
        );
        assert!(!state
            .community_receipt_routes
            .lock()
            .await
            .contains_key(&second_tx_hash));
    }

    #[tokio::test]
    async fn displayed_chain_origin_rejects_another_settlement_before_it_can_move_reads() {
        let state = test_state();
        *state.chain_host.lock().await = Some((
            ChainHostChoice {
                host: COORDINATOR_HOSTS[1].to_string(),
                block_timestamp_ms: 1,
                height: 1,
            },
            std::time::Instant::now(),
        ));
        pin_community_receipt_route(&state, COORDINATOR_HOSTS[0], &valid_community_result()).await;
        assert!(state.community_chain_host.lock().await.is_none());
        assert!(state.community_receipt_routes.lock().await.is_empty());
        assert_eq!(chain_host(&state).await, COORDINATOR_HOSTS[1]);
    }

    #[tokio::test]
    async fn displayed_remote_chain_blocks_local_and_other_origin_posts_before_network() {
        let local_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = test_state();
        state.node.lock().await.rpc_port = local_listener.local_addr().unwrap().port();
        *state.chain_host.lock().await = Some((
            ChainHostChoice {
                host: COORDINATOR_HOSTS[1].to_string(),
                block_timestamp_ms: 1,
                height: 1,
            },
            std::time::Instant::now(),
        ));

        let local_error = run_inference_local_inner(&state, "must not post", 1, true)
            .await
            .unwrap_err();
        assert!(local_error.starts_with(INFERENCE_PRE_DISPATCH_UNAVAILABLE));
        let other_origin = format!("http://{}", other_listener.local_addr().unwrap());
        let other_error = run_inference_via_coordinator_direct_inner(
            &state,
            "must not post",
            1,
            true,
            vec![other_origin],
            InferenceProxyPolicy::Configured,
        )
        .await
        .unwrap_err();
        assert!(other_error.starts_with(INFERENCE_PRE_DISPATCH_UNAVAILABLE));
        assert!(other_error.contains("refusing cross-origin inference"));
        for listener in [&local_listener, &other_listener] {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "a conflicting chain origin received an inference POST"
            );
        }
    }

    #[tokio::test]
    async fn malformed_or_unsubmitted_settlements_cannot_poison_route_or_chain_pins() {
        for mutate in 0..8 {
            let state = test_state();
            let mut result = valid_community_result();
            let mut source = COORDINATOR_HOSTS[0];
            if mutate == 7 {
                result.routed_via = "community:wrong".to_string();
            } else {
                let settlement = result.settlement.as_mut().unwrap();
                match mutate {
                    0 => settlement.submitted = false,
                    1 => settlement.tx_hash = format!("0x{}", "gg".repeat(32)),
                    2 => settlement.job_id.clear(),
                    3 => settlement.receipt_url = "/community/reward_receipt/wrong".to_string(),
                    4 => source = "https://140.82.16.112.evil.example",
                    5 => settlement.status = "verified_pending_approval".to_string(),
                    6 => settlement.tx_type = "0x16".to_string(),
                    _ => unreachable!(),
                }
            }
            pin_community_receipt_route(&state, source, &result).await;
            assert!(state.community_chain_host.lock().await.is_none());
            assert!(state.community_receipt_routes.lock().await.is_empty());
        }
    }

    fn readiness_body(safe: bool) -> String {
        serde_json::json!({
            "schema": "arc.inference.readiness.v1",
            "safe_to_dispatch": safe,
            "community_dispatch_ready": safe,
            "local_model_ready": false,
            "sharded_pipeline_ready": false,
            "live_community_workers": if safe { 1 } else { 0 },
            "model_id": if safe {
                serde_json::Value::String(format!("0x{}", "ab".repeat(32)))
            } else {
                serde_json::Value::Null
            },
            "required_community_execution_profile":
                arc_types::transaction::CANONICAL_REWARD_INFERENCE_PROFILE,
            "mutation_free_observation": true,
        })
        .to_string()
    }

    async fn serve_readiness_then_drop_post(
        listener: tokio::net::TcpListener,
        post_hits: Arc<AtomicUsize>,
    ) {
        for expected in ["GET /inference/readiness ", "POST /inference/run "] {
            let (mut stream, _) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                listener.accept(),
            )
            .await
            .expect("expected request timed out")
            .expect("accept request");
            let mut request = vec![0; 16 * 1024];
            let read = stream.read(&mut request).await.expect("read request");
            let head = String::from_utf8_lossy(&request[..read]);
            assert!(head.starts_with(expected), "unexpected request: {head}");
            if expected.starts_with("GET") {
                let body = readiness_body(true);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write readiness response");
            } else {
                post_hits.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[tokio::test]
    async fn local_post_reset_is_terminal_after_exactly_one_post() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let post_hits = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(serve_readiness_then_drop_post(
            listener,
            post_hits.clone(),
        ));
        let state = test_state();
        state.node.lock().await.rpc_port = port;

        let error = run_inference_local_inner(&state, "one local post", 1, true)
            .await
            .unwrap_err();
        assert!(error.starts_with(INFERENCE_POST_OUTCOME_AMBIGUOUS));
        assert!(!error.contains(INFERENCE_PRE_DISPATCH_UNAVAILABLE));
        server.await.unwrap();
        assert_eq!(post_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn direct_post_reset_never_probes_or_posts_a_second_candidate() {
        let first = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let first_origin = format!("http://{}", first.local_addr().unwrap());
        let second = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let second_origin = format!("http://{}", second.local_addr().unwrap());
        let post_hits = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(serve_readiness_then_drop_post(
            first,
            post_hits.clone(),
        ));
        let state = test_state();

        let error = run_inference_via_coordinator_direct_inner(
            &state,
            "one remote post",
            1,
            true,
            vec![first_origin, second_origin],
            InferenceProxyPolicy::Configured,
        )
        .await
        .unwrap_err();
        assert!(error.starts_with(INFERENCE_POST_OUTCOME_AMBIGUOUS));
        assert!(!error.contains(INFERENCE_PRE_DISPATCH_UNAVAILABLE));
        server.await.unwrap();
        assert_eq!(post_hits.load(Ordering::SeqCst), 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), second.accept())
                .await
                .is_err(),
            "a second candidate was contacted after the first POST"
        );
    }

    #[tokio::test]
    async fn consensus_post_reset_never_posts_a_second_candidate() {
        let first = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let first_origin = format!("http://{}", first.local_addr().unwrap());
        let second = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let second_origin = format!("http://{}", second.local_addr().unwrap());
        let post_hits = Arc::new(AtomicUsize::new(0));
        let counted = post_hits.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                first.accept(),
            )
            .await
            .expect("expected consensus POST timed out")
            .expect("accept consensus POST");
            let mut request = vec![0; 16 * 1024];
            let read = stream.read(&mut request).await.expect("read request");
            let head = String::from_utf8_lossy(&request[..read]);
            assert!(
                head.starts_with("POST /inference/run_consensus "),
                "unexpected request: {head}"
            );
            counted.fetch_add(1, Ordering::SeqCst);
        });
        let state = test_state();

        let error = run_inference_via_coordinator_inner(
            &state,
            "one consensus post",
            1,
            3,
            true,
            vec![first_origin, second_origin],
            InferenceProxyPolicy::Configured,
        )
        .await
        .unwrap_err();
        assert!(error.starts_with(INFERENCE_POST_OUTCOME_AMBIGUOUS));
        server.await.unwrap();
        assert_eq!(post_hits.load(Ordering::SeqCst), 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), second.accept())
                .await
                .is_err(),
            "a second consensus origin received a replayed POST"
        );
    }

    #[tokio::test]
    async fn inference_post_does_not_follow_a_temporary_redirect() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = source.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target_addr}/replayed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = inference_client(
            "redirect probe",
            1,
            InferenceProxyPolicy::AcceptanceDirect,
        )
        .unwrap();
        let response = client
            .post(format!("http://{source_addr}/inference/run"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        responder.await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), target.accept())
                .await
                .is_err(),
            "redirect target received a replayed inference POST"
        );
    }

    async fn recording_host(context_status: u16) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 8192];
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                    let (status, body) = match path.as_str() {
                        "/native-inference/context" if context_status == 200 => (
                            200,
                            r#"{"candidate_protocol":4,"chain_protocol":4,"native_only_chain":true,"request_admission_open":true,"request_admission":{"operator_enabled":true,"runtime_ready":true}}"#,
                        ),
                        "/native-inference/context" => (404, ""),
                        "/network/info" => (
                            200,
                            r#"{"protocol_version":"3.0.0","recovery_active":true,"recovery_domain":"0x0101010101010101010101010101010101010101010101010101010101010101"}"#,
                        ),
                        _ => (404, ""),
                    };
                    log.lock().unwrap().push(path);
                    let reason = if status == 200 { "OK" } else { "Not Found" };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (origin, seen)
    }

    #[tokio::test]
    async fn a_transfer_is_refused_before_signing_on_a_protocol_4_host_and_proceeds_elsewhere() {
        const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        for (context_status, protocol_4) in [(200u16, true), (404, false)] {
            let (origin, seen) = recording_host(context_status).await;
            let state = test_state();
            state.store.lock().await.identity = Some(crate::identity::derive(PHRASE).unwrap());
            *state.chain_host.lock().await = Some((
                ChainHostChoice {
                    host: origin,
                    block_timestamp_ms: 1,
                    height: 1,
                },
                std::time::Instant::now(),
            ));
            let outcome = super::send_arc_inner(&state, "11".repeat(32), "1".into()).await;
            let paths = seen.lock().unwrap().clone();
            if protocol_4 {
                let error = outcome.unwrap_err();
                assert!(error.contains("nothing was signed"), "{error}");
                assert_eq!(paths, ["/native-inference/context"]);
            } else {
                assert!(outcome.is_err());
                assert!(
                    paths.iter().any(|path| path.starts_with("/account/")),
                    "{paths:?}"
                );
                assert!(
                    !paths.iter().any(|path| path == "/tx/submit_signed"),
                    "{paths:?}"
                );
            }
        }
    }
}

#[tauri::command]
pub async fn run_inference_via_coordinator_direct(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    chat_template: Option<bool>,
) -> CmdResult<InferenceResult> {
    let explicit_origin = explicit_chain_origin();
    if let Some(result) = try_run_pinned_shard_inference(
        &state,
        crate::node_manager::PRODUCTION_SHARD_COORDINATOR,
        explicit_origin.as_deref(),
        &prompt,
        max_tokens.unwrap_or(32),
        chat_template.unwrap_or(true),
        InferenceProxyPolicy::Configured,
    )
    .await?
    {
        return Ok(result);
    }
    let candidates = coordinator_candidates(&state).await;
    if candidates.is_empty() {
        return Err(inference_pre_dispatch_unavailable(
            "no coordinator answered /health - check your internet connection",
        ));
    }
    run_inference_via_coordinator_direct_inner(
        &state,
        &prompt,
        max_tokens.unwrap_or(32),
        chat_template.unwrap_or(true),
        candidates,
        InferenceProxyPolicy::Configured,
    )
    .await
}

async fn try_run_pinned_shard_inference(
    state: &AppState,
    pin: Option<crate::node_manager::PinnedShardCoordinator>,
    explicit_origin: Option<&str>,
    prompt: &str,
    max_tokens: u32,
    chat_template: bool,
    proxy_policy: InferenceProxyPolicy,
) -> CmdResult<Option<InferenceResult>> {
    let _write_guard = state.community_inference_write.lock().await;
    let Some(pin) = pin else {
        return Ok(None);
    };
    if pin.validate().is_err() || prompt.trim().is_empty() || max_tokens == 0 {
        return Ok(None);
    }
    let origin = pin.coordinator_url.trim_end_matches('/');
    if (!pin.allow_insecure_for_loopback && !COORDINATOR_HOSTS.contains(&origin))
        || explicit_origin
            .map(|custom| custom.trim().trim_end_matches('/'))
            .filter(|custom| !custom.is_empty())
            .is_some_and(|custom| custom != origin)
    {
        return Ok(None);
    }
    if established_chain_origin(state)
        .await
        .is_some_and(|established| {
            let established = established.trim_end_matches('/');
            established != origin && !COORDINATOR_HOSTS.contains(&established)
        })
    {
        return Ok(None);
    }
    let client = inference_client(prompt, max_tokens, proxy_policy)
        .map_err(inference_pre_dispatch_unavailable)?;
    let Ok(Ok(readiness)) = tokio::time::timeout(
        COORDINATOR_HEALTH_TIMEOUT,
        rpc_client::community_shard_dispatch_readiness(&client, origin),
    )
    .await
    else {
        return Ok(None);
    };
    let coordinator_id = format!("0x{}", pin.coordinator_id);
    let transaction_domain = format!("0x{}", pin.transaction_domain);
    let model_id = format!("0x{}", pin.expected_model_id);
    let overhead = if chat_template {
        readiness.chat_template_overhead_bytes
    } else {
        0
    };
    if !readiness.safe_to_dispatch
        || !readiness.chat_template_supported
        || readiness.protocol_version != rpc_client::COMMUNITY_SHARD_PROTOCOL_VERSION_V1
        || readiness.coordinator_id.as_deref() != Some(coordinator_id.as_str())
        || readiness.transaction_domain.as_deref() != Some(transaction_domain.as_str())
        || readiness.model_id.as_deref() != Some(model_id.as_str())
        || prompt.len().saturating_add(overhead) > readiness.max_prompt_bytes.min(1000)
        || max_tokens > readiness.max_tokens_cap.min(64)
    {
        return Ok(None);
    }
    rpc_client::run_community_sharded_inference(
        &client,
        origin,
        prompt,
        max_tokens,
        chat_template,
        &model_id,
    )
    .await
    .map(Some)
    .map_err(inference_post_outcome_ambiguous)
}

#[cfg(test)]
mod community_shard_route_tests {
    use super::*;
    use crate::node_manager::PinnedShardCoordinator;
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn loopback_pin(origin: String) -> PinnedShardCoordinator {
        PinnedShardCoordinator {
            coordinator_url: Box::leak(origin.into_boxed_str()),
            coordinator_id: "1111111111111111111111111111111111111111111111111111111111111111",
            transaction_domain: "2222222222222222222222222222222222222222222222222222222222222222",
            expected_model_id: "3333333333333333333333333333333333333333333333333333333333333333",
            allow_insecure_for_loopback: true,
        }
    }

    fn readiness(pin: PinnedShardCoordinator) -> Value {
        json!({
            "schema": "arc.community_shard.dispatch_readiness.v1",
            "mutation_free_observation": true,
            "enabled": true,
            "protocol_version": 1,
            "verified_dispatch_enabled": true,
            "sharded_pipeline_ready": true,
            "safe_to_dispatch": true,
            "rewards_enabled": false,
            "chat_template_supported": true,
            "coordinator_id": format!("0x{}", pin.coordinator_id),
            "transaction_domain": format!("0x{}", pin.transaction_domain),
            "model_id": format!("0x{}", pin.expected_model_id),
            "execution_profile": arc_types::transaction::CANONICAL_REWARD_INFERENCE_PROFILE,
            "max_prompt_bytes": 1000,
            "chat_template_overhead_bytes": 15,
            "max_tokens_cap": 64
        })
    }

    fn verified_result(pin: PinnedShardCoordinator) -> Value {
        let lease = json!({"boot_nonce": format!("0x{}", "44".repeat(32)), "seq": 1});
        json!({
            "protocol_version": 1,
            "session_id": lease,
            "model_name": "test-model",
            "model_id": format!("0x{}", pin.expected_model_id),
            "execution_profile": arc_types::transaction::CANONICAL_REWARD_INFERENCE_PROFILE,
            "generated_tokens": [2],
            "tokens_generated": 1,
            "output": "answer",
            "output_hash": format!("0x{}", "55".repeat(32)),
            "stopped_by_eos": true,
            "total_ms": 123,
            "participants": [{"worker_id": format!("0x{}", "66".repeat(32)),
                "lease_id": lease, "lease_epoch": 1, "range": {"start_layer": 0, "end_layer": 2}}],
            "participant_identity_basis": "node_key_not_proof_of_independent_operator",
            "verification": {"profile_bound": true, "quorum_verified": true,
                "execution_profile": arc_types::transaction::CANONICAL_REWARD_INFERENCE_PROFILE},
            "rewards_enabled": false,
            "reward_eligible": false
        })
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 2048];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0 && bytes.len() + count <= 16 * 1024);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        return String::from_utf8(bytes).unwrap();
                    }
                }
            }
        })
        .await
        .unwrap()
    }

    async fn serve(
        listener: tokio::net::TcpListener,
        observation: Value,
        expect_post: bool,
        answer: Option<Value>,
    ) -> usize {
        let (mut stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
        assert!(read_request(&mut stream)
            .await
            .starts_with("GET /inference/community_shards/dispatch_readiness "));
        let body = observation.to_string();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        drop(stream);
        if !expect_post {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
            return 0;
        }
        let (mut stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let request = read_request(&mut stream).await;
        assert!(request.starts_with("POST /inference/run_community_sharded "));
        let sent: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            sent,
            json!({"input":"hello", "max_tokens":1, "chat_template":true})
        );
        if let Some(answer) = answer {
            let body = answer.to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        drop(stream);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        1
    }

    #[test]
    fn explicit_origin_ignores_blank_values_and_keeps_wallet_precedence() {
        for (wallet, tier, expected) in [
            (None, None, None),
            (Some(""), None, None),
            (Some(" \t "), Some(""), None),
            (None, Some(" \t "), None),
            (
                Some(""),
                Some("https://tier.example"),
                Some("https://tier.example"),
            ),
            (
                Some(" \t "),
                Some(" https://tier.example "),
                Some("https://tier.example"),
            ),
            (
                Some(" https://wallet.example "),
                Some("https://tier.example"),
                Some("https://wallet.example"),
            ),
        ] {
            let actual = explicit_chain_origin_with(|key| match key {
                "ARC_WALLET_HOST" => wallet.map(str::to_owned),
                "ARC_TIER1_RPC" => tier.map(str::to_owned),
                _ => panic!("unexpected origin variable {key}"),
            });
            assert_eq!(actual.as_deref(), expected);
        }
    }

    #[tokio::test]
    async fn compute_only_route_reaches_shards_without_changing_chain_or_receipt_origins() {
        for drop_response in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let pin = loopback_pin(format!("http://{}", listener.local_addr().unwrap()));
            let server = tokio::spawn(serve(
                listener,
                readiness(pin),
                true,
                (!drop_response).then(|| verified_result(pin)),
            ));
            let state = super::inference_retry_tests::test_state();
            *state.community_chain_host.lock().await = Some(COORDINATOR_HOSTS[0].into());
            let result = try_run_pinned_shard_inference(
                &state,
                Some(pin),
                Some(" \t "),
                "hello",
                1,
                true,
                InferenceProxyPolicy::AcceptanceDirect,
            )
            .await;
            if drop_response {
                assert!(result
                    .unwrap_err()
                    .starts_with(INFERENCE_POST_OUTCOME_AMBIGUOUS));
            } else {
                let result = result.unwrap().unwrap();
                assert_eq!(result.routed_via, "community_sharded_pipeline");
                assert!(result.quorum_verified && result.settlement.is_none());
                assert!(result.tx_hash.is_empty());
            }
            assert_eq!(server.await.unwrap(), 1);
            assert_eq!(
                state.community_chain_host.lock().await.as_deref(),
                Some(COORDINATOR_HOSTS[0])
            );
            assert!(state.chain_host.lock().await.is_none());
            assert!(state.community_receipt_routes.lock().await.is_empty());
        }
    }

    #[tokio::test]
    async fn shard_readiness_mismatches_and_request_caps_never_dispatch() {
        for case in 0..8 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let pin = loopback_pin(format!("http://{}", listener.local_addr().unwrap()));
            let mut observation = readiness(pin);
            let mut prompt = "hello".to_string();
            match case {
                0 => observation["coordinator_id"] = json!(format!("0x{}", "99".repeat(32))),
                1 => observation["transaction_domain"] = json!(format!("0x{}", "99".repeat(32))),
                2 => observation["model_id"] = json!(format!("0x{}", "99".repeat(32))),
                3 => observation["protocol_version"] = json!(2),
                4 => observation["chat_template_supported"] = json!(false),
                5 => observation["rewards_enabled"] = json!(true),
                6 => observation["safe_to_dispatch"] = json!(false),
                7 => prompt = "a".repeat(986),
                _ => unreachable!(),
            }
            let server = tokio::spawn(serve(listener, observation, false, None));
            let state = super::inference_retry_tests::test_state();
            assert!(try_run_pinned_shard_inference(
                &state,
                Some(pin),
                None,
                &prompt,
                1,
                true,
                InferenceProxyPolicy::AcceptanceDirect
            )
            .await
            .unwrap()
            .is_none());
            assert_eq!(server.await.unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn shard_route_preserves_custom_origins_and_disabled_pins_without_network_io() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pin = loopback_pin(format!("http://{}", listener.local_addr().unwrap()));
        let state = super::inference_retry_tests::test_state();
        for candidate in [None, Some(pin)] {
            assert!(try_run_pinned_shard_inference(
                &state,
                candidate,
                Some("https://custom.invalid"),
                "hello",
                1,
                true,
                InferenceProxyPolicy::AcceptanceDirect
            )
            .await
            .unwrap()
            .is_none());
        }
        *state.community_chain_host.lock().await = Some("https://custom.invalid".into());
        assert!(try_run_pinned_shard_inference(
            &state,
            Some(pin),
            None,
            "hello",
            1,
            true,
            InferenceProxyPolicy::AcceptanceDirect
        )
        .await
        .unwrap()
        .is_none());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }
}

async fn run_inference_via_coordinator_direct_inner(
    state: &AppState,
    prompt: &str,
    max_tokens: u32,
    chat_template: bool,
    candidates: Vec<String>,
    proxy_policy: InferenceProxyPolicy,
) -> CmdResult<InferenceResult> {
    let _write_guard = state.community_inference_write.lock().await;
    let candidates = match established_chain_origin(state).await {
        Some(origin) => candidates
            .into_iter()
            .filter(|candidate| candidate == &origin)
            .collect::<Vec<_>>(),
        None => candidates,
    };
    if candidates.is_empty() {
        return Err(inference_pre_dispatch_unavailable(
            "the immutable community receipt origin is unavailable; refusing cross-origin inference",
        ));
    }
    let mut selected = None;
    let mut readiness_failures = Vec::new();
    for host in candidates {
        match inference_readiness_ok(&state.http, &host).await {
            Ok(true) => {
                selected = Some(host);
                break;
            }
            Ok(false) => readiness_failures.push(format!("{host}: unavailable")),
            Err(error) => readiness_failures.push(error),
        }
    }
    let host = selected.ok_or_else(|| {
        inference_pre_dispatch_unavailable(format!(
            "no candidate proved mutation-free inference readiness ({})",
            readiness_failures.join("; ")
        ))
    })?;
    let client = inference_client(prompt, max_tokens, proxy_policy)
        .map_err(inference_pre_dispatch_unavailable)?;
    let local_prefix = paths::local_host(local_rpc_port(state).await);
    let mut result = rpc_client::run_inference_remote(
        &client,
        &host,
        prompt,
        max_tokens,
        chat_template,
    )
    .await
    .map_err(inference_post_outcome_ambiguous)?;
    result.served_locally = host == local_prefix;
    pin_community_receipt_route(state, &host, &result).await;
    Ok(result)
}

pub(crate) async fn run_inference_via_exact_production_coordinator_inner(
    state: &AppState,
    source_host: &str,
    prompt: &str,
    max_tokens: u32,
    chat_template: bool,
) -> CmdResult<InferenceResult> {
    if !COORDINATOR_HOSTS.contains(&source_host) {
        return Err("production acceptance origin is not a compiled ARC coordinator".to_string());
    }
    if !health_ok(&state.http, source_host, COORDINATOR_HEALTH_TIMEOUT).await {
        return Err(
            "production acceptance coordinator did not pass its bounded health probe".to_string(),
        );
    }
    run_inference_via_coordinator_direct_inner(
        state,
        prompt,
        max_tokens,
        chat_template,
        vec![source_host.to_string()],
        InferenceProxyPolicy::AcceptanceDirect,
    )
    .await
}

const SETTLEMENT_WRITE_UNAVAILABLE: &str = concat!("is unavailable in the v", env!("CARGO_PKG_VERSION"), " recovery candidate before any transaction is signed or submitted: exact model-artifact binding, validator-authenticated authorization, and settlement are not production-ready. VRF selection and server-derived replica labels are not validator approval. Free/community inference remains available.");

fn settlement_write_unavailable<T>(flow: &str) -> CmdResult<T> {
    Err(format!("{} {}", flow, SETTLEMENT_WRITE_UNAVAILABLE))
}

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

const COORDINATOR_HOSTS: [&str; 6] = rpc_client::PRODUCTION_RPC_ORIGINS;

const WALLET_HOSTS: [&str; 6] = rpc_client::PRODUCTION_RPC_ORIGINS;

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

#[tauri::command]
pub async fn set_worker_threads(
    app: AppHandle,
    state: State<'_, AppState>,
    threads: u32,
) -> CmdResult<ThreadsApplied> {
    require_settings_owner(&state).await?;
    let cores = cached_hardware().cpu_cores.max(1);
    if threads == 0 || threads > cores {
        return Err(format!(
            "core count must be between 1 and {} on this machine",
            cores
        ));
    }

    update_stored_config(&state, |config| config.worker_threads = Some(threads)).await?;
    let was_running = state.node.lock().await.is_running();

    if !was_running {
        return Ok(ThreadsApplied {
            worker_threads: threads,
            restarted: false,
            message: format!("Saved. The node will use {} cores when it starts.", threads),
        });
    }

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

fn classify_release_transport(stage: &str, error: reqwest::Error) -> StartupFailure {
    let message = format!("{stage} transport failed: {error}");
    if error.is_connect() || error.is_timeout() || error.is_body() {
        StartupFailure::Transient(message)
    } else {
        StartupFailure::Terminal(message)
    }
}

fn classify_release_status(
    stage: &str,
    status: reqwest::StatusCode,
    version: &str,
) -> StartupFailure {
    let message = format!("{stage} returned HTTP {status} for v{version}");
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
    {
        StartupFailure::Transient(message)
    } else {
        StartupFailure::Terminal(message)
    }
}

async fn read_bounded_release_body_for_startup(
    mut response: reqwest::Response,
    maximum: usize,
    label: &str,
) -> Result<Vec<u8>, StartupFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(StartupFailure::Terminal(format!(
            "{label} exceeds its {maximum}-byte safety limit"
        )));
    }
    let capacity = response
        .content_length()
        .unwrap_or_default()
        .min(maximum as u64) as usize;
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| classify_release_transport(label, error))?
    {
        if chunk.len() > maximum.saturating_sub(body.len()) {
            return Err(StartupFailure::Terminal(format!(
                "{label} exceeds its {maximum}-byte safety limit"
            )));
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

async fn acquire_binary_install_lock(target: &Path) -> Result<std::fs::File, String> {
    acquire_exclusive_download_lock(binary_install_lock_path(target), "arc-node install").await
}

async fn acquire_model_download_lock(target: &Path) -> Result<std::fs::File, String> {
    acquire_exclusive_download_lock(model_download_lock_path(target), "model download").await
}

#[tauri::command]
pub async fn ensure_binary(app: AppHandle, state: State<'_, AppState>) -> CmdResult<BinaryStatus> {
    require_data_migration_ready(&state).await?;
    {
        let node = state.node.lock().await;
        node.require_idle_for_binary_check().map_err(map_err)?;
    }
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

async fn ensure_binary_inner(app: &AppHandle) -> Result<BinaryStatus, String> {
    ensure_binary_inner_classified(app)
        .await
        .map_err(|failure| paths::redact_home_text(&failure.to_string()))
}

async fn ensure_binary_inner_classified(_app: &AppHandle) -> Result<BinaryStatus, StartupFailure> {
    if let Some(p) = crate::node_manager::env_binary_override() {
        if p.exists() {
            tracing::info!("using arc-node from env override: {}", p.display());
            return Ok(installed(&p));
        }
        return Err(format!(
            "ARC_NODE_BIN points at {}, which does not exist",
            p.display()
        )
        .into());
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

    let recovery_target = target.clone();
    tokio::task::spawn_blocking(move || {
        recover_interrupted_binary_install(&recovery_target, EXPECTED_NODE_VERSION)
    })
    .await
    .map_err(map_err)??;

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
                } else if semver_gt(&v, EXPECTED_NODE_VERSION) {
                    return Err(format!(
                        "managed arc-node v{} is newer than desktop v{}. Upgrade the desktop, or set ARC_NODE_BIN explicitly if this pairing is intentional",
                        v, EXPECTED_NODE_VERSION
                    )
                    .into());
                } else {
                    return Err(format!(
                        "managed arc-node at {} reports unrecognized version '{}'; expected v{}",
                        target.display(),
                        v,
                        EXPECTED_NODE_VERSION
                    )
                    .into());
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
        tracing::info!("using locally built arc-node: {}", dev.display());
        return Ok(installed(&dev));
    }

    match download_arc_node(&target).await {
        Ok(total_bytes) => Ok(BinaryStatus {
            path: target.to_string_lossy().into_owned(),
            downloaded_bytes: total_bytes,
            total_bytes,
            already_installed: false,
        }),
        Err(e) => {
            if let Some(dev) = crate::node_manager::dev_build_binary() {
                tracing::warn!(
                    "arc-node download failed ({}) - falling back to {}",
                    e,
                    dev.display()
                );
                return Ok(installed(&dev));
            }
            Err(e.with_context(
                "No arc-node is available to run. Build one with `cargo build --release -p arc-node` in the arc-chain checkout, or set ARC_NODE_BIN to an existing binary",
            ))
        }
    }
}

async fn download_arc_node(target: &Path) -> Result<u64, StartupFailure> {
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

    let checksum_resp = client
        .get(&checksum_url)
        .send()
        .await
        .map_err(|error| classify_release_transport("release checksum manifest", error))?;
    if !checksum_resp.status().is_success() {
        return Err(classify_release_status(
            "release checksum manifest",
            checksum_resp.status(),
            EXPECTED_NODE_VERSION,
        ));
    }
    let checksum_bytes = read_bounded_release_body_for_startup(
        checksum_resp,
        MAX_CHECKSUM_MANIFEST_BYTES,
        "release checksum manifest",
    )
    .await?;
    let signature_resp = client
        .get(&checksum_signature_url)
        .send()
        .await
        .map_err(|error| classify_release_transport("release checksum signature", error))?;
    if !signature_resp.status().is_success() {
        return Err(classify_release_status(
            "release checksum signature",
            signature_resp.status(),
            EXPECTED_NODE_VERSION,
        ));
    }
    let signature_bytes = read_bounded_release_body_for_startup(
        signature_resp,
        MAX_MANIFEST_SIGNATURE_BYTES,
        "release checksum signature",
    )
    .await?;
    verify_release_manifest_signature(&checksum_bytes, &signature_bytes)?;
    let checksum_manifest = std::str::from_utf8(&checksum_bytes)
        .map_err(|_| "release checksum manifest is not UTF-8".to_string())?;
    let expected_sha256 = expected_release_sha256(checksum_manifest, asset)?;

    let mut resp = client
        .get(&url)
        .send()
        .await
        .map_err(|error| classify_release_transport("release asset", error))?;
    if !resp.status().is_success() {
        return Err(classify_release_status(
            "release asset",
            resp.status(),
            EXPECTED_NODE_VERSION,
        ));
    }
    if resp
        .content_length()
        .is_some_and(|length| length > MAX_NODE_BINARY_BYTES)
    {
        return Err(StartupFailure::Terminal(format!(
            "release asset {} exceeds the 512 MiB safety limit",
            asset
        )));
    }
    let mut pending = create_binary_download_sidecar(target).await?;
    let mut hasher = Sha256::new();
    let mut total_bytes = 0u64;
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                return Err(classify_release_transport(
                    &format!("release asset {asset} after {total_bytes} bytes"),
                    error,
                ));
            }
        };
        total_bytes = total_bytes.saturating_add(chunk.len() as u64);
        if total_bytes > MAX_NODE_BINARY_BYTES {
            return Err(StartupFailure::Terminal(format!(
                "release asset {} exceeds the 512 MiB safety limit",
                asset
            )));
        }
        hasher.update(&chunk);
        if let Err(error) = pending.file_mut()?.write_all(&chunk).await {
            return Err(StartupFailure::Terminal(format!(
                "write {}: {}",
                pending.path.display(),
                error
            )));
        }
    }
    pending.file_mut()?.flush().await.map_err(map_err)?;
    pending.file_mut()?.sync_all().await.map_err(map_err)?;
    pending.close();

    let actual_sha256: [u8; 32] = hasher.finalize().into();
    if actual_sha256 != expected_sha256 {
        return Err(StartupFailure::Terminal(format!(
            "checksum verification failed for {} (expected {}, got {})",
            asset,
            hex::encode(expected_sha256),
            hex::encode(actual_sha256)
        )));
    }

    let persisted_path = pending.path.clone();
    let persisted_sha256 =
        tokio::task::spawn_blocking(move || sha256_regular_file(&persisted_path))
            .await
            .map_err(map_err)??;
    if persisted_sha256 != Some(expected_sha256) {
        return Err(StartupFailure::Terminal(format!(
            "durable checksum verification failed for {}",
            asset
        )));
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
        return Err(StartupFailure::Terminal(format!(
            "downloaded {} reports v{}, expected v{}",
            asset, downloaded_version, EXPECTED_NODE_VERSION
        )));
    }

    install_over(&pending.path, target, expected_sha256)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(target, perms).map_err(map_err)?;
    }

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

fn recover_interrupted_binary_install(target: &Path, expected_version: &str) -> Result<(), String> {
    let journal = binary_install_journal_path(target);
    let journal_metadata = match std::fs::symlink_metadata(&journal) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("inspect {}: {}", journal.display(), error)),
    };

    let Some(metadata) = journal_metadata else {
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

fn read_arc_node_version(binary: &std::path::Path) -> Option<String> {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("--version");
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

struct ModelTierSpec {
    id: &'static str,
    display_name: &'static str,
    url: &'static str,
    size_bytes: u64,
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

#[cfg(test)]
fn model_download_sidecar(target: &Path, nonce: u64) -> PathBuf {
    let suffix = format!("download-{}-{nonce:016x}", std::process::id());
    match target.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => target.with_extension(format!("{suffix}.{extension}")),
        None => target.with_extension(suffix),
    }
}

#[cfg(test)]
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

#[tauri::command]
pub async fn recommended_tier() -> CmdResult<String> {
    Ok(tier_for_ram_gb(hardware::detect().ram_gb).into())
}

pub(crate) fn tier_for_ram_gb(ram_gb: u64) -> &'static str {
    if ram_gb >= hardware::WORKER_MIN_NOMINAL_RAM_GB { "standard" } else { "none" }
}

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

const MODEL_DOWNLOAD_MAX_STALLED_ATTEMPTS: u32 = 8;
const MODEL_DOWNLOAD_MAX_RESTARTS: u32 = 2;
const MODEL_DOWNLOAD_RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(2);
const MODEL_DOWNLOAD_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);
const MODEL_DOWNLOAD_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const MODEL_DOWNLOAD_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MODEL_DOWNLOAD_DISK_MARGIN_BYTES: u64 = 512 * 1024 * 1024;
const MODEL_PROGRESS_EMIT_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
struct ModelDownloadPolicy {
    max_stalled_attempts: u32,
    retry_base: std::time::Duration,
    retry_max: std::time::Duration,
    idle_timeout: std::time::Duration,
}

impl ModelDownloadPolicy {
    const PRODUCTION: Self = Self {
        max_stalled_attempts: MODEL_DOWNLOAD_MAX_STALLED_ATTEMPTS,
        retry_base: MODEL_DOWNLOAD_RETRY_BASE,
        retry_max: MODEL_DOWNLOAD_RETRY_MAX,
        idle_timeout: MODEL_DOWNLOAD_IDLE_TIMEOUT,
    };

    fn retry_delay(&self, stalled: u32) -> std::time::Duration {
        let exponent = stalled.saturating_sub(1).min(16);
        self.retry_base
            .saturating_mul(1u32 << exponent)
            .min(self.retry_max)
    }
}

#[derive(Debug)]
enum ModelAttemptFailure {
    Transient(String),
    Restart(String),
    Fatal(String),
}

#[derive(Debug, PartialEq, Eq)]
enum ModelRangePlan {
    AppendFrom(u64),
    Complete,
}

fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, size) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    let size = match size.trim() {
        "*" => None,
        size => Some(size.parse::<u64>().ok()?),
    };
    (start <= end).then_some((start, end, size))
}

fn plan_model_range_response(
    status: u16,
    content_range: Option<&str>,
    content_length: Option<u64>,
    offset: u64,
    total: u64,
) -> Result<ModelRangePlan, ModelAttemptFailure> {
    match status {
        200 => match content_length {
            Some(length) if length != total => Err(ModelAttemptFailure::Fatal(format!(
                "the model mirror is serving {length} bytes, but the pinned artifact is {total} bytes"
            ))),
            _ => Ok(ModelRangePlan::AppendFrom(0)),
        },
        206 => {
            let Some((start, end, size)) = content_range.and_then(parse_content_range) else {
                return Err(ModelAttemptFailure::Restart(
                    "the model mirror sent a partial response without a valid Content-Range"
                        .to_string(),
                ));
            };
            if let Some(size) = size.filter(|size| *size != total) {
                return Err(ModelAttemptFailure::Fatal(format!(
                    "the model mirror is serving {size} bytes, but the pinned artifact is {total} bytes"
                )));
            }
            if end >= total {
                return Err(ModelAttemptFailure::Restart(format!(
                    "the model mirror sent bytes past the pinned size of {total}"
                )));
            }
            if start == offset || start == 0 {
                Ok(ModelRangePlan::AppendFrom(start))
            } else {
                Err(ModelAttemptFailure::Restart(format!(
                    "the model mirror resumed at byte {start} instead of {offset}"
                )))
            }
        }
        416 if offset >= total => Ok(ModelRangePlan::Complete),
        416 => Err(ModelAttemptFailure::Restart(
            "the model mirror could not resume the saved partial download".to_string(),
        )),
        408 | 425 | 429 | 500..=599 => Err(ModelAttemptFailure::Transient(format!(
            "the model mirror returned HTTP {status}"
        ))),
        _ => Err(ModelAttemptFailure::Fatal(format!(
            "the model mirror returned HTTP {status} for the pinned model URL"
        ))),
    }
}

fn describe_model_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "the connection to the model mirror timed out".to_string()
    } else if error.is_connect() {
        "could not connect to the model mirror (check the internet connection)".to_string()
    } else if error.is_body() || error.is_decode() {
        "the connection dropped while downloading".to_string()
    } else {
        format!("network error: {error}")
    }
}

fn model_write_failure(partial: &Path, error: std::io::Error) -> ModelAttemptFailure {
    if error.kind() == std::io::ErrorKind::StorageFull {
        ModelAttemptFailure::Fatal(format!(
            "the disk is full while saving {}; free some space and retry (the bytes already saved are kept)",
            partial.display()
        ))
    } else {
        ModelAttemptFailure::Fatal(format!("write {}: {error}", partial.display()))
    }
}

fn format_gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn model_partial_path(target: &Path, spec: &ModelTierSpec) -> PathBuf {
    let digest_prefix = &spec.sha256[..spec.sha256.len().min(16)];
    target.with_extension(format!("{digest_prefix}.partial"))
}

fn cleanup_model_partials(target: &Path, keep: Option<&Path>) -> Result<(), String> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    let stem = target
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "model filename is not UTF-8".to_string())?;
    let prefix = format!("{stem}.");
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read {}: {}", parent.display(), error)),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".partial") {
            continue;
        }
        let path = entry.path();
        if keep.is_some_and(|keep| keep == path.as_path()) {
            continue;
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file()) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

async fn prepare_model_partial(partial: &Path, total: u64, restart: bool) -> Result<u64, String> {
    match tokio::fs::symlink_metadata(partial).await {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(format!(
                "refusing a model partial download that is not a regular file: {}",
                partial.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect {}: {}", partial.display(), error)),
    }
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(partial)
        .await
        .map_err(|error| format!("open {}: {}", partial.display(), error))?;
    let length = file
        .metadata()
        .await
        .map_err(|error| format!("inspect {}: {}", partial.display(), error))?
        .len();
    if restart || length > total {
        file.set_len(0)
            .await
            .map_err(|error| format!("reset {}: {}", partial.display(), error))?;
        file.sync_all()
            .await
            .map_err(|error| format!("sync {}: {}", partial.display(), error))?;
        return Ok(0);
    }
    Ok(length)
}

async fn model_partial_len(partial: &Path) -> u64 {
    tokio::fs::metadata(partial)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn model_progress(
    tier: &str,
    stage: &str,
    downloaded_bytes: u64,
    total_bytes: u64,
) -> ModelDownloadProgress {
    ModelDownloadProgress {
        tier: tier.to_string(),
        downloaded_bytes,
        total_bytes,
        done: stage == "done",
        stage: stage.to_string(),
        resumed_from_bytes: 0,
        attempt: 0,
        retry_in_secs: None,
        message: None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn model_download_attempt<F>(
    client: &reqwest::Client,
    url: &str,
    partial: &Path,
    offset: u64,
    total: u64,
    tier: &str,
    attempt: u32,
    idle_timeout: std::time::Duration,
    rewound_to: &mut Option<u64>,
    emit: &mut F,
) -> Result<(), ModelAttemptFailure>
where
    F: FnMut(ModelDownloadProgress),
{
    use tokio::io::AsyncSeekExt as _;

    let mut request = client.get(url);
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let mut response = match tokio::time::timeout(idle_timeout, request.send()).await {
        Err(_) => {
            return Err(ModelAttemptFailure::Transient(format!(
                "the model mirror did not answer within {} s",
                idle_timeout.as_secs()
            )))
        }
        Ok(Err(error)) => {
            return Err(ModelAttemptFailure::Transient(
                describe_model_transport_error(&error),
            ))
        }
        Ok(Ok(response)) => response,
    };
    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let start = match plan_model_range_response(
        response.status().as_u16(),
        content_range.as_deref(),
        response.content_length(),
        offset,
        total,
    )? {
        ModelRangePlan::Complete => return Ok(()),
        ModelRangePlan::AppendFrom(start) => start,
    };
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(false)
        .open(partial)
        .await
        .map_err(|error| model_write_failure(partial, error))?;
    if start != offset {
        *rewound_to = Some(start);
        file.set_len(start)
            .await
            .map_err(|error| model_write_failure(partial, error))?;
    }
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|error| model_write_failure(partial, error))?;

    let mut written = start;
    emit(ModelDownloadProgress {
        resumed_from_bytes: start,
        attempt,
        ..model_progress(tier, "downloading", written, total)
    });
    let mut last_emit = std::time::Instant::now();
    let streamed: Result<(), ModelAttemptFailure> = async {
        loop {
            let chunk = match tokio::time::timeout(idle_timeout, response.chunk()).await {
                Err(_) => {
                    return Err(ModelAttemptFailure::Transient(format!(
                        "no data from the model mirror for {} s",
                        idle_timeout.as_secs()
                    )))
                }
                Ok(Err(error)) => {
                    return Err(ModelAttemptFailure::Transient(
                        describe_model_transport_error(&error),
                    ))
                }
                Ok(Ok(None)) => return Ok(()),
                Ok(Ok(Some(chunk))) => chunk,
            };
            let next = written.saturating_add(chunk.len() as u64);
            if next > total {
                return Err(ModelAttemptFailure::Restart(format!(
                    "the model mirror sent more than the pinned {total} bytes"
                )));
            }
            if let Err(error) = file.write_all(&chunk).await {
                return Err(model_write_failure(partial, error));
            }
            written = next;
            if last_emit.elapsed() >= MODEL_PROGRESS_EMIT_EVERY {
                emit(ModelDownloadProgress {
                    resumed_from_bytes: start,
                    attempt,
                    ..model_progress(tier, "downloading", written, total)
                });
                last_emit = std::time::Instant::now();
            }
        }
    }
    .await;
    let flushed = file.flush().await;
    streamed?;
    flushed.map_err(|error| model_write_failure(partial, error))?;
    file.sync_all()
        .await
        .map_err(|error| model_write_failure(partial, error))?;
    if written < total {
        return Err(ModelAttemptFailure::Transient(format!(
            "the connection closed after {} of {}",
            format_gb(written),
            format_gb(total)
        )));
    }
    emit(ModelDownloadProgress {
        resumed_from_bytes: start,
        attempt,
        ..model_progress(tier, "downloading", written, total)
    });
    Ok(())
}

async fn download_model_resumable<F>(
    client: &reqwest::Client,
    url: &str,
    partial: &Path,
    total: u64,
    tier: &str,
    policy: ModelDownloadPolicy,
    emit: &mut F,
) -> Result<(), String>
where
    F: FnMut(ModelDownloadProgress),
{
    let mut attempt: u32 = 0;
    let mut stalled: u32 = 0;
    let mut restarts: u32 = 0;
    let mut restart = false;
    loop {
        attempt = attempt.saturating_add(1);
        let offset = prepare_model_partial(partial, total, restart).await?;
        restart = false;
        if offset == total {
            return Ok(());
        }
        emit(ModelDownloadProgress {
            resumed_from_bytes: offset,
            attempt,
            ..model_progress(tier, "connecting", offset, total)
        });
        let mut rewound_to = None;
        let failure = match model_download_attempt(
            client,
            url,
            partial,
            offset,
            total,
            tier,
            attempt,
            policy.idle_timeout,
            &mut rewound_to,
            emit,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(failure) => failure,
        };
        let saved = model_partial_len(partial).await;
        let reason = match failure {
            ModelAttemptFailure::Fatal(reason) => {
                return Err(format!(
                    "Model download stopped: {reason}. {} of {} is saved and the next download resumes from there.",
                    format_gb(saved),
                    format_gb(total)
                ));
            }
            ModelAttemptFailure::Restart(reason) => {
                restart = true;
                reason
            }
            ModelAttemptFailure::Transient(reason) => reason,
        };
        let reason = match rewound_to {
            Some(start) => format!(
                "the model mirror restarted from byte {start} instead of resuming at byte {offset} ({reason})"
            ),
            None => reason,
        };
        if restart || rewound_to.is_some() {
            restarts = restarts.saturating_add(1);
            if restarts > MODEL_DOWNLOAD_MAX_RESTARTS {
                return Err(format!(
                    "Model download stopped: {reason}, {restarts} times. The mirror is not serving the pinned file consistently; try again later."
                ));
            }
        }
        stalled = if saved > offset && !restart && rewound_to.is_none() {
            1
        } else {
            stalled.saturating_add(1)
        };
        if stalled >= policy.max_stalled_attempts {
            return Err(format!(
                "Model download stopped after {stalled} attempts without progress: {reason}. {} of {} is saved; retry to resume from there.",
                format_gb(saved),
                format_gb(total)
            ));
        }
        let delay = policy.retry_delay(stalled);
        emit(ModelDownloadProgress {
            resumed_from_bytes: offset,
            attempt,
            retry_in_secs: Some(delay.as_secs().max(1)),
            message: Some(reason),
            ..model_progress(tier, "retrying", saved, total)
        });
        tokio::time::sleep(delay).await;
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DiskShortfall {
    needed: u64,
    available: u64,
    dir: PathBuf,
}

async fn model_disk_shortfall(target: &Path, spec: &ModelTierSpec) -> Option<DiskShortfall> {
    let parent = target.parent()?;
    let saved = model_partial_len(&model_partial_path(target, spec)).await;
    let available = fs2::available_space(parent).ok()?;
    disk_shortfall(spec.size_bytes, saved, available).map(|needed| DiskShortfall {
        needed,
        available,
        dir: parent.to_path_buf(),
    })
}

fn disk_shortfall(size_bytes: u64, saved_bytes: u64, available: u64) -> Option<u64> {
    let needed = size_bytes
        .saturating_sub(saved_bytes)
        .saturating_add(MODEL_DOWNLOAD_DISK_MARGIN_BYTES);
    (available < needed).then_some(needed)
}

async fn background_download_space_needed(tier: &str) -> Option<u64> {
    let spec = tier_spec(tier)?;
    let target = model_path_for(tier);
    let complete = tokio::fs::metadata(&target)
        .await
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == spec.size_bytes);
    if complete {
        None
    } else {
        model_disk_shortfall(&target, spec)
            .await
            .map(|space| space.needed)
    }
}

pub(crate) struct BackgroundPromotionEnv<'a> {
    pub app: &'a AppHandle,
    pub state: &'a AppState,
}

impl crate::background_promotion::TickEnv for BackgroundPromotionEnv<'_> {
    async fn facts(&self) -> crate::background_promotion::PromotionFacts {
        let config = current_config(self.state).await;
        let ram_gb = tokio::task::spawn_blocking(|| cached_hardware().ram_gb)
            .await
            .unwrap_or(0);
        let need = promotion_need(&config, ram_gb);
        let tier = tier_for_ram_gb(ram_gb);
        let model_unavailable = need == PromotionNeed::Ready
            && crate::node_manager::configured_model_unavailable(&config).is_some();
        let running_as_observer = self.state.node.lock().await.running_as_observer();
        let wants_download = need == PromotionNeed::Promote || model_unavailable;
        let cached_model_ready = if wants_download {
            cached_model_verified_for_tier(tier).await
        } else {
            false
        };
        crate::background_promotion::PromotionFacts {
            need,
            model_unavailable,
            model_fits: tier != "none",
            running_as_observer,
            cached_model_ready,
            disk_space_needed: if wants_download && !cached_model_ready {
                background_download_space_needed(tier).await
            } else {
                None
            },
        }
    }

    async fn promote_unless_busy(&self) -> Option<Result<(), String>> {
        PROMOTION_FLIGHT
            .run_unless_busy(|| promote_consented_install_once(self.app, self.state))
            .await
            .map(|result| result.map(|_| ()))
    }
}

async fn verify_model_file_blocking(
    path: &Path,
    spec: &'static ModelTierSpec,
) -> Result<bool, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || verify_model_file(&path, spec))
        .await
        .map_err(map_err)?
}

async fn download_verified_model<F>(
    client: &reqwest::Client,
    spec: &'static ModelTierSpec,
    target: &Path,
    policy: ModelDownloadPolicy,
    mut emit: F,
) -> Result<String, String>
where
    F: FnMut(ModelDownloadProgress),
{
    let tier = spec.id;
    if verify_model_file_blocking(target, spec).await? {
        emit(model_progress(
            tier,
            "done",
            spec.size_bytes,
            spec.size_bytes,
        ));
        return Ok(target.to_string_lossy().into_owned());
    }

    let partial = model_partial_path(target, spec);
    cleanup_model_partials(target, Some(partial.as_path()))?;
    if let Some(shortfall) = model_disk_shortfall(target, spec).await {
        return Err(format!(
            "Not enough free disk space for the model: {} is needed and {} is free in {}. Free some space and try again; anything already downloaded is kept.",
            format_gb(shortfall.needed),
            format_gb(shortfall.available),
            shortfall.dir.display()
        ));
    }

    download_model_resumable(
        client,
        spec.url,
        &partial,
        spec.size_bytes,
        tier,
        policy,
        &mut emit,
    )
    .await?;

    emit(model_progress(
        tier,
        "verifying",
        spec.size_bytes,
        spec.size_bytes,
    ));
    if !verify_model_file_blocking(&partial, spec).await? {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "The downloaded model did not match its pinned SHA-256 ({}), so it was deleted. Try the download again; if this repeats, the mirror is serving different bytes.",
            &spec.sha256[..spec.sha256.len().min(12)]
        ));
    }

    std::fs::rename(&partial, target)
        .map_err(|e| format!("rename to {}: {}", target.display(), e))?;
    sync_parent_best_effort(target);

    emit(model_progress(
        tier,
        "done",
        spec.size_bytes,
        spec.size_bytes,
    ));
    Ok(target.to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn download_model(app: AppHandle, tier: String) -> CmdResult<String> {
    let spec = tier_spec(&tier).ok_or_else(|| format!("unknown model tier: {}", tier))?;
    let target = model_path_for(&tier);

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(map_err)?;
    }
    let _download_guard = acquire_model_download_lock(&target).await?;
    cleanup_model_download_sidecars(&target)?;

    let client = reqwest::Client::builder()
        .connect_timeout(MODEL_DOWNLOAD_CONNECT_TIMEOUT)
        .build()
        .map_err(map_err)?;
    let emitter = app.clone();
    download_verified_model(
        &client,
        spec,
        &target,
        ModelDownloadPolicy::PRODUCTION,
        move |progress| {
            let _ = emitter.emit("model-download-progress", progress);
        },
    )
    .await
}

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
    let tmp = p.with_extension("download");
    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }
    cleanup_model_download_sidecars(&p)?;
    cleanup_model_partials(&p, None)?;
    Ok(())
}

#[cfg(test)]
mod model_readiness_tests {
    use super::*;

    #[test]
    fn a_model_file_is_ready_only_if_every_byte_matches_the_pinned_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.gguf");
        let bytes = b"FIXTURE gguf bytes, not a model".to_vec();
        std::fs::write(&path, &bytes).unwrap();
        let digest: &'static str = Box::leak(hex::encode(Sha256::digest(&bytes)).into_boxed_str());
        let spec = ModelTierSpec {
            id: "fixture",
            display_name: "Fixture",
            url: "https://example.invalid/model.gguf",
            size_bytes: bytes.len() as u64,
            sha256: digest,
        };
        assert_eq!(verify_model_file(&path, &spec), Ok(true));
        let mut flipped = bytes.clone();
        flipped[3] ^= 1;
        std::fs::write(&path, &flipped).unwrap();
        assert_eq!(verify_model_file(&path, &spec), Ok(false));
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(verify_model_file(&path, &spec), Ok(false));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(verify_model_file(&path, &spec), Ok(false));
    }

    #[test]
    fn only_a_machine_with_room_for_the_canonical_model_is_offered_it() {
        assert_eq!(tier_for_ram_gb(8), "none");
        assert_eq!(tier_for_ram_gb(15), "none");
        assert_eq!(tier_for_ram_gb(16), "standard");
        assert_eq!(tier_for_ram_gb(64), "standard");
    }

    #[test]
    fn sixteen_gb_windows_and_linux_readings_are_offered_the_model() {
        const GIB: u64 = 1024 * 1024 * 1024;
        for visible in [16 * GIB, 159 * GIB / 10, 152 * GIB / 10, 139 * GIB / 10] {
            assert_eq!(
                tier_for_ram_gb(hardware::nominal_ram_gb(visible)),
                "standard",
                "{visible} visible bytes"
            );
        }
        for visible in [77 * GIB / 10, 116 * GIB / 10] {
            assert_eq!(
                tier_for_ram_gb(hardware::nominal_ram_gb(visible)),
                "none",
                "{visible} visible bytes"
            );
        }
    }
}

#[cfg(test)]
mod model_download_tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FAST_RETRY: ModelDownloadPolicy = ModelDownloadPolicy {
        max_stalled_attempts: 3,
        retry_base: std::time::Duration::from_millis(5),
        retry_max: std::time::Duration::from_millis(20),
        idle_timeout: std::time::Duration::from_secs(10),
    };

    fn fixture_payload() -> Vec<u8> {
        (0..256 * 1024u32)
            .map(|index| (index.wrapping_mul(31).wrapping_add(7) % 251) as u8)
            .collect()
    }

    fn leaked_spec(url: String, genuine: &[u8]) -> &'static ModelTierSpec {
        Box::leak(Box::new(ModelTierSpec {
            id: "standard",
            display_name: "Fixture",
            url: Box::leak(url.into_boxed_str()),
            size_bytes: genuine.len() as u64,
            sha256: Box::leak(hex::encode(Sha256::digest(genuine)).into_boxed_str()),
        }))
    }

    #[derive(Clone, Copy)]
    enum Reply {
        FullThenDrop {
            send: usize,
        },
        Full,
        Partial,
        NotFound,
        Unavailable,
    }

    async fn read_head(stream: &mut tokio::net::TcpStream) -> String {
        let mut head = Vec::new();
        let mut buffer = [0u8; 1024];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.expect("read request");
            if read == 0 {
                break;
            }
            head.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    fn range_start(head: &str) -> Option<u64> {
        head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.trim().eq_ignore_ascii_case("range") {
                return None;
            }
            value
                .trim()
                .strip_prefix("bytes=")?
                .strip_suffix('-')?
                .parse()
                .ok()
        })
    }

    async fn spawn_mirror(
        payload: Vec<u8>,
        replies: Vec<Reply>,
    ) -> (
        String,
        Arc<StdMutex<Vec<Option<u64>>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/llama-2-7b-chat.Q4_K_M.gguf",
            listener.local_addr().unwrap()
        );
        let ranges = Arc::new(StdMutex::new(Vec::new()));
        let seen = ranges.clone();
        let server = tokio::spawn(async move {
            let total = payload.len();
            for reply in replies {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let head = read_head(&mut stream).await;
                let start = range_start(&head);
                seen.lock().unwrap().push(start);
                match reply {
                    Reply::FullThenDrop { send } => {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                        );
                        stream.write_all(header.as_bytes()).await.unwrap();
                        stream.write_all(&payload[..send]).await.unwrap();
                        stream.flush().await.unwrap();
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Reply::Full => {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                        );
                        stream.write_all(header.as_bytes()).await.unwrap();
                        stream.write_all(&payload).await.unwrap();
                    }
                    Reply::Partial => {
                        let from = start.unwrap_or(0) as usize;
                        let header = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{total}\r\nConnection: close\r\n\r\n",
                            total - from,
                            total - 1
                        );
                        stream.write_all(header.as_bytes()).await.unwrap();
                        stream.write_all(&payload[from..]).await.unwrap();
                    }
                    Reply::NotFound => {
                        stream
                            .write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await
                            .unwrap();
                    }
                    Reply::Unavailable => {
                        stream
                            .write_all(
                                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await
                            .unwrap();
                    }
                }
                let _ = stream.shutdown().await;
            }
        });
        (url, ranges, server)
    }

    #[test]
    fn content_ranges_and_resume_plans_follow_the_http_range_rules() {
        assert_eq!(
            parse_content_range("bytes 100-199/200"),
            Some((100, 199, Some(200)))
        );
        assert_eq!(parse_content_range("bytes 0-0/*"), Some((0, 0, None)));
        assert_eq!(parse_content_range("bytes */200"), None);
        assert_eq!(parse_content_range("items 1-2/3"), None);
        assert_eq!(parse_content_range("bytes 9-1/10"), None);

        let plan = |status, range: Option<&str>, length, offset| {
            plan_model_range_response(status, range, length, offset, 200)
        };
        assert_eq!(
            plan(206, Some("bytes 100-199/200"), Some(100), 100).unwrap(),
            ModelRangePlan::AppendFrom(100)
        );
        assert_eq!(
            plan(200, None, Some(200), 100).unwrap(),
            ModelRangePlan::AppendFrom(0)
        );
        assert_eq!(
            plan(206, Some("bytes 0-199/200"), Some(200), 100).unwrap(),
            ModelRangePlan::AppendFrom(0)
        );
        assert_eq!(
            plan(416, Some("bytes */200"), None, 200).unwrap(),
            ModelRangePlan::Complete
        );
        assert!(matches!(
            plan(416, None, None, 50),
            Err(ModelAttemptFailure::Restart(_))
        ));
        assert!(matches!(
            plan(206, Some("bytes 120-199/200"), None, 100),
            Err(ModelAttemptFailure::Restart(_))
        ));
        assert!(matches!(
            plan(206, None, None, 100),
            Err(ModelAttemptFailure::Restart(_))
        ));
        assert!(matches!(
            plan(206, Some("bytes 100-299/300"), None, 100),
            Err(ModelAttemptFailure::Fatal(_))
        ));
        assert!(matches!(
            plan(200, None, Some(300), 0),
            Err(ModelAttemptFailure::Fatal(_))
        ));
        for status in [408, 429, 500, 502, 503] {
            assert!(matches!(
                plan(status, None, None, 0),
                Err(ModelAttemptFailure::Transient(_))
            ));
        }
        for status in [401, 403, 404, 410] {
            assert!(matches!(
                plan(status, None, None, 0),
                Err(ModelAttemptFailure::Fatal(_))
            ));
        }
    }

    #[test]
    fn retries_back_off_exponentially_up_to_a_cap() {
        let policy = ModelDownloadPolicy::PRODUCTION;
        let secs = |stalled| policy.retry_delay(stalled).as_secs();
        assert_eq!(
            [secs(1), secs(2), secs(3), secs(5), secs(6), secs(40)],
            [2, 4, 8, 32, 60, 60]
        );
    }

    #[test]
    fn partials_are_pinned_to_the_digest_and_survive_legacy_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("standard.gguf");
        let spec = tier_spec("standard").unwrap();
        let partial = model_partial_path(&target, spec);
        assert_eq!(
            partial.file_name().unwrap(),
            "standard.08a5566d61d7cb6b.partial"
        );
        let other_pin = target.with_extension("0000000000000000.partial");
        let legacy = model_download_sidecar(&target, 7);
        for path in [&partial, &other_pin, &legacy] {
            std::fs::write(path, b"saved bytes").unwrap();
        }

        cleanup_model_download_sidecars(&target).unwrap();
        assert!(!legacy.exists(), "unresumable legacy sidecars are removed");
        assert!(partial.exists() && other_pin.exists());

        cleanup_model_partials(&target, Some(partial.as_path())).unwrap();
        assert!(partial.exists(), "the current pin's partial is resumed");
        assert!(!other_pin.exists(), "another pin's bytes are never resumed");

        cleanup_model_partials(&target, None).unwrap();
        assert!(!partial.exists(), "removing the model removes its partial");
    }

    #[tokio::test]
    async fn a_dropped_connection_resumes_from_the_saved_bytes_and_verifies() {
        let payload = fixture_payload();
        let cut = 96 * 1024;
        let (url, ranges, server) = spawn_mirror(
            payload.clone(),
            vec![Reply::FullThenDrop { send: cut }, Reply::Partial],
        )
        .await;
        let spec = leaked_spec(url, &payload);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("standard.gguf");

        let mut events = Vec::new();
        let installed = download_verified_model(
            &reqwest::Client::new(),
            spec,
            &target,
            FAST_RETRY,
            |event| events.push(event),
        )
        .await
        .expect("the interrupted download resumes and completes");
        server.await.unwrap();

        assert_eq!(PathBuf::from(installed), target);
        assert_eq!(std::fs::read(&target).unwrap(), payload);
        assert!(!model_partial_path(&target, spec).exists());
        let ranges = ranges.lock().unwrap().clone();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], None, "a fresh download sends no Range header");
        let resumed = ranges[1].expect("the retry resumes with a Range request");
        assert!(resumed > 0 && resumed <= cut as u64, "resumed at {resumed}");
        assert!(events.iter().any(|event| event.stage == "retrying"
            && event.retry_in_secs.is_some()
            && event.message.is_some()));
        assert!(events
            .iter()
            .any(|event| event.stage == "connecting" && event.resumed_from_bytes == resumed));
        assert!(events.iter().any(|event| event.stage == "verifying"));
        let last = events.last().unwrap();
        assert!(last.done && last.stage == "done");
        assert_eq!(last.downloaded_bytes, payload.len() as u64);
    }

    #[tokio::test]
    async fn bytes_saved_by_an_earlier_app_run_are_continued_not_refetched() {
        let payload = fixture_payload();
        let saved = 100_000;
        let (url, ranges, server) = spawn_mirror(payload.clone(), vec![Reply::Partial]).await;
        let spec = leaked_spec(url, &payload);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("standard.gguf");
        std::fs::write(model_partial_path(&target, spec), &payload[..saved]).unwrap();

        download_verified_model(&reqwest::Client::new(), spec, &target, FAST_RETRY, |_| {})
            .await
            .expect("a saved partial resumes after a restart");
        server.await.unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), payload);
        assert_eq!(*ranges.lock().unwrap(), vec![Some(saved as u64)]);
    }

    #[tokio::test]
    async fn a_mirror_that_ignores_range_restarts_from_byte_zero() {
        let payload = fixture_payload();
        let (url, ranges, server) = spawn_mirror(payload.clone(), vec![Reply::Full]).await;
        let spec = leaked_spec(url, &payload);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("standard.gguf");
        std::fs::write(model_partial_path(&target, spec), &payload[..50_000]).unwrap();

        download_verified_model(&reqwest::Client::new(), spec, &target, FAST_RETRY, |_| {})
            .await
            .expect("a full 200 response replaces the saved bytes");
        server.await.unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), payload);
        assert_eq!(*ranges.lock().unwrap(), vec![Some(50_000)]);
    }

    #[tokio::test]
    async fn bytes_that_fail_the_pinned_digest_are_deleted_never_installed() {
        let payload = fixture_payload();
        let mut tampered = payload.clone();
        tampered[1234] ^= 0xff;
        let (url, _ranges, server) = spawn_mirror(tampered, vec![Reply::Full]).await;
        let spec = leaked_spec(url, &payload);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("standard.gguf");

        let error =
            download_verified_model(&reqwest::Client::new(), spec, &target, FAST_RETRY, |_| {})
                .await
                .unwrap_err();
        server.await.unwrap();

        assert!(error.contains("SHA-256"), "{error}");
        assert!(!target.exists());
        assert!(
            !model_partial_path(&target, spec).exists(),
            "bytes that failed verification must not be resumed later"
        );
    }

    #[tokio::test]
    async fn a_missing_file_fails_once_with_its_http_status() {
        let payload = fixture_payload();
        let (url, ranges, server) = spawn_mirror(payload.clone(), vec![Reply::NotFound]).await;
        let spec = leaked_spec(url, &payload);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("standard.gguf");

        let error =
            download_verified_model(&reqwest::Client::new(), spec, &target, FAST_RETRY, |_| {})
                .await
                .unwrap_err();
        server.await.unwrap();

        assert!(error.contains("HTTP 404"), "{error}");
        assert_eq!(
            ranges.lock().unwrap().len(),
            1,
            "permanent errors are not retried"
        );
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn repeated_outages_stop_with_the_saved_bytes_kept_for_next_time() {
        let payload = fixture_payload();
        let saved = 10_000;
        let (url, ranges, server) =
            spawn_mirror(payload.clone(), vec![Reply::Unavailable; 3]).await;
        let spec = leaked_spec(url, &payload);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("standard.gguf");
        let partial = model_partial_path(&target, spec);
        std::fs::write(&partial, &payload[..saved]).unwrap();

        let error =
            download_verified_model(&reqwest::Client::new(), spec, &target, FAST_RETRY, |_| {})
                .await
                .unwrap_err();
        server.await.unwrap();

        assert!(error.contains("3 attempts without progress"), "{error}");
        assert!(error.contains("HTTP 503"), "{error}");
        assert_eq!(
            *ranges.lock().unwrap(),
            vec![Some(saved as u64); 3],
            "every retry resumes from the saved bytes"
        );
        assert_eq!(std::fs::read(&partial).unwrap(), &payload[..saved]);
    }

    #[tokio::test]
    async fn a_mirror_that_ignores_range_and_keeps_dropping_stops_within_the_restart_budget() {
        let payload = fixture_payload();
        let saved = 100usize;
        let drops = (1..=10)
            .map(|extra| Reply::FullThenDrop {
                send: saved + extra,
            })
            .collect();
        let (url, ranges, server) = spawn_mirror(payload.clone(), drops).await;
        let spec = leaked_spec(url, &payload);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("standard.gguf");
        let partial = model_partial_path(&target, spec);
        std::fs::write(&partial, &payload[..saved]).unwrap();
        let policy = ModelDownloadPolicy {
            max_stalled_attempts: MODEL_DOWNLOAD_MAX_STALLED_ATTEMPTS,
            ..FAST_RETRY
        };

        let error = download_verified_model(&reqwest::Client::new(), spec, &target, policy, |_| {})
            .await
            .unwrap_err();
        server.abort();

        assert!(error.contains("restarted from byte 0"), "{error}");
        assert!(error.contains("3 times"), "{error}");
        let ranges = ranges.lock().unwrap().clone();
        assert_eq!(
            ranges.len(),
            3,
            "two rewinds are tolerated and the third stops the download: {ranges:?}"
        );
        assert_eq!(ranges[0], Some(saved as u64));
        assert!(
            ranges.windows(2).all(|pair| pair[0] <= pair[1]),
            "every attempt resumed from the saved bytes: {ranges:?}"
        );
        let kept = std::fs::read(&partial).unwrap();
        assert!(
            kept.len() >= saved && kept.len() <= saved + 3,
            "kept {}",
            kept.len()
        );
        assert_eq!(kept, &payload[..kept.len()]);
        assert!(!target.exists());
    }
}

#[cfg(test)]
mod release_binary_tests {
    use super::*;

    #[test]
    fn startup_retry_classifies_only_transient_release_statuses() {
        for status in [
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::BAD_GATEWAY,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(
                classify_release_status("release asset", status, EXPECTED_NODE_VERSION)
                    .is_transient()
            );
        }
        for status in [
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::BAD_REQUEST,
        ] {
            assert!(
                !classify_release_status("release asset", status, EXPECTED_NODE_VERSION)
                    .is_transient()
            );
        }
    }

    #[tokio::test]
    async fn startup_retry_classifies_connectivity_errors_as_transient() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = reqwest::Client::new()
            .get(format!("http://{address}/release"))
            .send()
            .await
            .expect_err("the just-released local port must refuse the connection");
        assert!(classify_release_transport("release asset", error).is_transient());
    }

    #[test]
    fn integrity_and_identity_failures_are_terminal() {
        let error = StartupFailure::from("checksum verification failed");
        assert!(!error.is_transient());
        let error = StartupFailure::from("managed identity bytes changed");
        assert!(!error.is_transient());
    }

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
        assert!(error.contains(concat!(
            "v",
            env!("CARGO_PKG_VERSION"),
            " recovery candidate"
        )));
        assert!(error.contains("before any transaction is signed or submitted"));
        assert!(error.contains("exact model-artifact binding"));
        assert!(error.contains("validator-authenticated authorization"));
        assert!(error.contains("Free/community inference remains available"));
    }

    #[test]
    fn unresolved_data_migration_blocks_manual_start_and_restart() {
        use crate::start_fence::{check, StartFence};
        assert!(check(None, StartAccess::Ordinary).is_ok());
        let fence = StartFence::MigrationRefused("legacy data directory is a symbolic link".into());
        for access in [StartAccess::Ordinary, StartAccess::StartupRecovery] {
            let error = check(Some(&fence), access)
                .expect_err("an unresolved native migration fence must block every start path");
            assert!(error.contains("refused to start"));
            assert!(error.contains("migration is not safely resolved"));
            assert!(error.contains("preserved legacy directory"));
        }
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
            "pub async fn tier1_result(",
        );
        let paid = source_between(
            source,
            "pub async fn run_paid_inference(",
            "pub async fn save_logs(",
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

        let long_prompt = "x".repeat(255);
        let long_positions = 1 + inference_prompt_token_upper_bound(&long_prompt) + 1;
        assert_eq!(long_positions, 260);
        assert_eq!(inference_timeout(&long_prompt, 1).as_secs(), 3_951);
        assert_eq!(
            inference_timeout(&long_prompt, 1).as_secs(),
            admitted_coordinator_budget(long_positions).unwrap() + 60
        );

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
                "0.8.0",
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
        assert!(semver_gt("0.8.10", "0.8.9"));
        assert!(!semver_gt("0.8.9", "0.8.10"));
        assert!(!semver_gt("0.8.10", "0.8.10"));
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

#[cfg(test)]
mod compute_contribution_tests {
    use super::*;

    #[test]
    fn restored_missing_model_reconciles_running_observer_but_preserves_stopped_opt_out_and_floor()
    {
        let dir = tempfile::tempdir().unwrap();
        let model_path = dir.path().join("standard.gguf");
        let model_str = model_path.to_string_lossy().into_owned();
        let saved = config("worker", Some(&model_str), Some(true));

        assert_eq!(
            compute_reconcile_action(&saved, 16, true),
            ComputeReconcileAction::PromoteModel
        );

        std::fs::write(&model_path, b"model").unwrap();
        assert_eq!(
            compute_reconcile_action(&saved, 16, true),
            ComputeReconcileAction::RestartRunningObserver
        );
        assert_eq!(
            compute_reconcile_action(&saved, 16, false),
            ComputeReconcileAction::None
        );

        let opted_out = config("worker", Some(&model_str), Some(false));
        assert_eq!(
            compute_reconcile_action(&opted_out, 16, true),
            ComputeReconcileAction::None
        );

        assert_eq!(
            compute_reconcile_action(&saved, 8, true),
            ComputeReconcileAction::None
        );
    }

    #[test]
    fn startup_promotion_gate_allows_cached_model_but_defers_uncached_download() {
        struct StubConditions {
            power: crate::background_promotion::PowerSource,
            metered: Option<bool>,
        }
        impl crate::background_promotion::DownloadConditions for StubConditions {
            fn power_source(&self) -> crate::background_promotion::PowerSource {
                self.power
            }

            fn network_metered(&self) -> Option<bool> {
                self.metered
            }
        }

        let mut default_enabled = config("observer", None, Some(true));
        default_enabled.compute_default_notice_pending = true;
        let battery = StubConditions {
            power: crate::background_promotion::PowerSource::Battery,
            metered: Some(false),
        };
        let metered = StubConditions {
            power: crate::background_promotion::PowerSource::Mains,
            metered: Some(true),
        };
        let mains = StubConditions {
            power: crate::background_promotion::PowerSource::Mains,
            metered: Some(false),
        };

        assert_eq!(
            startup_promotion_skip_reason(&default_enabled, &battery, None, false),
            Some(crate::background_promotion::SkipReason::OnBattery)
        );
        assert_eq!(
            startup_promotion_skip_reason(&default_enabled, &battery, None, true),
            None
        );
        assert_eq!(
            startup_promotion_skip_reason(&default_enabled, &metered, None, false),
            Some(crate::background_promotion::SkipReason::MeteredNetwork)
        );
        assert_eq!(
            startup_promotion_skip_reason(&default_enabled, &metered, None, true),
            None
        );
        assert_eq!(
            startup_promotion_skip_reason(&default_enabled, &mains, Some(4096), false),
            Some(crate::background_promotion::SkipReason::DiskFull { needed_bytes: 4096 })
        );
        assert_eq!(
            startup_promotion_skip_reason(&default_enabled, &mains, Some(4096), true),
            None
        );
    }

    fn config(role: &str, model: Option<&str>, consent: Option<bool>) -> NodeConfig {
        NodeConfig {
            role: role.into(),
            model_path: model.map(str::to_string),
            compute_consent: consent,
            ..NodeConfig::default()
        }
    }

    #[test]
    fn nothing_is_downloaded_or_promoted_without_consent() {
        assert_eq!(
            promotion_need(&config("observer", None, None), 16),
            PromotionNeed::NoConsent
        );
        assert_eq!(
            promotion_need(&config("observer", Some("/m.gguf"), Some(false)), 64),
            PromotionNeed::NoConsent
        );
        assert!(!compute_contribution_enabled(&NodeConfig::default()));
        assert_eq!(
            promotion_need(&NodeConfig::default(), 64),
            PromotionNeed::NoConsent
        );
    }

    #[test]
    fn consented_observers_are_promoted_only_when_eligible() {
        assert_eq!(
            promotion_need(&config("observer", None, Some(true)), 16),
            PromotionNeed::Promote
        );
        assert_eq!(
            promotion_need(&config("observer", None, Some(true)), 12),
            PromotionNeed::Ineligible
        );
        assert_eq!(
            promotion_need(&config("worker", Some("/m.gguf"), Some(true)), 16),
            PromotionNeed::Ready
        );
    }

    #[test]
    fn a_worker_whose_model_file_is_gone_needs_promotion_again() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("standard.gguf");
        std::fs::write(&present, b"model").unwrap();
        let present = present.to_string_lossy().into_owned();
        let missing = dir.path().join("gone.gguf").to_string_lossy().into_owned();

        let ready = config("worker", Some(&present), Some(true));
        assert!(
            !needs_promotion(&ready, 16),
            "a readable model needs nothing"
        );
        let repair = config("worker", Some(&missing), Some(true));
        assert_eq!(promotion_need(&repair, 16), PromotionNeed::Ready);
        assert!(
            needs_promotion(&repair, 16),
            "a missing model is downloaded again"
        );
        assert!(needs_promotion(&config("observer", None, Some(true)), 16));
        assert!(!needs_promotion(
            &config("worker", Some(&missing), Some(false)),
            16
        ));
        assert!(!needs_promotion(&config("observer", None, Some(true)), 12));
    }

    #[test]
    fn the_disk_rule_counts_saved_bytes_and_the_margin() {
        let size = 4_081_004_224;
        let margin = MODEL_DOWNLOAD_DISK_MARGIN_BYTES;
        assert_eq!(disk_shortfall(size, 0, size + margin), None);
        assert_eq!(
            disk_shortfall(size, 0, size + margin - 1),
            Some(size + margin)
        );
        assert_eq!(disk_shortfall(size, size - 1, margin + 1), None);
        assert_eq!(disk_shortfall(size, size - 1, margin), Some(margin + 1));
    }

    #[test]
    fn workers_from_before_the_question_keep_contributing() {
        let legacy = config("worker", Some("/m.gguf"), None);
        assert!(compute_contribution_enabled(&legacy));
        assert_eq!(promotion_need(&legacy, 16), PromotionNeed::Ready);
    }

    #[test]
    fn switching_contribution_off_stops_loading_the_model() {
        let mut worker = config("worker", Some("/m.gguf"), Some(true));
        withdraw_compute_consent(&mut worker);
        assert_eq!(worker.compute_consent, Some(false));
        assert_eq!(worker.role, "observer");
        assert_eq!(worker.model_path, None);
        assert_eq!(promotion_need(&worker, 64), PromotionNeed::NoConsent);

        worker.compute_consent = Some(true);
        assert_eq!(promotion_need(&worker, 16), PromotionNeed::Promote);
    }

    #[test]
    fn a_save_that_omits_the_choices_keeps_the_stored_answers() {
        let persisted = NodeConfig {
            compute_consent: Some(true),
            prevent_sleep_during_jobs: Some(true),
            ..NodeConfig::default()
        };
        let mut stale_webview = NodeConfig {
            rpc_port: 10_001,
            ..NodeConfig::default()
        };
        preserve_unsent_contribution_choices(&mut stale_webview, Some(&persisted));
        assert_eq!(stale_webview.compute_consent, Some(true));
        assert_eq!(stale_webview.prevent_sleep_during_jobs, Some(true));

        let mut explicit = NodeConfig {
            compute_consent: Some(false),
            prevent_sleep_during_jobs: Some(false),
            ..NodeConfig::default()
        };
        preserve_unsent_contribution_choices(&mut explicit, Some(&persisted));
        assert_eq!(explicit.compute_consent, Some(false));
        assert_eq!(explicit.prevent_sleep_during_jobs, Some(false));
    }

    #[test]
    fn a_store_written_before_the_question_still_loads() {
        let config: NodeConfig = serde_json::from_value(serde_json::json!({
            "role": "worker",
            "modelPath": "/m.gguf",
            "rpcPort": 9090,
            "p2pPort": 9091,
            "autoStart": true,
            "autoUpdate": true,
            "dataDir": "~/.arc/data-v3"
        }))
        .unwrap();
        assert_eq!(config.compute_consent, None);
        assert_eq!(config.prevent_sleep_during_jobs, None);
        assert!(compute_contribution_enabled(&config));
    }

    fn launches_as_worker(config: &NodeConfig) -> bool {
        config.role == "worker" && config.model_path.is_some()
    }

    #[test]
    fn a_settings_save_cannot_restore_consent_the_user_withdrew() {
        let mut store = Store::default();
        apply_webview_config_save(
            &mut store,
            config("worker", Some("/m.gguf"), Some(true)),
            ConfigSave::Onboarding,
        );

        let withdrawn = update_stored_config_in(&mut store, withdraw_compute_consent);
        assert_eq!(withdrawn.compute_consent, Some(false));

        let stale = NodeConfig {
            rpc_port: 10_001,
            ..config("worker", Some("/m.gguf"), Some(true))
        };
        apply_webview_config_save(&mut store, stale, ConfigSave::Preferences);

        let dir = tempfile::tempdir().unwrap();
        store.save_to(dir.path()).unwrap();
        let persisted = Store::load_from(dir.path()).config.unwrap();
        assert_eq!(persisted.rpc_port, 10_001);
        assert_eq!(persisted.compute_consent, Some(false));
        assert_eq!(persisted.role, "observer");
        assert_eq!(persisted.model_path, None);
        assert!(!compute_contribution_enabled(&persisted));
        assert_eq!(promotion_need(&persisted, 64), PromotionNeed::NoConsent);
        assert!(!launches_as_worker(&persisted));
    }

    #[test]
    fn a_settings_save_cannot_revoke_consent_the_user_just_gave() {
        let mut store = Store::default();
        apply_webview_config_save(
            &mut store,
            config("observer", None, Some(false)),
            ConfigSave::Onboarding,
        );
        update_stored_config_in(&mut store, |config| config.compute_consent = Some(true));
        update_stored_config_in(&mut store, |config| {
            config.role = "worker".into();
            config.model_path = Some("/m.gguf".into());
        });
        update_stored_config_in(&mut store, |config| {
            config.prevent_sleep_during_jobs = Some(true)
        });

        let stale = NodeConfig {
            auto_start: false,
            ..config("observer", None, Some(false))
        };
        apply_webview_config_save(&mut store, stale, ConfigSave::Preferences);

        let persisted = store.config.unwrap();
        assert!(!persisted.auto_start);
        assert_eq!(persisted.compute_consent, Some(true));
        assert_eq!(persisted.role, "worker");
        assert_eq!(persisted.model_path.as_deref(), Some("/m.gguf"));
        assert_eq!(persisted.prevent_sleep_during_jobs, Some(true));
        assert!(launches_as_worker(&persisted));
    }

    #[test]
    fn onboarding_records_its_choice_and_a_first_save_is_kept() {
        let mut store = Store::default();
        apply_webview_config_save(
            &mut store,
            config("worker", Some("/m.gguf"), Some(true)),
            ConfigSave::Preferences,
        );
        let first = store.config.clone().unwrap();
        assert_eq!(first.compute_consent, Some(true));
        assert!(launches_as_worker(&first));

        update_stored_config_in(&mut store, |config| {
            config.prevent_sleep_during_jobs = Some(true)
        });
        apply_webview_config_save(
            &mut store,
            config("observer", None, Some(false)),
            ConfigSave::Onboarding,
        );
        let persisted = store.config.unwrap();
        assert_eq!(persisted.compute_consent, Some(false));
        assert_eq!(persisted.role, "observer");
        assert_eq!(persisted.model_path, None);
        assert_eq!(persisted.prevent_sleep_during_jobs, Some(true));
        assert!(!launches_as_worker(&persisted));
    }

    #[test]
    fn a_rerun_wizard_keeps_the_unattended_install_answer() {
        let mut store = Store::default();
        apply_webview_config_save(
            &mut store,
            config("observer", None, Some(false)),
            ConfigSave::Onboarding,
        );
        assert!(store.config.as_ref().unwrap().auto_install_updates);

        let off = NodeConfig {
            auto_install_updates: false,
            ..config("observer", None, Some(false))
        };
        apply_webview_config_save(&mut store, off, ConfigSave::Preferences);
        assert!(!store.config.as_ref().unwrap().auto_install_updates);

        apply_webview_config_save(
            &mut store,
            config("worker", Some("/m.gguf"), Some(true)),
            ConfigSave::Onboarding,
        );
        assert!(!store.config.as_ref().unwrap().auto_install_updates);

        apply_webview_config_save(
            &mut store,
            config("worker", Some("/m.gguf"), Some(true)),
            ConfigSave::Preferences,
        );
        assert!(store.config.as_ref().unwrap().auto_install_updates);
    }

    #[test]
    fn a_saved_settings_answer_is_never_overridden_by_the_one_time_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::default();
        let off = NodeConfig {
            auto_install_updates: false,
            ..config("observer", None, Some(false))
        };
        apply_webview_config_save(&mut store, off, ConfigSave::Preferences);
        persist_webview_config(&store, dir.path()).unwrap();

        let mut relaunched = Store::load_from(dir.path());
        assert!(
            !crate::auto_update::apply_unattended_install_default(&mut relaunched, dir.path())
                .unwrap()
        );
        assert!(!relaunched.config.unwrap().auto_install_updates);
    }

    #[test]
    fn the_setup_core_cap_is_a_quarter_of_the_logical_cores_and_at_least_one() {
        for (logical_cores, cap) in [
            (0, 1),
            (1, 1),
            (2, 1),
            (3, 1),
            (4, 1),
            (7, 1),
            (8, 2),
            (10, 2),
            (12, 3),
            (16, 4),
            (24, 6),
            (64, 16),
            (u32::MAX, u32::MAX / 4),
        ] {
            assert_eq!(
                default_worker_threads(logical_cores),
                cap,
                "{logical_cores} logical cores"
            );
        }
    }

    #[test]
    fn only_a_setup_that_turns_compute_on_without_a_core_count_takes_the_cap() {
        assert!(takes_setup_core_default(&config("observer", None, Some(true))));
        assert!(!takes_setup_core_default(&config("observer", None, Some(false))));
        assert!(!takes_setup_core_default(&config("observer", None, None)));
        let named = NodeConfig {
            worker_threads: Some(10),
            ..config("observer", None, Some(true))
        };
        assert!(!takes_setup_core_default(&named));
    }

    fn complete_setup(store: &mut Store, submitted: NodeConfig, logical_cores: u32) {
        let mut submitted = submitted;
        apply_setup_core_default(&mut submitted, logical_cores);
        apply_webview_config_save(store, submitted, ConfigSave::Onboarding);
    }

    #[test]
    fn a_new_install_that_contributes_compute_starts_at_a_quarter_of_the_cores() {
        let mut store = Store::default();
        complete_setup(&mut store, config("observer", None, Some(true)), 24);
        let saved = store.config.clone().unwrap();
        assert_eq!(saved.compute_consent, Some(true));
        assert_eq!(saved.worker_threads, Some(6));
        assert_eq!(promotion_need(&saved, 16), PromotionNeed::Promote);

        update_stored_config_in(&mut store, |config| {
            config.role = "worker".into();
            config.model_path = Some("/m.gguf".into());
        });
        assert_eq!(store.config.unwrap().worker_threads, Some(6));

        let mut skipped = Store::default();
        complete_setup(&mut skipped, config("observer", None, Some(false)), 24);
        assert_eq!(skipped.config.unwrap().worker_threads, None);
    }

    #[test]
    fn a_rerun_setup_keeps_the_core_count_already_stored() {
        let mut store = Store::default();
        complete_setup(&mut store, config("observer", None, Some(true)), 24);
        update_stored_config_in(&mut store, |config| config.worker_threads = Some(12));

        complete_setup(&mut store, config("observer", None, Some(true)), 24);
        assert_eq!(store.config.as_ref().unwrap().worker_threads, Some(12));
        complete_setup(&mut store, config("observer", None, Some(false)), 24);
        assert_eq!(store.config.as_ref().unwrap().worker_threads, Some(12));
    }

    #[test]
    fn existing_installs_keep_their_core_count_and_consent() {
        let existing: NodeConfig = serde_json::from_value(serde_json::json!({
            "role": "observer",
            "modelPath": null,
            "rpcPort": 9090,
            "p2pPort": 9091,
            "autoStart": true,
            "autoUpdate": true,
            "dataDir": "~/.arc/data-v3",
            "computeConsent": false
        }))
        .unwrap();
        assert_eq!(existing.worker_threads, None);
        let mut store = Store {
            config: Some(existing),
            ..Store::default()
        };

        let stale = NodeConfig {
            rpc_port: 10_001,
            worker_threads: Some(3),
            ..config("observer", None, Some(true))
        };
        apply_webview_config_save(&mut store, stale, ConfigSave::Preferences);
        let persisted = store.config.clone().unwrap();
        assert_eq!(persisted.rpc_port, 10_001);
        assert_eq!(persisted.compute_consent, Some(false));
        assert_eq!(persisted.worker_threads, None);

        update_stored_config_in(&mut store, |config| config.compute_consent = Some(true));
        update_stored_config_in(&mut store, |config| {
            config.role = "worker".into();
            config.model_path = Some("/m.gguf".into());
        });
        assert_eq!(store.config.as_ref().unwrap().worker_threads, None);

        update_stored_config_in(&mut store, |config| config.worker_threads = Some(8));
        apply_webview_config_save(
            &mut store,
            config("worker", Some("/m.gguf"), Some(true)),
            ConfigSave::Preferences,
        );
        assert_eq!(store.config.unwrap().worker_threads, Some(8));
    }

    fn test_identity() -> Identity {
        Identity {
            address: "11".repeat(32),
            public_key: format!("0x{}", "22".repeat(32)),
            seed_phrase: "a recovery phrase the tests never use".to_string(),
            created_at: 1,
        }
    }

    fn existing_install(role: &str, model: Option<&str>, consent: Option<bool>) -> Store {
        Store {
            identity: Some(test_identity()),
            config: Some(config(role, model, consent)),
            ..Store::default()
        }
    }

    #[test]
    fn sharing_default_respects_memory_refusal_and_legacy_question() {
        for (ram, consent, legacy, expected) in [
            (16, None, false, Some(true)),
            (12, None, false, None),
            (64, Some(false), false, Some(false)),
            (64, None, true, None),
        ] {
            let mut store = existing_install("observer", None, consent);
            apply_compute_default(&mut store, ram, 24, legacy);
            let config = store.config.unwrap();
            assert_eq!(config.compute_consent, expected);
            assert_eq!(
                config.compute_default_notice_pending,
                expected == Some(true)
            );
            if expected == Some(true) {
                assert_eq!(config.worker_threads, Some(6));
            } else {
                assert_eq!(config.role, "observer");
            }
        }
        let mut legacy = existing_install("worker", Some("model.gguf"), None);
        legacy.config.as_mut().unwrap().worker_threads = Some(3);
        assert!(apply_compute_default(&mut legacy, 64, 24, false));
        assert!(
            !legacy
                .config
                .as_ref()
                .unwrap()
                .compute_default_notice_pending
        );
        assert_eq!(legacy.config.unwrap().worker_threads, Some(3));
    }

    #[test]
    fn existing_workers_keep_their_effective_core_count() {
        for threads in [None, Some(3)] {
            let mut store = existing_install("worker", Some("model.gguf"), None);
            store.config.as_mut().unwrap().worker_threads = threads;
            assert!(apply_compute_default(&mut store, 64, 24, false));
            let config = store.config.as_ref().unwrap();
            assert_eq!(config.worker_threads, threads);
            assert_eq!(config.compute_consent, Some(true));
            assert!(!config.compute_default_notice_pending);
            assert_eq!(promotion_need(config, 64), PromotionNeed::Ready);
            let dir = tempfile::tempdir().unwrap();
            store.save_to(dir.path()).unwrap();
            let mut reopened = Store::load_from(dir.path());
            assert!(!apply_compute_default(&mut reopened, 64, 24, false));
            assert_eq!(reopened.config.unwrap().worker_threads, threads);
        }
    }

    #[test]
    fn below_floor_keeps_stored_model_and_choices_as_observer() {
        for consent in [None, Some(false), Some(true)] {
            let mut store = existing_install("worker", Some("model.gguf"), consent);
            let mut expected = store.config.clone().unwrap();
            expected.role = "observer".into();
            assert!(apply_compute_default(&mut store, 12, 24, false));
            let dir = tempfile::tempdir().unwrap();
            store.save_to(dir.path()).unwrap();
            let reopened = Store::load_from(dir.path());
            let config = reopened.config.unwrap();
            assert_eq!(
                serde_json::to_value(&config).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            assert!(!launches_as_worker(&config));
            assert!(!needs_promotion(&config, 12));
            let launched = crate::node_manager::effective_launch_config_for_ram(&config, 12);
            assert_eq!(launched.role, "observer");
            assert_eq!(launched.model_path, None);
            assert_eq!(
                Store::load_from(dir.path()).config.unwrap().model_path,
                config.model_path
            );
        }
    }

    #[test]
    fn explicit_yes_is_not_gated_but_default_enablement_survives_notice_and_relaunch() {
        use crate::background_promotion::{DownloadConditions, PowerSource, SkipReason};
        struct Conditions(PowerSource, Option<bool>);
        impl DownloadConditions for Conditions {
            fn power_source(&self) -> PowerSource {
                self.0
            }
            fn network_metered(&self) -> Option<bool> {
                self.1
            }
        }
        struct NeverSample;
        impl DownloadConditions for NeverSample {
            fn power_source(&self) -> PowerSource {
                panic!("explicit yes must not query power")
            }
            fn network_metered(&self) -> Option<bool> {
                panic!("explicit yes must not query network")
            }
        }
        for role in ["observer", "worker"] {
            let model = (role == "worker").then_some("missing-model.gguf");
            let mut store = existing_install(role, model, Some(true));
            assert!(!apply_compute_default(&mut store, 64, 24, false));
            let config = store.config.as_ref().unwrap();
            assert!(needs_promotion(config, 64));
            assert_eq!(startup_download_gate(config, &NeverSample), Ok(()));
        }
        let mut store = existing_install("observer", None, None);
        apply_compute_default(&mut store, 64, 24, false);
        apply_webview_config_save(&mut store, NodeConfig::default(), ConfigSave::Preferences);
        let dir = tempfile::tempdir().unwrap();
        arc_crypto::secret_file::durably_publish_new_private(
            &dir.path().join(COMPUTE_NOTICE_MARKER),
            b"shown\n",
        )
        .unwrap();
        store.save_to(dir.path()).unwrap();
        let reopened = Store::load_from(dir.path());
        assert!(!compute_notice_due(&reopened, dir.path(), 16));
        let mut config = reopened.config.unwrap();
        assert_eq!(
            startup_download_gate(&config, &Conditions(PowerSource::Battery, Some(false))),
            Err(SkipReason::OnBattery)
        );
        assert_eq!(
            startup_download_gate(&config, &Conditions(PowerSource::Mains, Some(true))),
            Err(SkipReason::MeteredNetwork)
        );
        assert_eq!(
            startup_download_gate(&config, &Conditions(PowerSource::Mains, Some(false))),
            Ok(())
        );
        record_explicit_compute_consent(&mut config);
        assert_eq!(startup_download_gate(&config, &NeverSample), Ok(()));
        apply_webview_config_save(&mut store, config, ConfigSave::Onboarding);
        assert_eq!(
            startup_download_gate(store.config.as_ref().unwrap(), &NeverSample),
            Ok(())
        );
    }

    #[test]
    fn sharing_notice_survives_claims_and_old_marker_until_rendered_once() {
        let dir = tempfile::tempdir().unwrap();
        record_compute_prompt(dir.path()).unwrap();
        let mut store = existing_install("observer", None, None);
        apply_compute_default(&mut store, 16, 24, false);
        store.save_to(dir.path()).unwrap();
        let reopened = Store::load_from(dir.path());
        assert!(compute_notice_due(&reopened, dir.path(), 16));
        assert!(!compute_notice_due(&reopened, dir.path(), 12));
        assert!(
            compute_notice_due(&reopened, dir.path(), 16),
            "claim does not consume"
        );
        arc_crypto::secret_file::durably_publish_new_private(
            &dir.path().join(COMPUTE_NOTICE_MARKER),
            b"shown\n",
        )
        .unwrap();
        assert!(!compute_notice_due(&reopened, dir.path(), 16));
        assert!(!apply_compute_default(&mut store, 16, 24, false));
        withdraw_compute_consent(store.config.as_mut().unwrap());
        assert_eq!(store.config.as_ref().unwrap().compute_consent, Some(false));
        assert_eq!(store.config.as_ref().unwrap().role, "observer");
        assert!(!apply_compute_default(&mut store, 16, 24, false));
    }

    #[test]
    fn sharing_awake_choice_defaults_off_and_survives_stale_saves() {
        let mut store = existing_install("observer", None, None);
        assert_eq!(
            store.config.as_ref().unwrap().keep_awake_while_sharing,
            None
        );
        update_stored_config_in(&mut store, |c| c.keep_awake_while_sharing = Some(true));
        apply_webview_config_save(&mut store, NodeConfig::default(), ConfigSave::Preferences);
        let dir = tempfile::tempdir().unwrap();
        store.save_to(dir.path()).unwrap();
        assert_eq!(
            Store::load_from(dir.path())
                .config
                .unwrap()
                .keep_awake_while_sharing,
            Some(true)
        );
    }

    #[test]
    fn turning_it_on_stores_what_a_setup_that_leaves_compute_on_stores() {
        let mut said_no = config("observer", None, Some(false));
        turn_on_compute(&mut said_no, 24);
        assert_eq!(said_no.compute_consent, Some(true));
        assert_eq!(said_no.worker_threads, Some(6));
        assert_eq!(promotion_need(&said_no, 16), PromotionNeed::Promote);

        let mut never_asked = config("observer", None, None);
        turn_on_compute(&mut never_asked, 3);
        assert_eq!(never_asked.compute_consent, Some(true));
        assert_eq!(never_asked.worker_threads, Some(1));

        let mut chosen = NodeConfig {
            worker_threads: Some(10),
            ..config("observer", None, Some(false))
        };
        turn_on_compute(&mut chosen, 24);
        assert_eq!(chosen.compute_consent, Some(true));
        assert_eq!(chosen.worker_threads, Some(10));

        let mut store = Store {
            config: Some(config("observer", None, Some(false))),
            ..Store::default()
        };
        update_stored_config_in(&mut store, |config| turn_on_compute(config, 24));
        apply_webview_config_save(
            &mut store,
            config("observer", None, Some(false)),
            ConfigSave::Preferences,
        );
        let persisted = store.config.unwrap();
        assert_eq!(persisted.compute_consent, Some(true));
        assert_eq!(persisted.worker_threads, Some(6));
    }
}

#[cfg(test)]
mod start_fence_tests {
    use super::{
        app_update_boundary, local_rpc_port, require_data_migration_ready, require_start_fence,
        run_inference_local_inner, start_block, try_local_rpc_port, StartTransaction,
        INFERENCE_PRE_DISPATCH_UNAVAILABLE,
    };
    use crate::start_fence::{
        after_startup_reconciliation, at_launch, AppUpdateBoundary, StartAccess, StartFence,
    };
    use crate::types::NodeConfig;
    use crate::AppState;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn below_floor_config_roundtrip_preserves_choices_and_launches_only_observer() {
        for consent in [None, Some(false), Some(true)] {
            for save in [
                super::ConfigSave::Preferences,
                super::ConfigSave::Onboarding,
            ] {
                let directory = tempfile::tempdir().unwrap();
                let state = state_with(None);
                *state.data_dir.lock().await = directory.path().to_path_buf();
                let original = NodeConfig {
                    role: "worker".into(),
                    model_path: Some("/models/standard.gguf".into()),
                    compute_consent: consent,
                    worker_threads: Some(3),
                    ..NodeConfig::default()
                };
                state.store.lock().await.config = Some(original.clone());
                let mut displayed = super::load_config_inner(&state).await.unwrap();
                assert_eq!(displayed.compute_consent, consent);
                assert_eq!(displayed.model_path, original.model_path);
                assert_eq!(displayed.role, original.role);
                let launched = crate::node_manager::effective_launch_config_for_ram(&displayed, 12);
                assert_eq!(launched.role, "observer");
                assert_eq!(launched.model_path, None);
                assert!(!super::needs_promotion(&displayed, 12));
                displayed.rpc_port = 29_195;
                {
                    let mut store = state.store.lock().await;
                    super::apply_webview_config_save(&mut store, displayed, save);
                    store.save_to(directory.path()).unwrap();
                }
                let reopened = crate::store::Store::load_from(directory.path());
                let persisted = reopened.config.as_ref().unwrap();
                assert_eq!(persisted.rpc_port, 29_195);
                assert_eq!(persisted.compute_consent, consent);
                assert_eq!(persisted.model_path, original.model_path);
                assert_eq!(persisted.worker_threads, Some(3));
                let launched = crate::node_manager::effective_launch_config_for_ram(persisted, 12);
                assert_eq!(launched.role, "observer");
                assert_eq!(launched.model_path, None);
                *state.store.lock().await = reopened;
                let displayed = super::load_config_inner(&state).await.unwrap();
                assert_eq!(displayed.compute_consent, consent);
                assert_eq!(displayed.model_path, original.model_path);
            }
        }
    }

    #[tokio::test]
    async fn other_desktop_cannot_save_contribution_or_preferences() {
        let state = state_with(Some(StartFence::OtherDesktop));
        assert!(state.another_desktop_owns_node().await);
        assert_eq!(
            super::require_settings_owner(&state).await.unwrap_err(),
            crate::start_fence::ALREADY_OPEN
        );
        let changed =
            super::update_stored_config(&state, |config| config.compute_consent = Some(false))
                .await;
        assert!(changed.is_err());
        assert!(state.store.lock().await.config.is_none());
    }

    #[tokio::test]
    async fn reconciliation_allows_preferences_and_durable_consent_withdrawal() {
        let directory = tempfile::tempdir().unwrap();
        let state = state_with(Some(StartFence::StartupReconciliation));
        *state.data_dir.lock().await = directory.path().to_path_buf();
        super::require_settings_owner(&state).await.unwrap();
        super::update_stored_config(&state, |config| {
            config.compute_consent = Some(true);
            config.role = "worker".into();
        }).await.unwrap();
        let config = super::update_stored_config(&state, super::withdraw_compute_consent)
            .await.unwrap();
        assert_eq!(config.compute_consent, Some(false));
        assert_eq!(config.role, "observer");
        let saved = crate::store::Store::load_for_owner_probe(directory.path()).unwrap();
        assert_eq!(saved.config.unwrap().compute_consent, Some(false));
        assert!(require_start_fence(&state, StartAccess::Ordinary).await.is_err());
    }

    fn state_with(fence: Option<StartFence>) -> AppState {
        AppState {
            node: Arc::new(Mutex::new(crate::node_manager::NodeManager::new())),
            store: Arc::new(Mutex::new(crate::store::Store::default())),
            data_dir: Arc::new(Mutex::new(PathBuf::new())),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            tier1_routes: Arc::new(Mutex::new(HashMap::new())),
            community_receipt_routes: Arc::new(Mutex::new(HashMap::new())),
            community_chain_host: Arc::new(Mutex::new(None)),
            community_inference_write: Arc::new(Mutex::new(())),
            chain_host: Arc::new(Mutex::new(None)),
            wallet_write: Arc::new(Mutex::new(())),
            auto_update_requests: Arc::new(Mutex::new(None)),
            has_tray: Arc::new(AtomicBool::new(false)),
            data_migration_error: Arc::new(Mutex::new(fence)),
            startup_retry_cancel: tokio::sync::watch::channel(()).0,
        }
    }

    fn test_identity(address: &str) -> crate::types::Identity {
        crate::types::Identity {
            address: address.into(),
            public_key: String::new(),
            seed_phrase: String::new(),
            created_at: 0,
        }
    }

    #[tokio::test]
    async fn launch_boundary_sees_promotion_while_waiting_for_node_and_blocks_opt_out_inside_launch(
    ) {
        let directory = tempfile::tempdir().unwrap();
        let locked_dir = directory
            .path()
            .join("node-data")
            .to_string_lossy()
            .into_owned();
        let model_path = directory.path().join("standard.gguf");
        std::fs::write(&model_path, b"model").unwrap();
        let model_str = model_path.to_string_lossy().into_owned();
        let expected_address = "0x1111111111111111111111111111111111111111";

        let state = Arc::new(state_with(None));
        *state.data_dir.lock().await = directory.path().to_path_buf();
        {
            let mut store = state.store.lock().await;
            store.identity = Some(test_identity(expected_address));
            store.config = Some(super::NodeConfig {
                data_dir: locked_dir.clone(),
                role: "observer".into(),
                model_path: None,
                compute_consent: Some(true),
                ..super::NodeConfig::default()
            });
            store.save_to(directory.path()).unwrap();
        }

        state.cancel_startup_retry();
        let cancel_watch = state.startup_retry_cancel.subscribe();
        let held_node = state.node.lock().await;
        let waiting_for_node = Arc::new(tokio::sync::Notify::new());
        let launch_entered = Arc::new(tokio::sync::Notify::new());
        let release_launch = Arc::new(tokio::sync::Notify::new());

        let launcher = {
            let state = state.clone();
            let locked_dir = locked_dir.clone();
            let waiting_for_node = waiting_for_node.clone();
            let launch_entered = launch_entered.clone();
            let release_launch = release_launch.clone();
            tokio::spawn(async move {
                waiting_for_node.notify_one();
                let _node = state.node.lock().await;
                super::with_current_launch_config(
                    &state.store,
                    &cancel_watch,
                    &locked_dir,
                    expected_address,
                    |launch_cfg| async move {
                        launch_entered.notify_one();
                        release_launch.notified().await;
                        Ok(launch_cfg)
                    },
                )
                .await
            })
        };

        waiting_for_node.notified().await;
        tokio::task::yield_now().await;

        let promoted = super::persist_promotion_if_still_consented(&state, model_str.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(promoted.role, "worker");
        assert_eq!(promoted.model_path.as_deref(), Some(model_str.as_str()));

        drop(held_node);
        launch_entered.notified().await;

        let opt_out_state = state.clone();
        let mut opt_out = tokio::spawn(async move {
            super::update_stored_config(&opt_out_state, super::withdraw_compute_consent).await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut opt_out)
                .await
                .is_err(),
            "opt-out writer must block on store guard until committed launch completes"
        );

        release_launch.notify_one();
        let launched_cfg = launcher.await.unwrap().unwrap();
        assert_eq!(launched_cfg.data_dir, locked_dir);
        assert_eq!(launched_cfg.role, "worker");
        assert_eq!(launched_cfg.model_path.as_deref(), Some(model_str.as_str()));
        assert_eq!(launched_cfg.compute_consent, Some(true));

        let opted_out = opt_out.await.unwrap().unwrap();
        assert_eq!(opted_out.compute_consent, Some(false));
        assert_eq!(opted_out.role, "observer");
        assert_eq!(opted_out.model_path, None);
        let saved = crate::store::Store::load_for_owner_probe(directory.path()).unwrap();
        assert_eq!(saved.config.unwrap().compute_consent, Some(false));
        assert_eq!(saved.identity.unwrap().address, expected_address);
    }

    #[tokio::test]
    async fn launch_boundary_rejects_superseded_cancel_changed_data_dir_and_changed_identity() {
        let locked_dir = "/locked/node-data";
        let expected_address = "0x1111111111111111111111111111111111111111";
        let other_address = "0x2222222222222222222222222222222222222222";
        let state = state_with(None);
        {
            let mut store = state.store.lock().await;
            store.identity = Some(test_identity(expected_address));
            store.config = Some(super::NodeConfig {
                data_dir: locked_dir.into(),
                role: "worker".into(),
                compute_consent: Some(true),
                ..super::NodeConfig::default()
            });
        }

        state.cancel_startup_retry();
        let cancel_watch = state.startup_retry_cancel.subscribe();
        state.cancel_startup_retry();
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let _node = state.node.lock().await;
            let count = launches.clone();
            let result = super::with_current_launch_config(
                &state.store,
                &cancel_watch,
                locked_dir,
                expected_address,
                |_cfg| async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
            assert!(result.is_err());
        }
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 0);

        let fresh_watch = state.startup_retry_cancel.subscribe();
        state.store.lock().await.config.as_mut().unwrap().data_dir = "/changed/node-data".into();
        {
            let _node = state.node.lock().await;
            let count = launches.clone();
            let result = super::with_current_launch_config(
                &state.store,
                &fresh_watch,
                locked_dir,
                expected_address,
                |_cfg| async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
            assert!(result.is_err());
        }
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 0);

        state.store.lock().await.config.as_mut().unwrap().data_dir = locked_dir.into();
        for mutated_identity in [Some(test_identity(other_address)), None] {
            state.store.lock().await.identity = mutated_identity;
            let _node = state.node.lock().await;
            let count = launches.clone();
            let result = super::with_current_launch_config(
                &state.store,
                &fresh_watch,
                locked_dir,
                expected_address,
                |_cfg| async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
            assert!(result.is_err());
        }
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 0);

        state.store.lock().await.identity = Some(test_identity(expected_address));
        {
            let _node = state.node.lock().await;
            let count = launches.clone();
            let launched = super::with_current_launch_config(
                &state.store,
                &fresh_watch,
                locked_dir,
                expected_address,
                |cfg| async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(cfg)
                },
            )
            .await
            .unwrap();
            assert_eq!(launched.data_dir, locked_dir);
        }
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn body_between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        let start_at = source.find(start).expect("start marker");
        let tail = &source[start_at..];
        &tail[..tail.find(end).expect("end marker")]
    }

    #[tokio::test]
    async fn startup_recovery_is_not_refused_by_its_own_launch_fence() {
        let state = state_with(Some(at_launch(None)));

        let before = require_start_fence(&state, StartAccess::Ordinary)
            .await
            .unwrap_err();
        assert!(before.contains("startup reconciliation is still in progress"));

        assert_eq!(
            require_start_fence(&state, StartTransaction::StartupRecoveryOnly.fence_access()).await,
            Ok(())
        );
        assert!(
            require_start_fence(&state, StartTransaction::Start.fence_access())
                .await
                .is_err()
        );
        assert!(require_data_migration_ready(&state).await.is_err());
    }

    #[tokio::test]
    async fn start_succeeds_once_startup_reconciliation_completes() {
        let state = state_with(Some(at_launch(None)));
        assert!(require_data_migration_ready(&state).await.is_err());

        *state.data_migration_error.lock().await = after_startup_reconciliation(Ok(()));

        assert_eq!(require_data_migration_ready(&state).await, Ok(()));
        assert_eq!(
            require_start_fence(&state, StartTransaction::Start.fence_access()).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_failed_startup_recovery_refuses_every_caller_with_its_reason() {
        let state = state_with(Some(at_launch(None)));
        *state.data_migration_error.lock().await = after_startup_reconciliation(Err(
            "managed-node durability recovery failed: recovery node exited before its WAL barrier"
                .into(),
        ));
        for access in [StartAccess::Ordinary, StartAccess::StartupRecovery] {
            let error = require_start_fence(&state, access).await.unwrap_err();
            assert!(error.contains("recovery node exited before its WAL barrier"));
            assert!(!error.contains("still in progress"));
            assert_eq!(error.matches("ARC refused to start the node").count(), 1);
        }
    }

    #[test]
    fn only_launch_reconciliation_runs_the_recovery_only_transaction() {
        let source = include_str!("commands.rs");
        let recovery = body_between(
            source,
            "pub(crate) async fn recover_managed_shutdown_inner(",
            "async fn require_start_fence(",
        );
        assert!(recovery.contains("StartTransaction::StartupRecoveryOnly"));
        for (name, start, end) in [
            (
                "start_node_inner",
                "pub async fn start_node_inner(",
                "pub(crate) async fn autostart_node_inner(",
            ),
            (
                "autostart_node_inner",
                "pub(crate) async fn autostart_node_inner(",
                "pub(crate) async fn recover_managed_shutdown_inner(",
            ),
        ] {
            let body = body_between(source, start, end);
            assert!(
                body.contains("StartTransaction::Start, cancel_watch)"),
                "{name}"
            );
            assert!(!body.contains("StartupRecoveryOnly"), "{name}");
        }
        let gate = body_between(
            source,
            "async fn require_data_migration_ready(",
            "#[tauri::command]",
        );
        assert!(gate.contains("StartAccess::Ordinary"));
        assert!(!gate.contains("StartupRecovery"));
    }

    #[tokio::test]
    async fn a_refusal_lets_the_app_update_through_and_keeps_node_starts_closed() {
        let state = state_with(Some(at_launch(None)));
        *state.data_migration_error.lock().await = after_startup_reconciliation(Err(
            "managed-node durability recovery failed: recovery node exited before its WAL barrier"
                .into(),
        ));
        assert_eq!(
            app_update_boundary(&state).await,
            Ok(AppUpdateBoundary::Fenced)
        );
        assert!(require_data_migration_ready(&state).await.is_err());
        assert!(
            require_start_fence(&state, StartTransaction::Start.fence_access())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn home_never_waits_on_the_node_mutex() {
        let state = state_with(Some(at_launch(None)));
        let lifecycle_step = state.node.lock().await;

        let waiting = start_block(&state).await.expect("reconciliation is a wait");
        assert!(waiting.contains("startup reconciliation is still in progress"));
        *state.data_migration_error.lock().await = after_startup_reconciliation(Err(
            "managed-node durability recovery failed: recovery node exited before its WAL barrier"
                .into(),
        ));
        let refused = start_block(&state).await.expect("a refusal blocks starts");
        assert!(refused.contains("ARC refused to start the node"));

        let skipped = run_inference_local_inner(&state, "The largest planet is", 1, true)
            .await
            .expect_err("a busy node mutex skips the local node");
        assert!(skipped.starts_with(INFERENCE_PRE_DISPATCH_UNAVAILABLE));
        assert_eq!(try_local_rpc_port(&state), None);
        let candidates = body_between(
            include_str!("commands.rs"),
            "async fn coordinator_candidates(",
            "async fn established_chain_origin(",
        );
        assert!(candidates.contains("if let Some(port) = try_local_rpc_port(state) {"));
        assert!(!candidates.contains("local_rpc_port(state).await"));
        assert_eq!(local_rpc_port(&state).await, NodeConfig::default().rpc_port);
        let rpc_port = lifecycle_step.rpc_port;
        drop(lifecycle_step);
        assert_eq!(try_local_rpc_port(&state), Some(rpc_port));

        *state.data_migration_error.lock().await = None;
        assert_eq!(start_block(&state).await, None);
    }

    #[tokio::test]
    async fn the_app_update_waits_for_startup_reconciliation_then_opens() {
        let state = state_with(Some(at_launch(None)));
        let error = app_update_boundary(&state).await.unwrap_err();
        assert!(error.contains("startup reconciliation is still in progress"));

        *state.data_migration_error.lock().await = after_startup_reconciliation(Ok(()));
        assert_eq!(
            app_update_boundary(&state).await,
            Ok(AppUpdateBoundary::Ordinary)
        );
    }

    #[test]
    fn only_the_app_update_entrypoints_pass_a_refusal() {
        let source = include_str!("commands.rs");
        for (name, start, end) in [
            (
                "prepare_update_relaunch",
                "pub async fn prepare_update_relaunch(",
                "pub async fn try_prepare_auto_update_relaunch(",
            ),
            (
                "try_prepare_auto_update_relaunch",
                "pub async fn try_prepare_auto_update_relaunch(",
                "pub async fn begin_update_handoff(",
            ),
        ] {
            let body = body_between(source, start, end);
            assert!(body.contains("app_update_boundary(&state)"), "{name}");
            assert!(body.contains("prepare_fenced_update_relaunch()"), "{name}");
            assert!(!body.contains("require_data_migration_ready"), "{name}");
            let boundary_at = body.find("app_update_boundary(&state)").unwrap();
            let first_cancel = body.find("state.cancel_startup_retry()").unwrap();
            assert!(boundary_at < first_cancel, "{name}");
            assert!(body.contains("if prepared.is_ok() {"), "{name}");
        }
        for (name, start, end) in [
            (
                "ensure_binary",
                "pub async fn ensure_binary(",
                "fn installed(",
            ),
            (
                "restart_node",
                "pub async fn restart_node(",
                "pub(crate) async fn restart_node_inner(",
            ),
        ] {
            let body = body_between(source, start, end);
            assert!(body.contains("require_data_migration_ready"), "{name}");
            assert!(!body.contains("app_update_boundary"), "{name}");
        }
    }

    #[test]
    fn both_app_update_entrypoints_check_where_arc_runs_before_anything_else() {
        let source = include_str!("commands.rs");
        for (name, start, end) in [
            (
                "prepare_update_relaunch",
                "pub async fn prepare_update_relaunch(",
                "pub async fn try_prepare_auto_update_relaunch(",
            ),
            (
                "try_prepare_auto_update_relaunch",
                "pub async fn try_prepare_auto_update_relaunch(",
                "pub async fn begin_update_handoff(",
            ),
        ] {
            let body = body_between(source, start, end);
            let check = body
                .find("refuse_update_from_wrong_location()?;")
                .unwrap_or_else(|| panic!("{name} does not check where ARC runs"));
            for later in [
                "app_update_boundary(&state)",
                "state.cancel_startup_retry()",
                "state.node.",
                "state.auto_update_requests",
                "update_install_policy()",
            ] {
                if let Some(at) = body.find(later) {
                    assert!(check < at, "{name} reaches {later} first");
                }
            }
        }
        let check = body_between(
            source,
            "fn refuse_update_from_wrong_location(",
            "async fn require_data_migration_ready(",
        );
        assert!(check.contains("crate::update_location::refusal()"));
    }

    #[test]
    fn start_and_restart_refuse_an_update_held_node_before_the_owner_lock() {
        let source = include_str!("commands.rs");
        for (name, start, end, first_reach) in [
            (
                "start_node_transaction",
                "async fn start_node_transaction(",
                "pub async fn start_node_inner(",
                "acquire_managed_lifecycle_lock(&lock_data_dir)",
            ),
            (
                "restart_node_inner",
                "pub(crate) async fn restart_node_inner(",
                "pub(crate) const COMPUTE_CONSENT_REQUIRED: &str =",
                "node.stop().await",
            ),
        ] {
            let body = body_between(source, start, end);
            let refusal = body
                .find(".refuse_start_during_update()?;")
                .unwrap_or_else(|| panic!("{name} does not refuse during an update"));
            assert!(refusal < body.find(first_reach).unwrap(), "{name}");
            assert!(
                refusal < body.find("acquire_managed_lifecycle_lock(").unwrap(),
                "{name}"
            );
        }
        assert_eq!(
            crate::node_manager::UPDATE_HOLDS_NODE,
            "ARC couldn't finish an update. Quit ARC and open it again."
        );
    }
}

#[cfg(test)]
mod slow_crash_wait_update_tests {
    use super::*;

    fn slow_wait_state(label: &str) -> (PathBuf, AppState) {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "arc-desktop-slow-wait-update-{label}-{}",
            std::process::id()
        ));
        let data = root.join("data-v3");
        arc_crypto::secret_file::secure_private_directory_tree(&data).unwrap();
        let state = super::inference_retry_tests::test_state();
        state.store.try_lock().unwrap().config = Some(NodeConfig {
            data_dir: data.to_string_lossy().into_owned(),
            ..NodeConfig::default()
        });
        state
            .node
            .try_lock()
            .unwrap()
            .enter_slow_crash_wait_for_test(&data.to_string_lossy());
        (root, state)
    }

    async fn assert_update_holds_and_nothing_relaunches(state: &AppState) {
        let mut node = state.node.lock().await;
        assert!(node.crash_retry_in_secs().is_none());
        assert!(!node.slow_crash_wait_pending());
        assert!(node.refuse_start_during_update().is_err());
        node.supervise_managed_node().await;
        assert!(node.pid().is_none());
        node.begin_update_handoff().unwrap();
        node.supervise_managed_node().await;
        assert!(node.pid().is_none());
    }

    #[tokio::test]
    async fn refused_or_closing_status_hides_slow_retry_without_waiting_for_node() {
        for closing in [false, true] {
            let (root, state) = slow_wait_state(if closing {
                "closing-status"
            } else {
                "refused-status"
            });
            if !closing {
                *state.data_migration_error.lock().await =
                    Some(crate::start_fence::StartFence::OtherDesktop);
            }
            let node = state.node.lock().await;
            assert!(node.crash_retry_in_secs().is_some());
            let status = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                node_status_inner(&state, closing),
            )
            .await
            .expect("refused status waited for the node")
            .unwrap();
            assert!(!status.running);
            assert!(status.crash_retry_in_secs.is_none());
            assert!(status.model_download_wait.is_none());
            let expected = if closing {
                "ARC is closing".to_owned()
            } else {
                crate::start_fence::check(
                    Some(&crate::start_fence::StartFence::OtherDesktop),
                    StartAccess::Ordinary,
                )
                .unwrap_err()
            };
            assert_eq!(status.last_error.as_deref(), Some(expected.as_str()));
            assert!(
                node.crash_retry_in_secs().is_some(),
                "status mutated recovery"
            );
            drop(node);
            drop(state);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn home_update_prepares_during_a_slow_crash_wait() {
        let (root, state) = slow_wait_state("manual");
        prepare_update_relaunch_inner(&state, AppUpdateBoundary::Ordinary)
            .await
            .unwrap();
        assert_update_holds_and_nothing_relaunches(&state).await;
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn unattended_update_prepares_during_a_slow_crash_wait() {
        if !update_install_policy().await.unwrap().can_install {
            return;
        }
        let (root, state) = slow_wait_state("unattended");
        let prepared = try_prepare_auto_update_relaunch_inner(&state, AppUpdateBoundary::Ordinary)
            .await
            .unwrap();
        assert!(
            matches!(prepared, crate::auto_update::PrepareResult::Prepared),
            "{prepared:?}"
        );
        assert_update_holds_and_nothing_relaunches(&state).await;
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }
}
