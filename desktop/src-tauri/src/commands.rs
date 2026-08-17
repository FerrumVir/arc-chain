use crate::node_manager::{managed_binary_path, TestnetResources};
use crate::types::*;
use crate::{hardware, identity, paths, rpc_client, AppState};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tokio::io::AsyncWriteExt;

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
    config: NodeConfig,
) -> CmdResult<()> {
    let auto_start = config.auto_start;
    {
        let mut store = state.store.lock().await;
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

#[tauri::command]
pub async fn get_autostart(app: AppHandle) -> CmdResult<bool> {
    Ok(app.autolaunch().is_enabled().unwrap_or(false))
}

#[tauri::command]
pub async fn load_config(state: State<'_, AppState>) -> CmdResult<Option<NodeConfig>> {
    let store = state.store.lock().await;
    Ok(store.config.clone())
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
pub async fn start_node_inner(
    app: &AppHandle,
    state: &AppState,
    config: &NodeConfig,
) -> Result<(), String> {
    // Make sure we have a runnable binary. This must never be able to block
    // a start when a usable binary already exists — see `ensure_binary`.
    ensure_binary_inner(app).await?;

    let validator_seed = {
        let store = state.store.lock().await;
        store
            .identity
            .as_ref()
            .map(|i| i.seed_phrase.clone())
            .ok_or_else(|| {
                "no identity - run onboarding so we can derive an on-chain validator address before starting arc-node".to_string()
            })?
    };
    let resources = resolve_testnet_resources(app);
    let mut node = state.node.lock().await;
    node.start(config, &validator_seed, &resources)
        .await
        .map_err(map_err)
}

#[tauri::command]
pub async fn start_node(
    app: AppHandle,
    state: State<'_, AppState>,
    config: NodeConfig,
) -> CmdResult<()> {
    start_node_inner(&app, &state, &config).await
}

#[tauri::command]
pub async fn stop_node(state: State<'_, AppState>) -> CmdResult<()> {
    let mut node = state.node.lock().await;
    node.stop().await.map_err(map_err)
}

#[tauri::command]
pub async fn restart_node(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    let (cfg, validator_seed) = {
        let store = state.store.lock().await;
        let cfg = store.config.clone().unwrap_or_default();
        let seed = store
            .identity
            .as_ref()
            .map(|i| i.seed_phrase.clone())
            .ok_or_else(|| "no identity - cannot restart arc-node".to_string())?;
        (cfg, seed)
    };

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

    // A restart is a good moment to pick up a newer arc-node, since the user
    // is already paying the restart cost. Now safe: nothing holds the file.
    ensure_binary_inner(&app).await?;

    let resources = resolve_testnet_resources(&app);
    let mut node = state.node.lock().await;
    node.start(&cfg, &validator_seed, &resources)
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
pub async fn reset_peer_state(app: AppHandle, state: State<'_, AppState>) -> CmdResult<ResetPeerStateResult> {
    // Resolve the data dir through the SAME helper node_manager uses.
    // Duplicating the expansion here (HOME-only) meant this deleted
    // known_peers.json from a different directory than the node actually
    // uses on Windows, then reported success.
    let cfg = {
        let store = state.store.lock().await;
        store.config.clone().unwrap_or_default()
    };
    let data_dir = crate::node_manager::resolve_data_dir(&cfg.data_dir);
    let peers_path = data_dir.join("known_peers.json");

    // Stop first so the node isn't holding the file open or racing on writes.
    {
        let mut node = state.node.lock().await;
        let _ = node.stop().await;
    }

    let removed = match std::fs::remove_file(&peers_path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("failed to remove {}: {}", peers_path.display(), e)),
    };

    // Restart. Reuses restart_node's plumbing.
    restart_node(app, state).await?;

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

    let mut status = rpc_client::fetch_status(
        &state.http,
        &local,
        &chain,
        port,
        pid,
        address,
        crash,
    )
    .await;

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
    Ok(rpc_client::fetch_attestations(
        &state.http,
        &host,
        limit.unwrap_or(20),
        address.as_deref(),
    )
    .await)
}

#[tauri::command]
pub async fn fetch_logs(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<Vec<LogEntry>> {
    let node = state.node.lock().await;
    Ok(node
        .logs_snapshot(limit.unwrap_or(200) as usize)
        .await)
}

#[tauri::command]
pub async fn fetch_network_stats(state: State<'_, AppState>) -> CmdResult<NetworkStats> {
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_network_stats(&state.http, &host).await)
}

#[tauri::command]
pub async fn open_external(app: AppHandle, url: String) -> CmdResult<()> {
    use tauri_plugin_opener::OpenerExt;
    app.opener().open_url(url, None::<&str>).map_err(map_err)
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
    let host = chain_host(&state).await;
    rpc_client::faucet_claim(&state.http, &host, &addr).await
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
            let resp = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                http.get(&url).send(),
            )
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
            tracing::warn!("no seed answered /block/latest; falling back to {}", WALLET_HOSTS[0]);
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

/// Per-request inference timeout.
///
/// Was 600s per host, tried across hosts sequentially — a single dead
/// coordinator could burn ten minutes before the next was attempted, and
/// five of them meant the UI could hang for the better part of an hour. 120s
/// is generous for a short prompt through the sharded pipeline and bounded
/// enough that a wedged host costs one visible pause, not a demo.
const INFERENCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How long a coordinator gets to answer `/health` before we skip it.
const COORDINATOR_HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

fn inference_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(INFERENCE_TIMEOUT)
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
    let client = inference_client()?;
    let mut result = rpc_client::run_inference(
        &client,
        &host,
        &prompt,
        max_tokens.unwrap_or(32),
        chat_template.unwrap_or(true),
    )
    .await?;
    result.served_locally = true;
    Ok(result)
}

/// Milestone A (#35): observer / no-model nodes route inference through a
/// testnet seed coordinator's `/inference/run_consensus` endpoint.
///
/// Iterates the built-in `COORDINATOR_HOSTS` list until one seed responds
/// with success. A 120s per-host timeout covers the full consensus pipeline
/// for short prompts (NYC typical: 15–60s at k=3 through 6 ranges).
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
    let client = inference_client()?;
    let max_tokens = max_tokens.unwrap_or(32);
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
            &client, host, &prompt, max_tokens, k, chat_template,
        )
        .await
        {
            Ok(mut r) => {
                r.served_locally = *host == local_prefix;
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

/// Direct single-node inference fallback. Used by the desktop UI when
/// `/inference/run_consensus` fails on every coordinator (the current
/// failure mode: retired-but-still-registered SAO+JNB shards cause every
/// coordinator's pipeline planner to return `Pipeline gap: expected
/// layer 32 next, got [28, 30)` before any token is generated). Hitting
/// `/inference/run` directly skips the sharded pipeline entirely — the
/// coordinator serves the model from its local shards and still emits
/// an on-chain attestation. We lose the k-of-n cross-replica consensus
/// (no `consensus` field in the result), but the user gets a real
/// answer instead of an error.
#[tauri::command]
pub async fn run_inference_via_coordinator_direct(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    chat_template: Option<bool>,
) -> CmdResult<InferenceResult> {
    let client = inference_client()?;
    let max_tokens = max_tokens.unwrap_or(32);
    let chat_template = chat_template.unwrap_or(true);

    let candidates = coordinator_candidates(&state).await;
    if candidates.is_empty() {
        return Err("no coordinator answered /health - check your internet connection".into());
    }
    let local_prefix = paths::local_host(state.node.lock().await.rpc_port);

    let mut last_err = String::new();
    for host in &candidates {
        match rpc_client::run_inference_remote(
            &client, host, &prompt, max_tokens, chat_template,
        )
        .await
        {
            Ok(mut r) => {
                r.served_locally = *host == local_prefix;
                return Ok(r);
            }
            Err(e) => last_err = e,
        }
    }
    Err(format!(
        "all {} reachable coordinators failed (direct path); last: {}",
        candidates.len(),
        last_err
    ))
}

/// Tier 1 on-chain inference: submit an InferenceRequest to one of the live
/// testnet seed VPSes via its `/inference/onchain/submit` convenience endpoint.
/// The seed signs with its validator keypair and forwards to its mempool.
///
/// Picks a random host from `TIER1_HOSTS` (shuffled, then tried in order) so
/// load spreads across the 6 seeds. The picked host is pinned to the returned
/// request_id in `state.tier1_routes`, because each seed runs its own chain
/// with a different `anchor_height` — polling a different seed for the result
/// would 404.
///
/// `ARC_TIER1_RPC` env var, if set, overrides the host list (used for local
/// dev: `ARC_TIER1_RPC=http://127.0.0.1:9090`). See
/// `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md`.
#[tauri::command]
pub async fn tier1_submit(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    max_reward: Option<u64>,
    deadline_blocks: Option<u64>,
    committee_size: Option<u8>,
) -> CmdResult<rpc_client::Tier1Submitted> {
    use ed25519_dalek::Signer;

    // Pull the user's keypair so the InferenceRequest tx is signed by
    // the user — not the seed validator's convenience endpoint. The
    // resulting tx.from = user, which arc-state's apply path stores as
    // `tier1.requester`. When the seed votes, it reads that back and
    // sets it as the `beneficiary` on the InferenceAttestation, so
    // /worker/earnings credits the user (Option C).
    let phrase = {
        let store = state.store.lock().await;
        store
            .identity
            .as_ref()
            .map(|i| i.seed_phrase.clone())
            .ok_or_else(|| "no identity - run onboarding first".to_string())?
    };
    let signing_key = keypair_from_phrase(&phrase);
    let public_key = signing_key.verifying_key().to_bytes();
    let payer_addr = arc_crypto::Hash256(*blake3::hash(&public_key).as_bytes());

    let candidates = tier1_candidate_hosts();
    let mut last_err = String::from("no tier1 hosts configured");

    let max_tokens = max_tokens.unwrap_or(32);
    let max_reward = max_reward.unwrap_or(10);
    let deadline_blocks = deadline_blocks.unwrap_or(20);
    // Alpha is a solo chain with 1 validator — committee_size > 1 stalls
    // forever waiting for votes that will never come. Default to 1 so
    // requests finalize on alpha. Users can still override via the
    // Inference UI's committee_size input when targeting multi-validator
    // chains in the future.
    let committee_size = committee_size.unwrap_or(1);

    for host in &candidates {
        // Fetch the user's current nonce and the chain's current height
        // (used in the deterministic request_id derivation).
        // No `0x` prefix — the /account handler requires bare 64-hex.
        // With the prefix it returns 400 "Invalid address", fallback in
        // the match below uses nonce=0 and every submit after the first
        // gets rejected at apply with InvalidNonce.
        let account_url = format!(
            "{}/account/{}",
            host,
            hex::encode(&payer_addr.0)
        );
        let nonce: u64 = match state.http.get(&account_url).send().await {
            Ok(r) if r.status().is_success() => r
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("nonce").and_then(|n| n.as_u64()))
                .unwrap_or(0),
            _ => 0,
        };
        let height_url = format!("{}/health", host);
        let height: u64 = match state.http.get(&height_url).send().await {
            Ok(r) if r.status().is_success() => r
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("height").and_then(|n| n.as_u64()))
                .unwrap_or(0),
            _ => 0,
        };

        // request_id mirrors the chain's derivation in
        // arc-node/src/rpc.rs:inference_onchain_submit, so the
        // generated id is the same one apply uses to key the escrow.
        let input_blob = prompt.as_bytes().to_vec();
        let input_hash = arc_crypto::hash_bytes(&input_blob);
        let mut id_input = Vec::with_capacity(72);
        id_input.extend_from_slice(&payer_addr.0);
        id_input.extend_from_slice(&input_hash.0);
        id_input.extend_from_slice(&height.to_le_bytes());
        let request_id_hash = arc_crypto::hash_bytes(&id_input);
        let request_id = request_id_hash.0;

        let model_id = arc_crypto::hash_bytes(b"arc-32L-test");
        let body = arc_types::transaction::InferenceRequestBody {
            request_id,
            model_id,
            input_hash,
            input_blob,
            max_tokens,
            tier: 1,
            max_reward,
            deadline_blocks,
            committee_size,
        };
        let mut tx = arc_types::Transaction {
            tx_type: arc_types::TxType::InferenceRequest,
            from: payer_addr,
            nonce,
            body: arc_types::TxBody::InferenceRequest(body),
            fee: 0,
            gas_limit: 0,
            hash: arc_crypto::Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        let sig = signing_key.sign(tx.hash.as_bytes());
        tx.signature = arc_crypto::Signature::Ed25519 {
            public_key,
            signature: sig.to_bytes().to_vec(),
        };
        let tx_hash = tx.hash;

        let resp = state
            .http
            .post(format!("{}/tx/submit_signed", host))
            .json(&tx)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let request_id_hex = format!("0x{}", hex::encode(&request_id));
                state
                    .tier1_routes
                    .lock()
                    .await
                    .insert(request_id_hex.clone(), host.clone());
                return Ok(rpc_client::Tier1Submitted {
                    request_id: request_id_hex,
                    tx_hash: tx_hash.to_hex(),
                    anchor_height: height,
                    committee_size,
                    deadline_blocks,
                    max_reward,
                });
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                last_err = format!("{}: HTTP {} - {}", host, status, body);
                tracing::warn!("tier1_submit fallback: {}", last_err);
            }
            Err(e) => {
                last_err = format!("{}: {}", host, e);
                tracing::warn!("tier1_submit fallback: {}", last_err);
            }
        }
    }
    Err(format!(
        "all {} tier1 hosts failed; last: {}",
        candidates.len(),
        last_err
    ))
}

/// Poll the chain for the on-chain state of a Tier 1 request. Called every
/// 500 ms from the desktop UI until status transitions to a terminal value.
/// Looks up the host that accepted the original submit from
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
    Err(format!("tier1_result not found on any host; last: {}", last_err))
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
    let mut hosts: Vec<String> =
        COORDINATOR_HOSTS.iter().map(|s| s.to_string()).collect();
    hosts.shuffle(&mut rand::thread_rng());
    hosts
}

/// Tier 1 inference hosts — same 5 testnet seeds the wallet reads
/// from. Requires the v0.7.6 BlockSTM fix to be deployed on the seeds
/// for InferenceRequest tx to actually land. Before v0.7.6 the
/// speculative executor silently dropped the tx; the alpha solo host
/// was the temporary stopgap. With v0.7.6 rolled out, alpha retires
/// and tier 1 lives on the public testnet alongside everything else.
///
/// NYC was missing from this list. It is a live seed and, for long stretches,
/// the healthiest one — so both the coordinator fallback and the chain-host
/// election were choosing among five hosts while ignoring the sixth.
const COORDINATOR_HOSTS: [&str; 6] = [
    "http://149.28.32.76:9090",   // NYC
    "http://140.82.16.112:9090",  // LAX
    "http://136.244.109.1:9090",  // AMS
    "http://104.238.171.11:9090", // LHR
    "http://202.182.107.41:9090", // NRT
    "http://149.28.153.31:9090",  // SGP
];

/// The public testnet seeds, as candidates for chain reads.
///
/// No longer a priority list with a pinned `[0]` — `chain_host()` elects
/// among these by block freshness on every TTL expiry. Order is
/// presentational only.
const WALLET_HOSTS: [&str; 6] = [
    "http://149.28.32.76:9090",   // NYC
    "http://140.82.16.112:9090",  // LAX
    "http://136.244.109.1:9090",  // AMS
    "http://104.238.171.11:9090", // LHR
    "http://202.182.107.41:9090", // NRT
    "http://149.28.153.31:9090",  // SGP
];

/// Milestone B (#36): testnet model commitment. Both ends - the
/// InferenceEscrowOpen tx and the InferenceEscrowRelease tx the
/// coordinator will auto-submit - must use the same value or the
/// state-layer metadata-hash check rejects the release.
fn testnet_model_id() -> arc_crypto::Hash256 {
    arc_crypto::hash_bytes(b"arc-testnet-llama-2-7b-chat-q4")
}

/// Milestone B default escrow timeout (in blocks). Conservative enough
/// that a slow 50s/token run_consensus pass can't auto-refund out from
/// under the coordinator before it submits the release.
const DEFAULT_ESCROW_TIMEOUT_BLOCKS: u64 = 10_000;

/// Default "Pay N ARC" max_fee for the UI. Matches the PLAN.md example
/// (Alice pays 10 ARC × 10 inferences = 100 ARC debited).
const DEFAULT_MAX_FEE: u64 = 10_000;

/// Derive the signing keypair from a BIP-39 phrase the same way
/// `identity::derive` does. Duplicated (not factored) because
/// `identity::derive` returns an opaque `Identity` struct meant for the
/// UI; here we need the raw `SigningKey` so we can put the public key
/// into the Transaction's signature slot.
fn keypair_from_phrase(phrase: &str) -> ed25519_dalek::SigningKey {
    const DOMAIN_TAG: &str = "ARC-chain-validator-keypair-v1";
    let seed_bytes = blake3::derive_key(DOMAIN_TAG, phrase.trim().as_bytes());
    ed25519_dalek::SigningKey::from_bytes(&seed_bytes)
}

/// Milestone B (#36): open an inference-escrow on a coordinator, then
/// call `/inference/run_consensus` against it. The coordinator validates
/// the escrow is present before running model work, and on success
/// auto-submits the release tx that pays out 40/25/15/20 to
/// proposer / replicas / observer pool / treasury.
#[tauri::command]
pub async fn run_paid_inference(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
    max_fee: Option<u64>,
    k: Option<u32>,
) -> CmdResult<PaidInferenceResult> {
    use ed25519_dalek::Signer;

    let phrase = {
        let store = state.store.lock().await;
        store
            .identity
            .as_ref()
            .map(|i| i.seed_phrase.clone())
            .ok_or_else(|| "no identity - run onboarding first".to_string())?
    };
    let signing_key = keypair_from_phrase(&phrase);
    let public_key = signing_key.verifying_key().to_bytes();
    // ARC address = BLAKE3(public_key) - matches chain derivation.
    let payer_addr = arc_crypto::Hash256(*blake3::hash(&public_key).as_bytes());

    // Pick the best reachable coordinator (parallel probe, local first).
    // The escrow is opened against whichever host is chosen, so this must
    // resolve before any transaction is signed.
    let probe = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(map_err)?;
    let coord_url = coordinator_candidates(&state)
        .await
        .into_iter()
        .next()
        .ok_or_else(|| {
            "no coordinator reachable - every testnet seed timed out on /health".to_string()
        })?;

    // Pull the payer's current on-chain nonce so the open tx lands.
    let account_url = format!(
        "{}/account/0x{}",
        coord_url,
        hex::encode(&payer_addr.0)
    );
    let nonce: u64 = match probe.get(&account_url).send().await {
        Ok(r) if r.status().is_success() => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("nonce").and_then(|n| n.as_u64()))
            .unwrap_or(0),
        _ => 0,
    };

    let mut request_id = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut request_id);
    let model_id = testnet_model_id();
    let max_tokens = max_tokens.unwrap_or(32);
    let max_fee = max_fee.unwrap_or(DEFAULT_MAX_FEE);
    let timeout_blocks = DEFAULT_ESCROW_TIMEOUT_BLOCKS;

    // Build + sign the InferenceEscrowOpen tx.
    let body = arc_types::transaction::InferenceEscrowOpenBody {
        request_id,
        model_id,
        max_fee,
        max_tokens,
        timeout_blocks,
    };
    let mut tx = arc_types::Transaction {
        tx_type: arc_types::TxType::InferenceEscrowOpen,
        from: payer_addr,
        nonce,
        body: arc_types::TxBody::InferenceEscrowOpen(body),
        fee: 0,
        gas_limit: 0,
        hash: arc_crypto::Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: false,
    };
    tx.hash = tx.compute_hash();
    let sig = signing_key.sign(tx.hash.as_bytes());
    tx.signature = arc_crypto::Signature::Ed25519 {
        public_key,
        signature: sig.to_bytes().to_vec(),
    };
    let open_tx_hash = tx.hash;

    let submit_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(map_err)?;
    let open_resp = submit_client
        .post(format!("{}/tx/submit_signed", coord_url))
        .json(&tx)
        .send()
        .await
        .map_err(map_err)?;
    if !open_resp.status().is_success() {
        return Err(format!(
            "escrow open failed: {} - payer=0x{} nonce={}",
            open_resp.status(),
            hex::encode(&payer_addr.0),
            nonce
        ));
    }

    // Wait for the open tx to commit (mempool → block). ≤ 15s × 200ms.
    let open_hash_hex = hex::encode(&open_tx_hash.0);
    let mut committed = false;
    for _ in 0..75 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Ok(r) = submit_client
            .get(format!("{}/tx/0x{}", coord_url, open_hash_hex))
            .send()
            .await
        {
            if r.status().is_success() {
                committed = true;
                break;
            }
        }
    }
    if !committed {
        return Err(format!(
            "escrow open tx did not commit within 15s (hash=0x{})",
            open_hash_hex
        ));
    }

    // Run inference with the escrow-gated flags.
    //
    // The prompt is sent raw with `chat_template: true` rather than wrapped
    // client-side in `[INST] ... [/INST]`. The node applies the model's own
    // template from GGUF metadata, which is correct for whatever model is
    // actually loaded; hardcoding Llama-2's tags corrupted the prompt for
    // every other architecture and double-wrapped when the node templated
    // too.
    let k = k.unwrap_or(3);
    let infer_client = inference_client()?;
    let infer_resp = infer_client
        .post(format!("{}/inference/run_consensus", coord_url))
        .json(&serde_json::json!({
            "input": prompt,
            "chat_template": true,
            "max_tokens": max_tokens,
            "k": k,
            "payer": format!("0x{}", hex::encode(&payer_addr.0)),
            "request_id": format!("0x{}", hex::encode(&request_id)),
            "max_fee": max_fee,
            "model_id": format!("0x{}", hex::encode(&model_id.0)),
            "timeout_blocks": timeout_blocks,
        }))
        .send()
        .await
        .map_err(map_err)?;
    if !infer_resp.status().is_success() {
        return Err(format!(
            "run_consensus failed: {} (escrow will refund after {} blocks)",
            infer_resp.status(),
            timeout_blocks
        ));
    }
    let v: serde_json::Value = infer_resp.json().await.map_err(map_err)?;
    let c = v.get("consensus").cloned().unwrap_or(serde_json::Value::Null);
    let escrow_block = v.get("escrow").cloned().unwrap_or(serde_json::Value::Null);

    Ok(PaidInferenceResult {
        input: v
            .get("input")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        output: v
            .get("output")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        output_hash: v
            .get("output_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        tokens_generated: v
            .get("tokens_generated")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        inference_ms: v
            .get("total_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        coordinator: coord_url,
        consensus: InferenceConsensus {
            k: c.get("k").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            votes_total: c
                .get("votes_total")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32,
            unanimous: c.get("unanimous").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            majority: c.get("majority").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            split: c.get("split").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            divergent_replica_count: c
                .get("divergent_replicas")
                .and_then(|x| x.as_object())
                .map(|m| m.len() as u32)
                .unwrap_or(0),
        },
        payer_address: format!("0x{}", hex::encode(&payer_addr.0)),
        max_fee,
        open_tx_hash: format!("0x{}", open_hash_hex),
        release_tx_hash: escrow_block
            .get("release_tx_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    })
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
        return Ok(SavedLogs { path: None, lines: entries.len() });
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
        Err(e) => tracing::info!("POST /node/threads failed ({}) - falling back to a restart", e),
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
const EXPECTED_NODE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// First-launch readiness check. Confirms the bundled testnet resources are
/// resolvable AND the arc-node binary is present at the version this desktop
/// was built against. If the binary is missing OR its `--version` doesn't
/// match this desktop's `CARGO_PKG_VERSION`, downloads the matching arc-node
/// binary from the latest GitHub release for this platform. The onboarding
/// screen calls this before launching the node, and `start_node` also calls
/// it on every start so existing users picked up by the desktop auto-updater
/// always get the matching arc-node binary instead of running a stale one.
#[tauri::command]
pub async fn ensure_binary(app: AppHandle) -> CmdResult<BinaryStatus> {
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

/// Make sure *some* runnable arc-node exists, and prefer a current one.
///
/// The governing rule, learned the hard way: **a usable binary that is
/// present must never be blocked by a failed refresh.** The previous version
/// returned `Err` on any non-200 from GitHub, and `start_node` propagated it
/// with `?`. Because v0.7.10 and v0.7.11 were both published desktop-only via
/// `workflow_dispatch`, the release carries no `arc-node-*` assets at all, so
/// that download 404s on every platform — and every Start click, on every
/// machine, failed before arc-node was ever spawned. A stale arc-node beats
/// no arc-node; no arc-node beats nothing.
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

    // 2. A binary already in the managed location. Compare versions and
    //    *warn* — do not force a redownload. A hand-built or locally patched
    //    arc-node is a deliberate act; clobbering it because its version
    //    string differs from the desktop's is user-hostile, and with the
    //    release assets missing it replaces something that works with
    //    nothing at all. Only a genuinely OLDER binary is worth refreshing.
    if target.exists() {
        match read_arc_node_version(&target) {
            Some(ref v) if v == EXPECTED_NODE_VERSION => return Ok(installed(&target)),
            Some(v) => {
                if semver_gt(EXPECTED_NODE_VERSION, &v) {
                    tracing::info!(
                        "arc-node {} at {} is older than this desktop's {} - attempting refresh",
                        v, target.display(), EXPECTED_NODE_VERSION
                    );
                    // Fall through to the download attempt below.
                } else {
                    tracing::warn!(
                        "arc-node {} at {} does not match this desktop's {} - keeping it \
                         (newer or unrecognized versions are left alone)",
                        v, target.display(), EXPECTED_NODE_VERSION
                    );
                    return Ok(installed(&target));
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

    // 4. Download. Any failure past this point is non-fatal when we already
    //    have something to run.
    let have_fallback = target.exists();
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
        Err(e) if have_fallback => {
            tracing::warn!(
                "arc-node refresh failed ({}) - continuing with the existing binary at {}",
                e, target.display()
            );
            Ok(installed(&target))
        }
        Err(e) => {
            // Last chance: a dev build we skipped earlier because the
            // managed path existed but turned out unusable.
            if let Some(dev) = crate::node_manager::dev_build_binary() {
                tracing::warn!("arc-node download failed ({}) - falling back to {}", e, dev.display());
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
    let url = format!(
        "https://github.com/FerrumVir/arc-chain/releases/latest/download/{}",
        asset
    );

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(map_err)?;
    }

    let client = reqwest::Client::builder()
        .user_agent("arc-desktop/0.1")
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(map_err)?;

    let resp = client.get(&url).send().await.map_err(map_err)?;
    if !resp.status().is_success() {
        return Err(format!(
            "release asset {} returned HTTP {}",
            asset,
            resp.status()
        ));
    }
    let total_bytes = resp.content_length().unwrap_or(0);
    let tmp = target.with_extension("download");
    {
        let mut file = std::fs::File::create(&tmp).map_err(map_err)?;
        let bytes = resp.bytes().await.map_err(map_err)?;
        file.write_all(&bytes).map_err(map_err)?;
        file.sync_all().ok();
    }

    install_over(&tmp, target)?;

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

/// Move `tmp` onto `target`, tolerating a locked destination.
///
/// Windows refuses to overwrite a running executable's image file, but it
/// *does* allow renaming one out of the way. So on a failed direct rename,
/// displace the old binary to `.old` first and retry. The stale `.old` is
/// cleaned up opportunistically on the next successful install.
fn install_over(tmp: &Path, target: &Path) -> Result<(), String> {
    if std::fs::rename(tmp, target).is_ok() {
        let _ = std::fs::remove_file(target.with_extension("old"));
        return Ok(());
    }
    let displaced = target.with_extension("old");
    let _ = std::fs::remove_file(&displaced);
    if let Err(e) = std::fs::rename(target, &displaced) {
        let _ = std::fs::remove_file(tmp);
        return Err(format!(
            "could not replace {} (in use, and moving it aside failed: {})",
            target.display(),
            e
        ));
    }
    std::fs::rename(tmp, target).map_err(|e| {
        // Put the original back rather than leaving the user with nothing.
        let _ = std::fs::rename(&displaced, target);
        let _ = std::fs::remove_file(tmp);
        format!("could not install new arc-node at {}: {}", target.display(), e)
    })
}

/// Run `arc-node --version` and return the version token (e.g. "0.5.7").
/// Returns None if the binary fails to launch (corrupt, wrong arch, missing
/// Returns true if semver string `a` is strictly greater than `b`.
/// Compares major.minor.patch numerically. Falls back to false on parse error.
fn semver_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Option<(u64, u64, u64)> {
        let mut parts = s.trim().splitn(3, '.');
        let maj = parts.next()?.parse().ok()?;
        let min = parts.next()?.parse().ok()?;
        let pat = parts.next().and_then(|p| p.split('-').next()).and_then(|p| p.parse().ok()).unwrap_or(0);
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

fn resolve_testnet_resources(app: &AppHandle) -> TestnetResources {
    let resolver = app.path();
    let seeds = resolver
        .resolve("resources/testnet-seeds.txt", tauri::path::BaseDirectory::Resource)
        .ok()
        .filter(|p: &PathBuf| p.exists());
    let genesis = resolver
        .resolve("resources/genesis.toml", tauri::path::BaseDirectory::Resource)
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
// v0.6.0 fixes that: onboarding picks a hardware-appropriate model, downloads
// the GGUF from a stable HF mirror, configures the node as a worker. The
// runtime distinction (worker vs observer) becomes a consequence of "did
// the user download a model?" instead of a UI choice they make blindly.
//
// All three tiers are llama-architecture (the only family arc-inference's
// candle backend supports today - see `quantized_llama::ModelWeights` in
// crates/arc-inference/src/candle_backend.rs). Sizes are Q4_K_M quantization
// from TheBloke's GGUF mirrors:
//   tiny     ~669 MB   TinyLlama-1.1B-Chat   - laptops without GPU, mobile-class
//   standard ~4.08 GB  Llama-2-7B-Chat       - 16GB+ RAM, the network's primary tier
//   big      ~7.87 GB  Llama-2-13B-Chat      - workstations w/ GPU, top earner
//
// Llama-2-70B (~39 GB) intentionally not in the auto-download set: too large
// to push through a one-click onboarding without scaring users. Operators
// who want it can `huggingface-cli download` manually + Settings → "use
// existing model".
struct ModelTierSpec {
    id: &'static str,
    display_name: &'static str,
    url: &'static str,
    size_bytes: u64,
}

const MODEL_TIERS: &[ModelTierSpec] = &[
    ModelTierSpec {
        id: "tiny",
        display_name: "TinyLlama 1.1B (Q4_K_M)",
        url: "https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf",
        size_bytes: 669_262_336,
    },
    ModelTierSpec {
        id: "standard",
        display_name: "Llama-2 7B Chat (Q4_K_M)",
        url: "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf",
        size_bytes: 4_081_004_544,
    },
    ModelTierSpec {
        id: "big",
        display_name: "Llama-2 13B Chat (Q4_K_M)",
        url: "https://huggingface.co/TheBloke/Llama-2-13B-chat-GGUF/resolve/main/llama-2-13b-chat.Q4_K_M.gguf",
        size_bytes: 7_866_070_016,
    },
];

fn tier_spec(id: &str) -> Option<&'static ModelTierSpec> {
    MODEL_TIERS.iter().find(|t| t.id == id)
}

fn models_dir() -> PathBuf {
    paths::arc_home().join("models")
}

fn model_path_for(tier: &str) -> PathBuf {
    models_dir().join(format!("{}.gguf", tier))
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

/// Map detected hardware → recommended tier id. Mirrors the existing
/// `hardware::recommend()` mapping but returns just the tier id (not the
/// human-readable model name) so the frontend has something stable to
/// pre-select in the picker.
///
/// Returns "none" when the machine isn't strong enough to run any tier
/// usefully — frontend should offer "verifier-only" mode instead.
#[tauri::command]
pub async fn recommended_tier() -> CmdResult<String> {
    let hw = hardware::detect();
    let vram = hw.gpu_vram_gb.unwrap_or(0);
    let tier = if hw.ram_gb >= 32 && vram >= 16 {
        "big"
    } else if hw.ram_gb >= 16 {
        "standard"
    } else if hw.ram_gb >= 8 {
        "tiny"
    } else {
        "none"
    };
    Ok(tier.into())
}

/// Returns `Some(path)` if the matching tier's GGUF is already on disk and
/// looks at least mostly downloaded (size within 1% of expected). Frontend
/// uses this to skip the download step on a re-install or after the upgrade
/// flow ran successfully.
#[tauri::command]
pub async fn existing_model_for_tier(tier: String) -> CmdResult<Option<String>> {
    let Some(spec) = tier_spec(&tier) else { return Ok(None) };
    let p = model_path_for(&tier);
    if !p.exists() {
        return Ok(None);
    }
    let metadata = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    let actual = metadata.len();
    let expected = spec.size_bytes;
    // Within 1% tolerance — accommodates HF re-uploads with tiny size deltas
    // without false-positiving on a half-downloaded file.
    let tolerance = expected / 100;
    if actual + tolerance < expected || actual > expected + tolerance {
        return Ok(None);
    }
    Ok(Some(p.to_string_lossy().into_owned()))
}

/// Download the GGUF for `tier` to ~/.arc/models/<tier>.gguf, streaming
/// progress events on the `model-download-progress` channel so the UI can
/// render a real progress bar.
///
/// Idempotent — if the target file already exists at the expected size,
/// returns immediately without re-downloading. Atomically renames into place
/// from a `.download` sidecar so a crashed mid-download leaves the previous
/// good copy intact.
#[tauri::command]
pub async fn download_model(app: AppHandle, tier: String) -> CmdResult<String> {
    let spec = tier_spec(&tier)
        .ok_or_else(|| format!("unknown model tier: {}", tier))?;
    let target = model_path_for(&tier);

    // Already downloaded at expected size → done.
    if let Ok(meta) = std::fs::metadata(&target) {
        let actual = meta.len();
        let tolerance = spec.size_bytes / 100;
        if actual + tolerance >= spec.size_bytes && actual <= spec.size_bytes + tolerance {
            // Emit a final 100% progress so the UI doesn't hang on a stale
            // "downloading" state when the model was already there.
            let _ = app.emit(
                "model-download-progress",
                ModelDownloadProgress {
                    tier: tier.clone(),
                    downloaded_bytes: actual,
                    total_bytes: spec.size_bytes,
                    done: true,
                },
            );
            return Ok(target.to_string_lossy().into_owned());
        }
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(map_err)?;
    }

    // Long timeout for the slowest tier on a residential connection: 13B
    // chat is ~7.9 GB, on a 5 Mbps link that's ~3.5 hours. 4 hours.
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
    let total_bytes = resp.content_length().unwrap_or(spec.size_bytes);

    let tmp = target.with_extension("download");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("create temp file: {}", e))?;

    let mut stream = resp;
    let mut downloaded: u64 = 0;
    let mut last_emit = std::time::Instant::now();
    // Emit progress at most every 250ms. HF chunks tend to land in 8-64 KB
    // units; emitting on every chunk would flood the IPC channel and pin
    // the UI thread re-rendering progress.
    let emit_every = std::time::Duration::from_millis(250);

    loop {
        let chunk = match stream.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => return Err(format!("chunk read failed at {} bytes: {}", downloaded, e)),
        };
        downloaded += chunk.len() as u64;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write to temp file: {}", e))?;

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

    file.flush().await.map_err(map_err)?;
    drop(file);

    // Atomic rename over any existing target. std::fs::rename uses
    // MoveFileEx(REPLACE_EXISTING) on Windows since Rust 1.62, so this
    // works cross-platform.
    std::fs::rename(&tmp, &target).map_err(|e| {
        // Best-effort cleanup of the temp on failure — caller should be able
        // to retry without a stale ".download" sidecar accumulating.
        let _ = std::fs::remove_file(&tmp);
        format!("rename to {}: {}", target.display(), e)
    })?;

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
    if p.exists() {
        std::fs::remove_file(&p).map_err(map_err)?;
    }
    // Also clean any stale .download sidecar from a crashed download.
    let tmp = p.with_extension("download");
    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

#[allow(dead_code)]
fn _path_helper(_: &Path) {} // keep `Path` import used if `model_path_for` returns inline
