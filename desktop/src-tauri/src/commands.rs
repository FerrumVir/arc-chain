use crate::node_manager::{managed_binary_path, TestnetResources};
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
    let mut store = state.store.lock().await;
    let dir = state.data_dir.lock().await.clone();
    generate_identity_if_missing(&mut store, &dir)
}

/// Return the existing wallet during onboarding even before a node config
/// exists. The caller holds the store mutex across this check and creation so
/// concurrent IPC requests cannot replace one generated seed with another.
fn generate_identity_if_missing(store: &mut Store, dir: &Path) -> CmdResult<IdentityPublic> {
    if store.identity.is_none() {
        store.identity = Some(identity::generate());
    }

    // Retain the generated identity if saving fails. Store::save_to can
    // publish store.json before a later durability check fails, so a retry
    // must persist this same key rather than generate a replacement.
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
    state.cancel_startup_retry();
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
        preserve_unsent_contribution_choices(&mut config, store.config.as_ref());
        auto_start = config.auto_start;
        store.config = Some(config);
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
    }
    if !auto_start {
        state.cancel_startup_retry();
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

async fn start_node_transaction(
    app: &AppHandle,
    state: &AppState,
    start_after_recovery: bool,
) -> Result<(), StartupFailure> {
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
        ensure_binary_inner_classified(app).await?;
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
    ensure_binary_inner_classified(app).await?;
    let mut node = state.node.lock().await;
    node.start(&config, &validator_keyfile, &resources, lifecycle_lock)
        .await
        .map_err(map_err)?;
    Ok(())
}

pub async fn start_node_inner(app: &AppHandle, state: &AppState) -> Result<(), String> {
    state.startup_retry_cancel.send_replace(());
    start_node_transaction(app, state, true)
        .await
        .map_err(|failure| failure.to_string())
}

pub(crate) async fn autostart_node_inner(
    app: &AppHandle,
    state: &AppState,
) -> Result<(), StartupFailure> {
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
    start_node_transaction(app, state, true).await
}

pub(crate) async fn recover_managed_shutdown_inner(
    app: &AppHandle,
    state: &AppState,
) -> Result<(), String> {
    start_node_transaction(app, state, false)
        .await
        .map_err(|failure| failure.to_string())
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
    state.cancel_startup_retry();
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
    state.cancel_startup_retry();
    require_data_migration_ready(&state).await?;
    let mut node = state.node.lock().await;
    node.prepare_update_relaunch()
        .await
        .map_err(|error| format!("could not establish the native update lifecycle fence: {error}"))
}

/// Unattended updates require separate saved consent, idle app-side writes,
/// and an authenticated compute drain from the exact managed child.
#[tauri::command]
pub async fn try_prepare_auto_update_relaunch(
    state: State<'_, AppState>,
) -> CmdResult<crate::auto_update::PrepareResult> {
    use crate::auto_update::{PrepareResult, RequestFence};
    require_data_migration_ready(&state).await?;
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
    // Store before awaiting any child mutation. An interrupted IPC leaves the
    // fence intact until Abort proves the node has resumed or safely stopped.
    *held = Some(fence);
    let Ok(mut node) = state.node.try_lock() else {
        *held = None;
        return Ok(PrepareResult::busy(
            "A node lifecycle operation is in progress",
            0,
        ));
    };
    if node.pid().is_some() {
        state.cancel_startup_retry();
    }
    let result = node.prepare_auto_update_relaunch().await;
    if matches!(result, Ok(PrepareResult::Busy { .. })) {
        *held = None;
    }
    result.map_err(|error| format!("automatic update preparation requires a safe abort: {error}"))
}

/// Seal the one-way native updater boundary immediately before invoking the
/// signed installer. From this point the old node cannot be resumed by an
/// abort command, even if installer IPC later rejects or disconnects.
#[tauri::command]
pub async fn begin_update_handoff(state: State<'_, AppState>) -> CmdResult<()> {
    // Serialize with Abort, but do not introduce a new consent refusal here:
    // the frontend commits its irreversible IPC boundary before this await.
    // Native Prepare checks saved consent and the frontend checks generation
    // immediately before invoking Handoff, with no intervening await.
    let _requests = state.auto_update_requests.lock().await;
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
    restart_node_inner(&app, &state).await
}

pub(crate) async fn restart_node_inner(app: &AppHandle, state: &AppState) -> Result<(), String> {
    state.cancel_startup_retry();
    require_data_migration_ready(state).await?;
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
    ensure_binary_inner(app).await?;

    let resources = resolve_testnet_resources(app);
    let mut node = state.node.lock().await;
    node.start(&cfg, &validator_keyfile, &resources, lifecycle_lock)
        .await
        .map_err(map_err)
}

// ── Compute contribution (explicit opt-in) ─────────────────────────────────
//
// An install contributes compute only after its user says yes: onboarding's
// model choice, the observer banner, or the Settings switch. Nothing here
// downloads a model or switches to worker mode without that answer, and
// turning the switch off returns the node to observer mode at once.

/// Shown when the app is asked to promote an install whose user never opted in.
pub(crate) const COMPUTE_CONSENT_REQUIRED: &str =
    "Compute contribution is off. Turn it on in Settings to download the model and take ARC jobs on this computer.";

/// Shown when the machine is below the worker memory floor.
pub(crate) const COMPUTE_INELIGIBLE: &str =
    "This computer has less than 16 GB of memory, so it cannot run the ARC model. It stays an observer, which still relays and verifies.";

/// Whether the user agreed to contribute compute. An explicit answer is
/// authoritative. Installs from before the question existed count as opted
/// in only if their user had already chosen worker mode with a model.
pub(crate) fn compute_contribution_enabled(config: &NodeConfig) -> bool {
    config
        .compute_consent
        .unwrap_or(config.role == "worker" && config.model_path.is_some())
}

/// What a consented install still needs before it can take jobs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PromotionNeed {
    /// The user has not opted in; never touch the machine.
    NoConsent,
    /// Below the memory floor; stays an observer.
    Ineligible,
    /// Already a worker with a model; nothing to do.
    Ready,
    /// Download (or reuse) the model, switch to worker mode, restart.
    Promote,
}

pub(crate) fn promotion_need(config: &NodeConfig, ram_gb: u64) -> PromotionNeed {
    if !compute_contribution_enabled(config) {
        PromotionNeed::NoConsent
    } else if config.role == "worker" && config.model_path.is_some() {
        PromotionNeed::Ready
    } else if tier_for_ram_gb(ram_gb) == "none" {
        PromotionNeed::Ineligible
    } else {
        PromotionNeed::Promote
    }
}

/// Contribution choices are changed by their own commands. A WebView save
/// that does not carry them (a screen built before they existed) keeps the
/// stored answers instead of silently erasing the user's consent.
fn preserve_unsent_contribution_choices(config: &mut NodeConfig, persisted: Option<&NodeConfig>) {
    if let Some(persisted) = persisted {
        if config.compute_consent.is_none() {
            config.compute_consent = persisted.compute_consent;
        }
        if config.prevent_sleep_during_jobs.is_none() {
            config.prevent_sleep_during_jobs = persisted.prevent_sleep_during_jobs;
        }
    }
}

async fn current_config(state: &AppState) -> NodeConfig {
    state.store.lock().await.config.clone().unwrap_or_default()
}

async fn persist_config(state: &AppState, mut config: NodeConfig) -> Result<NodeConfig, String> {
    let mut store = state.store.lock().await;
    preserve_authoritative_data_dir(&mut config, store.config.as_ref());
    store.config = Some(config.clone());
    let dir = state.data_dir.lock().await.clone();
    store.save_to(&dir).map_err(map_err)?;
    Ok(config)
}

/// Turn compute contribution on or off.
///
/// On: record the consent, then download the model if needed (resumable,
/// with progress on `model-download-progress`), switch to worker mode, and
/// restart the node so it registers and starts taking jobs. Off: record the
/// refusal and return to observer mode now; the model stays on disk.
#[tauri::command]
pub async fn set_compute_contribution(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> CmdResult<NodeConfig> {
    let mut config = current_config(&state).await;
    config.compute_consent = Some(enabled);
    if !enabled {
        config.role = "observer".into();
        let config = persist_config(&state, config).await?;
        let running = state.node.lock().await.is_running();
        if running {
            restart_node_inner(&app, &state).await?;
        }
        return Ok(config);
    }
    persist_config(&state, config).await?;
    promote_consented_install_inner(&app, &state).await
}

/// Finish promoting an install whose user opted in: the model download may
/// have been interrupted, or the user agreed on a machine that was offline.
#[tauri::command]
pub async fn promote_consented_install(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<NodeConfig> {
    promote_consented_install_inner(&app, &state).await
}

pub(crate) async fn promote_consented_install_inner(
    app: &AppHandle,
    state: &AppState,
) -> Result<NodeConfig, String> {
    let config = current_config(state).await;
    // The first hardware read can block for seconds (system_profiler).
    let ram_gb = tokio::task::spawn_blocking(|| cached_hardware().ram_gb)
        .await
        .map_err(map_err)?;
    let tier = tier_for_ram_gb(ram_gb);
    match promotion_need(&config, ram_gb) {
        PromotionNeed::NoConsent => return Err(COMPUTE_CONSENT_REQUIRED.to_string()),
        PromotionNeed::Ineligible => return Err(COMPUTE_INELIGIBLE.to_string()),
        PromotionNeed::Ready => return Ok(config),
        PromotionNeed::Promote => {}
    }
    let model_path = match existing_model_for_tier(tier.to_string()).await? {
        Some(path) => path,
        None => download_model(app.clone(), tier.to_string()).await?,
    };
    // The download can take an hour. Honour a "turn it off" made meanwhile.
    let latest = current_config(state).await;
    if !compute_contribution_enabled(&latest) {
        return Ok(latest);
    }
    let promoted = persist_config(
        state,
        NodeConfig {
            role: "worker".into(),
            model_path: Some(model_path),
            compute_consent: Some(true),
            ..latest
        },
    )
    .await?;
    // Apply it to a running node. A node the user stopped stays stopped and
    // starts as a worker next time.
    let running = state.node.lock().await.is_running();
    if running {
        restart_node_inner(app, state).await?;
    }
    Ok(promoted)
}

/// Startup hook: once the node is up, finish enabling contribution for a
/// consented install that is not a worker yet. Silent when there is nothing
/// to do; a failure is logged and retried on the next launch or toggle.
pub(crate) async fn promote_consented_install_on_startup(app: &AppHandle, state: &AppState) {
    let config = current_config(state).await;
    let ram_gb = tokio::task::spawn_blocking(|| cached_hardware().ram_gb)
        .await
        .unwrap_or(0);
    if promotion_need(&config, ram_gb) != PromotionNeed::Promote {
        return;
    }
    match promote_consented_install_inner(app, state).await {
        Ok(_) => tracing::info!("compute contribution enabled: worker mode with a verified model"),
        Err(error) => {
            tracing::warn!(%error, "could not finish enabling compute contribution; will retry")
        }
    }
}

/// Keep the computer awake while a job runs. Takes effect the next time the
/// node starts, so an in-flight job is not interrupted by a restart.
#[tauri::command]
pub async fn set_prevent_sleep_during_jobs(
    state: State<'_, AppState>,
    enabled: bool,
) -> CmdResult<NodeConfig> {
    let mut config = current_config(&state).await;
    config.prevent_sleep_during_jobs = Some(enabled);
    persist_config(&state, config).await
}

/// This machine's community worker: state and job counters, read from the
/// local node only.
#[tauri::command]
pub async fn fetch_worker_status(
    state: State<'_, AppState>,
) -> CmdResult<crate::types::WorkerStatus> {
    let port = state.node.lock().await.rpc_port;
    let local = paths::local_host(port);
    Ok(rpc_client::fetch_worker_status(&state.http, &local).await)
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
    state.cancel_startup_retry();
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
    Ok(fetch_earnings_projection_inner(&state, address.as_deref()).await)
}

pub(crate) async fn fetch_earnings_projection_inner(
    state: &AppState,
    address: Option<&str>,
) -> crate::types::EarningsProjection {
    let host = chain_host(state).await;
    rpc_client::fetch_earnings_projection(&state.http, &host, address).await
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
    Ok(fetch_network_overview_inner(&state).await)
}

pub(crate) async fn fetch_network_overview_inner(
    state: &AppState,
) -> crate::types::NetworkOverview {
    let host = chain_host(state).await;
    fetch_network_overview_from_host_inner(state, &host).await
}

/// Read-only production acceptance preflight against one compiled origin.
/// This does not establish or mutate a community receipt pin; only an exact
/// successfully submitted 0x25 inference result may do that.
pub(crate) async fn fetch_network_overview_from_host_inner(
    state: &AppState,
    host: &str,
) -> crate::types::NetworkOverview {
    rpc_client::fetch_network_overview(&state.http, host).await
}

/// Read-only packaged-acceptance preflight for the exact profile-bound model
/// that the sealed rollout says the production coordinator must serve.
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

/// Sign and submit an ARC transfer without ever handing recovery material to
/// the WebView. IPC contains only the recipient and a decimal amount string.
#[tauri::command]
pub async fn send_arc(
    state: State<'_, AppState>,
    to: String,
    amount_arc: String,
) -> CmdResult<WalletTxResult> {
    send_arc_inner(&state, to, amount_arc).await
}

/// The transfer core, callable without a Tauri runtime (the live journey in
/// `native_paid` drives it against a real chain).
pub(crate) async fn send_arc_inner(
    state: &AppState,
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

    let host = crate::wallet::validate_rpc_origin(&chain_host(state).await)?;
    // A protocol-4 block carries only a native paid-inference transaction:
    // its nodes refuse any other at submission and its blocks cannot include
    // one. Say so before signing instead of after a refused submission. A
    // host that cannot say is left to refuse the transfer itself.
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
// So: pick the freshest seed on the first chain read — which is the part the
// old code got wrong, hard-pinning LAX even when it was six days stale — then
// stay there and render unavailability if it later stops answering.

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
pub(crate) async fn chain_host(state: &AppState) -> String {
    for key in ["ARC_WALLET_HOST", "ARC_TIER1_RPC"] {
        if let Ok(env) = std::env::var(key) {
            let trimmed = env.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    // A successful community inference creates an exact receipt-origin
    // boundary for the remainder of this app session.  Do not health-failover
    // this pin: an unavailable source must render unavailable, never silently
    // substitute a structurally independent seed that cannot contain the
    // transaction the user is inspecting.
    if let Some(host) = state.community_chain_host.lock().await.clone() {
        return host;
    }

    // A displayed structural chain is an immutable session identity. If it
    // stops answering, callers must render unavailable instead of switching
    // wallet, earnings, or future reward writes to an independent seed.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InferenceProxyPolicy {
    /// Interactive packaged UI honors an explicitly supplied HTTPS proxy.
    /// This is required for restricted/headless environments and corporate
    /// networks; redirects and automatic retries remain forbidden below.
    Configured,
    /// Sealed packaged-native evidence must connect directly to its exact
    /// compiled coordinator and separately rejects all ambient proxy inputs.
    AcceptanceDirect,
}

fn inference_client(
    prompt: &str,
    max_tokens: u32,
    proxy_policy: InferenceProxyPolicy,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(inference_timeout(prompt, max_tokens))
        // Never let reqwest replay a protocol NACK behind the caller's
        // exact-one-dispatch accounting.
        .retry(reqwest::retry::never())
        // A POST may create a paid-on-chain reward.  Never let an HTTP
        // redirect replay that write at a second origin; the caller selected
        // and pinned one exact coordinator.
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

/// The only error class the WebView may use to choose another inference
/// route.  It is emitted exclusively before any inference POST is sent.
pub(crate) const INFERENCE_PRE_DISPATCH_UNAVAILABLE: &str =
    "ARC_INFERENCE_PRE_DISPATCH_UNAVAILABLE";
/// Every failure observed after the single POST begins is ambiguous with
/// respect to remote assignment/settlement and therefore terminal.
pub(crate) const INFERENCE_POST_OUTCOME_AMBIGUOUS: &str =
    "ARC_INFERENCE_POST_OUTCOME_AMBIGUOUS";

fn inference_pre_dispatch_unavailable(detail: impl std::fmt::Display) -> String {
    format!("{INFERENCE_PRE_DISPATCH_UNAVAILABLE}: {detail}")
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

    if let Some(origin) = established_chain_origin(state).await {
        if health_ok(&state.http, &origin, COORDINATOR_HEALTH_TIMEOUT).await {
            ordered.push(origin);
        }
        return ordered;
    }

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

/// Once the session has either displayed an elected chain or accepted one
/// exact community settlement, every later reward-producing write must stay
/// on that structural chain. A receipt pin takes precedence; otherwise the
/// already displayed cached read origin constrains the first write.
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
    let local_host = paths::local_host(state.node.lock().await.rpc_port);
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
    let local_host = paths::local_host(state.node.lock().await.rpc_port);
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
    let port = state.node.lock().await.rpc_port;
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
    let local_prefix = paths::local_host(state.node.lock().await.rpc_port);
    // `/run_consensus` is itself a write/compute operation.  Health probes
    // may inspect as many candidates as needed, but after choosing the first
    // reachable origin the app sends exactly one POST and never migrates the
    // same click to another coordinator on an ambiguous outcome.
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
    pin_community_receipt_route(&state, host, &result).await;
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

    fn test_state() -> AppState {
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
                // Dropping the accepted socket without an HTTP response
                // models a reset/EOF after the server may have accepted the
                // write. The client must not try another origin.
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
            // EOF before an HTTP response is an ambiguous write result.
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

    /// A loopback protocol-4 or protocol-3 host, recording each requested path.
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
                // Nothing past the check: no balance read, no submission.
                assert_eq!(paths, ["/native-inference/context"]);
            } else {
                // Not protocol-4: the transfer goes on to read the account. The
                // stub has none, so it stops there, before any submission.
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
    // Readiness probes are mutation-free and may walk candidates.  The first
    // origin that proves the exact v1 admission contract wins; after that
    // selection there is exactly one POST, regardless of its outcome.
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
    let local_prefix = paths::local_host(state.node.lock().await.rpc_port);
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

/// One exact packaged-production dispatch.  The origin must be one of the six
/// compiled ARC coordinators; it is health-probed and then receives exactly
/// one POST.  Unlike the interactive availability path above, this function
/// never retries a different coordinator after an ambiguous write outcome.
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

const SETTLEMENT_WRITE_UNAVAILABLE: &str = "is unavailable in the v0.8.10 recovery candidate before any transaction is signed or submitted: exact model-artifact binding, validator-authenticated authorization, and settlement are not production-ready. VRF selection and server-derived replica labels are not validator approval. Free/community inference remains available.";

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
/// No longer a priority list with a pinned `[0]` — `chain_host()` elects the
/// first session source by block freshness. Order is presentational only.
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
    ensure_binary_inner_classified(app)
        .await
        .map_err(|failure| failure.to_string())
}

async fn ensure_binary_inner_classified(app: &AppHandle) -> Result<BinaryStatus, StartupFailure> {
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
            Err(e.with_context(
                "No arc-node is available to run. Build one with `cargo build --release -p arc-node` in the arc-chain checkout, or set ARC_NODE_BIN to an existing binary",
            ))
        }
    }
}

/// Fetch the platform's arc-node release asset and install it at `target`.
/// Returns the byte count on success.
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
    // The checksum text is attacker-controlled until this succeeds. Verify the
    // namespaced owner signature and exact repo/tag/commit header before using
    // even one digest from it to authenticate an executable child process.
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

    // Verify the durable file, not only the network byte stream. The unique
    // create-new sidecar means this digest belongs solely to this invocation.
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

/// Sidecar naming used by builds before model downloads became resumable.
/// Kept for tests that fabricate the leftovers `cleanup_model_download_sidecars`
/// must still remove; production now resumes from [`model_partial_path`].
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
    Ok(tier_for_ram_gb(hardware::detect().ram_gb).into())
}

/// The model tier a machine with `ram_gb` of memory can run. The canonical
/// 7B package needs about 7.8 GB resident plus its KV cache (M1 manifest),
/// so anything under 16 GB is offered no model rather than a failing one.
///
/// `ram_gb` is the marketed size from [`hardware::nominal_ram_gb`], not the
/// OS-visible total floored to whole GiB: a 16 GB Windows or Linux machine
/// reports 15.x GiB and must still be offered the model.
fn tier_for_ram_gb(ram_gb: u64) -> &'static str {
    if ram_gb >= hardware::WORKER_MIN_NOMINAL_RAM_GB { "standard" } else { "none" }
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

/// Consecutive attempts without a single new byte before a model download
/// gives up. Any attempt that saves data resets the count, so a connection
/// that drops every few minutes still finishes, and whatever is saved stays
/// on disk for the next start either way.
const MODEL_DOWNLOAD_MAX_STALLED_ATTEMPTS: u32 = 8;
/// Times the saved bytes may be discarded, because a mirror cannot continue
/// them (a `Restart`) or answered from byte 0 instead of resuming (a rewind),
/// before the download stops instead of re-fetching gigabytes forever.
const MODEL_DOWNLOAD_MAX_RESTARTS: u32 = 2;
const MODEL_DOWNLOAD_RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(2);
const MODEL_DOWNLOAD_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// A request or read that delivers nothing for this long is treated as a
/// dropped connection: Wi-Fi changes, residential NATs, and sleeping laptops
/// leave sockets half-open rather than closed.
const MODEL_DOWNLOAD_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const MODEL_DOWNLOAD_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Free space required beyond the bytes still to download, so completing
/// the model never fills the disk.
const MODEL_DOWNLOAD_DISK_MARGIN_BYTES: u64 = 512 * 1024 * 1024;
/// Emit progress at most every 250ms. Mirror chunks land in 8-64 KB units;
/// emitting on every chunk would flood the IPC channel and pin the UI thread.
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

    /// Exponential backoff for the `stalled`-th consecutive failure.
    fn retry_delay(&self, stalled: u32) -> std::time::Duration {
        let exponent = stalled.saturating_sub(1).min(16);
        self.retry_base
            .saturating_mul(1u32 << exponent)
            .min(self.retry_max)
    }
}

/// Why one HTTP attempt of a model download stopped.
#[derive(Debug)]
enum ModelAttemptFailure {
    /// Network trouble: keep the saved bytes and retry from there.
    Transient(String),
    /// The saved bytes cannot be continued (the mirror ignored or misreported
    /// the range): discard them and retry from byte 0.
    Restart(String),
    /// Retrying cannot help: the file is gone, the disk is full, or the
    /// mirror serves an artifact of the wrong size.
    Fatal(String),
}

/// How to use a response to `GET` with `Range: bytes=<offset>-`.
#[derive(Debug, PartialEq, Eq)]
enum ModelRangePlan {
    /// Cut the partial file to this length, then append the body. A length
    /// below the saved bytes is a rewind (the mirror ignored or rejected the
    /// Range), which the retry loop counts against the restart budget.
    AppendFrom(u64),
    /// The partial file already holds every byte.
    Complete,
}

/// Parse `Content-Range: bytes <start>-<end>/<size|*>`.
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

/// The partial file a model download resumes from. Its name carries the
/// pinned digest, so bytes saved for one artifact are never continued as
/// another after a pin changes.
fn model_partial_path(target: &Path, spec: &ModelTierSpec) -> PathBuf {
    let digest_prefix = &spec.sha256[..spec.sha256.len().min(16)];
    target.with_extension(format!("{digest_prefix}.partial"))
}

/// Remove resumable partials for `target` other than `keep`.
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

/// Open (creating if needed) the partial download and return how many bytes
/// it already holds. `restart`, or a partial longer than the artifact, cuts
/// it back to empty.
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

/// One HTTP attempt: request the bytes after `offset` and append them to the
/// partial file. Returns once the partial holds all `total` bytes. When the
/// mirror answers from an earlier byte than `offset`, the saved bytes from
/// there are discarded and re-fetched, and `rewound_to` records that start
/// so the caller can count the rewind whether or not the stream then fails.
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
    // A plain write handle positioned at `start`: Windows append-only
    // handles can neither truncate nor flush to disk.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(false)
        .open(partial)
        .await
        .map_err(|error| model_write_failure(partial, error))?;
    if start != offset {
        // The mirror restarted from byte 0 instead of resuming. The saved
        // bytes past `start` are discarded and fetched again; report that so
        // the caller counts it against the restart budget whatever happens
        // to this stream next.
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
    // Flush even after a failure: tokio completes writes in the background,
    // and the next attempt must measure every byte already saved.
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

/// Fill `partial` with all `total` bytes from `url`, resuming with HTTP Range
/// requests from whatever an earlier attempt or app run saved. Transient
/// failures retry with exponential backoff; the partial is never deleted
/// here, so the next call resumes even after this one gives up.
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
        // A `Restart` discards the saved bytes before the next attempt; a
        // response that rewound has already discarded and re-fetched them.
        // Both count against the same budget. Otherwise a mirror that ignores
        // Range and keeps dropping the connection would re-fetch the file
        // forever: each slightly longer partial would look like progress
        // below, and nothing would ever report a `Restart`.
        if restart || rewound_to.is_some() {
            restarts = restarts.saturating_add(1);
            if restarts > MODEL_DOWNLOAD_MAX_RESTARTS {
                return Err(format!(
                    "Model download stopped: {reason}, {restarts} times. The mirror is not serving the pinned file consistently; try again later."
                ));
            }
        }
        // Only a resumable attempt that saved new bytes counts as progress.
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

async fn verify_model_file_blocking(
    path: &Path,
    spec: &'static ModelTierSpec,
) -> Result<bool, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || verify_model_file(&path, spec))
        .await
        .map_err(map_err)?
}

/// Download, verify, and install `spec` at `target`. Callers hold the
/// per-target download lock. Only bytes whose size and SHA-256 match the pin
/// are ever renamed into place; a mismatching download is deleted.
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
    // Already downloaded and hash-verified → done.
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
    if let Some(parent) = target.parent() {
        let needed = spec
            .size_bytes
            .saturating_sub(model_partial_len(&partial).await)
            .saturating_add(MODEL_DOWNLOAD_DISK_MARGIN_BYTES);
        if let Ok(available) = fs2::available_space(parent) {
            if available < needed {
                return Err(format!(
                    "Not enough free disk space for the model: {} is needed and {} is free in {}. Free some space and try again; anything already downloaded is kept.",
                    format_gb(needed),
                    format_gb(available),
                    parent.display()
                ));
            }
        }
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

    // Atomic rename over any existing target. std::fs::rename uses
    // MoveFileEx(REPLACE_EXISTING) on Windows since Rust 1.62, so this
    // works cross-platform.
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

/// Download the GGUF for `tier` to ~/.arc/models/<tier>.gguf, streaming
/// progress events on the `model-download-progress` channel so the UI can
/// render a real progress bar.
///
/// Resumable: bytes are saved to `<tier>.<digest>.partial` and later
/// attempts (including after an app restart or reboot) continue with an HTTP
/// Range request instead of starting over. The finished file must match the
/// pinned size and SHA-256 before an atomic rename, so a crash or mirror
/// mutation cannot replace a known good model.
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
    // the completed target instead of writing the same partial file.
    let _download_guard = acquire_model_download_lock(&target).await?;
    // Unique sidecars from earlier non-resumable builds are never resumed.
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
    // unique create-new scheme after holding the same per-target lock, plus
    // any resumable partial download.
    let tmp = p.with_extension("download");
    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }
    cleanup_model_download_sidecars(&p)?;
    cleanup_model_partials(&p, None)?;
    Ok(())
}

#[allow(dead_code)]
fn _path_helper(_: &Path) {} // keep `Path` import used if `model_path_for` returns inline

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
        // Same size, one byte flipped: never ready.
        let mut flipped = bytes.clone();
        flipped[3] ^= 1;
        std::fs::write(&path, &flipped).unwrap();
        assert_eq!(verify_model_file(&path, &spec), Ok(false));
        // Truncated, then missing: not ready either.
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
        // What the OS reports on 16 GB machines: macOS exact, Windows and
        // Linux 15.x GiB, an AMD APU with a 2 GiB frame buffer ~13.9 GiB.
        for visible in [16 * GIB, 159 * GIB / 10, 152 * GIB / 10, 139 * GIB / 10] {
            assert_eq!(
                tier_for_ram_gb(hardware::nominal_ram_gb(visible)),
                "standard",
                "{visible} visible bytes"
            );
        }
        // 8 GB and 12 GB machines are still not offered a model they cannot run.
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

    /// One scripted response per accepted connection.
    #[derive(Clone, Copy)]
    enum Reply {
        /// 200 announcing the full length, then the socket closes after
        /// `send` bytes: a lost connection mid-download.
        FullThenDrop {
            send: usize,
        },
        /// 200 with the whole payload, ignoring any Range header.
        Full,
        /// 206 from the requested offset to the end.
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

    /// Serve `replies` in order, recording each request's Range start.
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
        // The mirror ignored Range or chose to start over.
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
        // A different-size artifact can never become the pinned model.
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
        // ARC-48 F5: every reply ignores Range, answers 200 from byte 0 and
        // drops the connection one byte further than the saved partial. The
        // partial grew 101, 102, ... bytes, each attempt looked like
        // progress, and nothing reported a `Restart`, so the loop re-fetched
        // the file indefinitely. A rewind now spends the restart budget.
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
        // More stall budget than rewinds, so only the restart budget can
        // stop this download.
        let policy = ModelDownloadPolicy {
            max_stalled_attempts: MODEL_DOWNLOAD_MAX_STALLED_ATTEMPTS,
            ..FAST_RETRY
        };

        let error = download_verified_model(&reqwest::Client::new(), spec, &target, policy, |_| {})
            .await
            .unwrap_err();
        // Ten replies were scripted; the download must stop long before.
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
        // Whatever the last response saved is kept for the next run.
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
        // Never asked: an observer stays an observer.
        assert_eq!(
            promotion_need(&config("observer", None, None), 16),
            PromotionNeed::NoConsent
        );
        // An explicit no wins even with a model already on disk.
        assert_eq!(
            promotion_need(&config("observer", Some("/m.gguf"), Some(false)), 64),
            PromotionNeed::NoConsent
        );
        // The default config's "worker" role without a model is not consent.
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
    fn workers_from_before_the_question_keep_contributing() {
        let legacy = config("worker", Some("/m.gguf"), None);
        assert!(compute_contribution_enabled(&legacy));
        assert_eq!(promotion_need(&legacy, 16), PromotionNeed::Ready);
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
}
