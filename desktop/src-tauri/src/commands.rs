use crate::node_manager::{managed_binary_path, TestnetResources};
use crate::types::*;
use crate::{hardware, identity, paths, rpc_client, AppState};
use sha2::{Digest as _, Sha256};
use std::io::Read as _;
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
pub async fn reset_peer_state(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ResetPeerStateResult> {
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

#[tauri::command]
pub async fn open_external(app: AppHandle, url: String) -> CmdResult<()> {
    use tauri_plugin_opener::OpenerExt;
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
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_earnings_projection(&state.http, &host, address.as_deref()).await)
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
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_network_overview(&state.http, &host).await)
}

#[tauri::command]
pub async fn fetch_recent_blocks(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<crate::types::RecentBlocks> {
    let host = chain_host(&state).await;
    Ok(rpc_client::fetch_recent_blocks(&state.http, &host, limit.unwrap_or(10)).await)
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
    let host = chain_host(&state).await;
    Ok(rpc_client::lookup_tx(&state.http, &host, &hash).await)
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

    let host = crate::wallet::validate_rpc_origin(&chain_host(&state).await)?;
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
            &client,
            host,
            &prompt,
            max_tokens,
            k,
            chat_template,
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
        match rpc_client::run_inference_remote(&client, host, &prompt, max_tokens, chat_template)
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

const SETTLEMENT_WRITE_UNAVAILABLE: &str =
    "is unavailable in the v0.8.0 recovery candidate before any transaction is signed or submitted: exact model-artifact binding, validator-authenticated authorization, and settlement are not production-ready. VRF selection and server-derived replica labels are not validator approval. Free/community inference remains available.";

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
/// No longer a priority list with a pinned `[0]` — `chain_host()` elects
/// among these by block freshness on every TTL expiry. Order is
/// presentational only.
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
const EXPECTED_NODE_VERSION: &str = env!("CARGO_PKG_VERSION");
const ARC_RELEASE_DOWNLOAD_ROOT: &str = "https://github.com/FerrumVir/arc-chain/releases/download";
const MAX_NODE_BINARY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CHECKSUM_MANIFEST_BYTES: usize = 1024 * 1024;

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

fn binary_download_sidecar(target: &Path) -> PathBuf {
    match target.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => target.with_extension(format!("download.{}", extension)),
        None => target.with_extension("download"),
    }
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
                    ));
                } else {
                    return Err(format!(
                        "managed arc-node at {} reports unrecognized version '{}'; expected v{}",
                        target.display(),
                        v,
                        EXPECTED_NODE_VERSION
                    ));
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
    let url = exact_release_asset_url(asset);
    let checksum_url = exact_release_asset_url("SHA256SUMS");

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(map_err)?;
    }

    let client = reqwest::Client::builder()
        .user_agent(format!("arc-desktop/{}", EXPECTED_NODE_VERSION))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(map_err)?;

    let checksum_resp = client.get(&checksum_url).send().await.map_err(map_err)?;
    if !checksum_resp.status().is_success() {
        return Err(format!(
            "release checksum manifest returned HTTP {} for v{}",
            checksum_resp.status(),
            EXPECTED_NODE_VERSION
        ));
    }
    if checksum_resp
        .content_length()
        .is_some_and(|length| length > MAX_CHECKSUM_MANIFEST_BYTES as u64)
    {
        return Err("release checksum manifest exceeds the 1 MiB safety limit".to_string());
    }
    let checksum_bytes = checksum_resp.bytes().await.map_err(map_err)?;
    if checksum_bytes.len() > MAX_CHECKSUM_MANIFEST_BYTES {
        return Err("release checksum manifest exceeds the 1 MiB safety limit".to_string());
    }
    let checksum_manifest = std::str::from_utf8(&checksum_bytes)
        .map_err(|_| "release checksum manifest is not UTF-8".to_string())?;
    let expected_sha256 = expected_release_sha256(checksum_manifest, asset)?;

    let mut resp = client.get(&url).send().await.map_err(map_err)?;
    if !resp.status().is_success() {
        return Err(format!(
            "release asset {} returned HTTP {}",
            asset,
            resp.status()
        ));
    }
    if resp
        .content_length()
        .is_some_and(|length| length > MAX_NODE_BINARY_BYTES)
    {
        return Err(format!(
            "release asset {} exceeds the 512 MiB safety limit",
            asset
        ));
    }
    let tmp = binary_download_sidecar(target);
    let mut file = tokio::fs::File::create(&tmp).await.map_err(map_err)?;
    let mut hasher = Sha256::new();
    let mut total_bytes = 0u64;
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(format!(
                    "release asset {} failed after {} bytes: {}",
                    asset, total_bytes, error
                ));
            }
        };
        total_bytes = total_bytes.saturating_add(chunk.len() as u64);
        if total_bytes > MAX_NODE_BINARY_BYTES {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!(
                "release asset {} exceeds the 512 MiB safety limit",
                asset
            ));
        }
        hasher.update(&chunk);
        if let Err(error) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!("write {}: {}", tmp.display(), error));
        }
    }
    file.flush().await.map_err(map_err)?;
    file.sync_all().await.map_err(map_err)?;
    drop(file);

    let actual_sha256: [u8; 32] = hasher.finalize().into();
    if actual_sha256 != expected_sha256 {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!(
            "checksum verification failed for {} (expected {}, got {})",
            asset,
            hex::encode(expected_sha256),
            hex::encode(actual_sha256)
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&tmp, perms).map_err(map_err)?;
    }
    let downloaded_version = read_arc_node_version(&tmp).ok_or_else(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("downloaded {} did not report a parseable version", asset)
    })?;
    if downloaded_version != EXPECTED_NODE_VERSION {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "downloaded {} reports v{}, expected v{}",
            asset, downloaded_version, EXPECTED_NODE_VERSION
        ));
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
        format!(
            "could not install new arc-node at {}: {}",
            target.display(),
            e
        )
    })
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

fn resolve_testnet_resources(app: &AppHandle) -> TestnetResources {
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
//   big      ~7.87 GB  Llama-2-13B-Chat      - workstations w/ GPU
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
    /// SHA-256 from the repository's immutable LFS object ID. URLs may move,
    /// but a desktop-selected tier must always resolve to these exact bytes.
    sha256: &'static str,
}

const MODEL_TIERS: &[ModelTierSpec] = &[
    ModelTierSpec {
        id: "tiny",
        display_name: "TinyLlama 1.1B (Q4_K_M)",
        url: "https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf",
        size_bytes: 668_788_096,
        sha256: "9fecc3b3cd76bba89d504f29b616eedf7da85b96540e490ca5824d3f7d2776a0",
    },
    ModelTierSpec {
        id: "standard",
        display_name: "Llama-2 7B Chat (Q4_K_M)",
        url: "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf",
        size_bytes: 4_081_004_224,
        sha256: "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa",
    },
    ModelTierSpec {
        id: "big",
        display_name: "Llama-2 13B Chat (Q4_K_M)",
        url: "https://huggingface.co/TheBloke/Llama-2-13B-chat-GGUF/resolve/main/llama-2-13b-chat.Q4_K_M.gguf",
        size_bytes: 7_865_956_224,
        sha256: "7ddfe27f61bf994542c22aca213c46ecbd8a624cca74abff02a7b5a8c18f787f",
    },
];

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

/// Download the GGUF for `tier` to ~/.arc/models/<tier>.gguf, streaming
/// progress events on the `model-download-progress` channel so the UI can
/// render a real progress bar.
///
/// Idempotent only for an exact pinned artifact. The stream is checked against
/// both its exact LFS size and SHA-256 before an atomic rename from the
/// `.download` sidecar, so a crash or mirror mutation cannot replace a known
/// good model.
#[tauri::command]
pub async fn download_model(app: AppHandle, tier: String) -> CmdResult<String> {
    let spec = tier_spec(&tier).ok_or_else(|| format!("unknown model tier: {}", tier))?;
    let target = model_path_for(&tier);

    // Already downloaded and hash-verified → done.
    let verify_path = target.clone();
    if tokio::task::spawn_blocking(move || verify_model_file(&verify_path, spec))
        .await
        .map_err(map_err)??
    {
        let _ = app.emit(
            "model-download-progress",
            ModelDownloadProgress {
                tier: tier.clone(),
                downloaded_bytes: spec.size_bytes,
                total_bytes: spec.size_bytes,
                done: true,
            },
        );
        return Ok(target.to_string_lossy().into_owned());
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
    if let Some(length) = resp.content_length() {
        if length != spec.size_bytes {
            return Err(format!(
                "GGUF mirror reported {} bytes for tier {}, expected {}",
                length, tier, spec.size_bytes
            ));
        }
    }
    let total_bytes = spec.size_bytes;

    let tmp = target.with_extension("download");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("create temp file: {}", e))?;

    let mut stream = resp;
    let mut downloaded: u64 = 0;
    let mut hasher = Sha256::new();
    let mut last_emit = std::time::Instant::now();
    // Emit progress at most every 250ms. HF chunks tend to land in 8-64 KB
    // units; emitting on every chunk would flood the IPC channel and pin
    // the UI thread re-rendering progress.
    let emit_every = std::time::Duration::from_millis(250);

    loop {
        let chunk = match stream.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(format!(
                    "chunk read failed at {} bytes: {}",
                    downloaded, error
                ));
            }
        };
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > spec.size_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!(
                "model tier {} exceeded its pinned size of {} bytes",
                tier, spec.size_bytes
            ));
        }
        hasher.update(&chunk);
        if let Err(error) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!("write to temp file: {}", error));
        }

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
    file.sync_all().await.map_err(map_err)?;
    drop(file);

    if downloaded != spec.size_bytes {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!(
            "model tier {} ended at {} bytes, expected {}",
            tier, downloaded, spec.size_bytes
        ));
    }
    let actual_sha256: [u8; 32] = hasher.finalize().into();
    let expected_sha256 = model_digest(spec)?;
    if actual_sha256 != expected_sha256 {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!(
            "SHA-256 verification failed for model tier {}",
            tier
        ));
    }

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

#[cfg(test)]
mod release_binary_tests {
    use super::*;

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
    fn node_asset_urls_are_version_pinned() {
        let url = exact_release_asset_url("arc-node-linux-x86_64");
        assert!(url.contains(&format!("/v{}/", EXPECTED_NODE_VERSION)));
        assert!(!url.contains("/latest/"));
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

    #[test]
    fn download_sidecar_preserves_windows_executable_suffix() {
        assert_eq!(
            binary_download_sidecar(Path::new("arc-node.exe")),
            PathBuf::from("arc-node.download.exe")
        );
        assert_eq!(
            binary_download_sidecar(Path::new("arc-node")),
            PathBuf::from("arc-node.download")
        );
    }

    #[test]
    fn version_comparison_rejects_non_strict_values() {
        assert!(semver_gt("0.8.0", "0.7.11"));
        assert!(!semver_gt("0.8.0-beta", "0.7.11"));
        assert!(!semver_gt("0.7", "0.6.99"));
        assert!(!semver_gt("0.8.0.1", "0.8.0"));
    }

    #[test]
    fn every_builtin_model_has_a_fixed_sha256() {
        for spec in MODEL_TIERS {
            assert_eq!(model_digest(spec).unwrap().len(), 32, "{}", spec.id);
            assert_eq!(spec.sha256.len(), 64, "{}", spec.id);
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
