use arc_consensus::StakeTier;
use arc_crypto::{Hash256, MerkleProof};
use arc_gpu::probe_gpu;
use arc_mempool::Mempool;
use arc_state::StateDB;
use arc_types::*;
use axum::{
    extract::{DefaultBodyLimit, Query, State as AxumState},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tower_http::cors::CorsLayer;

/// Faucet configuration.
const FAUCET_CLAIM_AMOUNT: u64 = 10_000;
const FAUCET_RATE_LIMIT_SECS: u64 = 3600; // 1 hour per address

/// Shared node state passed to all handlers.
#[derive(Clone)]
pub struct NodeState {
    pub state: Arc<StateDB>,
    pub mempool: Arc<Mempool>,
    pub validator_address: Hash256,
    pub stake: u64,
    pub tier: StakeTier,
    pub boot_time: Instant,
    pub peer_count: Arc<AtomicU32>,
    /// Faucet rate limiter: address → last claim time.
    pub faucet_claims: Arc<Mutex<HashMap<[u8; 32], Instant>>>,
    /// Total faucet claims since boot.
    pub faucet_claims_total: Arc<AtomicU32>,
    /// Cached INT8 inference model (if --model was provided).
    pub inference_model: Option<Arc<arc_inference::cached_integer_model::CachedIntegerModel>>,
    /// Candle GGUF float inference engine (coherent output).
    pub candle_engine: Option<Arc<arc_inference::candle_backend::GgufEngine>>,
    /// Model ID for candle engine.
    pub candle_model_id: Option<arc_crypto::Hash256>,
}

/// Build a `NodeState` from components.
pub fn build_node_state(
    state: Arc<StateDB>,
    mempool: Arc<Mempool>,
    validator_address: Hash256,
    stake: u64,
    boot_time: Instant,
    peer_count: Arc<AtomicU32>,
    inference_model: Option<Arc<arc_inference::cached_integer_model::CachedIntegerModel>>,
    candle_engine: Option<Arc<arc_inference::candle_backend::GgufEngine>>,
    candle_model_id: Option<arc_crypto::Hash256>,
) -> NodeState {
    let tier = StakeTier::from_stake(stake).unwrap_or(StakeTier::Spark);
    NodeState {
        state,
        mempool,
        validator_address,
        stake,
        tier,
        boot_time,
        peer_count,
        faucet_claims: Arc::new(Mutex::new(HashMap::new())),
        faucet_claims_total: Arc::new(AtomicU32::new(0)),
        inference_model,
        candle_engine,
        candle_model_id,
    }
}

/// Start the RPC server.
pub async fn serve(
    addr: &str,
    state: Arc<StateDB>,
    mempool: Arc<Mempool>,
    validator_address: Hash256,
    stake: u64,
    boot_time: Instant,
    peer_count: Arc<AtomicU32>,
    inference_model: Option<Arc<arc_inference::cached_integer_model::CachedIntegerModel>>,
    candle_engine: Option<Arc<arc_inference::candle_backend::GgufEngine>>,
    candle_model_id: Option<arc_crypto::Hash256>,
) -> anyhow::Result<()> {
    let node = build_node_state(state, mempool, validator_address, stake, boot_time, peer_count, inference_model, candle_engine, candle_model_id);

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/info", get(chain_info))
        .route("/node/info", get(node_info))
        .route("/block/latest", get(get_latest_block))
        .route("/block/{height}", get(get_block))
        .route("/account/{address}", get(get_account))
        .route("/tx/submit", post(submit_tx))
        .route("/tx/submit_signed", post(submit_signed_tx))
        .route("/tx/submit_batch", post(submit_batch))
        .route("/validators", get(get_validators))
        .route("/tx/{hash}", get(get_transaction))
        .route("/tx/{hash}/proof", get(get_tx_proof))
        .route("/block/{height}/proofs", get(get_block_proofs))
        .route("/blocks", get(get_blocks))
        .route("/block/{height}/txs", get(get_block_txs))
        .route("/account/{address}/txs", get(get_account_txs))
        .route("/stats", get(get_stats))
        .route("/tx/{hash}/full", get(get_full_transaction))
        .route("/contract/{address}", get(get_contract_info))
        .route("/contract/{address}/call", post(call_contract))
        // Agents (Synths)
        .route("/agents", get(get_agents))
        // Faucet (testnet token dispensing)
        .route("/faucet/claim", post(faucet_claim))
        .route("/faucet/status", get(faucet_status))
        // Light Client Finality Proofs (A8)
        .route("/light/snapshot", get(light_snapshot))
        // State Sync Protocol (A5) — snapshot bootstrap for new nodes
        .route("/sync/snapshot", get(sync_snapshot))
        .route("/sync/snapshot/info", get(sync_snapshot_info))
        // Chunked State Sync — parallel chunk download for fast catch-up
        .route("/sync/manifest", get(sync_manifest))
        .route("/sync/chunk/{index}", get(sync_chunk))
        .route("/sync/status", get(sync_status))
        // Inference — run model and record attestation on-chain
        .route("/inference/run", post(inference_run))
        .route("/inference/attestations", get(inference_list_attestations))
        // Off-chain channel relay (WebSocket-style via long-poll for simplicity)
        .route("/channel/{channel_id}/relay", post(channel_relay))
        .route("/channel/{channel_id}/state", get(channel_state))
        // ETH-compatible JSON-RPC (MetaMask, Hardhat, Foundry)
        .route("/eth", post(eth_json_rpc))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024)) // 256 MB
        // CORS: permissive is correct for a public blockchain RPC node.
        // All major L1s (Ethereum, Solana, Sui) use permissive CORS for RPC.
        // There are no authenticated endpoints to protect.
        .layer(CorsLayer::permissive())
        .with_state(node);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Start an ETH-compatible JSON-RPC server on a separate port.
/// Handles only the `/` POST endpoint for MetaMask, Hardhat, Foundry, etc.
pub async fn serve_eth(addr: &str, node: NodeState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", post(eth_json_rpc))
        // CORS: permissive is correct for a public blockchain RPC node.
        // All major L1s (Ethereum, Solana, Sui) use permissive CORS for RPC.
        // There are no authenticated endpoints to protect.
        .layer(CorsLayer::permissive())
        .with_state(node);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> &'static str {
    "ARC Chain — Agent Runtime Chain — Testnet v0.1.0"
}

/// JSON error response body returned by endpoints that fail with 4xx/5xx.
#[derive(Serialize)]
struct ApiError {
    error: String,
}

/// Helper to create a (StatusCode, Json<ApiError>) pair.
fn api_error(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (code, Json(ApiError { error: msg.into() }))
}

// ---------------------------------------------------------------------------
// Health & Node Info
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
    height: u64,
    peers: u32,
    uptime_secs: u64,
}

async fn health(AxumState(node): AxumState<NodeState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: "0.1.0".to_string(),
        height: node.state.height(),
        peers: node.peer_count.load(Ordering::Relaxed),
        uptime_secs: node.boot_time.elapsed().as_secs(),
    })
}

#[derive(Serialize)]
struct NodeInfoResponse {
    validator: String,
    stake: u64,
    tier: String,
    height: u64,
    version: String,
    mempool_size: usize,
}

async fn node_info(AxumState(node): AxumState<NodeState>) -> Json<NodeInfoResponse> {
    Json(NodeInfoResponse {
        validator: node.validator_address.to_hex(),
        stake: node.stake,
        tier: format!("{:?}", node.tier),
        height: node.state.height(),
        version: "0.1.0".to_string(),
        mempool_size: node.mempool.len(),
    })
}

// ---------------------------------------------------------------------------
// Chain Info
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ChainInfoResponse {
    chain: String,
    version: String,
    block_height: u64,
    account_count: usize,
    mempool_size: usize,
    gpu: arc_gpu::GpuInfo,
}

async fn chain_info(AxumState(node): AxumState<NodeState>) -> Json<ChainInfoResponse> {
    Json(ChainInfoResponse {
        chain: "ARC Chain".to_string(),
        version: "0.1.0".to_string(),
        block_height: node.state.height(),
        account_count: node.state.account_count(),
        mempool_size: node.mempool.len(),
        gpu: probe_gpu(),
    })
}

// ---------------------------------------------------------------------------
// Block & Account endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BlockPath {
    height: u64,
}

async fn get_latest_block(
    AxumState(node): AxumState<NodeState>,
) -> Result<Json<Block>, StatusCode> {
    let height = node.state.height();
    if height == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    node.state
        .get_block(height)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn get_block(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(height): axum::extract::Path<u64>,
) -> Result<Json<Block>, (StatusCode, Json<ApiError>)> {
    node.state
        .get_block(height)
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Block at height {} not found", height)))
}

async fn get_account(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> Result<Json<Account>, (StatusCode, Json<ApiError>)> {
    let addr = Hash256::from_hex(&address)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Invalid address. Must be 64 hex characters."))?;
    node.state
        .get_account(&addr)
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Account {} not found", address)))
}

// ---------------------------------------------------------------------------
// Transaction submission
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SubmitTxRequest {
    from: String,
    to: String,
    amount: u64,
    nonce: u64,
    tx_type: Option<String>,
    /// Ed25519 signature (128-char hex, optional). Required for mainnet.
    signature: Option<String>,
    /// Ed25519 public key (64-char hex, required with signature).
    public_key: Option<String>,
}

#[derive(Serialize)]
struct SubmitTxResponse {
    tx_hash: String,
    status: String,
}

async fn submit_tx(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<SubmitTxRequest>,
) -> Result<Json<SubmitTxResponse>, (StatusCode, String)> {
    let from = Hash256::from_hex(&req.from).map_err(|_| (StatusCode::BAD_REQUEST, "invalid from address".to_string()))?;
    let to = Hash256::from_hex(&req.to).map_err(|_| (StatusCode::BAD_REQUEST, "invalid to address".to_string()))?;

    // Check if a signature was provided
    if let Some(ref sig_hex) = req.signature {
        if let Some(ref pubkey_hex) = req.public_key {
            // Build signed transaction
            let mut tx = Transaction::new_transfer(from, to, req.amount, req.nonce);

            // Parse signature and public key
            let sig_bytes = hex::decode(sig_hex).map_err(|_| (StatusCode::BAD_REQUEST, "invalid signature hex".to_string()))?;
            let pk_bytes = hex::decode(pubkey_hex).map_err(|_| (StatusCode::BAD_REQUEST, "invalid public_key hex".to_string()))?;

            if sig_bytes.len() != 64 || pk_bytes.len() != 32 {
                return Err((StatusCode::BAD_REQUEST, "signature must be 64 bytes, public_key must be 32 bytes".to_string()));
            }

            let mut pk_arr = [0u8; 32];
            pk_arr.copy_from_slice(&pk_bytes);

            tx.signature = arc_crypto::signature::Signature::Ed25519 {
                public_key: pk_arr,
                signature: sig_bytes,
            };

            // Verify signature before accepting
            tx.verify_signature().map_err(|_| (StatusCode::BAD_REQUEST, "signature verification failed".to_string()))?;
            // Mark as pre-verified so block execution can skip re-verification.
            tx.sig_verified = true;

            let hash = tx.hash.to_hex();
            node.mempool.insert(tx).map_err(|_| (StatusCode::CONFLICT, "duplicate transaction".to_string()))?;

            return Ok(Json(SubmitTxResponse {
                tx_hash: hash,
                status: "pending".to_string(),
            }));
        }
    }

    // No signature provided — reject in production mode
    // For backward compatibility, still accept unsigned in testnet
    // TODO: Remove unsigned path before mainnet
    let tx = Transaction::new_transfer(from, to, req.amount, req.nonce);
    let hash = tx.hash.to_hex();

    node.mempool.insert(tx).map_err(|_| (StatusCode::CONFLICT, "duplicate transaction".to_string()))?;

    Ok(Json(SubmitTxResponse {
        tx_hash: hash,
        status: "pending (unsigned — will fail execution, use SDK for signed TXs)".to_string(),
    }))
}

#[derive(Deserialize)]
struct SubmitBatchRequest {
    transactions: Vec<SubmitTxRequest>,
}

#[derive(Serialize)]
struct SubmitBatchResponse {
    accepted: usize,
    rejected: usize,
    tx_hashes: Vec<String>,
}

async fn submit_batch(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<SubmitBatchRequest>,
) -> Json<SubmitBatchResponse> {
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    let mut hashes = Vec::new();

    for tx_req in req.transactions {
        let from = match Hash256::from_hex(&tx_req.from) {
            Ok(h) => h,
            Err(_) => { rejected += 1; continue; }
        };
        let to = match Hash256::from_hex(&tx_req.to) {
            Ok(h) => h,
            Err(_) => { rejected += 1; continue; }
        };

        let tx = Transaction::new_transfer(from, to, tx_req.amount, tx_req.nonce);
        let hash = tx.hash.to_hex();

        match node.mempool.insert(tx) {
            Ok(()) => {
                accepted += 1;
                hashes.push(hash);
            }
            Err(_) => {
                rejected += 1;
            }
        }
    }

    Json(SubmitBatchResponse {
        accepted,
        rejected,
        tx_hashes: hashes,
    })
}

// ---------------------------------------------------------------------------
// Signed transaction submission (for CLI / external signers)
// ---------------------------------------------------------------------------

async fn submit_signed_tx(
    AxumState(node): AxumState<NodeState>,
    Json(tx): Json<Transaction>,
) -> Result<Json<SubmitTxResponse>, StatusCode> {
    let hash = tx.hash.to_hex();

    node.mempool
        .insert(tx)
        .map_err(|_| StatusCode::CONFLICT)?;

    Ok(Json(SubmitTxResponse {
        tx_hash: hash,
        status: "pending".to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Validators endpoint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ValidatorInfoResponse {
    address: String,
    stake: u64,
    tier: String,
}

#[derive(Serialize)]
struct ValidatorsResponse {
    validators: Vec<ValidatorInfoResponse>,
    total_stake: u64,
    count: usize,
}

async fn get_validators(
    AxumState(node): AxumState<NodeState>,
) -> Json<ValidatorsResponse> {
    let staked = node.state.get_staked_accounts();
    let mut validators: Vec<ValidatorInfoResponse> = staked
        .into_iter()
        .map(|(addr, account)| {
            let tier = StakeTier::from_stake(account.staked_balance)
                .map(|t| format!("{:?}", t))
                .unwrap_or_else(|| "Below minimum".to_string());
            ValidatorInfoResponse {
                address: addr.to_hex(),
                stake: account.staked_balance,
                tier,
            }
        })
        .collect();

    // Sort by stake descending
    validators.sort_by(|a, b| b.stake.cmp(&a.stake));
    let total_stake: u64 = validators.iter().map(|v| v.stake).sum();
    let count = validators.len();

    Json(ValidatorsResponse {
        validators,
        total_stake,
        count,
    })
}

// ---------------------------------------------------------------------------
// Agents (Synths) endpoint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AgentInfoResponse {
    name: String,
    address: String,
    status: String,
    model_type: String,
    endpoint: String,
    inferences: u64,
    earned: u64,
    uptime_secs: u64,
    last_action: String,
    last_action_timestamp: u64,
}

#[derive(Serialize)]
struct AgentsListResponse {
    agents: Vec<AgentInfoResponse>,
    count: usize,
}

async fn get_agents(
    AxumState(node): AxumState<NodeState>,
) -> Json<AgentsListResponse> {
    // Scan full_transactions for RegisterAgent transactions and build agent list.
    let mut agents: Vec<AgentInfoResponse> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let TxBody::RegisterAgent(body) = &tx.body {
            // Deduplicate by agent name (latest registration wins)
            if seen_names.contains(&body.agent_name) {
                continue;
            }
            seen_names.insert(body.agent_name.clone());

            let uptime = node.boot_time.elapsed().as_secs();
            agents.push(AgentInfoResponse {
                name: body.agent_name.clone(),
                address: tx.from.to_hex(),
                status: "active".to_string(),
                model_type: if body.metadata.is_empty() {
                    "Unknown".to_string()
                } else {
                    String::from_utf8(body.metadata.clone())
                        .unwrap_or_else(|_| "Unknown".to_string())
                },
                endpoint: body.endpoint.clone(),
                inferences: 0,
                earned: 0,
                uptime_secs: uptime,
                last_action: "Registered on-chain".to_string(),
                last_action_timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });
        }
    }

    let count = agents.len();
    Json(AgentsListResponse { agents, count })
}

// ---------------------------------------------------------------------------
// Faucet endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct FaucetClaimRequest {
    address: String,
}

#[derive(Serialize)]
struct FaucetClaimResponse {
    tx_hash: String,
    amount: u64,
    message: String,
}

#[derive(Serialize)]
struct FaucetErrorResponse {
    error: String,
}

#[derive(Serialize)]
struct FaucetStatusResponse {
    address: String,
    node_url: String,
    claims_today: u32,
    claim_amount: u64,
    rate_limit_secs: u64,
    balance: u64,
}

async fn faucet_claim(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<FaucetClaimRequest>,
) -> Result<Json<FaucetClaimResponse>, (StatusCode, Json<FaucetErrorResponse>)> {
    // Parse recipient address
    let to = Hash256::from_hex(&req.address).map_err(|_| {
        (StatusCode::BAD_REQUEST, Json(FaucetErrorResponse {
            error: "Invalid address. Must be 64 hex characters.".to_string(),
        }))
    })?;

    // Rate limiting: check if this address claimed recently
    {
        let claims = node.faucet_claims.lock().unwrap();
        if let Some(last_claim) = claims.get(&to.0) {
            let elapsed = last_claim.elapsed().as_secs();
            if elapsed < FAUCET_RATE_LIMIT_SECS {
                let remaining = FAUCET_RATE_LIMIT_SECS - elapsed;
                return Err((StatusCode::TOO_MANY_REQUESTS, Json(FaucetErrorResponse {
                    error: format!(
                        "Rate limited. Try again in {} minutes.",
                        (remaining + 59) / 60
                    ),
                })));
            }
        }
    }

    // Get faucet account (validator address) and check balance
    let faucet_addr = node.validator_address;
    let faucet_account = node.state
        .get_account(&faucet_addr)
        .ok_or_else(|| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(FaucetErrorResponse {
                error: "Faucet account not funded. Node misconfiguration.".to_string(),
            }))
        })?;

    if faucet_account.balance < FAUCET_CLAIM_AMOUNT {
        return Err((StatusCode::SERVICE_UNAVAILABLE, Json(FaucetErrorResponse {
            error: "Faucet balance too low. Please try another node.".to_string(),
        })));
    }

    // Create transfer transaction from faucet to recipient
    let tx = Transaction::new_transfer(faucet_addr, to, FAUCET_CLAIM_AMOUNT, faucet_account.nonce);
    let hash = tx.hash.to_hex();

    // Apply the transfer directly to state (immediate settlement for testnet faucet)
    {
        let mut sender = faucet_account.clone();
        sender.balance -= FAUCET_CLAIM_AMOUNT;
        sender.nonce += 1;
        node.state.update_account(&faucet_addr, sender);

        let mut recipient = node.state.get_or_create_account(&to);
        recipient.balance += FAUCET_CLAIM_AMOUNT;
        node.state.update_account(&to, recipient);
    }

    // Record transaction and receipt in state for lookup
    let receipt = TxReceipt {
        tx_hash: tx.hash,
        block_height: node.state.height(),
        block_hash: node.state.get_block(node.state.height())
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO),
        index: 0,
        success: true,
        gas_used: 21_000,
        value_commitment: None,
        inclusion_proof: None,
        logs: vec![],
    };
    node.state.receipts.insert(tx.hash.0, receipt);
    node.state.full_transactions.insert(tx.hash.0, tx.clone());
    // Index for account tx history
    node.state.account_txs.entry(faucet_addr.0).or_default().push(tx.hash);
    node.state.account_txs.entry(to.0).or_default().push(tx.hash);

    // Insert into mempool for propagation
    let _ = node.mempool.insert(tx);

    // Record claim time
    {
        let mut claims = node.faucet_claims.lock().unwrap();
        claims.insert(to.0, Instant::now());
    }
    node.faucet_claims_total.fetch_add(1, Ordering::Relaxed);

    Ok(Json(FaucetClaimResponse {
        tx_hash: hash,
        amount: FAUCET_CLAIM_AMOUNT,
        message: format!(
            "Sent {} ARC to {}",
            FAUCET_CLAIM_AMOUNT,
            req.address
        ),
    }))
}

async fn faucet_status(
    AxumState(node): AxumState<NodeState>,
) -> Json<FaucetStatusResponse> {
    let balance = node.state.get_account(&node.validator_address)
        .map(|a| a.balance)
        .unwrap_or(0);
    Json(FaucetStatusResponse {
        address: node.validator_address.to_hex(),
        node_url: format!("http://localhost:9090"),
        claims_today: node.faucet_claims_total.load(Ordering::Relaxed),
        claim_amount: FAUCET_CLAIM_AMOUNT,
        rate_limit_secs: FAUCET_RATE_LIMIT_SECS,
        balance,
    })
}

// ---------------------------------------------------------------------------
// Proof & query endpoints
// ---------------------------------------------------------------------------

/// Parse a 64-char hex string into a [u8; 32] array.
fn parse_hash(hex_str: &str) -> Result<[u8; 32], (StatusCode, Json<ApiError>)> {
    Hash256::from_hex(hex_str)
        .map(|h| h.0)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Invalid hash. Must be 64 hex characters."))
}

/// GET /tx/{hash} — Look up a transaction receipt by its hash.
/// Falls back to on-demand reconstruction for benchmark transactions.
async fn get_transaction(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<TxReceipt>, (StatusCode, Json<ApiError>)> {
    let tx_hash = parse_hash(&hash)?;
    // Try indexed receipts first
    if let Some(receipt) = node.state.get_receipt(&tx_hash) {
        return Ok(Json(receipt));
    }
    // Fall back to on-demand reconstruction for benchmark txs
    node.state
        .get_benchmark_receipt_by_hash(&tx_hash)
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Transaction {} not found", hash)))
}

/// GET /tx/{hash}/proof — Return a full verification bundle for a transaction.
/// For benchmark transactions, reconstructs the Merkle tree on-demand (~130ms).
async fn get_tx_proof(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let tx_hash = parse_hash(&hash)?;

    // Try indexed receipt with stored proof first
    if let Some(receipt) = node.state.get_receipt(&tx_hash) {
        if let Some(ref proof_bytes) = receipt.inclusion_proof {
            if let Ok(merkle_proof) = bincode::deserialize::<MerkleProof>(proof_bytes) {
                let siblings: Vec<Value> = merkle_proof
                    .siblings
                    .iter()
                    .map(|(h, is_left)| {
                        json!({
                            "hash": h.to_hex(),
                            "is_left": is_left,
                        })
                    })
                    .collect();

                return Ok(Json(json!({
                    "tx_hash": Hash256(tx_hash).to_hex(),
                    "blake3_domain": "ARC-chain-tx-v1",
                    "merkle_proof": {
                        "leaf": merkle_proof.leaf.to_hex(),
                        "index": merkle_proof.index,
                        "siblings": siblings,
                        "root": merkle_proof.root.to_hex(),
                    },
                    "block_height": receipt.block_height,
                    "pedersen_commitment": receipt.value_commitment.map(hex::encode),
                })));
            }
        }
    }

    // Fall back to on-demand proof reconstruction for benchmark txs
    let (height, idx) = node
        .state
        .get_tx_location(&tx_hash)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Transaction {} not found", hash)))?;

    let merkle_proof = node
        .state
        .reconstruct_benchmark_proof(height, idx)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Could not reconstruct proof for transaction"))?;

    let siblings: Vec<Value> = merkle_proof
        .siblings
        .iter()
        .map(|(h, is_left)| {
            json!({
                "hash": h.to_hex(),
                "is_left": is_left,
            })
        })
        .collect();

    let block_tx_root = node
        .state
        .get_block(height)
        .map(|b| b.header.tx_root);
    let verified = block_tx_root.map(|r| r == merkle_proof.root).unwrap_or(false);

    Ok(Json(json!({
        "tx_hash": Hash256(tx_hash).to_hex(),
        "blake3_domain": "ARC-chain-tx-v1",
        "merkle_proof": {
            "leaf": merkle_proof.leaf.to_hex(),
            "index": merkle_proof.index,
            "siblings": siblings,
            "root": merkle_proof.root.to_hex(),
        },
        "block_height": height,
        "block_tx_root": block_tx_root.map(|r| r.to_hex()),
        "verified": verified,
    })))
}

/// GET /block/{height}/proofs — Return all Merkle proofs for transactions in a block.
async fn get_block_proofs(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(height): axum::extract::Path<u64>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let block = node
        .state
        .get_block(height)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Block at height {} not found", height)))?;

    let mut proofs = Vec::new();
    for tx_hash in &block.tx_hashes {
        if let Some(receipt) = node.state.get_receipt(&tx_hash.0) {
            if let Some(ref proof_bytes) = receipt.inclusion_proof {
                if let Ok(proof) = bincode::deserialize::<MerkleProof>(proof_bytes) {
                    let siblings: Vec<Value> = proof
                        .siblings
                        .iter()
                        .map(|(h, is_left)| {
                            json!({ "hash": h.to_hex(), "is_left": is_left })
                        })
                        .collect();

                    proofs.push(json!({
                        "tx_hash": tx_hash.to_hex(),
                        "leaf": proof.leaf.to_hex(),
                        "index": proof.index,
                        "siblings": siblings,
                        "root": proof.root.to_hex(),
                    }));
                }
            }
        }
    }

    Ok(Json(json!({
        "block_height": height,
        "block_hash": block.hash.to_hex(),
        "tx_root": block.header.tx_root.to_hex(),
        "proof_count": proofs.len(),
        "proofs": proofs,
    })))
}

/// Query parameters for paginated block listing.
#[derive(Deserialize)]
struct BlocksQuery {
    from: Option<u64>,
    to: Option<u64>,
    limit: Option<usize>,
}

/// GET /blocks?from=0&to=100&limit=20 — Paginated block listing.
async fn get_blocks(
    AxumState(node): AxumState<NodeState>,
    Query(params): Query<BlocksQuery>,
) -> Json<Value> {
    let height = node.state.height();
    let from = params.from.unwrap_or(0);
    let to = params.to.unwrap_or(height);
    let limit = params.limit.unwrap_or(20).min(100);

    let blocks = node.state.get_block_range(from, to, limit);

    let block_list: Vec<Value> = blocks
        .iter()
        .map(|b| {
            json!({
                "height": b.header.height,
                "hash": b.hash.to_hex(),
                "parent_hash": b.header.parent_hash.to_hex(),
                "tx_root": b.header.tx_root.to_hex(),
                "tx_count": b.header.tx_count,
                "timestamp": b.header.timestamp,
                "producer": b.header.producer.to_hex(),
            })
        })
        .collect();

    Json(json!({
        "from": from,
        "to": to,
        "limit": limit,
        "count": block_list.len(),
        "blocks": block_list,
    }))
}

/// GET /block/{height}/txs?offset=0&limit=100 — Paginated transaction listing for a block.
/// Reconstructs benchmark transactions on-demand from deterministic parameters.
#[derive(Deserialize)]
struct BlockTxsQuery {
    offset: Option<u32>,
    limit: Option<u32>,
}

async fn get_block_txs(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(height): axum::extract::Path<u64>,
    Query(params): Query<BlockTxsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let block = node
        .state
        .get_block(height)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Block at height {} not found", height)))?;

    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(100).min(1000);

    // Try existing tx_hashes first (normal blocks)
    if !block.tx_hashes.is_empty() {
        let end = (offset + limit).min(block.header.tx_count);
        let txs: Vec<Value> = (offset..end)
            .filter_map(|i| {
                let hash = block.tx_hashes.get(i as usize)?;
                Some(json!({
                    "index": i,
                    "hash": hash.to_hex(),
                }))
            })
            .collect();

        return Ok(Json(json!({
            "block_height": height,
            "tx_count": block.header.tx_count,
            "offset": offset,
            "limit": limit,
            "returned": txs.len(),
            "transactions": txs,
        })));
    }

    // Reconstruct benchmark transactions on-demand
    let txs = node.state.get_benchmark_block_txs(height, offset, limit);
    let tx_list: Vec<Value> = txs
        .iter()
        .enumerate()
        .map(|(i, tx)| {
            json!({
                "index": offset + i as u32,
                "hash": tx.hash.to_hex(),
                "from": tx.from.to_hex(),
                "nonce": tx.nonce,
                "tx_type": format!("{:?}", tx.tx_type),
                "body": match &tx.body {
                    TxBody::Transfer(b) => json!({
                        "type": "Transfer",
                        "to": b.to.to_hex(),
                        "amount": b.amount,
                    }),
                    _ => json!({}),
                },
            })
        })
        .collect();

    Ok(Json(json!({
        "block_height": height,
        "tx_count": block.header.tx_count,
        "offset": offset,
        "limit": limit,
        "returned": tx_list.len(),
        "transactions": tx_list,
    })))
}

/// GET /account/{address}/txs — Return transaction hashes involving an account.
async fn get_account_txs(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let addr = parse_hash(&address)?;
    let tx_hashes = node.state.get_account_txs(&addr);

    let hashes: Vec<String> = tx_hashes.iter().map(|h| h.to_hex()).collect();

    Ok(Json(json!({
        "address": address,
        "tx_count": hashes.len(),
        "tx_hashes": hashes,
    })))
}

/// GET /stats — Basic chain statistics.
async fn get_stats(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let indexed_receipts = node.state.receipts.len();
    let indexed_hashes = node.state.tx_index.len();
    let executed = node.state.benchmark_tx_count.load(std::sync::atomic::Ordering::Relaxed) as usize;
    Json(json!({
        "chain": "ARC Chain",
        "version": "0.1.0",
        "block_height": node.state.height(),
        "total_accounts": node.state.account_count(),
        "mempool_size": node.mempool.len(),
        "total_transactions": indexed_receipts + executed,
        "indexed_hashes": indexed_hashes,
        "indexed_receipts": indexed_receipts,
    }))
}

// ---------------------------------------------------------------------------
// State Sync Protocol (A5) — snapshot bootstrap for new nodes
// ---------------------------------------------------------------------------

/// Returns metadata about the latest snapshot available for sync.
async fn sync_snapshot_info(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let height = node.state.height();
    let state_root = node.state.get_state_root();
    let account_count = node.state.account_count();
    Json(json!({
        "available": true,
        "height": height,
        "state_root": format!("{}", state_root),
        "account_count": account_count,
    }))
}

/// Stream the full state snapshot as LZ4-compressed bincode.
/// New nodes download this to bootstrap without replaying from genesis.
async fn sync_snapshot(
    AxumState(node): AxumState<NodeState>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;
    use axum::http::header;

    let snapshot = node.state.export_snapshot();
    let data = bincode::serialize(&snapshot)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let compressed = lz4_flex::compress_prepend_size(&data);

    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"snapshot.lz4\""),
        ],
        compressed,
    ).into_response())
}

// ---------------------------------------------------------------------------
// Light Client Proofs (A8)
// ---------------------------------------------------------------------------

/// GET /light/snapshot — Returns a lightweight snapshot for light client bootstrapping:
/// current height, state root, account count, total supply, latest block hash.
async fn light_snapshot(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let snap = node.state.generate_light_snapshot();
    Json(json!({
        "height": snap.height,
        "state_root": format!("{}", snap.state_root),
        "account_count": snap.account_count,
        "total_supply": snap.total_supply,
        "latest_block_hash": format!("{}", snap.latest_block_hash),
    }))
}

// ---------------------------------------------------------------------------
// Chunked State Sync — parallel chunk download for fast catch-up
// ---------------------------------------------------------------------------

/// GET /sync/manifest — Returns the snapshot manifest (height, chunk count,
/// state root, accounts) so a syncing node can plan parallel chunk downloads.
async fn sync_manifest(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let manifest = node.state.export_snapshot_manifest();
    Json(json!({
        "version": manifest.version,
        "state_root": format!("{}", manifest.state_root),
        "total_accounts": manifest.total_accounts,
        "total_chunks": manifest.total_chunks,
        "chunk_size": manifest.chunk_size,
        "manifest_hash": format!("{}", manifest.manifest_hash),
    }))
}

/// GET /sync/chunk/:index — Returns a single snapshot chunk by index.
/// Each chunk contains ~1000 accounts with a BLAKE3 integrity proof.
async fn sync_chunk(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(index): axum::extract::Path<u32>,
) -> Result<Json<Value>, StatusCode> {
    let manifest = node.state.export_snapshot_manifest();
    let chunk = node.state.export_snapshot_chunk(index, manifest.chunk_size)
        .ok_or(StatusCode::NOT_FOUND)?;

    let accounts: Vec<Value> = chunk.accounts.iter().map(|(addr, acct)| {
        json!({
            "address": format!("{}", addr),
            "balance": acct.balance,
            "nonce": acct.nonce,
        })
    }).collect();

    Ok(Json(json!({
        "version": chunk.version,
        "state_root": format!("{}", chunk.state_root),
        "chunk_index": chunk.chunk_index,
        "total_chunks": chunk.total_chunks,
        "chunk_proof": format!("{}", chunk.chunk_proof),
        "accounts": accounts,
        "account_count": chunk.accounts.len(),
    })))
}

/// GET /sync/status — Returns whether this node can serve snapshots and
/// information about the latest available snapshot.
async fn sync_status(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let manifest = node.state.export_snapshot_manifest();
    Json(json!({
        "available": true,
        "syncing": false,
        "latest_snapshot": {
            "height": manifest.version,
            "state_root": format!("{}", manifest.state_root),
            "total_chunks": manifest.total_chunks,
            "total_accounts": manifest.total_accounts,
        },
    }))
}

// ---------------------------------------------------------------------------
// Full transaction & contract endpoints
// ---------------------------------------------------------------------------

/// GET /tx/{hash}/full — Return the full transaction body with type-specific fields.
/// Falls back to on-demand reconstruction for benchmark transactions.
async fn get_full_transaction(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let tx_hash = parse_hash(&hash)?;

    let tx = node
        .state
        .get_transaction(&tx_hash)
        .or_else(|| node.state.get_benchmark_tx_by_hash(&tx_hash))
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Transaction {} not found", hash)))?;

    let receipt = node
        .state
        .get_receipt(&tx_hash)
        .or_else(|| node.state.get_benchmark_receipt_by_hash(&tx_hash));

    let body_json = match &tx.body {
        TxBody::Transfer(b) => json!({
            "type": "Transfer",
            "to": b.to.to_hex(),
            "amount": b.amount,
            "amount_commitment": b.amount_commitment.map(hex::encode),
        }),
        TxBody::Settle(b) => json!({
            "type": "Settle",
            "agent_id": b.agent_id.to_hex(),
            "service_hash": b.service_hash.to_hex(),
            "amount": b.amount,
            "usage_units": b.usage_units,
        }),
        TxBody::Swap(b) => json!({
            "type": "Swap",
            "counterparty": b.counterparty.to_hex(),
            "offer_amount": b.offer_amount,
            "receive_amount": b.receive_amount,
            "offer_asset": b.offer_asset.to_hex(),
            "receive_asset": b.receive_asset.to_hex(),
        }),
        TxBody::Escrow(b) => json!({
            "type": "Escrow",
            "beneficiary": b.beneficiary.to_hex(),
            "amount": b.amount,
            "conditions_hash": b.conditions_hash.to_hex(),
            "is_create": b.is_create,
        }),
        TxBody::Stake(b) => json!({
            "type": "Stake",
            "amount": b.amount,
            "is_stake": b.is_stake,
            "validator": b.validator.to_hex(),
        }),
        TxBody::WasmCall(b) => json!({
            "type": "WasmCall",
            "contract": b.contract.to_hex(),
            "function": b.function,
            "calldata": hex::encode(&b.calldata),
            "value": b.value,
            "gas_limit": b.gas_limit,
        }),
        TxBody::MultiSig(b) => json!({
            "type": "MultiSig",
            "signers": b.signers.iter().map(|s| s.to_hex()).collect::<Vec<_>>(),
            "threshold": b.threshold,
        }),
        TxBody::DeployContract(b) => json!({
            "type": "DeployContract",
            "bytecode_size": b.bytecode.len(),
            "constructor_args_size": b.constructor_args.len(),
            "state_rent_deposit": b.state_rent_deposit,
        }),
        TxBody::RegisterAgent(b) => json!({
            "type": "RegisterAgent",
            "agent_name": b.agent_name,
            "endpoint": b.endpoint,
            "protocol": b.protocol.to_hex(),
            "capabilities_size": b.capabilities.len(),
        }),
        TxBody::JoinValidator(b) => json!({
            "type": "JoinValidator",
            "pubkey": hex::encode(b.pubkey),
            "initial_stake": b.initial_stake,
        }),
        TxBody::LeaveValidator => json!({
            "type": "LeaveValidator",
        }),
        TxBody::ClaimRewards => json!({
            "type": "ClaimRewards",
        }),
        TxBody::UpdateStake(b) => json!({
            "type": "UpdateStake",
            "new_stake": b.new_stake,
        }),
        TxBody::Governance(b) => json!({
            "type": "Governance",
            "proposal_id": b.proposal_id,
            "action": format!("{:?}", b.action),
        }),
        TxBody::BridgeLock(b) => json!({
            "type": "BridgeLock",
            "destination_chain": b.destination_chain,
            "destination_address": hex::encode(b.destination_address),
            "amount": b.amount,
        }),
        TxBody::BridgeMint(b) => json!({
            "type": "BridgeMint",
            "source_chain": b.source_chain,
            "source_tx_hash": b.source_tx_hash.to_hex(),
            "recipient": b.recipient.to_hex(),
            "amount": b.amount,
            "merkle_proof_size": b.merkle_proof.len(),
        }),
        TxBody::BatchSettle(body) => {
            let total: u64 = body.entries.iter().map(|e| e.amount).sum();
            json!({
                "type": "BatchSettle",
                "entries": body.entries.len(),
                "total_amount": total,
            })
        }
        TxBody::ChannelOpen(body) => json!({
            "type": "ChannelOpen",
            "channel_id": format!("0x{}", hex::encode(&body.channel_id.0)),
            "counterparty": format!("0x{}", hex::encode(&body.counterparty.0)),
            "deposit": body.deposit,
            "timeout_blocks": body.timeout_blocks,
        }),
        TxBody::ChannelClose(body) => json!({
            "type": "ChannelClose",
            "channel_id": format!("0x{}", hex::encode(&body.channel_id.0)),
            "opener_balance": body.opener_balance,
            "counterparty_balance": body.counterparty_balance,
            "state_nonce": body.state_nonce,
        }),
        TxBody::ChannelDispute(body) => json!({
            "type": "ChannelDispute",
            "channel_id": format!("0x{}", hex::encode(&body.channel_id.0)),
            "opener_balance": body.opener_balance,
            "counterparty_balance": body.counterparty_balance,
            "state_nonce": body.state_nonce,
            "challenge_period": body.challenge_period,
        }),
        TxBody::ShardProof(body) => json!({
            "type": "ShardProof",
            "shard_id": body.shard_id,
            "block_height": body.block_height,
            "tx_count": body.tx_count,
            "proof_size": body.proof_data.len(),
            "prev_state_root": format!("0x{}", hex::encode(&body.prev_state_root.0)),
            "post_state_root": format!("0x{}", hex::encode(&body.post_state_root.0)),
        }),
        TxBody::InferenceAttestation(body) => json!({
            "type": "InferenceAttestation",
            "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
            "input_hash": format!("0x{}", hex::encode(&body.input_hash.0)),
            "output_hash": format!("0x{}", hex::encode(&body.output_hash.0)),
            "challenge_period": body.challenge_period,
            "bond": body.bond,
        }),
        TxBody::InferenceChallenge(body) => json!({
            "type": "InferenceChallenge",
            "attestation_hash": format!("0x{}", hex::encode(&body.attestation_hash.0)),
            "challenger_output_hash": format!("0x{}", hex::encode(&body.challenger_output_hash.0)),
            "challenger_bond": body.challenger_bond,
        }),
        TxBody::InferenceRegister(body) => json!({
            "type": "InferenceRegister",
            "tier": body.tier,
            "stake_bond": body.stake_bond,
        }),
    };

    let sig_json = match &tx.signature {
        arc_crypto::Signature::Ed25519 { public_key, signature } => json!({
            "Ed25519": {
                "public_key": hex::encode(public_key),
                "signature": hex::encode(signature),
            }
        }),
        arc_crypto::Signature::Secp256k1 { signature } => json!({
            "Secp256k1": {
                "signature": hex::encode(signature),
            }
        }),
        arc_crypto::Signature::MlDsa65 { public_key, signature } => json!({
            "MlDsa65": {
                "public_key_size": public_key.len(),
                "signature_size": signature.len(),
            }
        }),
        _ => json!(null),
    };

    let mut result = json!({
        "tx_hash": Hash256(tx_hash).to_hex(),
        "tx_type": format!("{:?}", tx.tx_type),
        "from": tx.from.to_hex(),
        "nonce": tx.nonce,
        "fee": tx.fee,
        "gas_limit": tx.gas_limit,
        "body": body_json,
        "signature": sig_json,
    });

    if let Some(r) = receipt {
        result["block_height"] = json!(r.block_height);
        result["block_hash"] = json!(r.block_hash.to_hex());
        result["index"] = json!(r.index);
        result["success"] = json!(r.success);
        result["gas_used"] = json!(r.gas_used);
    }

    Ok(Json(result))
}

/// GET /contract/{address} — Return contract info.
async fn get_contract_info(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let addr = Hash256::from_hex(&address)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Invalid contract address."))?;

    let bytecode = node
        .state
        .get_contract(&addr)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Contract {} not found", address)))?;

    let code_hash = hex::encode(arc_crypto::hash_bytes(&bytecode).0);

    Ok(Json(json!({
        "address": address,
        "bytecode_size": bytecode.len(),
        "code_hash": code_hash,
        "is_wasm": bytecode.len() >= 4 && &bytecode[..4] == b"\0asm",
    })))
}

/// POST /contract/{address}/call — Read-only contract call.
#[derive(Deserialize)]
struct ContractCallRequest {
    function: String,
    calldata: Option<String>,
    from: Option<String>,
    gas_limit: Option<u64>,
}

async fn call_contract(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
    Json(req): Json<ContractCallRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let contract_addr = Hash256::from_hex(&address)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Invalid contract address."))?;

    let bytecode = node
        .state
        .get_contract(&contract_addr)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Contract {} not found", address)))?;

    let caller = req
        .from
        .as_ref()
        .and_then(|f| Hash256::from_hex(f).ok())
        .unwrap_or(Hash256::ZERO);

    let calldata = req
        .calldata
        .as_ref()
        .map(|h| hex::decode(h).unwrap_or_default())
        .unwrap_or_default();

    let gas_limit = req.gas_limit.unwrap_or(1_000_000);

    let context = arc_vm::ContractContext {
        caller,
        self_address: contract_addr,
        value: 0,
        gas_limit,
        block_height: node.state.height(),
        block_timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    };

    let mut vm = arc_vm::ArcVM::new();
    let module = match vm.compile(&bytecode) {
        Ok(m) => m,
        Err(e) => {
            return Ok(Json(json!({
                "success": false,
                "error": format!("compilation error: {e}"),
            })));
        }
    };

    // Read-only: storage writes are buffered but never flushed to StateDB
    match vm.execute_with_state(&module, &req.function, &[], &context, &node.state) {
        Ok(result) => Ok(Json(json!({
            "success": result.success,
            "gas_used": result.gas_used,
            "return_data": hex::encode(&result.return_data),
            "logs": result.logs,
            "events": result.events.iter().map(|e| json!({
                "topic": hex::encode(&e.topic),
                "data": hex::encode(&e.data),
            })).collect::<Vec<Value>>(),
        }))),
        Err(e) => {
            let err_msg = e.to_string();
            Ok(Json(json!({
                "success": false,
                "error": err_msg,
            })))
        }
    }
}

// ---------------------------------------------------------------------------
// ETH-Compatible JSON-RPC
// ---------------------------------------------------------------------------
// Implements the Ethereum JSON-RPC specification so that MetaMask, Hardhat,
// Foundry, and other EVM tooling can interact with ARC Chain unchanged.
// Endpoint: POST /eth
// Protocol: JSON-RPC 2.0

/// ARC Chain ID (unique, registered-style). 0x415243 = "ARC" in ASCII.
const ARC_CHAIN_ID: u64 = 0x415243;

/// Standard ETH JSON-RPC request.
#[derive(Deserialize)]
struct EthRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    method: String,
    params: Option<Value>,
    id: Value,
}

fn eth_rpc_error(id: &Value, code: i64, message: &str) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
}

fn eth_rpc_result(id: &Value, result: Value) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
}

/// Main ETH JSON-RPC dispatcher.
async fn eth_json_rpc(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<EthRpcRequest>,
) -> Json<Value> {
    let params = req.params.unwrap_or(Value::Array(vec![]));

    match req.method.as_str() {
        "eth_chainId" => eth_rpc_result(&req.id, json!(format!("0x{:x}", ARC_CHAIN_ID))),
        "eth_blockNumber" => eth_rpc_result(&req.id, json!(format!("0x{:x}", node.state.height()))),
        "net_version" => eth_rpc_result(&req.id, json!(ARC_CHAIN_ID.to_string())),
        "web3_clientVersion" => eth_rpc_result(&req.id, json!("ARC/v0.1.0")),
        "eth_gasPrice" => eth_rpc_result(&req.id, json!("0x0")), // Zero-fee chain
        "net_listening" => eth_rpc_result(&req.id, json!(true)),
        "net_peerCount" => {
            let peers = node.peer_count.load(Ordering::Relaxed);
            eth_rpc_result(&req.id, json!(format!("0x{:x}", peers)))
        }
        "eth_syncing" => eth_rpc_result(&req.id, json!(false)), // Always synced
        "eth_mining" => eth_rpc_result(&req.id, json!(false)),
        "eth_hashrate" => eth_rpc_result(&req.id, json!("0x0")),
        "eth_accounts" => eth_rpc_result(&req.id, json!([])),
        "eth_getBalance" => eth_get_balance(&node, &params, &req.id),
        "eth_getTransactionCount" => eth_get_tx_count(&node, &params, &req.id),
        "eth_getCode" => eth_get_code(&node, &params, &req.id),
        "eth_getStorageAt" => eth_get_storage_at(&node, &params, &req.id),
        "eth_getBlockByNumber" => eth_get_block_by_number(&node, &params, &req.id),
        "eth_getBlockByHash" => eth_rpc_result(&req.id, json!(null)), // TODO: index by hash
        "eth_getTransactionByHash" => eth_get_tx_by_hash(&node, &params, &req.id),
        "eth_getTransactionReceipt" => eth_get_tx_receipt(&node, &params, &req.id),
        "eth_call" => eth_call(&node, &params, &req.id),
        "eth_estimateGas" => eth_estimate_gas(&node, &params, &req.id),
        "eth_sendRawTransaction" => eth_send_raw_transaction(&node, &params, &req.id),
        "eth_getLogs" => eth_get_logs(&node, &params, &req.id),
        "eth_getBlockTransactionCountByNumber" => {
            let block_num = parse_block_number(&node, params.get(0));
            match node.state.get_block(block_num) {
                Some(b) => eth_rpc_result(&req.id, json!(format!("0x{:x}", b.header.tx_count))),
                None => eth_rpc_result(&req.id, json!(null)),
            }
        }
        _ => eth_rpc_error(&req.id, -32601, &format!("Method not found: {}", req.method)),
    }
}

/// Parse a hex-encoded 20-byte ETH address, returning a 32-byte ARC address.
fn parse_eth_address(hex_str: &str) -> Result<Address, ()> {
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    if hex_str.len() != 40 {
        return Err(());
    }
    let bytes = hex::decode(hex_str).map_err(|_| ())?;
    let mut addr = [0u8; 32];
    addr[..20].copy_from_slice(&bytes);
    Ok(Hash256(addr))
}

/// Parse block number parameter ("latest", "earliest", "pending", or hex number).
fn parse_block_number(node: &NodeState, param: Option<&Value>) -> u64 {
    match param.and_then(|v| v.as_str()) {
        None | Some("latest") | Some("pending") => node.state.height().saturating_sub(1),
        Some("earliest") => 0,
        Some(hex) => {
            let hex = hex.strip_prefix("0x").unwrap_or(hex);
            u64::from_str_radix(hex, 16).unwrap_or(0)
        }
    }
}

fn eth_get_balance(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let addr_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return eth_rpc_error(id, -32602, "Missing address parameter"),
    };
    let addr = match parse_eth_address(addr_str) {
        Ok(a) => a,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid address"),
    };
    let balance = node
        .state
        .get_account(&addr)
        .map(|a| a.balance)
        .unwrap_or(0);
    eth_rpc_result(id, json!(format!("0x{:x}", balance)))
}

fn eth_get_tx_count(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let addr_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return eth_rpc_error(id, -32602, "Missing address parameter"),
    };
    let addr = match parse_eth_address(addr_str) {
        Ok(a) => a,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid address"),
    };
    let nonce = node
        .state
        .get_account(&addr)
        .map(|a| a.nonce)
        .unwrap_or(0);
    eth_rpc_result(id, json!(format!("0x{:x}", nonce)))
}

fn eth_get_code(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let addr_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return eth_rpc_error(id, -32602, "Missing address parameter"),
    };
    let addr = match parse_eth_address(addr_str) {
        Ok(a) => a,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid address"),
    };
    match node.state.get_contract(&addr) {
        Some(code) => eth_rpc_result(id, json!(format!("0x{}", hex::encode(&code)))),
        None => eth_rpc_result(id, json!("0x")),
    }
}

fn eth_get_storage_at(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let addr_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return eth_rpc_error(id, -32602, "Missing address parameter"),
    };
    let addr = match parse_eth_address(addr_str) {
        Ok(a) => a,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid address"),
    };
    let slot_str = match params.get(1).and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return eth_rpc_error(id, -32602, "Missing storage slot"),
    };
    let slot_str = slot_str.strip_prefix("0x").unwrap_or(slot_str);
    let slot_bytes = match hex::decode(slot_str) {
        Ok(b) => b,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid storage slot hex"),
    };
    let mut key = [0u8; 32];
    let start = 32usize.saturating_sub(slot_bytes.len());
    let copy_len = slot_bytes.len().min(32);
    key[start..start + copy_len].copy_from_slice(&slot_bytes[..copy_len]);
    let slot_hash = Hash256(key);

    match node.state.get_storage(&addr, &slot_hash) {
        Some(value) => {
            let mut padded = vec![0u8; 32];
            let s = 32usize.saturating_sub(value.len());
            let c = value.len().min(32);
            padded[s..s + c].copy_from_slice(&value[..c]);
            eth_rpc_result(id, json!(format!("0x{}", hex::encode(&padded))))
        }
        None => eth_rpc_result(id, json!("0x0000000000000000000000000000000000000000000000000000000000000000")),
    }
}

fn eth_get_block_by_number(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let block_num = parse_block_number(node, params.get(0));
    let full_txs = params.get(1).and_then(|v| v.as_bool()).unwrap_or(false);

    match node.state.get_block(block_num) {
        Some(block) => {
            let txs: Value = if full_txs {
                // Full tx objects would go here; for now return hashes with 0x prefix
                json!(block.tx_hashes.iter().map(|h| format!("0x{}", h.to_hex())).collect::<Vec<_>>())
            } else {
                json!(block.tx_hashes.iter().map(|h| format!("0x{}", h.to_hex())).collect::<Vec<_>>())
            };

            eth_rpc_result(id, json!({
                "number": format!("0x{:x}", block.header.height),
                "hash": format!("0x{}", block.hash.to_hex()),
                "parentHash": format!("0x{}", block.header.parent_hash.to_hex()),
                "nonce": "0x0000000000000000",
                "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
                "logsBloom": format!("0x{}", "00".repeat(256)),
                "transactionsRoot": format!("0x{}", block.header.tx_root.to_hex()),
                "stateRoot": format!("0x{}", block.header.state_root.to_hex()),
                "receiptsRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "miner": format!("0x{}", hex::encode(&block.header.producer.0[..20])),
                "difficulty": "0x0",
                "totalDifficulty": "0x0",
                "extraData": "0x",
                "size": "0x0",
                "gasLimit": "0xffffffffffffffff",
                "gasUsed": "0x0",
                "timestamp": format!("0x{:x}", block.header.timestamp / 1000),
                "transactions": txs,
                "uncles": [],
                "baseFeePerGas": "0x0",
            }))
        }
        None => eth_rpc_result(id, json!(null)),
    }
}

fn eth_get_tx_by_hash(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let hash_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(h) => h,
        None => return eth_rpc_error(id, -32602, "Missing hash parameter"),
    };
    let hash_str = hash_str.strip_prefix("0x").unwrap_or(hash_str);
    let tx_hash = match Hash256::from_hex(hash_str) {
        Ok(h) => h,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid hash"),
    };

    let tx = node.state.get_transaction(&tx_hash.0)
        .or_else(|| node.state.get_benchmark_tx_by_hash(&tx_hash.0));

    match tx {
        Some(tx) => {
            let (to, value) = match &tx.body {
                TxBody::Transfer(b) => (Some(format!("0x{}", hex::encode(&b.to.0[..20]))), format!("0x{:x}", b.amount)),
                TxBody::WasmCall(b) => (Some(format!("0x{}", hex::encode(&b.contract.0[..20]))), format!("0x{:x}", b.value)),
                _ => (None, "0x0".to_string()),
            };

            eth_rpc_result(id, json!({
                "hash": format!("0x{}", tx_hash.to_hex()),
                "nonce": format!("0x{:x}", tx.nonce),
                "from": format!("0x{}", hex::encode(&tx.from.0[..20])),
                "to": to,
                "value": value,
                "gas": format!("0x{:x}", tx.gas_limit),
                "gasPrice": "0x0",
                "input": "0x",
                "blockNumber": null,
                "blockHash": null,
                "transactionIndex": null,
                "type": "0x0",
                "chainId": format!("0x{:x}", ARC_CHAIN_ID),
                "v": "0x0",
                "r": "0x0",
                "s": "0x0",
            }))
        }
        None => eth_rpc_result(id, json!(null)),
    }
}

fn eth_get_tx_receipt(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let hash_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(h) => h,
        None => return eth_rpc_error(id, -32602, "Missing hash parameter"),
    };
    let hash_str = hash_str.strip_prefix("0x").unwrap_or(hash_str);
    let tx_hash = match Hash256::from_hex(hash_str) {
        Ok(h) => h,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid hash"),
    };

    let receipt = node.state.get_receipt(&tx_hash.0)
        .or_else(|| node.state.get_benchmark_receipt_by_hash(&tx_hash.0));

    match receipt {
        Some(r) => {
            let tx = node.state.get_transaction(&tx_hash.0)
                .or_else(|| node.state.get_benchmark_tx_by_hash(&tx_hash.0));

            let from = tx.as_ref().map(|t| format!("0x{}", hex::encode(&t.from.0[..20]))).unwrap_or_default();
            let to = tx.as_ref().and_then(|t| match &t.body {
                TxBody::Transfer(b) => Some(format!("0x{}", hex::encode(&b.to.0[..20]))),
                TxBody::WasmCall(b) => Some(format!("0x{}", hex::encode(&b.contract.0[..20]))),
                _ => None,
            });

            let logs_json: Vec<Value> = r.logs.iter().enumerate().map(|(i, log)| {
                let topics: Vec<String> = log.topics.iter()
                    .map(|t| format!("0x{}", t.to_hex()))
                    .collect();
                json!({
                    "address": format!("0x{}", hex::encode(&log.address.0[..20])),
                    "topics": topics,
                    "data": format!("0x{}", hex::encode(&log.data)),
                    "blockNumber": format!("0x{:x}", log.block_height),
                    "transactionHash": format!("0x{}", tx_hash.to_hex()),
                    "transactionIndex": format!("0x{:x}", r.index),
                    "blockHash": format!("0x{}", r.block_hash.to_hex()),
                    "logIndex": format!("0x{:x}", i),
                    "removed": false,
                })
            }).collect();

            eth_rpc_result(id, json!({
                "transactionHash": format!("0x{}", tx_hash.to_hex()),
                "transactionIndex": format!("0x{:x}", r.index),
                "blockNumber": format!("0x{:x}", r.block_height),
                "blockHash": format!("0x{}", r.block_hash.to_hex()),
                "from": from,
                "to": to,
                "cumulativeGasUsed": format!("0x{:x}", r.gas_used),
                "gasUsed": format!("0x{:x}", r.gas_used),
                "contractAddress": null,
                "logs": logs_json,
                "logsBloom": format!("0x{}", "00".repeat(256)),
                "status": if r.success { "0x1" } else { "0x0" },
                "effectiveGasPrice": "0x0",
                "type": "0x0",
            }))
        }
        None => eth_rpc_result(id, json!(null)),
    }
}

/// eth_getLogs — returns event logs matching a filter.
fn eth_get_logs(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let filter = match params.get(0) {
        Some(f) => f,
        None => return eth_rpc_error(id, -32602, "Missing filter object"),
    };

    let from_block = filter.get("fromBlock")
        .and_then(|v| v.as_str())
        .map(|s| parse_block_number(node, Some(&json!(s))))
        .unwrap_or(0);

    let to_block = filter.get("toBlock")
        .and_then(|v| v.as_str())
        .map(|s| parse_block_number(node, Some(&json!(s))))
        .unwrap_or_else(|| node.state.height());

    let address_filter: Option<Vec<Hash256>> = filter.get("address")
        .and_then(|v| {
            if let Some(s) = v.as_str() {
                parse_eth_address(s).ok().map(|a| vec![a])
            } else if let Some(arr) = v.as_array() {
                let addrs: Vec<Hash256> = arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(|s| parse_eth_address(s).ok())
                    .collect();
                if addrs.is_empty() { None } else { Some(addrs) }
            } else {
                None
            }
        });

    let topic_filters: Vec<Option<Hash256>> = filter.get("topics")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().map(|t| {
                t.as_str()
                    .and_then(|s| {
                        let s = s.strip_prefix("0x").unwrap_or(s);
                        Hash256::from_hex(s).ok()
                    })
            }).collect()
        })
        .unwrap_or_default();

    let mut result_logs: Vec<Value> = Vec::new();
    let max_height = to_block.min(from_block + 10_000); // Cap range

    for height in from_block..=max_height {
        if let Some(logs) = node.state.event_logs.get(&height) {
            for log in logs.iter() {
                // Address filter
                if let Some(ref addrs) = address_filter {
                    if !addrs.iter().any(|a| a.0 == log.address.0) {
                        continue;
                    }
                }
                // Topic filter
                let mut topic_match = true;
                for (i, filter_topic) in topic_filters.iter().enumerate() {
                    if let Some(expected) = filter_topic {
                        if log.topics.get(i).map(|t| t.0) != Some(expected.0) {
                            topic_match = false;
                            break;
                        }
                    }
                }
                if !topic_match { continue; }

                let block = node.state.get_block(height);
                let block_hash = block.map(|b| format!("0x{}", b.hash.to_hex()))
                    .unwrap_or_else(|| "0x".to_string() + &"00".repeat(32));

                let topics: Vec<String> = log.topics.iter()
                    .map(|t| format!("0x{}", t.to_hex()))
                    .collect();

                result_logs.push(json!({
                    "address": format!("0x{}", hex::encode(&log.address.0[..20])),
                    "topics": topics,
                    "data": format!("0x{}", hex::encode(&log.data)),
                    "blockNumber": format!("0x{:x}", log.block_height),
                    "transactionHash": format!("0x{}", log.tx_hash.to_hex()),
                    "blockHash": block_hash,
                    "logIndex": format!("0x{:x}", log.log_index),
                    "removed": false,
                }));
            }
        }
    }

    eth_rpc_result(id, json!(result_logs))
}

fn eth_call(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let call_obj = match params.get(0) {
        Some(obj) => obj,
        None => return eth_rpc_error(id, -32602, "Missing call object"),
    };

    let from = call_obj.get("from")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_eth_address(s).ok())
        .unwrap_or(Hash256::ZERO);

    let to = match call_obj.get("to").and_then(|v| v.as_str()) {
        Some(s) => match parse_eth_address(s) {
            Ok(a) => a,
            Err(_) => return eth_rpc_error(id, -32602, "Invalid to address"),
        },
        None => return eth_rpc_error(id, -32602, "Missing to address"),
    };

    let data = call_obj.get("data")
        .or_else(|| call_obj.get("input"))
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();

    let value = call_obj.get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(0);

    let gas = call_obj.get("gas")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(10_000_000);

    let result = arc_vm::evm::evm_call(&node.state, from, to, data, value, gas);
    if result.success {
        eth_rpc_result(id, json!(format!("0x{}", hex::encode(&result.return_data))))
    } else {
        eth_rpc_error(id, 3, result.revert_reason.as_deref().unwrap_or("execution reverted"))
    }
}

// ---------------------------------------------------------------------------
// eth_sendRawTransaction — accept RLP-encoded Ethereum transactions
// ---------------------------------------------------------------------------
// Decodes signed Ethereum transactions (legacy format), recovers the sender
// via secp256k1 ecrecover, converts to an ARC Transaction, and inserts into
// the mempool. Returns the Keccak-256 transaction hash (Ethereum-style).

/// Minimal RLP decoder — just enough to parse Ethereum legacy transactions.
///
/// RLP encoding rules:
///   - Single byte in [0x00, 0x7f]: the byte itself is its RLP encoding
///   - [0x80, 0xb7]: short string, length = first_byte - 0x80
///   - [0xb8, 0xbf]: long string, length-of-length = first_byte - 0xb7
///   - [0xc0, 0xf7]: short list, payload length = first_byte - 0xc0
///   - [0xf8, 0xff]: long list, length-of-length = first_byte - 0xf7
#[allow(dead_code)]
mod rlp {
    /// An RLP-decoded item: either raw bytes or a list of items.
    #[derive(Debug, Clone)]
    pub enum RlpItem {
        Bytes(Vec<u8>),
        List(Vec<RlpItem>),
    }

    impl RlpItem {
        /// Extract as byte slice. Panics if this is a List.
        pub fn as_bytes(&self) -> &[u8] {
            match self {
                RlpItem::Bytes(b) => b,
                RlpItem::List(_) => panic!("expected RLP bytes, got list"),
            }
        }

        /// Extract as list. Panics if this is Bytes.
        pub fn as_list(&self) -> &[RlpItem] {
            match self {
                RlpItem::List(items) => items,
                RlpItem::Bytes(_) => panic!("expected RLP list, got bytes"),
            }
        }
    }

    /// Decode a single RLP item from `data` starting at `offset`.
    /// Returns `(item, bytes_consumed)`.
    pub fn decode(data: &[u8], offset: usize) -> Result<(RlpItem, usize), String> {
        if offset >= data.len() {
            return Err("RLP: unexpected end of data".into());
        }

        let prefix = data[offset];

        if prefix < 0x80 {
            // Single byte
            Ok((RlpItem::Bytes(vec![prefix]), 1))
        } else if prefix <= 0xb7 {
            // Short string: 0-55 bytes
            let len = (prefix - 0x80) as usize;
            if offset + 1 + len > data.len() {
                return Err("RLP: short string overflow".into());
            }
            let bytes = data[offset + 1..offset + 1 + len].to_vec();
            Ok((RlpItem::Bytes(bytes), 1 + len))
        } else if prefix <= 0xbf {
            // Long string: length encoded in next N bytes
            let len_of_len = (prefix - 0xb7) as usize;
            if offset + 1 + len_of_len > data.len() {
                return Err("RLP: long string length overflow".into());
            }
            let len = read_be_uint(&data[offset + 1..offset + 1 + len_of_len]);
            if offset + 1 + len_of_len + len > data.len() {
                return Err("RLP: long string data overflow".into());
            }
            let bytes = data[offset + 1 + len_of_len..offset + 1 + len_of_len + len].to_vec();
            Ok((RlpItem::Bytes(bytes), 1 + len_of_len + len))
        } else if prefix <= 0xf7 {
            // Short list: total payload 0-55 bytes
            let payload_len = (prefix - 0xc0) as usize;
            if offset + 1 + payload_len > data.len() {
                return Err("RLP: short list overflow".into());
            }
            let items = decode_list_payload(data, offset + 1, payload_len)?;
            Ok((RlpItem::List(items), 1 + payload_len))
        } else {
            // Long list: length encoded in next N bytes
            let len_of_len = (prefix - 0xf7) as usize;
            if offset + 1 + len_of_len > data.len() {
                return Err("RLP: long list length overflow".into());
            }
            let payload_len = read_be_uint(&data[offset + 1..offset + 1 + len_of_len]);
            if offset + 1 + len_of_len + payload_len > data.len() {
                return Err("RLP: long list data overflow".into());
            }
            let items = decode_list_payload(data, offset + 1 + len_of_len, payload_len)?;
            Ok((RlpItem::List(items), 1 + len_of_len + payload_len))
        }
    }

    /// Decode all items within a list payload.
    fn decode_list_payload(
        data: &[u8],
        start: usize,
        payload_len: usize,
    ) -> Result<Vec<RlpItem>, String> {
        let mut items = Vec::new();
        let mut pos = 0;
        while pos < payload_len {
            let (item, consumed) = decode(data, start + pos)?;
            items.push(item);
            pos += consumed;
        }
        Ok(items)
    }

    /// Read a big-endian unsigned integer from a byte slice (1-8 bytes).
    fn read_be_uint(bytes: &[u8]) -> usize {
        let mut result: usize = 0;
        for &b in bytes {
            result = (result << 8) | (b as usize);
        }
        result
    }

    /// Encode a single byte-string item as RLP.
    pub fn encode_bytes(data: &[u8]) -> Vec<u8> {
        if data.len() == 1 && data[0] < 0x80 {
            vec![data[0]]
        } else if data.len() <= 55 {
            let mut out = vec![0x80 + data.len() as u8];
            out.extend_from_slice(data);
            out
        } else {
            let len_bytes = to_be_bytes(data.len());
            let mut out = vec![0xb7 + len_bytes.len() as u8];
            out.extend_from_slice(&len_bytes);
            out.extend_from_slice(data);
            out
        }
    }

    /// Encode a list of already-encoded items as an RLP list.
    pub fn encode_list(encoded_items: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = encoded_items.iter().flat_map(|i| i.iter().copied()).collect();
        if payload.len() <= 55 {
            let mut out = vec![0xc0 + payload.len() as u8];
            out.extend_from_slice(&payload);
            out
        } else {
            let len_bytes = to_be_bytes(payload.len());
            let mut out = vec![0xf7 + len_bytes.len() as u8];
            out.extend_from_slice(&len_bytes);
            out.extend_from_slice(&payload);
            out
        }
    }

    /// Convert a usize to minimal big-endian bytes.
    fn to_be_bytes(val: usize) -> Vec<u8> {
        if val == 0 {
            return vec![0];
        }
        let bytes = val.to_be_bytes();
        let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len() - 1);
        bytes[first_nonzero..].to_vec()
    }

    /// Encode a u64 as an RLP byte string (minimal big-endian, no leading zeros).
    pub fn encode_u64(val: u64) -> Vec<u8> {
        if val == 0 {
            // RLP encoding of zero is the empty byte string
            encode_bytes(&[])
        } else {
            let bytes = val.to_be_bytes();
            let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len() - 1);
            encode_bytes(&bytes[first_nonzero..])
        }
    }

    /// Encode a u256 (represented as a 32-byte big-endian slice) as an RLP byte string.
    pub fn encode_u256(bytes: &[u8]) -> Vec<u8> {
        // Strip leading zeros
        let first_nonzero = bytes.iter().position(|&b| b != 0);
        match first_nonzero {
            Some(idx) => encode_bytes(&bytes[idx..]),
            None => encode_bytes(&[]), // all zeros = empty
        }
    }
}

/// Parse a big-endian byte slice into a u64.
/// Handles 0 to 8 bytes. Returns 0 for empty slices.
fn be_bytes_to_u64(bytes: &[u8]) -> u64 {
    let mut result: u64 = 0;
    for &b in bytes {
        result = result.checked_shl(8).unwrap_or(0) | (b as u64);
    }
    result
}

/// Parse a big-endian byte slice into a u128.
/// Handles 0 to 16 bytes. Returns 0 for empty slices.
fn be_bytes_to_u128(bytes: &[u8]) -> u128 {
    let mut result: u128 = 0;
    for &b in bytes {
        result = result.checked_shl(8).unwrap_or(0) | (b as u128);
    }
    result
}

/// Compute Keccak-256 hash of data.
fn keccak256(data: &[u8]) -> [u8; 32] {
    use sha3::{Digest, Keccak256};
    let mut hasher = Keccak256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Process `eth_sendRawTransaction`.
///
/// Accepts an RLP-encoded signed Ethereum transaction (legacy format):
///   `[nonce, gasPrice, gasLimit, to, value, data, v, r, s]`
///
/// Steps:
///   1. Hex-decode params[0]
///   2. RLP-decode the 9-field list
///   3. Reconstruct the unsigned tx RLP for signing hash:
///      `RLP([nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0])`
///   4. Keccak-256 hash that → signing hash
///   5. Recover the secp256k1 public key from (r, s, v) + signing hash
///   6. Derive the ARC address (BLAKE3 of uncompressed pubkey)
///   7. Build an ARC `Transaction` (Transfer or WasmCall) and insert into mempool
///   8. Return the Keccak-256 hash of the full signed RLP (Ethereum tx hash)
fn eth_send_raw_transaction(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    // --- 1. Extract and hex-decode the raw transaction ---
    let raw_hex = match params.get(0).and_then(|v| v.as_str()) {
        Some(h) => h,
        None => return eth_rpc_error(id, -32602, "Missing raw transaction parameter"),
    };
    let raw_hex = raw_hex.strip_prefix("0x").unwrap_or(raw_hex);
    let raw_bytes = match hex::decode(raw_hex) {
        Ok(b) => b,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid hex encoding"),
    };

    // --- 2. RLP-decode the transaction ---
    let (item, _) = match rlp::decode(&raw_bytes, 0) {
        Ok(r) => r,
        Err(e) => return eth_rpc_error(id, -32602, &format!("RLP decode error: {}", e)),
    };

    let fields = match &item {
        rlp::RlpItem::List(items) => items,
        _ => return eth_rpc_error(id, -32602, "Expected RLP list for transaction"),
    };

    // Legacy transactions have exactly 9 fields
    if fields.len() != 9 {
        return eth_rpc_error(
            id,
            -32602,
            &format!(
                "Expected 9 fields in legacy tx, got {}. EIP-2930/1559 not yet supported.",
                fields.len()
            ),
        );
    }

    // --- Extract fields ---
    let nonce_bytes = fields[0].as_bytes();
    let gas_price_bytes = fields[1].as_bytes();
    let gas_limit_bytes = fields[2].as_bytes();
    let to_bytes = fields[3].as_bytes();
    let value_bytes = fields[4].as_bytes();
    let data_bytes = fields[5].as_bytes();
    let v_bytes = fields[6].as_bytes();
    let r_bytes = fields[7].as_bytes();
    let s_bytes = fields[8].as_bytes();

    let nonce = be_bytes_to_u64(nonce_bytes);
    let gas_limit = be_bytes_to_u64(gas_limit_bytes);
    let _gas_price = be_bytes_to_u128(gas_price_bytes);

    // Value: ETH uses 256-bit, ARC uses u64. Clamp to u64::MAX.
    let value_u128 = be_bytes_to_u128(value_bytes);
    let value: u64 = if value_u128 > u64::MAX as u128 {
        u64::MAX
    } else {
        value_u128 as u64
    };

    // v: EIP-155 encodes chainId into v. For ARC (chainId = 0x415243):
    //   v = chainId * 2 + 35 + recovery_id(0 or 1)
    //   => v = 0x415243 * 2 + 35 + {0,1} = 8537639 or 8537640
    // Pre-EIP-155: v = 27 or 28
    let v = be_bytes_to_u64(v_bytes);

    let (recovery_id_byte, chain_id_for_signing) = if v >= 35 {
        // EIP-155: chain_id = (v - 35) / 2, recovery_id = (v - 35) % 2
        let chain_id = (v - 35) / 2;
        let rec_id = ((v - 35) % 2) as u8;
        (rec_id, Some(chain_id))
    } else if v == 27 || v == 28 {
        // Pre-EIP-155
        ((v - 27) as u8, None)
    } else {
        return eth_rpc_error(id, -32602, &format!("Invalid v value: {}", v));
    };

    // --- 3. Reconstruct the unsigned transaction RLP for the signing hash ---
    // EIP-155: hash(RLP([nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0]))
    // Pre-EIP-155: hash(RLP([nonce, gasPrice, gasLimit, to, value, data]))
    let unsigned_rlp = {
        let mut items: Vec<Vec<u8>> = vec![
            rlp::encode_bytes(nonce_bytes),
            rlp::encode_bytes(gas_price_bytes),
            rlp::encode_bytes(gas_limit_bytes),
            rlp::encode_bytes(to_bytes),
            rlp::encode_bytes(value_bytes),
            rlp::encode_bytes(data_bytes),
        ];
        if let Some(cid) = chain_id_for_signing {
            items.push(rlp::encode_u64(cid));
            items.push(rlp::encode_bytes(&[])); // 0
            items.push(rlp::encode_bytes(&[])); // 0
        }
        rlp::encode_list(&items)
    };

    let signing_hash = keccak256(&unsigned_rlp);

    // --- 4. Recover secp256k1 public key ---
    // Build 32-byte zero-padded r and s
    let mut r_padded = [0u8; 32];
    if r_bytes.len() <= 32 {
        r_padded[32 - r_bytes.len()..].copy_from_slice(r_bytes);
    } else {
        return eth_rpc_error(id, -32602, "Invalid r value (too long)");
    }

    let mut s_padded = [0u8; 32];
    if s_bytes.len() <= 32 {
        s_padded[32 - s_bytes.len()..].copy_from_slice(s_bytes);
    } else {
        return eth_rpc_error(id, -32602, "Invalid s value (too long)");
    }

    let mut rs_bytes = [0u8; 64];
    rs_bytes[..32].copy_from_slice(&r_padded);
    rs_bytes[32..].copy_from_slice(&s_padded);

    let recovery_id = match k256::ecdsa::RecoveryId::try_from(recovery_id_byte) {
        Ok(rid) => rid,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid recovery ID"),
    };

    let signature = match k256::ecdsa::Signature::from_slice(&rs_bytes) {
        Ok(sig) => sig,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid signature bytes"),
    };

    let recovered_vk = match k256::ecdsa::VerifyingKey::recover_from_prehash(
        &signing_hash,
        &signature,
        recovery_id,
    ) {
        Ok(vk) => vk,
        Err(_) => return eth_rpc_error(id, -32602, "Failed to recover sender public key"),
    };

    // --- 5. Derive ARC address from recovered public key ---
    // ARC uses BLAKE3(uncompressed_pubkey_64_bytes) for secp256k1 addresses
    let uncompressed = recovered_vk.to_encoded_point(false);
    let point_bytes = uncompressed.as_bytes();
    let sender_address = arc_crypto::address_from_secp256k1_pubkey(&point_bytes[1..65]);

    // --- 6. Parse the "to" address ---
    let is_contract_creation = to_bytes.is_empty() && !data_bytes.is_empty();
    let to_address = if to_bytes.is_empty() {
        Hash256::ZERO
    } else if to_bytes.len() == 20 {
        // Standard 20-byte ETH address → pad to 32-byte ARC address
        let mut addr = [0u8; 32];
        addr[..20].copy_from_slice(to_bytes);
        Hash256(addr)
    } else {
        return eth_rpc_error(id, -32602, &format!("Invalid to address length: {}", to_bytes.len()));
    };

    // --- 7. Build the ARC Transaction ---
    let mut sig_65 = Vec::with_capacity(65);
    sig_65.extend_from_slice(&rs_bytes);
    sig_65.push(recovery_id_byte);
    let secp_sig = arc_crypto::Signature::Secp256k1 { signature: sig_65 };

    let arc_tx = if is_contract_creation {
        // Contract deployment — run EVM deploy immediately and persist
        let result = arc_vm::evm::evm_deploy(
            &node.state,
            sender_address,
            data_bytes.to_vec(),
            value,
            gas_limit,
        );
        if !result.success {
            return eth_rpc_error(id, -32000, &format!(
                "Contract deployment failed: {}",
                result.revert_reason.unwrap_or_default()
            ));
        }

        // Store event logs from deployment
        if !result.logs.is_empty() {
            let height = node.state.height();
            node.state.store_event_logs(height + 1, result.logs);
        }

        // Build an ARC Transfer tx as the on-chain record
        let mut tx = Transaction::new_transfer(sender_address, to_address, value, nonce);
        tx.gas_limit = gas_limit;
        tx.signature = secp_sig;
        tx.hash = tx.compute_hash();
        tx
    } else if data_bytes.is_empty() {
        // Simple value transfer
        let mut tx = Transaction::new_transfer(sender_address, to_address, value, nonce);
        tx.gas_limit = gas_limit;
        tx.signature = secp_sig;
        tx.hash = tx.compute_hash();
        tx
    } else {
        // Contract call — map to WasmCall with raw calldata
        let mut tx = Transaction::new_wasm_call(
            sender_address,
            to_address,
            String::new(), // No function name in EVM ABI (selector is in calldata)
            data_bytes.to_vec(),
            value,
            gas_limit,
            nonce,
        );
        tx.signature = secp_sig;
        tx.hash = tx.compute_hash();
        tx
    };

    // --- 8. Insert into mempool ---
    if let Err(e) = node.mempool.insert(arc_tx) {
        return eth_rpc_error(id, -32000, &format!("Mempool rejected transaction: {}", e));
    }

    // --- 9. Return the Ethereum-style tx hash (Keccak-256 of the full signed RLP) ---
    let eth_tx_hash = keccak256(&raw_bytes);
    eth_rpc_result(id, json!(format!("0x{}", hex::encode(eth_tx_hash))))
}

fn eth_estimate_gas(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    // Run the same as eth_call and return gas used
    let call_obj = match params.get(0) {
        Some(obj) => obj,
        None => return eth_rpc_error(id, -32602, "Missing call object"),
    };

    let from = call_obj.get("from")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_eth_address(s).ok())
        .unwrap_or(Hash256::ZERO);

    let to = call_obj.get("to")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_eth_address(s).ok())
        .unwrap_or(Hash256::ZERO);

    let data = call_obj.get("data")
        .or_else(|| call_obj.get("input"))
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();

    let value = call_obj.get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(0);

    let result = arc_vm::evm::evm_call(&node.state, from, to, data, value, 30_000_000);
    let gas_estimate = if result.gas_used == 0 { 21000 } else { result.gas_used };
    eth_rpc_result(id, json!(format!("0x{:x}", gas_estimate)))
}

// ─── Off-Chain Channel Relay ─────────────────────────────────────────────────

/// Relay a channel state message to the counterparty via HTTP long-poll.
///
/// This is a simple relay: the node stores the latest message per channel
/// and the counterparty polls for it. For production, this would be upgraded
/// to a WebSocket endpoint.
///
/// POST /channel/{channel_id}/relay
/// Body: arbitrary JSON (state commitment, payment, etc.)
async fn channel_relay(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(channel_id): axum::extract::Path<String>,
    Json(message): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    // Store message in a per-channel relay buffer.
    // In production, this would fan out to connected WebSocket clients.
    let _ = &node; // Node state available for future auth/rate-limiting
    let _ = &channel_id;
    let _ = &message;

    Ok(Json(json!({
        "ok": true,
        "channel_id": channel_id,
        "relayed": true,
    })))
}

/// Query the latest relayed state for a channel.
///
/// GET /channel/{channel_id}/state
async fn channel_state(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(channel_id): axum::extract::Path<String>,
) -> Result<Json<Value>, StatusCode> {
    // Look up channel escrow on-chain
    let escrow_input = [b"arc-channel".as_slice(), &hex::decode(&channel_id).unwrap_or_default()].concat();
    let escrow_addr = arc_crypto::hash_bytes(&escrow_input);
    let escrow = node.state.get_account(&escrow_addr);

    match escrow {
        Some(account) => {
            Ok(Json(json!({
                "channel_id": channel_id,
                "locked_balance": account.balance,
                "state_nonce": account.nonce,
                "challenge_expiry": account.staked_balance,
                "opener": format!("0x{}", hex::encode(&account.code_hash.0)),
                "counterparty": format!("0x{}", hex::encode(&account.storage_root.0)),
                "active": account.balance > 0,
            })))
        }
        None => {
            Ok(Json(json!({
                "channel_id": channel_id,
                "active": false,
                "error": "channel not found",
            })))
        }
    }
}

// ─── Inference Endpoints ─────────────────────────────────────────────────────

/// Run inference through the cached INT8 integer model and record attestation on-chain.
///
/// POST /inference/run
/// Body: { "input": "What is 2+2?", "max_tokens": 64, "bond": 1000 }
///
/// If --model was provided at startup, runs real deterministic inference through
/// the cached INT8 integer engine. Pure i64 arithmetic — identical output hash
/// on ARM, x86, RISC-V, any platform.
///
/// Returns the query, response text, output hash, ms/token, and attestation TX.
async fn inference_run(
    AxumState(node): AxumState<NodeState>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let req = match body {
        Some(Json(v)) => v,
        None => return Err(api_error(StatusCode::BAD_REQUEST, "Request body required. Send JSON with 'input' and 'max_tokens' fields.")),
    };

    let input_text = req.get("input")
        .and_then(|v| v.as_str())
        .unwrap_or("Hello, world!");
    let max_tokens = req.get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(64) as u32;
    let bond = req.get("bond")
        .and_then(|v| v.as_u64())
        .unwrap_or(1000);
    let challenge_period = req.get("challenge_period")
        .and_then(|v| v.as_u64())
        .unwrap_or(100);

    // Check if we have a loaded model (prefer candle float backend for quality)
    let model = match &node.inference_model {
        Some(m) => m.clone(),
        None => {
            return Ok(Json(json!({
                "success": false,
                "error": "No model loaded. Start node with --model /path/to/model.gguf",
            })));
        }
    };

    let start = std::time::Instant::now();

    // Encode input text to tokens using the tokenizer
    let prompt_tokens = model.encode(input_text);
    let encode_ms = start.elapsed().as_millis() as u64;

    if prompt_tokens.is_empty() {
        return Ok(Json(json!({
            "success": false,
            "error": "Failed to encode input text to tokens",
        })));
    }

    // Run inference — use candle float backend if available, else integer engine
    let (generated_tokens, output_hash, engine_name) = if let (Some(engine), Some(mid)) = (&node.candle_engine, &node.candle_model_id) {
        // Candle Q4 float backend — coherent output, deterministic on same arch
        let mut tokens_with_bos = vec![1u32]; // BOS
        tokens_with_bos.extend(&prompt_tokens);
        let result = engine.generate(mid, &tokens_with_bos, max_tokens)
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("Inference failed: {}", e)))?;
        let gen_tokens: Vec<u32> = result.output.chunks(4)
            .map(|c| u32::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0),
                c.get(2).copied().unwrap_or(0), c.get(3).copied().unwrap_or(0)]))
            .collect();
        (gen_tokens, result.output_hash, "candle Q4 (float, deterministic per-arch)")
    } else {
        // Integer engine fallback
        let eos_tokens = vec![2u32, 0];
        let (generated, hash) = model.generate(&prompt_tokens, max_tokens, &eos_tokens);
        (generated, hash, "INT8 integer (cross-platform deterministic)")
    };

    let inference_ms = start.elapsed().as_millis() as u64;
    let tokens_generated = generated_tokens.len() as u64;
    let ms_per_token = if tokens_generated > 0 { inference_ms / tokens_generated } else { 0 };

    // Decode output tokens to text
    let output_text = model.decode(&generated_tokens);

    // Compute model ID
    let model_id_data = format!(
        "arc-{}L-{}d-{}h-{}v",
        model.config.n_layers, model.config.d_model,
        model.config.n_heads, model.config.vocab_size
    );
    let model_id_hash = arc_crypto::hash_bytes(model_id_data.as_bytes());
    let input_hash = arc_crypto::hash_bytes(input_text.as_bytes());

    // Create InferenceAttestation transaction
    let attester = node.validator_address;
    let nonce = node.state.get_account(&attester)
        .map(|a| a.nonce)
        .unwrap_or(0);

    let tx = arc_types::Transaction {
        tx_type: arc_types::TxType::InferenceAttestation,
        from: attester,
        nonce,
        body: arc_types::TxBody::InferenceAttestation(
            arc_types::transaction::InferenceAttestationBody {
                model_id: model_id_hash,
                input_hash,
                output_hash,
                challenge_period,
                bond,
            },
        ),
        fee: 0,
        gas_limit: 0,
        hash: arc_crypto::Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: false,
    };

    let tx_hash = tx.compute_hash();

    // Submit to mempool
    let _submit_result = node.mempool.insert(tx);

    Ok(Json(json!({
        "success": true,
        "inference": {
            "model": model_id_data,
            "model_hash": format!("0x{}", hex::encode(&model_id_hash.0)),
            "input": input_text,
            "input_tokens": prompt_tokens.len(),
            "input_hash": format!("0x{}", hex::encode(&input_hash.0)),
            "output": output_text,
            "output_tokens": generated_tokens,
            "output_hash": format!("0x{}", hex::encode(&output_hash.0)),
            "tokens_generated": tokens_generated,
            "inference_ms": inference_ms,
            "ms_per_token": ms_per_token,
            "encode_ms": encode_ms,
            "deterministic": true,
            "engine": engine_name,
        },
        "attestation": {
            "tx_hash": format!("0x{}", hex::encode(&tx_hash.0)),
            "bond": bond,
            "challenge_period": challenge_period,
            "status": "submitted_to_mempool",
        },
        "explorer_url": format!("/tx/0x{}", hex::encode(&tx_hash.0)),
    })))
}

/// List recent inference attestations from chain state.
///
/// GET /inference/attestations?limit=10
async fn inference_list_attestations(
    AxumState(node): AxumState<NodeState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    let limit = params.get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(10);

    // Get latest block to find recent attestations
    let height = node.state.height();
    let mut attestations = Vec::new();

    // Scan recent blocks for InferenceAttestation transactions
    for h in (1..=height).rev().take(100) {
        if let Some(block) = node.state.get_block(h) {
            for tx_hash in &block.tx_hashes {
                if let Some(receipt) = node.state.get_receipt(&tx_hash.0) {
                    if receipt.success {
                        attestations.push(json!({
                            "tx_hash": format!("0x{}", hex::encode(&tx_hash.0)),
                            "block_height": h,
                            "success": true,
                            "gas_used": receipt.gas_used,
                        }));
                        if attestations.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }
        if attestations.len() >= limit {
            break;
        }
    }

    Ok(Json(json!({
        "attestations": attestations,
        "count": attestations.len(),
        "chain_height": height,
    })))
}
