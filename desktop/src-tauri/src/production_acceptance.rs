//! Fail-closed, one-shot production acceptance for the exact shipped app.
//!
//! This is deliberately a command-line mode of the real Tauri executable,
//! not an IPC command, WebDriver server, devtools hook, or arbitrary-origin
//! diagnostic. It is entered only with one exact flag/argument shape before a
//! Tauri Builder, WebView, plugin, or IPC handler is constructed. It calls the
//! same native command-core functions as the WebView from an isolated
//! `AppState`, emits one canonical create-only receipt, and exits.

use crate::commands;
use crate::types::{
    Earnings, EarningsProjection, InferenceResult, InferenceSettlement, NetworkOverview,
    RecentBlocks, TxLookup,
};
use crate::AppState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const MODE: &str = "--arc-production-acceptance";
const INPUT_SCHEMA: &str = "arc.packaged-desktop-native-input.v1";
const OUTPUT_SCHEMA: &str = "arc.packaged-desktop-native-acceptance.v1";
const ATTEMPT_SCHEMA: &str = "arc.packaged-desktop-native-dispatch-attempt.v1";
const REPOSITORY: &str = "FerrumVir/arc-chain";
const VERSION: &str = "0.8.0";
const EXPECTED_ORIGIN: &str = "https://140.82.16.112";
const MAX_INPUT_BYTES: u64 = 1024 * 1024;
const MAX_BUNDLE_ENTRIES: usize = 20_000;
const MAX_BUNDLE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_BUNDLE_MEMBER_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BUNDLE_DEPTH: usize = 32;
const MAX_RECEIPT_POLLS: u32 = 61;
const MAX_RECEIPT_WAIT: Duration = Duration::from_secs(180);
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(3);
const MAX_EARNINGS_POLLS: u32 = 11;
const MAX_EARNINGS_WAIT: Duration = Duration::from_secs(30);
const MAX_PREFLIGHT_WAIT: Duration = Duration::from_secs(30);
// Match the normal desktop's protocol-capped inference deadline: the server
// may legitimately consume 3,900 seconds across worker execution, independent
// verification, and reward approval, plus 60 seconds of client headroom.
const MAX_DISPATCH_WAIT: Duration = Duration::from_secs(3_960);
const MAX_FINAL_READ_WAIT: Duration = Duration::from_secs(30);
const MAX_FINAL_READ_POLLS: u32 = 11;
const MAX_TOTAL_WAIT: Duration = Duration::from_secs(4_300);
const MAX_TOKENS: u32 = 2;
const REQUIRED_REWARD_BASE: u64 = arc_types::economics::INFERENCE_ATTESTATION_REWARD;
const REQUIRED_REWARD_ARC: f64 = 2.5;
const BUILD_SOURCE_COMMIT: &str = env!("ARC_BUILD_SOURCE_COMMIT");

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub input: PathBuf,
    pub output: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssetBinding {
    id: u64,
    name: String,
    size: u64,
    sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssetBindings {
    app_archive: AssetBinding,
    app_archive_signature: AssetBinding,
    dmg: AssetBinding,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcceptanceInput {
    schema: String,
    repository: String,
    source_commit: String,
    release_version: String,
    release_id: u64,
    release_run_id: u64,
    release_run_attempt: u64,
    frontend_commit: String,
    pages_origin: String,
    frontend_config_sha256: String,
    rollout_manifest_sha256: String,
    expected_coordinator: String,
    expected_model_id: String,
    recovery_epoch: u64,
    validator_set_id: u64,
    validator_set_commitment: String,
    transaction_domain: String,
    validator_approvals_required: u64,
    expected_active_validators: u32,
    expected_registered_validators: u32,
    minimum_peers: u32,
    maximum_block_age_seconds: u64,
    issued_at_unix: i64,
    expires_at_unix: i64,
    challenge: String,
    assets: AssetBindings,
    expected_bundle: BundleIdentity,
    budgets: AcceptanceBudgets,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BundleIdentity {
    app_bundle_tree_sha256: String,
    executable_relative_path: String,
    executable_sha256: String,
    executable_size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcceptanceBudgets {
    dispatch_max_wait_ms: u64,
    earnings_max_polls: u32,
    earnings_max_wait_ms: u64,
    final_read_max_wait_ms: u64,
    final_read_max_polls: u32,
    preflight_max_wait_ms: u64,
    receipt_max_polls: u32,
    receipt_max_wait_ms: u64,
    total_max_wait_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeIdentity {
    app_data_relative_path: String,
    app_version: String,
    architecture: String,
    build_source_commit: String,
    environment_names: Vec<String>,
    environment_sha256: String,
    ipc_handlers_registered: bool,
    isolated_home_basename: String,
    operating_system: String,
    plugins_loaded: bool,
    tauri_builder_started: bool,
    webviews_created: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NegativeRouteChecks {
    job: bool,
    origin: bool,
    receipt_url: bool,
    transaction: bool,
    worker: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DispatchEvidence {
    chat_template: bool,
    count: u32,
    elapsed_ms: u64,
    max_wait_ms: u64,
    max_tokens: u32,
    prompt_sha256: String,
    result: InferenceResult,
    source_host: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PollEvidence {
    attempts: u32,
    elapsed_ms: u64,
    max_polls: u32,
    max_wait_ms: u64,
    receipt: InferenceSettlement,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EarningsEvidence {
    attempts: u32,
    elapsed_ms: u64,
    max_polls: u32,
    max_wait_ms: u64,
    value: Earnings,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptanceReceipt {
    schema: String,
    repository: String,
    source_commit: String,
    release_version: String,
    release_id: u64,
    release_run_id: u64,
    release_run_attempt: u64,
    frontend_commit: String,
    pages_origin: String,
    frontend_config_sha256: String,
    rollout_manifest_sha256: String,
    worker: String,
    expected_coordinator: String,
    expected_model_id: String,
    challenge: String,
    issued_at_unix: i64,
    expires_at_unix: i64,
    input_sha256: String,
    dispatch_attempt_sha256: String,
    started_at: String,
    completed_at: String,
    assets: AssetBindings,
    budgets: AcceptanceBudgets,
    bundle: BundleIdentity,
    runtime: RuntimeIdentity,
    preflight_network: NetworkOverview,
    preflight_model_id: String,
    dispatch: DispatchEvidence,
    negative_route_checks: NegativeRouteChecks,
    receipt_poll: PollEvidence,
    earnings: EarningsEvidence,
    projection: EarningsProjection,
    network: NetworkOverview,
    recent_blocks: RecentBlocks,
    transaction: TxLookup,
    final_read_attempts: u32,
    final_read_elapsed_ms: u64,
    immutable_session_origin: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DispatchAttempt {
    schema: String,
    challenge: String,
    input_sha256: String,
    executable_sha256: String,
    source_commit: String,
    source_host: String,
    dispatch_limit: u32,
    armed_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TreeEntry {
    kind: &'static str,
    mode: u32,
    path: String,
    sha256: Option<String>,
    size: Option<u64>,
    target: Option<String>,
}

pub(crate) fn request_from_args<I>(args: I) -> Result<Option<Request>, String>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    if !args.iter().any(|arg| arg == OsStr::new(MODE)) {
        return Ok(None);
    }
    if args.len() != 4 || args.get(1).map(OsString::as_os_str) != Some(OsStr::new(MODE)) {
        return Err(format!(
            "{MODE} requires exactly one sealed input and one create-only output"
        ));
    }
    let input = PathBuf::from(&args[2]);
    let output = PathBuf::from(&args[3]);
    if !input.is_absolute() || !output.is_absolute() || input == output {
        return Err("production acceptance paths must be distinct absolute paths".to_string());
    }
    if input.file_name() != Some(OsStr::new("DESKTOP-LIVE-INPUT.json"))
        || output.file_name() != Some(OsStr::new("PACKAGED-NATIVE-ACCEPTANCE.json"))
    {
        return Err("production acceptance input/output basenames are not canonical".to_string());
    }
    Ok(Some(Request { input, output }))
}

pub(crate) fn store_is_empty(store: &crate::store::Store) -> bool {
    store.identity.is_none() && store.config.is_none() && store.data_migration_notice.is_none()
}

/// Run the packaged native-core proof without starting Tauri at all.
///
/// The normal desktop and this mode share `AppState` plus the command-core
/// functions, but this evidence path deliberately has no renderer, IPC actor,
/// plugin, tray, updater, autostart hook, or background page that could race
/// its exact-one-dispatch state.
pub(crate) async fn run_standalone(request: &Request) -> Result<(), String> {
    let app_data = prepare_isolated_app_data()?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .retry(reqwest::retry::never())
        .build()
        .map_err(|error| format!("build fail-closed acceptance HTTP client: {error}"))?;
    let state = AppState {
        node: Arc::new(Mutex::new(crate::node_manager::NodeManager::new())),
        store: Arc::new(Mutex::new(crate::store::Store::default())),
        data_dir: Arc::new(Mutex::new(app_data.clone())),
        http,
        tier1_routes: Arc::new(Mutex::new(HashMap::new())),
        community_receipt_routes: Arc::new(Mutex::new(HashMap::new())),
        community_chain_host: Arc::new(Mutex::new(None)),
        community_inference_write: Arc::new(Mutex::new(())),
        chain_host: Arc::new(Mutex::new(None)),
        wallet_write: Arc::new(Mutex::new(())),
        has_tray: Arc::new(AtomicBool::new(false)),
        data_migration_error: Arc::new(Mutex::new(None)),
    };
    run(&state, &app_data, request).await
}

fn prepare_isolated_app_data() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "production acceptance requires an isolated HOME".to_string())?;
    if !home.is_absolute() || home.file_name() != Some(OsStr::new("isolated-home")) {
        return Err("production acceptance HOME is not the canonical isolated root".into());
    }
    validate_private_directory(&home)?;
    let canonical_home = fs::canonicalize(&home).map_err(|error| error.to_string())?;
    let temporary = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .ok_or_else(|| "production acceptance requires an isolated TMPDIR".to_string())?;
    validate_private_directory(&temporary)?;
    let canonical_temporary =
        fs::canonicalize(&temporary).map_err(|error| error.to_string())?;
    if !canonical_temporary.starts_with(&canonical_home) {
        return Err("production acceptance TMPDIR escaped the isolated HOME".into());
    }
    let mut current = home;
    for component in ["Library", "Application Support", "network.arc.desktop"] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .map_err(|error| format!("create {}: {error}", current.display()))?;
                #[cfg(unix)]
                fs::set_permissions(&current, fs::Permissions::from_mode(0o700))
                    .map_err(|error| format!("chmod {}: {error}", current.display()))?;
            }
            Err(error) => {
                return Err(format!("inspect {}: {error}", current.display()));
            }
        }
        validate_private_directory(&current)?;
    }
    let mut entries = fs::read_dir(&current)
        .map_err(|error| format!("enumerate isolated app-data {}: {error}", current.display()))?;
    if let Some(entry) = entries.next() {
        let entry = entry.map_err(|error| {
            format!("enumerate isolated app-data {}: {error}", current.display())
        })?;
        return Err(format!(
            "packaged production acceptance requires empty isolated app-data; found {}",
            entry.file_name().to_string_lossy()
        ));
    }
    Ok(current)
}

pub(crate) async fn run(
    state: &AppState,
    resolved_app_data: &Path,
    request: &Request,
) -> Result<(), String> {
    let watchdog_started = Instant::now();
    reject_ambient_network_authority()?;
    validate_isolated_app_data(state, resolved_app_data).await?;
    let (input, input_sha256) = read_and_validate_input(&request.input)?;
    validate_input(&input)?;
    validate_output_boundary(&request.input, &request.output)?;
    let bundle = current_bundle_identity()?;
    if bundle != input.expected_bundle {
        return Err(
            "running app bundle/executable differs from the sealed release artifact identity"
                .into(),
        );
    }
    let runtime = runtime_identity(resolved_app_data)?;
    let preflight_budget = MAX_PREFLIGHT_WAIT
        .min(MAX_TOTAL_WAIT.saturating_sub(watchdog_started.elapsed()));
    if preflight_budget.is_zero() {
        return Err("packaged native identity checks exhausted the 4,300-second watchdog".into());
    }
    let (preflight_network, preflight_model_id) = tokio::time::timeout(
        preflight_budget,
        async {
            tokio::join!(
                commands::fetch_network_overview_from_host_inner(
                    state,
                    &input.expected_coordinator
                ),
                commands::fetch_production_model_identity_from_host_inner(
                    state,
                    &input.expected_coordinator,
                    &input.expected_model_id,
                )
            )
        },
    )
    .await
    .map_err(|_| "production network preflight exceeded 30 seconds".to_string())?;
    let preflight_model_id = preflight_model_id?;
    validate_network_preflight(&input, &preflight_network)?;

    validate_plan_has_full_runtime(&input)?;
    let started_at = chrono::Utc::now();
    let attempt = DispatchAttempt {
        schema: ATTEMPT_SCHEMA.to_string(),
        challenge: input.challenge.clone(),
        input_sha256: input_sha256.clone(),
        executable_sha256: bundle.executable_sha256.clone(),
        source_commit: input.source_commit.clone(),
        source_host: input.expected_coordinator.clone(),
        dispatch_limit: 1,
        armed_at: started_at.to_rfc3339(),
    };
    let attempt_path = request
        .output
        .with_file_name("PACKAGED-NATIVE-DISPATCH-ATTEMPT.json");
    let dispatch_attempt_sha256 = write_create_only(&attempt_path, &attempt)?;

    let remaining = MAX_TOTAL_WAIT.saturating_sub(watchdog_started.elapsed());
    if remaining.is_zero() {
        return Err(
            "dispatch attempt marker is armed; packaged native preflight exhausted the 4,300-second watchdog; preserve all evidence and never retry this release/challenge"
                .into(),
        );
    }
    let armed_result = tokio::time::timeout(remaining, async {
        let prompt = acceptance_prompt(&input.challenge, &input_sha256);
        let dispatch_started = Instant::now();
        let result = tokio::time::timeout(
            MAX_DISPATCH_WAIT,
            commands::run_inference_via_exact_production_coordinator_inner(
                state,
                &input.expected_coordinator,
                &prompt,
                MAX_TOKENS,
                true,
            ),
        )
        .await
        .map_err(|_| {
            "production inference exceeded 3,960 seconds after dispatch was armed; outcome is ambiguous"
                .to_string()
        })??;
        let dispatch_elapsed_ms = elapsed_ms(dispatch_started);
        let provisional = validate_inference(&input, &prompt, &result)?;
        if state.community_chain_host.lock().await.as_deref()
            != Some(input.expected_coordinator.as_str())
        {
            return Err(
                "native inference did not establish the exact immutable session origin".into(),
            );
        }

        let negative_route_checks = negative_route_checks(state, &input, provisional).await?;
        let receipt_poll = poll_receipt(state, &input, &result, provisional).await?;
        validate_terminal_receipt(&input, &result, &receipt_poll.receipt)?;
        let earnings = poll_earnings(state, &input, &receipt_poll.receipt).await?;
        let (
            projection,
            network,
            recent_blocks,
            transaction,
            final_read_attempts,
            final_read_elapsed_ms,
        ) = poll_final_reads(state, &input, &receipt_poll.receipt, &earnings.value).await?;

        validate_plan_is_still_fresh(&input)?;
        if watchdog_started.elapsed() > MAX_TOTAL_WAIT {
            return Err("packaged native acceptance exceeded its sealed 4,300-second total budget".into());
        }

        let immutable_session_origin = state.community_chain_host.lock().await.as_deref()
            == Some(input.expected_coordinator.as_str());
        if !immutable_session_origin {
            return Err("native session origin moved after the final same-source reads".into());
        }

        let receipt = AcceptanceReceipt {
            schema: OUTPUT_SCHEMA.to_string(),
            repository: input.repository,
            source_commit: input.source_commit,
            release_version: input.release_version,
            release_id: input.release_id,
            release_run_id: input.release_run_id,
            release_run_attempt: input.release_run_attempt,
            frontend_commit: input.frontend_commit,
            pages_origin: input.pages_origin,
            frontend_config_sha256: input.frontend_config_sha256,
            rollout_manifest_sha256: input.rollout_manifest_sha256,
            worker: receipt_poll.receipt.worker.clone(),
            expected_coordinator: input.expected_coordinator,
            expected_model_id: input.expected_model_id,
            challenge: input.challenge,
            issued_at_unix: input.issued_at_unix,
            expires_at_unix: input.expires_at_unix,
            input_sha256,
            dispatch_attempt_sha256,
            started_at: started_at.to_rfc3339(),
            completed_at: chrono::Utc::now().to_rfc3339(),
            assets: input.assets,
            budgets: input.budgets,
            bundle,
            runtime,
            preflight_network,
            preflight_model_id,
            dispatch: DispatchEvidence {
                chat_template: true,
                count: 1,
                elapsed_ms: dispatch_elapsed_ms,
                max_wait_ms: MAX_DISPATCH_WAIT.as_millis() as u64,
                max_tokens: MAX_TOKENS,
                prompt_sha256: sha256_bytes(prompt.as_bytes()),
                result,
                source_host: EXPECTED_ORIGIN.to_string(),
            },
            negative_route_checks,
            receipt_poll,
            earnings,
            projection,
            network,
            recent_blocks,
            transaction,
            final_read_attempts,
            final_read_elapsed_ms,
            immutable_session_origin,
        };
        write_create_only(&request.output, &receipt).map(|_| ())
    })
    .await;
    match armed_result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!(
            "dispatch attempt marker is armed; {error}; preserve all evidence and never retry this release/challenge"
        )),
        Err(_) => Err(
            "dispatch attempt marker is armed; packaged native acceptance hit its 4,300-second watchdog; preserve all evidence and never retry this release/challenge"
                .into(),
        ),
    }
}

fn validate_input(input: &AcceptanceInput) -> Result<(), String> {
    if input.schema != INPUT_SCHEMA
        || input.repository != REPOSITORY
        || input.source_commit != BUILD_SOURCE_COMMIT
        || input.release_version != VERSION
        || input.release_id == 0
        || input.release_run_id == 0
        || input.release_run_attempt == 0
        || input.pages_origin != "https://ferrumvir.github.io/arc-chain"
        || input.expected_coordinator != EXPECTED_ORIGIN
    {
        return Err("packaged native input differs from its release/source/origin contract".into());
    }
    for (label, value, length) in [
        ("source commit", input.source_commit.as_str(), 40),
        ("frontend commit", input.frontend_commit.as_str(), 40),
        ("frontend config", input.frontend_config_sha256.as_str(), 64),
        (
            "rollout manifest",
            input.rollout_manifest_sha256.as_str(),
            64,
        ),
        ("challenge", input.challenge.as_str(), 64),
    ] {
        require_lower_hex(value, length, label)?;
    }
    require_hash32(&input.validator_set_commitment, "validator set commitment")?;
    require_hash32(&input.transaction_domain, "transaction domain")?;
    require_hash32(&input.expected_model_id, "expected production model")?;
    if input.recovery_epoch == 0
        || input.validator_set_id == 0
        || input.validator_approvals_required
            != arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED as u64
        || input.expected_active_validators != 6
        || input.expected_registered_validators != 6
        || input.minimum_peers < 5
        || input.maximum_block_age_seconds == 0
        || input.maximum_block_age_seconds > 300
    {
        return Err("packaged native input has an unsafe network/reward policy".into());
    }
    let now = chrono::Utc::now().timestamp();
    let required_remaining = i64::try_from(MAX_TOTAL_WAIT.as_secs())
        .unwrap_or(i64::MAX)
        .saturating_add(60);
    if input.issued_at_unix > now + 300
        || now > input.expires_at_unix
        || input.expires_at_unix <= input.issued_at_unix
        || input.expires_at_unix - input.issued_at_unix > 21_600
        || input.expires_at_unix.saturating_sub(now) < required_remaining
    {
        return Err(
            "packaged native input challenge is not within its bounded validity window".into(),
        );
    }
    validate_asset(
        &input.assets.app_archive,
        "arc-desktop-macos-arm64.app.tar.gz",
    )?;
    validate_asset(
        &input.assets.app_archive_signature,
        "arc-desktop-macos-arm64.app.tar.gz.sig",
    )?;
    validate_asset(&input.assets.dmg, "arc-desktop-macos-arm64.dmg")?;
    require_lower_hex(
        &input.expected_bundle.app_bundle_tree_sha256,
        64,
        "expected app bundle tree",
    )?;
    require_lower_hex(
        &input.expected_bundle.executable_sha256,
        64,
        "expected app executable",
    )?;
    if input.expected_bundle.executable_relative_path != "Contents/MacOS/arc-desktop"
        || input.expected_bundle.executable_size == 0
        || input.expected_bundle.executable_size > 512 * 1024 * 1024
    {
        return Err("sealed app executable identity is invalid".into());
    }
    let exact_budgets = AcceptanceBudgets {
        dispatch_max_wait_ms: MAX_DISPATCH_WAIT.as_millis() as u64,
        earnings_max_polls: MAX_EARNINGS_POLLS,
        earnings_max_wait_ms: MAX_EARNINGS_WAIT.as_millis() as u64,
        final_read_max_wait_ms: MAX_FINAL_READ_WAIT.as_millis() as u64,
        final_read_max_polls: MAX_FINAL_READ_POLLS,
        preflight_max_wait_ms: MAX_PREFLIGHT_WAIT.as_millis() as u64,
        receipt_max_polls: MAX_RECEIPT_POLLS,
        receipt_max_wait_ms: MAX_RECEIPT_WAIT.as_millis() as u64,
        total_max_wait_ms: MAX_TOTAL_WAIT.as_millis() as u64,
    };
    if input.budgets != exact_budgets {
        return Err("sealed packaged-native time/poll budgets differ from the binary".into());
    }
    Ok(())
}

fn validate_plan_is_still_fresh(input: &AcceptanceInput) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    if now < input.issued_at_unix - 300 || now > input.expires_at_unix {
        return Err("packaged native input expired before success evidence was sealed".into());
    }
    Ok(())
}

fn validate_plan_has_full_runtime(input: &AcceptanceInput) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    let required_remaining = i64::try_from(MAX_TOTAL_WAIT.as_secs())
        .unwrap_or(i64::MAX)
        .saturating_add(60);
    if input.expires_at_unix.saturating_sub(now) < required_remaining {
        return Err(
            "packaged native input no longer has the complete sealed runtime remaining before dispatch"
                .into(),
        );
    }
    Ok(())
}

fn validate_asset(asset: &AssetBinding, name: &str) -> Result<(), String> {
    if asset.id == 0 || asset.name != name || asset.size == 0 || asset.size > 2_147_483_648 {
        return Err(format!("release asset binding is invalid for {name}"));
    }
    require_lower_hex(&asset.sha256, 64, name)
}

fn validate_inference<'a>(
    input: &AcceptanceInput,
    prompt: &str,
    result: &'a InferenceResult,
) -> Result<&'a InferenceSettlement, String> {
    if result.input != prompt
        || result.output.trim().is_empty()
        || result.tokens_generated == 0
        || result.tokens_generated > MAX_TOKENS
        || !result.deterministic
        || !result.profile_bound
        || !result.quorum_verified
        || result.execution_profile
            != arc_types::transaction::CANONICAL_REWARD_INFERENCE_PROFILE
        || result.model_hash != input.expected_model_id
        || result.served_locally
        || result.coordinator.as_deref() != Some(input.expected_coordinator.as_str())
    {
        return Err("packaged native inference omitted exact verified production evidence".into());
    }
    require_hash32(&result.output_hash, "inference output hash")?;
    require_hash32(&result.model_hash, "inference model hash")?;
    let settlement = result
        .settlement
        .as_ref()
        .ok_or_else(|| "packaged native inference omitted community settlement".to_string())?;
    if settlement.tx_type != "0x25"
        || !settlement.submitted
        || !matches!(
            settlement.status.as_str(),
            "pending_mined_receipt" | "mined_success"
        )
        || result.routed_via != format!("community:{}", settlement.worker)
        || settlement.receipt_url != format!("/community/reward_receipt/{}", settlement.tx_hash)
    {
        return Err("packaged native inference settlement identity is malformed".into());
    }
    require_hash32(&settlement.tx_hash, "settlement transaction")?;
    require_hash32(&settlement.job_id, "settlement job")?;
    require_hash32(&settlement.worker, "settlement worker")?;
    Ok(settlement)
}

async fn negative_route_checks(
    state: &AppState,
    input: &AcceptanceInput,
    settlement: &InferenceSettlement,
) -> Result<NegativeRouteChecks, String> {
    async fn rejected(
        state: &AppState,
        source: &str,
        tx: &str,
        job: &str,
        worker: &str,
        url: &str,
        expected_error: &str,
    ) -> bool {
        matches!(
            commands::fetch_community_reward_receipt_inner(state, source, tx, job, worker, url)
                .await,
            Err(error) if error == expected_error
        )
    }
    let wrong_tx = mutate_hash(&settlement.tx_hash);
    let wrong_job = mutate_hash(&settlement.job_id);
    let wrong_worker = mutate_hash(&settlement.worker);
    let wrong_url = format!("/community/reward_receipt/{wrong_tx}");
    let checks = NegativeRouteChecks {
        origin: rejected(
            state,
            "https://149.28.32.76",
            &settlement.tx_hash,
            &settlement.job_id,
            &settlement.worker,
            &settlement.receipt_url,
            commands::RECEIPT_SOURCE_REJECTED,
        )
        .await,
        transaction: rejected(
            state,
            &input.expected_coordinator,
            &wrong_tx,
            &settlement.job_id,
            &settlement.worker,
            &wrong_url,
            commands::RECEIPT_ROUTE_MISSING,
        )
        .await,
        job: rejected(
            state,
            &input.expected_coordinator,
            &settlement.tx_hash,
            &wrong_job,
            &settlement.worker,
            &settlement.receipt_url,
            commands::RECEIPT_ROUTE_MISMATCH,
        )
        .await,
        worker: rejected(
            state,
            &input.expected_coordinator,
            &settlement.tx_hash,
            &settlement.job_id,
            &wrong_worker,
            &settlement.receipt_url,
            commands::RECEIPT_ROUTE_MISMATCH,
        )
        .await,
        receipt_url: rejected(
            state,
            &input.expected_coordinator,
            &settlement.tx_hash,
            &settlement.job_id,
            &settlement.worker,
            &wrong_url,
            commands::RECEIPT_ROUTE_MISMATCH,
        )
        .await,
    };
    if !(checks.origin && checks.transaction && checks.job && checks.worker && checks.receipt_url) {
        return Err("a mutated reward receipt identity reached the native network path".into());
    }
    Ok(checks)
}

async fn poll_receipt(
    state: &AppState,
    input: &AcceptanceInput,
    result: &InferenceResult,
    provisional: &InferenceSettlement,
) -> Result<PollEvidence, String> {
    let started = Instant::now();
    for attempt in 1..=MAX_RECEIPT_POLLS {
        let remaining = MAX_RECEIPT_WAIT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let fetched = tokio::time::timeout(
            remaining,
            commands::fetch_community_reward_receipt_inner(
                state,
                &input.expected_coordinator,
                &provisional.tx_hash,
                &provisional.job_id,
                &provisional.worker,
                &provisional.receipt_url,
            ),
        )
        .await;
        let fetched = match fetched {
            Ok(Ok(receipt)) => receipt,
            Ok(Err(error))
                if matches!(
                    error.as_str(),
                    commands::RECEIPT_SOURCE_REJECTED
                        | commands::RECEIPT_ROUTE_MISSING
                        | commands::RECEIPT_ROUTE_MISMATCH
                ) =>
            {
                return Err(error);
            }
            Ok(Err(_)) | Err(_) => {
                if attempt < MAX_RECEIPT_POLLS && started.elapsed() < MAX_RECEIPT_WAIT {
                    tokio::time::sleep(
                        RECEIPT_POLL_INTERVAL
                            .min(MAX_RECEIPT_WAIT.saturating_sub(started.elapsed())),
                    )
                    .await;
                    continue;
                }
                break;
            }
        };
        if fetched.status == "mined_success" {
            return Ok(PollEvidence {
                attempts: attempt,
                elapsed_ms: elapsed_ms(started),
                max_polls: MAX_RECEIPT_POLLS,
                max_wait_ms: MAX_RECEIPT_WAIT.as_millis() as u64,
                receipt: fetched,
            });
        }
        if fetched.status != "pending_mined_receipt" {
            return Err(format!(
                "community reward reached terminal non-success state {}",
                fetched.status
            ));
        }
        if attempt < MAX_RECEIPT_POLLS {
            tokio::time::sleep(
                RECEIPT_POLL_INTERVAL.min(MAX_RECEIPT_WAIT.saturating_sub(started.elapsed())),
            )
            .await;
        }
    }
    let _ = result;
    Err("community reward did not mine within 61 polls / 180 seconds".into())
}

async fn poll_final_reads(
    state: &AppState,
    input: &AcceptanceInput,
    receipt: &InferenceSettlement,
    earnings: &Earnings,
) -> Result<
    (
        EarningsProjection,
        NetworkOverview,
        RecentBlocks,
        TxLookup,
        u32,
        u64,
    ),
    String,
> {
    let started = Instant::now();
    let mut last_error = "no final read completed".to_string();
    for attempt in 1..=MAX_FINAL_READ_POLLS {
        let remaining = MAX_FINAL_READ_WAIT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let reads = tokio::time::timeout(remaining, async {
            tokio::join!(
                commands::fetch_earnings_projection_inner(state, Some(&receipt.worker)),
                commands::fetch_network_overview_inner(state),
                commands::fetch_recent_blocks_inner(state, 100),
                commands::lookup_tx_inner(state, &receipt.tx_hash),
            )
        })
        .await;
        if let Ok((projection, network, recent_blocks, transaction)) = reads {
            let validation = validate_projection(input, receipt, earnings, &projection)
                .and_then(|_| validate_network(input, receipt, &network, &recent_blocks))
                .and_then(|_| validate_transaction(input, receipt, &transaction));
            match validation {
                Ok(()) => {
                    return Ok((
                        projection,
                        network,
                        recent_blocks,
                        transaction,
                        attempt,
                        elapsed_ms(started),
                    ));
                }
                Err(error) => last_error = error,
            }
        } else {
            last_error = "one or more same-origin final reads exceeded the remaining budget".into();
        }
        if attempt < MAX_FINAL_READ_POLLS && started.elapsed() < MAX_FINAL_READ_WAIT {
            tokio::time::sleep(
                RECEIPT_POLL_INTERVAL.min(MAX_FINAL_READ_WAIT.saturating_sub(started.elapsed())),
            )
            .await;
        }
    }
    Err(format!(
        "final same-origin native reads did not form one valid snapshot within 11 polls / 30 seconds: {last_error}"
    ))
}

fn validate_terminal_receipt(
    input: &AcceptanceInput,
    result: &InferenceResult,
    receipt: &InferenceSettlement,
) -> Result<(), String> {
    let initial = result.settlement.as_ref().unwrap();
    let expected_input_hash = format!("0x{}", arc_crypto::hash_bytes(result.input.as_bytes()).to_hex());
    if receipt.status != "mined_success"
        || receipt.tx_type != "0x25"
        || receipt.tx_hash != initial.tx_hash
        || receipt.job_id != initial.job_id
        || receipt.worker != initial.worker
        || receipt.receipt_url != initial.receipt_url
        || !receipt.submitted
        || !receipt.included
        || !receipt.confirmed
        || receipt.success != Some(true)
        || receipt.reward_base != Some(REQUIRED_REWARD_BASE)
        || receipt.reward_arc != Some(REQUIRED_REWARD_ARC)
        || receipt.model_id != result.model_hash
        || receipt.input_hash != expected_input_hash
        || receipt.output_hash != result.output_hash
        || receipt.recovery_epoch != Some(input.recovery_epoch)
        || receipt.validator_set_id != Some(input.validator_set_id)
        || receipt.validator_set_commitment != input.validator_set_commitment
        || receipt.transaction_domain != input.transaction_domain
        || receipt.validator_approvals.unwrap_or(0) < input.validator_approvals_required
        || receipt.evidence_source != "successful mined CommunityInferenceReward receipt"
        || receipt.block_height.is_none()
        || receipt.index.is_none()
    {
        return Err(
            "terminal native reward receipt differs from the exact inference/reward contract"
                .into(),
        );
    }
    for (label, value) in [
        ("receipt model", receipt.model_id.as_str()),
        ("receipt input", receipt.input_hash.as_str()),
        ("receipt output", receipt.output_hash.as_str()),
        ("receipt assignment", receipt.assignment_epoch.as_str()),
        ("receipt domain", receipt.transaction_domain.as_str()),
        (
            "receipt validator set",
            receipt.validator_set_commitment.as_str(),
        ),
        (
            "receipt block",
            receipt.block_hash.as_deref().unwrap_or_default(),
        ),
    ] {
        require_hash32(value, label)?;
    }
    Ok(())
}

async fn poll_earnings(
    state: &AppState,
    input: &AcceptanceInput,
    receipt: &InferenceSettlement,
) -> Result<EarningsEvidence, String> {
    let started = Instant::now();
    for attempt in 1..=MAX_EARNINGS_POLLS {
        let remaining = MAX_EARNINGS_WAIT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let earnings = tokio::time::timeout(
            remaining,
            commands::fetch_earnings_inner(state, Some(&receipt.worker)),
        )
        .await
        .map_err(|_| "native earnings poll exceeded the 30-second contract".to_string())?;
        if validate_earnings(input, receipt, &earnings).is_ok() {
            return Ok(EarningsEvidence {
                attempts: attempt,
                elapsed_ms: elapsed_ms(started),
                max_polls: MAX_EARNINGS_POLLS,
                max_wait_ms: MAX_EARNINGS_WAIT.as_millis() as u64,
                value: earnings,
            });
        }
        if attempt < MAX_EARNINGS_POLLS && started.elapsed() < MAX_EARNINGS_WAIT {
            tokio::time::sleep(
                RECEIPT_POLL_INTERVAL.min(MAX_EARNINGS_WAIT.saturating_sub(started.elapsed())),
            )
            .await;
        }
    }
    Err("native earnings never included the exact newly mined reward".into())
}

fn validate_earnings(
    input: &AcceptanceInput,
    receipt: &InferenceSettlement,
    earnings: &Earnings,
) -> Result<(), String> {
    if !earnings.from_chain
        || earnings.unavailable_reason.is_some()
        || earnings.recovery_epoch != Some(input.recovery_epoch)
        || earnings.validator_set_id != Some(input.validator_set_id)
        || earnings.attestations != earnings.confirmed_receipts.len() as u64
        || (earnings.total_arc - earnings.attestations as f64 * REQUIRED_REWARD_ARC).abs() > 1e-9
        || earnings.receipt_source.as_deref()
            != Some("scan of this node's in-memory full_transactions map")
    {
        return Err("native earnings summary is unavailable or internally inconsistent".into());
    }
    let rows: Vec<_> = earnings
        .confirmed_receipts
        .iter()
        .filter(|row| row.tx_hash == receipt.tx_hash)
        .collect();
    if rows.len() != 1 {
        return Err("native earnings does not contain the exact reward once".into());
    }
    let row = rows[0];
    if row.job_id != receipt.job_id
        || Some(row.block_height) != receipt.block_height
        || Some(row.block_hash.as_str()) != receipt.block_hash.as_deref()
        || row.reward_base != REQUIRED_REWARD_BASE
        || row.reward_arc != REQUIRED_REWARD_ARC
        || row.receipt_url != receipt.receipt_url
        || row.recovery_epoch != Some(input.recovery_epoch)
        || row.validator_set_id != Some(input.validator_set_id)
    {
        return Err("native earnings row differs from the independently polled receipt".into());
    }
    Ok(())
}

fn validate_projection(
    input: &AcceptanceInput,
    receipt: &InferenceSettlement,
    earnings: &Earnings,
    projection: &EarningsProjection,
) -> Result<(), String> {
    let value = projection
        .projected_daily_arc
        .is_some_and(|value| value.is_finite() && value >= 0.0);
    let reason = projection
        .projected_daily_unavailable_reason
        .as_deref()
        .is_some_and(|reason| !reason.trim().is_empty());
    if projection.source_host != input.expected_coordinator
        || projection.unavailable.is_some()
        || projection.reward_per_attestation != Some(REQUIRED_REWARD_ARC)
        || projection.reward_rate_source != "chain"
        || projection.community_rewards_enabled != Some(true)
        || projection.issuance_ready_for_worker != Some(true)
        || projection.reward_program.as_deref()
            != Some("protocol-capped testnet promotional compute subsidy")
        || projection.reward_is_customer_demand != Some(false)
        || projection
            .reward_policy_hash
            .as_deref()
            .is_none_or(|hash| !is_hash32(hash))
        || value == reason
        || projection.attestations_total < earnings.attestations
        || (projection.attestations_total == earnings.attestations
            && (projection.projected_daily_arc != earnings.projected_daily_arc
                || projection.projected_daily_unavailable_reason
                    != earnings.projected_daily_unavailable_reason))
        || projection.attestations_total == 0
        || projection
            .first_attestation_block
            .is_none_or(|height| height > receipt.block_height.unwrap_or(0))
        || !is_hash32(&receipt.worker)
    {
        return Err("native earnings projection is not an exact ready value/reason XOR".into());
    }
    Ok(())
}

fn validate_network_preflight(
    input: &AcceptanceInput,
    network: &NetworkOverview,
) -> Result<(), String> {
    if network.source_host != input.expected_coordinator
        || network.unavailable.is_some()
        || network.host_version.as_deref() != Some(VERSION)
        || network.height.is_none_or(|height| height == 0)
        || network
            .last_block_age_secs
            .is_none_or(|age| age > input.maximum_block_age_seconds)
        || network.is_block_producing != Some(true)
        || network.validators_active != Some(input.expected_active_validators)
        || network.validators_registered != Some(input.expected_registered_validators)
        || network
            .peers
            .is_none_or(|peers| peers < input.minimum_peers)
        || !network_validator_set_is_exact(network, input.expected_registered_validators)
    {
        return Err(
            "production coordinator failed the read-only version/freshness/6-validator preflight"
                .into(),
        );
    }
    Ok(())
}

fn network_validator_set_is_exact(network: &NetworkOverview, expected: u32) -> bool {
    if network.validators.len() != expected as usize {
        return false;
    }
    let mut identities = HashSet::with_capacity(network.validators.len());
    network.validators.iter().all(|validator| {
        validator.active
            && validator.stake > 0
            && require_lower_hex(&validator.address, 64, "validator identity").is_ok()
            && identities.insert(&validator.address)
    })
}

fn validate_network(
    input: &AcceptanceInput,
    receipt: &InferenceSettlement,
    network: &NetworkOverview,
    recent: &RecentBlocks,
) -> Result<(), String> {
    let receipt_height = receipt.block_height.unwrap();
    if network.source_host != input.expected_coordinator
        || network.unavailable.is_some()
        || network.host_version.as_deref() != Some(VERSION)
        || network.height.is_none_or(|height| height < receipt_height)
        || network
            .last_block_age_secs
            .is_none_or(|age| age > input.maximum_block_age_seconds)
        || network.is_block_producing != Some(true)
        || network.validators_active != Some(input.expected_active_validators)
        || network.validators_registered != Some(input.expected_registered_validators)
        || network
            .peers
            .is_none_or(|peers| peers < input.minimum_peers)
        || !network_validator_set_is_exact(network, input.expected_registered_validators)
        || recent.source_host != input.expected_coordinator
        || recent.unavailable.is_some()
        || recent.blocks.is_empty()
        || !recent.blocks.iter().any(|block| {
            Some(block.height) == receipt.block_height
                && receipt
                    .block_hash
                    .as_deref()
                    .and_then(|hash| hash.strip_prefix("0x"))
                    == Some(block.hash.as_str())
        })
    {
        return Err("native network view is not same-source, six-validator, and fresh".into());
    }
    Ok(())
}

fn validate_transaction(
    input: &AcceptanceInput,
    receipt: &InferenceSettlement,
    transaction: &TxLookup,
) -> Result<(), String> {
    if transaction.source_host != input.expected_coordinator
        || transaction.unavailable.is_some()
        || transaction.status != "mined"
        || format!("0x{}", transaction.hash) != receipt.tx_hash
        || transaction.block_height != receipt.block_height
        || transaction
            .block_hash
            .as_ref()
            .map(|hash| format!("0x{hash}"))
            != receipt.block_hash
        || transaction.tx_index.map(u64::from) != receipt.index
        || transaction.success != Some(true)
    {
        return Err(
            "native transaction lookup differs from the exact reward block identity".into(),
        );
    }
    Ok(())
}

pub(crate) fn reject_ambient_network_authority() -> Result<(), String> {
    reviewed_environment_identity(std::env::vars_os()).map(|_| ())
}

fn reviewed_environment_identity<I>(variables: I) -> Result<(Vec<String>, String), String>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    // This mode is launched through `/usr/bin/env -i`. Rejecting every name
    // outside this tiny data-only allowlist covers both known injection/trust
    // variables (TAURI_AUTOMATION, TAURI_WEBVIEW_AUTOMATION, DYLD_*, LD_*,
    // SSL_CERT_*, OPENSSL_*, language hooks, proxies) and future aliases we do
    // not know to enumerate today.
    const ALLOWED: [&str; 6] = ["HOME", "LANG", "LC_ALL", "PATH", "RUST_LOG", "TMPDIR"];
    let mut normalized = Vec::new();
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for (raw_name, raw_value) in variables {
        let name = raw_name
            .into_string()
            .map_err(|_| "production acceptance refuses a non-Unicode environment name")?;
        let value = raw_value.into_string().map_err(|_| {
            format!("production acceptance refuses non-Unicode environment value in {name}")
        })?;
        if !ALLOWED.contains(&name.as_str()) {
            return Err(format!(
                "production acceptance refuses unreviewed runtime/network authority in {name}"
            ));
        }
        if !seen.insert(name.clone()) {
            return Err(format!(
                "production acceptance environment contains duplicate {name}"
            ));
        }
        match name.as_str() {
            "PATH" if value != "/usr/bin:/bin:/usr/sbin:/sbin" => {
                return Err("production acceptance PATH is not the reviewed system-only value".into())
            }
            "LANG" | "LC_ALL" if value != "C" => {
                return Err(format!("production acceptance {name} must equal C"))
            }
            "HOME" | "TMPDIR" if !Path::new(&value).is_absolute() => {
                return Err(format!("production acceptance {name} must be absolute"))
            }
            "RUST_LOG"
                if value.len() > 128
                    || value
                        .bytes()
                        .any(|byte| !(0x20..=0x7e).contains(&byte)) =>
            {
                return Err("production acceptance RUST_LOG is not bounded printable ASCII".into())
            }
            _ => {}
        }
        names.push(name.clone());
        normalized.push(format!("{name}={value}"));
    }
    names.sort_unstable();
    normalized.sort_unstable();
    for required in ["HOME", "LANG", "LC_ALL", "PATH", "TMPDIR"] {
        if !names.iter().any(|name| name == required) {
            return Err(format!(
                "production acceptance sanitized environment omitted {required}"
            ));
        }
    }
    let mut bytes = Vec::new();
    for entry in normalized {
        bytes.extend_from_slice(entry.as_bytes());
        bytes.push(0);
    }
    Ok((names, sha256_bytes(&bytes)))
}

async fn validate_isolated_app_data(state: &AppState, data: &Path) -> Result<(), String> {
    let store = state.store.lock().await;
    if !store_is_empty(&store) {
        return Err("production acceptance app-data Store was not empty".into());
    }
    drop(store);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "production acceptance requires an isolated HOME".to_string())?;
    if !home.is_absolute() || home.file_name() != Some(OsStr::new("isolated-home")) {
        return Err("production acceptance HOME is not the canonical isolated root".into());
    }
    validate_private_directory(&home)?;
    let home = fs::canonicalize(home).map_err(|error| error.to_string())?;
    let data = fs::canonicalize(data).map_err(|error| error.to_string())?;
    if !data.starts_with(&home) {
        return Err("Tauri app-data directory escaped the isolated HOME".into());
    }
    Ok(())
}

fn runtime_identity(app_data: &Path) -> Result<RuntimeIdentity, String> {
    let (environment_names, environment_sha256) =
        reviewed_environment_identity(std::env::vars_os())?;
    let home = fs::canonicalize(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "isolated HOME vanished".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let app_data = fs::canonicalize(app_data).map_err(|error| error.to_string())?;
    let relative = app_data
        .strip_prefix(&home)
        .map_err(|_| "app-data path escaped isolated HOME".to_string())?;
    Ok(RuntimeIdentity {
        app_data_relative_path: relative.to_string_lossy().replace('\\', "/"),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        build_source_commit: BUILD_SOURCE_COMMIT.to_string(),
        environment_names,
        environment_sha256,
        ipc_handlers_registered: false,
        isolated_home_basename: "isolated-home".to_string(),
        operating_system: std::env::consts::OS.to_string(),
        plugins_loaded: false,
        tauri_builder_started: false,
        webviews_created: 0,
    })
}

fn read_and_validate_input(path: &Path) -> Result<(AcceptanceInput, String), String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(path)
        .map_err(|error| format!("open sealed input {}: {error}", path.display()))?;
    let before = file
        .metadata()
        .map_err(|error| format!("stat sealed input fd: {error}"))?;
    validate_private_input_metadata(&before, path)?;
    let path_before = fs::symlink_metadata(path)
        .map_err(|error| format!("lstat sealed input {}: {error}", path.display()))?;
    if !same_file_identity(&before, &path_before) {
        return Err("sealed input path and opened file identity differ".into());
    }
    let mut raw = Vec::with_capacity(before.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|error| format!("read sealed input fd: {error}"))?;
    if raw.len() as u64 != before.len() {
        return Err("sealed input length changed or exceeded its bound while read".into());
    }
    let after = file
        .metadata()
        .map_err(|error| format!("restat sealed input fd: {error}"))?;
    let path_after = fs::symlink_metadata(path)
        .map_err(|error| format!("relstat sealed input {}: {error}", path.display()))?;
    if !same_file_snapshot(&before, &after)
        || !same_file_identity(&after, &path_after)
        || !same_file_snapshot(&path_before, &path_after)
    {
        return Err("sealed input changed or its path was replaced while read".into());
    }
    let value: Value = serde_json::from_slice(&raw)
        .map_err(|error| format!("parse packaged native input: {error}"))?;
    if canonical_json(&value)? != raw {
        return Err("packaged native input is not canonical sorted JSON".into());
    }
    let input: AcceptanceInput = serde_json::from_value(value)
        .map_err(|error| format!("validate packaged native input: {error}"))?;
    Ok((input, sha256_bytes(&raw)))
}

fn validate_output_boundary(input: &Path, output: &Path) -> Result<(), String> {
    if output.exists() || fs::symlink_metadata(output).is_ok() {
        return Err("packaged native acceptance output already exists".into());
    }
    let input_parent = fs::canonicalize(
        input
            .parent()
            .ok_or_else(|| "input has no parent".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let output_parent = fs::canonicalize(
        output
            .parent()
            .ok_or_else(|| "output has no parent".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if input_parent != output_parent {
        return Err("packaged native input and output must share one private directory".into());
    }
    validate_private_directory(&input_parent)
}

fn write_create_only<T: Serialize>(path: &Path, value: &T) -> Result<String, String> {
    let raw = canonical_json(value)?;
    let parent_path = path
        .parent()
        .ok_or_else(|| "create-only output has no parent".to_string())?;
    let name = path
        .file_name()
        .ok_or_else(|| "create-only output has no basename".to_string())?;
    validate_private_directory(parent_path)?;
    let directory = File::open(parent_path)
        .map_err(|error| format!("open output directory {}: {error}", parent_path.display()))?;
    let directory_before = directory
        .metadata()
        .map_err(|error| format!("stat output directory fd: {error}"))?;
    let directory_path_before = fs::symlink_metadata(parent_path)
        .map_err(|error| format!("lstat output directory: {error}"))?;
    if !same_file_identity(&directory_before, &directory_path_before) {
        return Err("output directory path and opened directory identity differ".into());
    }
    let mut file = create_file_at(&directory, name)?;
    file.write_all(&raw)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    #[cfg(unix)]
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o400) } != 0 {
        return Err(format!(
            "fchmod {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    let output_metadata = file
        .metadata()
        .map_err(|error| format!("stat output fd: {error}"))?;
    validate_private_output_metadata(&output_metadata, path, raw.len() as u64)?;
    directory
        .sync_all()
        .map_err(|error| format!("sync output directory: {error}"))?;
    let directory_after = directory
        .metadata()
        .map_err(|error| format!("restat output directory fd: {error}"))?;
    let directory_path_after = fs::symlink_metadata(parent_path)
        .map_err(|error| format!("relstat output directory: {error}"))?;
    if !same_file_identity(&directory_before, &directory_after)
        || !same_file_identity(&directory_after, &directory_path_after)
        || !same_file_identity(&directory_path_before, &directory_path_after)
    {
        return Err("output directory changed or its path was replaced while sealing".into());
    }
    let reopened = open_file_at(&directory, name)?;
    let reopened_metadata = reopened
        .metadata()
        .map_err(|error| format!("restat create-only output by basename: {error}"))?;
    if !same_file_snapshot(&output_metadata, &reopened_metadata) {
        return Err("create-only output basename no longer names the sealed file".into());
    }
    validate_private_directory(parent_path)?;
    Ok(sha256_bytes(&raw))
}

#[cfg(unix)]
fn create_file_at(directory: &File, name: &OsStr) -> Result<File, String> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| "create-only output basename contains NUL".to_string())?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            0o400,
        )
    };
    if fd < 0 {
        return Err(format!(
            "create-only openat failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_file_at(directory: &File, name: &OsStr) -> Result<File, String> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| "create-only output basename contains NUL".to_string())?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "reopen create-only output by basename failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
fn create_file_at(_directory: &File, name: &OsStr) -> Result<File, String> {
    let _ = name;
    Err("packaged native create-only output requires Unix openat semantics".into())
}

#[cfg(not(unix))]
fn open_file_at(_directory: &File, name: &OsStr) -> Result<File, String> {
    let _ = name;
    Err("packaged native output verification requires Unix openat semantics".into())
}

fn acceptance_prompt(challenge: &str, input_sha256: &str) -> String {
    format!(
        "ARC packaged v0.8.0 production acceptance challenge {challenge} input {input_sha256}"
    )
}

fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    fn sorted(value: Value) -> Value {
        match value {
            Value::Array(values) => Value::Array(values.into_iter().map(sorted).collect()),
            Value::Object(values) => {
                let mut keys: Vec<_> = values.into_iter().collect();
                keys.sort_by(|left, right| left.0.cmp(&right.0));
                Value::Object(
                    keys.into_iter()
                        .map(|(key, value)| (key, sorted(value)))
                        .collect(),
                )
            }
            value => value,
        }
    }
    let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    let mut raw = serde_json::to_vec(&sorted(value)).map_err(|error| error.to_string())?;
    raw.push(b'\n');
    Ok(raw)
}

fn current_bundle_identity() -> Result<BundleIdentity, String> {
    if std::env::consts::OS != "macos" || std::env::consts::ARCH != "aarch64" {
        return Err("packaged native acceptance requires the macOS arm64 release app".into());
    }
    let executable = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("resolve current executable: {error}"))?;
    validate_regular_executable(&executable)?;
    let macos = executable
        .parent()
        .filter(|path| path.file_name() == Some(OsStr::new("MacOS")))
        .ok_or_else(|| {
            "current executable is not inside an app bundle MacOS directory".to_string()
        })?;
    let contents = macos
        .parent()
        .filter(|path| path.file_name() == Some(OsStr::new("Contents")))
        .ok_or_else(|| "current executable is not inside app bundle Contents".to_string())?;
    let bundle = contents
        .parent()
        .filter(|path| path.extension() == Some(OsStr::new("app")))
        .ok_or_else(|| "current executable parent is not a .app bundle".to_string())?;
    let executable_relative_path = executable
        .strip_prefix(bundle)
        .map_err(|_| "current executable escaped its app bundle".to_string())?
        .to_string_lossy()
        .replace('\\', "/");
    let executable_size = fs::metadata(&executable)
        .map_err(|error| error.to_string())?
        .len();
    Ok(BundleIdentity {
        app_bundle_tree_sha256: bundle_tree_sha256(bundle)?,
        executable_relative_path,
        executable_sha256: sha256_file(&executable)?,
        executable_size,
    })
}

fn bundle_tree_sha256(root: &Path) -> Result<String, String> {
    let canonical_root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    collect_tree(
        &canonical_root,
        &canonical_root,
        0,
        &mut total_bytes,
        &mut entries,
    )?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(sha256_bytes(&canonical_json(&entries)?))
}

fn collect_tree(
    root: &Path,
    directory: &Path,
    depth: usize,
    total_bytes: &mut u64,
    entries: &mut Vec<TreeEntry>,
) -> Result<(), String> {
    if depth > MAX_BUNDLE_DEPTH {
        return Err("app bundle exceeds the reviewed traversal depth".into());
    }
    let mut children = Vec::new();
    for child in fs::read_dir(directory).map_err(|error| error.to_string())? {
        children.push(child.map_err(|error| error.to_string())?);
    }
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        if entries.len() >= MAX_BUNDLE_ENTRIES {
            return Err("app bundle exceeds the reviewed member count".into());
        }
        let path = child.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "bundle member escaped root".to_string())?
            .to_str()
            .ok_or_else(|| "bundle path is not UTF-8".to_string())?
            .replace('\\', "/");
        if relative.contains(['\t', '\n', '\r']) {
            return Err("bundle path contains a control separator".into());
        }
        #[cfg(unix)]
        let mode = metadata.permissions().mode() & 0o7777;
        #[cfg(not(unix))]
        let mode = 0;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).map_err(|error| error.to_string())?;
            if target.is_absolute() {
                return Err("bundle contains an absolute symbolic link".into());
            }
            let target_text = target
                .to_str()
                .ok_or_else(|| "bundle link target is not UTF-8".to_string())?
                .to_string();
            if target_text.contains(['\t', '\n', '\r']) {
                return Err("bundle link target contains a control separator".into());
            }
            let resolved = fs::canonicalize(path.parent().unwrap().join(&target))
                .map_err(|error| format!("resolve bundle link {relative}: {error}"))?;
            if !resolved.starts_with(root) {
                return Err("bundle symbolic link escapes the app".into());
            }
            entries.push(TreeEntry {
                kind: "symlink",
                mode,
                path: relative,
                sha256: None,
                size: None,
                target: Some(target_text),
            });
        } else if metadata.is_dir() {
            entries.push(TreeEntry {
                kind: "directory",
                mode,
                path: relative,
                sha256: None,
                size: None,
                target: None,
            });
            collect_tree(root, &path, depth + 1, total_bytes, entries)?;
        } else if metadata.is_file() {
            if metadata.len() > MAX_BUNDLE_MEMBER_BYTES {
                return Err("app bundle member exceeds the reviewed size bound".into());
            }
            *total_bytes = total_bytes
                .checked_add(metadata.len())
                .ok_or_else(|| "app bundle byte count overflowed".to_string())?;
            if *total_bytes > MAX_BUNDLE_BYTES {
                return Err("app bundle exceeds the reviewed total byte bound".into());
            }
            entries.push(TreeEntry {
                kind: "file",
                mode,
                path: relative,
                sha256: Some(sha256_file(&path)?),
                size: Some(metadata.len()),
                target: None,
            });
        } else {
            return Err("app bundle contains a special filesystem entry".into());
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let before = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let after = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    if (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.ctime(),
    ) != (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.ctime(),
    ) {
        return Err(format!("{} changed while it was hashed", path.display()));
    }
    #[cfg(not(unix))]
    if before.len() != after.len() {
        return Err(format!("{} changed while it was hashed", path.display()));
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_private_input_metadata(metadata: &fs::Metadata, path: &Path) -> Result<(), String> {
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_INPUT_BYTES {
        return Err(format!("{} is not a bounded regular file", path.display()));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o400
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(format!(
            "{} is not private, owned, mode 0400, link-count one",
            path.display()
        ));
    }
    Ok(())
}

fn validate_private_output_metadata(
    metadata: &fs::Metadata,
    path: &Path,
    exact_size: u64,
) -> Result<(), String> {
    if !metadata.is_file() || metadata.len() != exact_size || exact_size == 0 {
        return Err(format!(
            "{} output fd is not the exact written regular file",
            path.display()
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o400
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(format!(
            "{} output is not private, owned, mode 0400, link-count one",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
}

#[cfg(unix)]
fn same_file_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file_identity(left, right)
        && left.len() == right.len()
        && left.mode() == right.mode()
        && left.uid() == right.uid()
        && left.gid() == right.gid()
        && left.nlink() == right.nlink()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_file_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file_identity(left, right) && left.modified().ok() == right.modified().ok()
}

fn validate_private_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("{} is not a real directory", path.display()));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o700
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(format!(
            "{} is not a private owned mode-0700 directory",
            path.display()
        ));
    }
    Ok(())
}

fn validate_regular_executable(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("current executable is not a regular file".into());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err("current executable lacks an execute bit".into());
    }
    Ok(())
}

fn require_hash32(value: &str, label: &str) -> Result<(), String> {
    if value.len() != 66 || !value.starts_with("0x") {
        return Err(format!("{label} is not canonical 0x-prefixed 32-byte hex"));
    }
    require_lower_hex(&value[2..], 64, label)
}

fn is_hash32(value: &str) -> bool {
    require_hash32(value, "hash").is_ok()
}

fn require_lower_hex(value: &str, length: usize, label: &str) -> Result<(), String> {
    if value.len() != length
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("{label} is not exact lowercase hex"));
    }
    Ok(())
}

fn mutate_hash(value: &str) -> String {
    let mut bytes = value.as_bytes().to_vec();
    let last = bytes.last_mut().expect("validated nonempty hash");
    *last = if *last == b'0' { b'1' } else { b'0' };
    String::from_utf8(bytes).expect("hex is UTF-8")
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: &str) -> String {
        format!("0x{}", byte.repeat(32))
    }

    fn acceptance_input() -> AcceptanceInput {
        let now = chrono::Utc::now().timestamp();
        let asset = |id, name: &str, byte: &str| AssetBinding {
            id,
            name: name.to_string(),
            size: 100,
            sha256: byte.repeat(32),
        };
        AcceptanceInput {
            schema: INPUT_SCHEMA.to_string(),
            repository: REPOSITORY.to_string(),
            source_commit: BUILD_SOURCE_COMMIT.to_string(),
            release_version: VERSION.to_string(),
            release_id: 1,
            release_run_id: 2,
            release_run_attempt: 1,
            frontend_commit: "ab".repeat(20),
            pages_origin: "https://ferrumvir.github.io/arc-chain".to_string(),
            frontend_config_sha256: "bc".repeat(32),
            rollout_manifest_sha256: "cd".repeat(32),
            expected_coordinator: EXPECTED_ORIGIN.to_string(),
            expected_model_id: hash("77"),
            recovery_epoch: 1,
            validator_set_id: 1,
            validator_set_commitment: hash("22"),
            transaction_domain: hash("33"),
            validator_approvals_required: 5,
            expected_active_validators: 6,
            expected_registered_validators: 6,
            minimum_peers: 5,
            maximum_block_age_seconds: 300,
            issued_at_unix: now,
            expires_at_unix: now + 7200,
            challenge: "de".repeat(32),
            assets: AssetBindings {
                app_archive: asset(1, "arc-desktop-macos-arm64.app.tar.gz", "41"),
                app_archive_signature: asset(
                    2,
                    "arc-desktop-macos-arm64.app.tar.gz.sig",
                    "42",
                ),
                dmg: asset(3, "arc-desktop-macos-arm64.dmg", "43"),
            },
            expected_bundle: BundleIdentity {
                app_bundle_tree_sha256: "44".repeat(32),
                executable_relative_path: "Contents/MacOS/arc-desktop".to_string(),
                executable_sha256: "45".repeat(32),
                executable_size: 123,
            },
            budgets: AcceptanceBudgets {
                dispatch_max_wait_ms: MAX_DISPATCH_WAIT.as_millis() as u64,
                earnings_max_polls: MAX_EARNINGS_POLLS,
                earnings_max_wait_ms: MAX_EARNINGS_WAIT.as_millis() as u64,
                final_read_max_wait_ms: MAX_FINAL_READ_WAIT.as_millis() as u64,
                final_read_max_polls: MAX_FINAL_READ_POLLS,
                preflight_max_wait_ms: MAX_PREFLIGHT_WAIT.as_millis() as u64,
                receipt_max_polls: MAX_RECEIPT_POLLS,
                receipt_max_wait_ms: MAX_RECEIPT_WAIT.as_millis() as u64,
                total_max_wait_ms: MAX_TOTAL_WAIT.as_millis() as u64,
            },
        }
    }

    fn valid_result(input: &AcceptanceInput, prompt: &str) -> InferenceResult {
        let output_hash = hash("66");
        let model_hash = input.expected_model_id.clone();
        let tx_hash = hash("88");
        let worker = hash("11");
        let settlement = InferenceSettlement {
            status: "mined_success".to_string(),
            tx_type: "0x25".to_string(),
            tx_hash: tx_hash.clone(),
            job_id: hash("99"),
            worker: worker.clone(),
            submitted: true,
            included: true,
            confirmed: true,
            success: Some(true),
            block_height: Some(42),
            block_hash: Some(hash("aa")),
            index: Some(0),
            reward_base: Some(REQUIRED_REWARD_BASE),
            reward_arc: Some(REQUIRED_REWARD_ARC),
            receipt_url: format!("/community/reward_receipt/{tx_hash}"),
            model_id: model_hash.clone(),
            input_hash: format!("0x{}", arc_crypto::hash_bytes(prompt.as_bytes()).to_hex()),
            output_hash: output_hash.clone(),
            assignment_epoch: hash("bb"),
            transaction_domain: input.transaction_domain.clone(),
            recovery_epoch: Some(input.recovery_epoch),
            validator_set_id: Some(input.validator_set_id),
            validator_set_commitment: input.validator_set_commitment.clone(),
            validator_approvals: Some(5),
            evidence_source: arc_types::transaction::COMMUNITY_REWARD_SUCCESS_EVIDENCE
                .to_string(),
        };
        InferenceResult {
            input: prompt.to_string(),
            output: "accepted".to_string(),
            output_hash,
            model_hash,
            tokens_generated: 1,
            inference_ms: 1,
            tx_hash: String::new(),
            deterministic: true,
            profile_bound: true,
            quorum_verified: true,
            execution_profile: arc_types::transaction::CANONICAL_REWARD_INFERENCE_PROFILE
                .to_string(),
            engine: "test".to_string(),
            explorer_url: String::new(),
            routed_via: format!("community:{worker}"),
            settlement: Some(settlement),
            consensus: None,
            coordinator: Some(input.expected_coordinator.clone()),
            trace: None,
            served_locally: false,
        }
    }

    fn valid_network(input: &AcceptanceInput) -> NetworkOverview {
        NetworkOverview {
            source_host: input.expected_coordinator.clone(),
            unavailable: None,
            network_name: Some("ARC Testnet".to_string()),
            network_name_unavailable_reason: None,
            chain_id: Some("arc-testnet-v3".to_string()),
            declares_mainnet: Some(false),
            is_block_producing: Some(true),
            is_block_producing_basis: Some("fresh block".to_string()),
            host_version: Some(VERSION.to_string()),
            height: Some(50),
            last_block_age_secs: Some(1),
            dag_round: Some(50),
            dag_committed: Some(50),
            peers: Some(5),
            validators_active: Some(6),
            validators_registered: Some(6),
            min_active_stake: Some(1),
            validator_split_derived: false,
            validators: (1u8..=6)
                .map(|byte| crate::types::ValidatorInfo {
                    address: format!("{:02x}", byte).repeat(32),
                    stake: 1,
                    active: true,
                })
                .collect(),
        }
    }

    fn valid_earnings(receipt: &InferenceSettlement) -> Earnings {
        Earnings {
            total_arc: REQUIRED_REWARD_ARC,
            today_arc: Some(REQUIRED_REWARD_ARC),
            pending_arc: Some(0.0),
            rank: None,
            attestations: 1,
            last_payout_at: None,
            last_payout_block: receipt.block_height,
            confirmed_receipts: vec![crate::types::ConfirmedRewardReceipt {
                tx_hash: receipt.tx_hash.clone(),
                job_id: receipt.job_id.clone(),
                block_height: receipt.block_height.unwrap(),
                block_hash: receipt.block_hash.clone().unwrap(),
                reward_base: REQUIRED_REWARD_BASE,
                reward_arc: REQUIRED_REWARD_ARC,
                receipt_url: receipt.receipt_url.clone(),
                recovery_epoch: receipt.recovery_epoch,
                validator_set_id: receipt.validator_set_id,
            }],
            projected_daily_arc: None,
            projected_daily_unavailable_reason: Some(
                "needs more observations".to_string(),
            ),
            recovery_epoch: receipt.recovery_epoch,
            validator_set_id: receipt.validator_set_id,
            unavailable_reason: None,
            receipt_source: Some(
                "scan of this node's in-memory full_transactions map".to_string(),
            ),
            archive_mode: Some(false),
            history_complete_since_recovery: Some(false),
            history_scope: Some(
                "this node's bounded retained reward-receipt window".to_string(),
            ),
            from_chain: true,
        }
    }

    #[test]
    fn acceptance_flag_has_one_exact_non_ambient_argument_shape() {
        let normal = request_from_args([OsString::from("arc-desktop")]).unwrap();
        assert!(normal.is_none());
        let fixture_root = std::env::temp_dir().join("arc-production-acceptance-args");
        let input = fixture_root.join("DESKTOP-LIVE-INPUT.json");
        let output = fixture_root.join("PACKAGED-NATIVE-ACCEPTANCE.json");
        let valid = request_from_args([
            OsString::from("arc-desktop"),
            OsString::from(MODE),
            input.clone().into_os_string(),
            output.clone().into_os_string(),
        ])
        .unwrap()
        .unwrap();
        assert!(valid.input.is_absolute());
        for invalid in [
            vec![OsString::from("arc-desktop"), OsString::from(MODE)],
            vec![
                OsString::from("arc-desktop"),
                OsString::from("--minimized"),
                OsString::from(MODE),
                input.into_os_string(),
                output.clone().into_os_string(),
            ],
            vec![
                OsString::from("arc-desktop"),
                OsString::from(MODE),
                OsString::from("relative.json"),
                output.into_os_string(),
            ],
        ] {
            assert!(request_from_args(invalid).is_err());
        }
    }

    #[test]
    fn standalone_environment_is_exact_and_rejects_automation_tls_loader_and_proxy_authority() {
        let isolated_home = std::env::temp_dir().join("arc-production-acceptance-home");
        let isolated_tmp = isolated_home.join("tmp");
        let baseline = vec![
            (OsString::from("HOME"), isolated_home.into_os_string()),
            (OsString::from("TMPDIR"), isolated_tmp.into_os_string()),
            (
                OsString::from("PATH"),
                OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"),
            ),
            (OsString::from("LANG"), OsString::from("C")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("RUST_LOG"), OsString::from("info")),
        ];
        let (names, digest) = reviewed_environment_identity(baseline.clone()).unwrap();
        assert_eq!(
            names,
            vec!["HOME", "LANG", "LC_ALL", "PATH", "RUST_LOG", "TMPDIR"]
        );
        require_lower_hex(&digest, 64, "environment digest").unwrap();
        for forbidden in [
            "TAURI_AUTOMATION",
            "TAURI_WEBVIEW_AUTOMATION",
            "DYLD_INSERT_LIBRARIES",
            "LD_PRELOAD",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "OPENSSL_CONF",
            "OPENSSL_MODULES",
            "OPENSSL_ENGINES",
            "PYTHONPATH",
            "NODE_OPTIONS",
            "HTTPS_PROXY",
            "ARC_WALLET_HOST",
        ] {
            let mut hostile = baseline.clone();
            hostile.push((OsString::from(forbidden), OsString::from("/attacker")));
            assert!(
                reviewed_environment_identity(hostile).is_err(),
                "{forbidden} must be rejected before native acceptance"
            );
        }
    }

    #[test]
    fn canonical_json_is_sorted_newline_terminated_and_stable() {
        let value = serde_json::json!({"z": 1, "a": {"y": 2, "b": 3}});
        assert_eq!(
            canonical_json(&value).unwrap(),
            b"{\"a\":{\"b\":3,\"y\":2},\"z\":1}\n"
        );
    }

    #[test]
    fn hash_mutation_stays_canonical_and_changes_identity() {
        let hash = format!("0x{}", "ab".repeat(32));
        let changed = mutate_hash(&hash);
        assert_ne!(changed, hash);
        require_hash32(&changed, "changed").unwrap();
    }

    #[test]
    fn terminal_receipt_is_cryptographically_bound_to_the_challenge_prompt() {
        let input = acceptance_input();
        let input_sha256 = sha256_bytes(&canonical_json(&input).unwrap());
        let prompt = acceptance_prompt(&input.challenge, &input_sha256);
        let result = valid_result(&input, &prompt);
        validate_inference(&input, &prompt, &result).unwrap();
        validate_terminal_receipt(&input, &result, result.settlement.as_ref().unwrap()).unwrap();

        let mut wrong = result.settlement.clone().unwrap();
        wrong.input_hash = hash("fe");
        assert!(validate_terminal_receipt(&input, &result, &wrong).is_err());
        let mut wrong_worker = result.clone();
        wrong_worker.settlement.as_mut().unwrap().worker = hash("ef");
        assert!(validate_inference(&input, &prompt, &wrong_worker).is_err());
    }

    #[test]
    fn network_and_projection_validators_fail_closed_on_mutation() {
        let input = acceptance_input();
        let input_sha256 = sha256_bytes(&canonical_json(&input).unwrap());
        let prompt = acceptance_prompt(&input.challenge, &input_sha256);
        let result = valid_result(&input, &prompt);
        let receipt = result.settlement.as_ref().unwrap();
        let network = valid_network(&input);
        validate_network_preflight(&input, &network).unwrap();
        let recent = RecentBlocks {
            source_host: input.expected_coordinator.clone(),
            unavailable: None,
            blocks: vec![crate::types::BlockSummary {
                height: receipt.block_height.unwrap(),
                hash: receipt
                    .block_hash
                    .as_deref()
                    .unwrap()
                    .trim_start_matches("0x")
                    .to_string(),
                timestamp_ms: Some(1),
                tx_count: Some(1),
                proposer: None,
            }],
        };
        validate_network(&input, receipt, &network, &recent).unwrap();

        let mut wrong_network = network.clone();
        wrong_network.validators[1].address = wrong_network.validators[0].address.clone();
        assert!(validate_network_preflight(&input, &wrong_network).is_err());
        let mut wrong_recent = recent.clone();
        wrong_recent.blocks[0].hash = "ff".repeat(32);
        assert!(validate_network(&input, receipt, &network, &wrong_recent).is_err());

        let earnings = valid_earnings(receipt);
        validate_earnings(&input, receipt, &earnings).unwrap();
        let mut wrong_earnings = earnings.clone();
        wrong_earnings.confirmed_receipts[0].job_id = hash("fe");
        assert!(validate_earnings(&input, receipt, &wrong_earnings).is_err());

        let transaction = TxLookup {
            source_host: input.expected_coordinator.clone(),
            unavailable: None,
            hash: receipt.tx_hash.trim_start_matches("0x").to_string(),
            status: "mined".to_string(),
            block_height: receipt.block_height,
            block_hash: receipt
                .block_hash
                .as_deref()
                .map(|hash| hash.trim_start_matches("0x").to_string()),
            tx_index: receipt.index.map(|index| index as u32),
            success: Some(true),
            gas_used: Some(1),
        };
        validate_transaction(&input, receipt, &transaction).unwrap();
        let mut wrong_transaction = transaction.clone();
        wrong_transaction.source_host = "https://149.28.32.76".to_string();
        assert!(validate_transaction(&input, receipt, &wrong_transaction).is_err());

        let projection = EarningsProjection {
            source_host: input.expected_coordinator.clone(),
            unavailable: None,
            reward_per_attestation: Some(REQUIRED_REWARD_ARC),
            reward_rate_source: "chain".to_string(),
            community_rewards_enabled: Some(true),
            projected_daily_arc: None,
            projected_daily_unavailable_reason: Some("needs more observations".to_string()),
            reward_policy_hash: Some(hash("12")),
            reward_budget_epoch: Some(1),
            rewards_remaining_this_epoch: Some(1),
            worker_rewards_remaining_this_epoch: Some(1),
            coordinator_rewards_remaining_this_epoch: Some(1),
            issuance_ready_for_worker: Some(true),
            reward_program: Some(
                "protocol-capped testnet promotional compute subsidy".to_string(),
            ),
            reward_is_customer_demand: Some(false),
            attestations_total: 1,
            first_attestation_block: Some(42),
            attestations_per_day: None,
            rate_unavailable_reason: Some("needs more observations".to_string()),
            observed_over_blocks: None,
            rate_caveat: None,
        };
        validate_projection(&input, receipt, &earnings, &projection).unwrap();
        let mut ambiguous = projection.clone();
        ambiguous.projected_daily_arc = Some(1.0);
        assert!(validate_projection(&input, receipt, &earnings, &ambiguous).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sealed_input_is_single_fd_private_and_create_only_output_cannot_collide() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let input_path = directory.path().join("DESKTOP-LIVE-INPUT.json");
        let raw = canonical_json(&acceptance_input()).unwrap();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o400);
        let mut file = options.open(&input_path).unwrap();
        file.write_all(&raw).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let (_, digest) = read_and_validate_input(&input_path).unwrap();
        assert_eq!(digest, sha256_bytes(&raw));

        let output = directory.path().join("PACKAGED-NATIVE-ACCEPTANCE.json");
        fs::write(&output, b"preserve").unwrap();
        assert!(write_create_only(&output, &serde_json::json!({"ok": true})).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"preserve");

        let linked_dir = tempfile::tempdir().unwrap();
        fs::set_permissions(linked_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let linked = linked_dir.path().join("DESKTOP-LIVE-INPUT.json");
        fs::hard_link(&input_path, &linked).unwrap();
        assert!(read_and_validate_input(&input_path).is_err());

        let symlink_dir = tempfile::tempdir().unwrap();
        fs::set_permissions(symlink_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let symlink_input = symlink_dir.path().join("DESKTOP-LIVE-INPUT.json");
        std::os::unix::fs::symlink(&linked, &symlink_input).unwrap();
        assert!(read_and_validate_input(&symlink_input).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn full_bundle_tree_hash_is_deterministic_and_rejects_escape_links() {
        fn populate(root: &Path) {
            fs::create_dir(root.join("Contents")).unwrap();
            fs::set_permissions(root.join("Contents"), fs::Permissions::from_mode(0o755))
                .unwrap();
            fs::write(root.join("Contents/data"), b"exact bundle bytes").unwrap();
            fs::set_permissions(
                root.join("Contents/data"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            std::os::unix::fs::symlink("data", root.join("Contents/current")).unwrap();
        }
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        populate(one.path());
        populate(two.path());
        assert_eq!(
            bundle_tree_sha256(one.path()).unwrap(),
            bundle_tree_sha256(two.path()).unwrap()
        );
        fs::write(two.path().join("Contents/data"), b"mutated bundle bytes").unwrap();
        assert_ne!(
            bundle_tree_sha256(one.path()).unwrap(),
            bundle_tree_sha256(two.path()).unwrap()
        );

        let escape_parent = tempfile::tempdir().unwrap();
        let app = escape_parent.path().join("App.app");
        fs::create_dir(&app).unwrap();
        fs::write(escape_parent.path().join("outside"), b"outside").unwrap();
        std::os::unix::fs::symlink("../outside", app.join("escape")).unwrap();
        assert!(bundle_tree_sha256(&app).is_err());
    }
}
