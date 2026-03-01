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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tower_http::cors::CorsLayer;

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
) -> anyhow::Result<()> {
    let tier = StakeTier::from_stake(stake).unwrap_or(StakeTier::Spark);

    let node = NodeState {
        state,
        mempool,
        validator_address,
        stake,
        tier,
        boot_time,
        peer_count,
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/info", get(chain_info))
        .route("/node/info", get(node_info))
        .route("/block/{height}", get(get_block))
        .route("/account/{address}", get(get_account))
        .route("/tx/submit", post(submit_tx))
        .route("/tx/submit_batch", post(submit_batch))
        .route("/tx/{hash}", get(get_transaction))
        .route("/tx/{hash}/proof", get(get_tx_proof))
        .route("/block/{height}/proofs", get(get_block_proofs))
        .route("/blocks", get(get_blocks))
        .route("/account/{address}/txs", get(get_account_txs))
        .route("/stats", get(get_stats))
        .route("/tx/{hash}/full", get(get_full_transaction))
        .route("/contract/{address}", get(get_contract_info))
        .route("/contract/{address}/call", post(call_contract))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024)) // 256 MB
        .layer(CorsLayer::permissive())
        .with_state(node);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> &'static str {
    "ARC Chain — Agent Runtime Chain — Testnet v0.1.0"
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

async fn get_block(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(height): axum::extract::Path<u64>,
) -> Result<Json<Block>, StatusCode> {
    node.state
        .get_block(height)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn get_account(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> Result<Json<Account>, StatusCode> {
    let addr = Hash256::from_hex(&address).map_err(|_| StatusCode::BAD_REQUEST)?;
    node.state
        .get_account(&addr)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
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
}

#[derive(Serialize)]
struct SubmitTxResponse {
    tx_hash: String,
    status: String,
}

async fn submit_tx(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<SubmitTxRequest>,
) -> Result<Json<SubmitTxResponse>, StatusCode> {
    let from = Hash256::from_hex(&req.from).map_err(|_| StatusCode::BAD_REQUEST)?;
    let to = Hash256::from_hex(&req.to).map_err(|_| StatusCode::BAD_REQUEST)?;

    let tx = Transaction::new_transfer(from, to, req.amount, req.nonce);
    let hash = tx.hash.to_hex();

    node.mempool
        .insert(tx)
        .map_err(|_| StatusCode::CONFLICT)?;

    Ok(Json(SubmitTxResponse {
        tx_hash: hash,
        status: "pending".to_string(),
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
// Proof & query endpoints
// ---------------------------------------------------------------------------

/// Parse a 64-char hex string into a [u8; 32] array.
fn parse_hash(hex_str: &str) -> Result<[u8; 32], StatusCode> {
    Hash256::from_hex(hex_str)
        .map(|h| h.0)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

/// GET /tx/{hash} — Look up a transaction receipt by its hash.
async fn get_transaction(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<TxReceipt>, StatusCode> {
    let tx_hash = parse_hash(&hash)?;
    node.state
        .get_receipt(&tx_hash)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// GET /tx/{hash}/proof — Return a full verification bundle for a transaction.
async fn get_tx_proof(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let tx_hash = parse_hash(&hash)?;

    let receipt = node
        .state
        .get_receipt(&tx_hash)
        .ok_or(StatusCode::NOT_FOUND)?;

    // Deserialize the stored Merkle proof
    let proof_bytes = receipt
        .inclusion_proof
        .as_ref()
        .ok_or(StatusCode::NOT_FOUND)?;

    let merkle_proof: MerkleProof =
        bincode::deserialize(proof_bytes).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Build sibling list for JSON
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

    let body = json!({
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
    });

    Ok(Json(body))
}

/// GET /block/{height}/proofs — Return all Merkle proofs for transactions in a block.
async fn get_block_proofs(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(height): axum::extract::Path<u64>,
) -> Result<Json<Value>, StatusCode> {
    let block = node
        .state
        .get_block(height)
        .ok_or(StatusCode::NOT_FOUND)?;

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

/// GET /account/{address}/txs — Return transaction hashes involving an account.
async fn get_account_txs(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> Result<Json<Value>, StatusCode> {
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
    Json(json!({
        "chain": "ARC Chain",
        "version": "0.1.0",
        "block_height": node.state.height(),
        "total_accounts": node.state.account_count(),
        "mempool_size": node.mempool.len(),
        "total_receipts": node.state.receipts.len(),
    }))
}

// ---------------------------------------------------------------------------
// Full transaction & contract endpoints
// ---------------------------------------------------------------------------

/// GET /tx/{hash}/full — Return the full transaction body with type-specific fields.
async fn get_full_transaction(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let tx_hash = parse_hash(&hash)?;

    let tx = node
        .state
        .get_transaction(&tx_hash)
        .ok_or(StatusCode::NOT_FOUND)?;

    let receipt = node.state.get_receipt(&tx_hash);

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
    };

    let mut result = json!({
        "tx_hash": Hash256(tx_hash).to_hex(),
        "tx_type": format!("{:?}", tx.tx_type),
        "from": tx.from.to_hex(),
        "nonce": tx.nonce,
        "fee": tx.fee,
        "gas_limit": tx.gas_limit,
        "body": body_json,
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
) -> Result<Json<Value>, StatusCode> {
    let addr = Hash256::from_hex(&address).map_err(|_| StatusCode::BAD_REQUEST)?;

    let bytecode = node
        .state
        .get_contract(&addr)
        .ok_or(StatusCode::NOT_FOUND)?;

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
) -> Result<Json<Value>, StatusCode> {
    let contract_addr = Hash256::from_hex(&address).map_err(|_| StatusCode::BAD_REQUEST)?;

    let bytecode = node
        .state
        .get_contract(&contract_addr)
        .ok_or(StatusCode::NOT_FOUND)?;

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
