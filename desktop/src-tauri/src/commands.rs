use crate::node_manager::{managed_binary_path, TestnetResources};
use crate::types::*;
use crate::{hardware, identity, rpc_client, AppState};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tokio::io::AsyncWriteExt;

type CmdResult<T> = Result<T, String>;

fn map_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
pub async fn detect_hardware() -> CmdResult<HardwareInfo> {
    Ok(hardware::detect())
}

#[tauri::command]
pub async fn generate_identity(state: State<'_, AppState>) -> CmdResult<Identity> {
    let id = identity::generate();
    {
        let mut store = state.store.lock().await;
        store.identity = Some(id.clone());
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
    }
    Ok(id)
}

#[tauri::command]
pub async fn import_identity(
    state: State<'_, AppState>,
    phrase: String,
) -> CmdResult<Identity> {
    // Restoration path: user types their 12-word phrase on a new device
    // and gets back the exact same address + signing keys.
    identity::validate_bip39(&phrase)?;
    let id = identity::derive(&phrase)?;
    {
        let mut store = state.store.lock().await;
        store.identity = Some(id.clone());
        let dir = state.data_dir.lock().await.clone();
        store.save_to(&dir).map_err(map_err)?;
    }
    Ok(id)
}

#[tauri::command]
pub async fn load_identity(state: State<'_, AppState>) -> CmdResult<Option<Identity>> {
    let store = state.store.lock().await;
    Ok(store.identity.clone())
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

#[tauri::command]
pub async fn start_node(
    app: AppHandle,
    state: State<'_, AppState>,
    config: NodeConfig,
) -> CmdResult<()> {
    // Version-check (and upgrade if needed) the arc-node binary on every
    // start. Cheap if it's current - one --version call - and ensures
    // existing users picked up by the desktop auto-updater don't keep
    // running a stale arc-node from a previous release. Without this,
    // chain-side bug fixes (e.g. the v0.5.7 ephemeral-UDP fallback that
    // unblocks Windows users whose Hyper-V port range covers 9091) never
    // reach anyone past their first launch.
    ensure_binary(app.clone()).await?;

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
    let resources = resolve_testnet_resources(&app);
    let mut node = state.node.lock().await;
    node.start(&config, &validator_seed, &resources)
        .await
        .map_err(map_err)
}

#[tauri::command]
pub async fn stop_node(state: State<'_, AppState>) -> CmdResult<()> {
    let mut node = state.node.lock().await;
    node.stop().await.map_err(map_err)
}

#[tauri::command]
pub async fn restart_node(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    // Same version-check as start_node - a restart is a great moment to
    // pick up a newer arc-node, since the user is already incurring the
    // restart cost.
    ensure_binary(app.clone()).await?;

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
    let resources = resolve_testnet_resources(&app);
    let mut node = state.node.lock().await;
    node.restart(&cfg, &validator_seed, &resources)
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
    use std::path::PathBuf;

    // Resolve the data dir the same way node_manager does
    let cfg = {
        let store = state.store.lock().await;
        store.config.clone().unwrap_or_default()
    };
    let data_dir: PathBuf = if let Some(rest) = cfg.data_dir.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(rest)
    } else {
        PathBuf::from(&cfg.data_dir)
    };
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

#[tauri::command]
pub async fn node_status(state: State<'_, AppState>) -> CmdResult<NodeStatus> {
    let (port, pid, address, crash) = {
        let mut node = state.node.lock().await;
        // Opportunistic crash detection - checks if our child process exited
        // unexpectedly since the last poll.
        node.try_reap_if_crashed().await;
        let pid = if node.is_running() { node.pid() } else { None };
        let port = node.rpc_port;
        let crash = node
            .crash_info
            .lock()
            .await
            .as_ref()
            .map(|c| c.message.clone());
        drop(node);
        let address = {
            let store = state.store.lock().await;
            store.identity.as_ref().map(|i| i.address.clone())
        };
        (port, pid, address, crash)
    };
    let host = wallet_host(port);
    Ok(rpc_client::fetch_status(&state.http, &host, port, pid, address, crash).await)
}

#[tauri::command]
pub async fn clear_crash(state: State<'_, AppState>) -> CmdResult<()> {
    state.node.lock().await.clear_crash().await;
    Ok(())
}

#[tauri::command]
pub async fn fetch_earnings(state: State<'_, AppState>) -> CmdResult<Earnings> {
    let port = state.node.lock().await.rpc_port;
    let address = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    };
    let host = wallet_host(port);
    Ok(rpc_client::fetch_earnings(&state.http, &host, address.as_deref()).await)
}

#[tauri::command]
pub async fn fetch_attestations(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> CmdResult<Vec<Attestation>> {
    let port = state.node.lock().await.rpc_port;
    let host = wallet_host(port);
    Ok(rpc_client::fetch_attestations(&state.http, &host, limit.unwrap_or(20)).await)
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
    let port = state.node.lock().await.rpc_port;
    let host = wallet_host(port);
    Ok(rpc_client::fetch_network_stats(&state.http, &host).await)
}

#[tauri::command]
pub async fn open_external(app: AppHandle, url: String) -> CmdResult<()> {
    use tauri_plugin_opener::OpenerExt;
    app.opener().open_url(url, None::<&str>).map_err(map_err)
}

#[tauri::command]
pub async fn fetch_balance(state: State<'_, AppState>) -> CmdResult<AccountBalance> {
    let port = state.node.lock().await.rpc_port;
    let addr = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    }
    .ok_or_else(|| "no identity".to_string())?;
    let host = wallet_host(port);
    rpc_client::fetch_balance(&state.http, &host, &addr).await
}

#[tauri::command]
pub async fn faucet_claim(state: State<'_, AppState>) -> CmdResult<FaucetResult> {
    let port = state.node.lock().await.rpc_port;
    let addr = {
        let store = state.store.lock().await;
        store.identity.as_ref().map(|i| i.address.clone())
    }
    .ok_or_else(|| "no identity".to_string())?;
    let host = wallet_host(port);
    rpc_client::faucet_claim(&state.http, &host, &addr).await
}

/// Where the wallet RPCs (balance / faucet / earnings / status /
/// attestations / network / legacy run_inference) go. Pinned to the
/// locally-spawned arc-node on `127.0.0.1:<port>` — same as v0.7.0
/// through v0.7.4. The local node's bundled genesis pre-funds the
/// user's identity (= local validator) with 1T ARC and accumulates
/// attestations as the user runs legacy inference, so the wallet
/// shows real per-user state out of the box. Tier 1 inference is
/// the only flow that goes to a different host (alpha) because the
/// alpha solo chain is what actually finalizes tier 1 requests.
fn wallet_host(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

#[tauri::command]
pub async fn run_inference(
    state: State<'_, AppState>,
    prompt: String,
    max_tokens: Option<u32>,
) -> CmdResult<InferenceResult> {
    let port = state.node.lock().await.rpc_port;
    let host = wallet_host(port);
    // Inference can take 3-30s depending on token count and hardware.
    // The shared state.http has a 3s timeout (fine for health polls) which
    // is too short here — build a dedicated client with a generous limit.
    let long_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(map_err)?;
    rpc_client::run_inference(&long_client, &host, &prompt, max_tokens.unwrap_or(32)).await
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
    prompt: String,
    max_tokens: Option<u32>,
    k: Option<u32>,
) -> CmdResult<InferenceResult> {
    // 600s / 10 min per-host timeout. Observed testnet behavior: a 3-token
    // generation through the 6-range pipeline at k=3 takes ~160s (≈54s/
    // token × 3), and prompts with longer prefill scale linearly until
    // run_consensus gains pipelined prefill (the followup noted in #35's
    // close comment).
    let long_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(map_err)?;
    let max_tokens = max_tokens.unwrap_or(32);
    let k = k.unwrap_or(3);
    let mut last_err = String::new();
    for host in COORDINATOR_HOSTS {
        match rpc_client::run_inference_consensus(&long_client, host, &prompt, max_tokens, k)
            .await
        {
            Ok(r) => return Ok(r),
            Err(e) => last_err = e,
        }
    }
    Err(format!(
        "all {} coordinators failed; last: {}",
        COORDINATOR_HOSTS.len(),
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
    prompt: String,
    max_tokens: Option<u32>,
) -> CmdResult<InferenceResult> {
    let long_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(map_err)?;
    let max_tokens = max_tokens.unwrap_or(32);
    let mut last_err = String::new();
    for host in COORDINATOR_HOSTS {
        match rpc_client::run_inference_remote(&long_client, host, &prompt, max_tokens).await
        {
            Ok(r) => return Ok(r),
            Err(e) => last_err = e,
        }
    }
    Err(format!(
        "all {} coordinators failed (direct path); last: {}",
        COORDINATOR_HOSTS.len(),
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
    let candidates = tier1_candidate_hosts();
    let mut last_err = String::from("no tier1 hosts configured");
    for host in &candidates {
        match rpc_client::tier1_submit(
            &state.http,
            host,
            &prompt,
            max_tokens.unwrap_or(32),
            max_reward.unwrap_or(10),
            deadline_blocks.unwrap_or(20),
            committee_size.unwrap_or(5),
        )
        .await
        {
            Ok(sub) => {
                state
                    .tier1_routes
                    .lock()
                    .await
                    .insert(sub.request_id.clone(), host.clone());
                return Ok(sub);
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

/// Tier 1 inference is pinned to the GCP solo host (us-central1-a,
/// v0.7.2). Multi-validator chains hit a BlockSTM regression on the
/// InferenceRequest apply path that makes tier1 hang on "no such
/// request"; the solo host avoids that codepath.
const COORDINATOR_HOSTS: [&str; 1] = [
    "http://34.133.106.125:9090", // GCP us-central1-a, solo v0.7.2
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

    // Pick the first reachable coordinator.
    let probe = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(map_err)?;
    let mut coord_url: Option<String> = None;
    for host in COORDINATOR_HOSTS {
        if let Ok(r) = probe.get(format!("{}/health", host)).send().await {
            if r.status().is_success() {
                coord_url = Some(host.to_string());
                break;
            }
        }
    }
    let coord_url = coord_url.ok_or_else(|| {
        "no coordinator reachable - all 6 testnet seeds timed out on /health".to_string()
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
    let k = k.unwrap_or(3);
    let wrapped = if prompt.contains("[INST]") {
        prompt.clone()
    } else {
        format!("[INST] {} [/INST]", prompt)
    };
    let infer_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(map_err)?;
    let infer_resp = infer_client
        .post(format!("{}/inference/run_consensus", coord_url))
        .json(&serde_json::json!({
            "input": wrapped,
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

#[tauri::command]
pub async fn check_for_update() -> CmdResult<UpdateCheck> {
    // Query the public GitHub releases API for the latest tag.
    let client = reqwest::Client::builder()
        .user_agent("arc-desktop/0.1")
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(map_err)?;
    let resp = client
        .get("https://api.github.com/repos/FerrumVir/arc-chain/releases/latest")
        .send()
        .await
        .map_err(map_err)?;
    let v: serde_json::Value = resp.json().await.map_err(map_err)?;
    let version = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .map(|s| s.trim_start_matches('v').to_string())
        .unwrap_or_else(|| "unknown".into());
    let current = env!("CARGO_PKG_VERSION");
    Ok(UpdateCheck {
        has_update: version != current && version != "unknown",
        version,
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
    let target = managed_binary_path();
    if target.exists() {
        match read_arc_node_version(&target) {
            Some(ref v) if v == EXPECTED_NODE_VERSION => {
                return Ok(BinaryStatus {
                    path: target.to_string_lossy().into_owned(),
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    already_installed: true,
                });
            }
            Some(v) => {
                let relation = if semver_gt(&v, EXPECTED_NODE_VERSION) { "newer" } else { "older" };
                tracing::info!(
                    "arc-node {} at {} is {} than desktop's expected {} - replacing with matched version",
                    v,
                    target.display(),
                    relation,
                    EXPECTED_NODE_VERSION
                );
            }
            None => {
                tracing::warn!(
                    "arc-node binary at {} is unreadable or missing --version - replacing",
                    target.display()
                );
            }
        }
        // Fall through to download. We don't pre-remove the target; the
        // download writes to a `.download` sidecar then atomically renames
        // over the existing binary, so a failed download leaves the working
        // copy in place.
    }

    let asset = platform_release_asset().ok_or_else(|| {
        format!(
            "no prebuilt arc-node binary for platform {}-{} - build from source or open an issue",
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
    std::fs::rename(&tmp, &target).map_err(map_err)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&target, perms).map_err(map_err)?;
    }

    // Best-effort: strip any macOS quarantine flag on our own download.
    // User still needs to allow the desktop .app itself past Gatekeeper.
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(&target)
            .output();
    }

    let _ = app; // reserved for future progress events via app.emit(...)

    Ok(BinaryStatus {
        path: target.to_string_lossy().into_owned(),
        downloaded_bytes: total_bytes,
        total_bytes,
        already_installed: false,
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
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".arc").join("models")
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
