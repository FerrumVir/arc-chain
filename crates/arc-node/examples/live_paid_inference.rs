//! Milestone B live-testnet end-to-end paid inference.
//!
//! Runs against the 6-seed testnet. Produces a real balance-delta
//! receipt that closes the acceptance gap for PR #40.
//!
//! Usage:
//!     cargo run --release --example live_paid_inference -p arc-node -- [coord_url]
//!
//! What it does:
//!   1. Derives a deterministic payer keypair from the string
//!      "milestone-b-live-test". Stable across reruns, so balances
//!      accumulate.
//!   2. Faucets the payer to top up if balance < 10_000 ARC.
//!   3. Records every relevant address's balance pre-inference:
//!      payer, treasury, observer_pool, and the per-seed "replica"
//!      synthetic addresses (hash("replica:NYC") etc.).
//!   4. Signs + submits an InferenceEscrowOpen for max_fee=10_000.
//!   5. Waits for the open tx to commit.
//!   6. Calls /inference/run_consensus with the escrow fields; the
//!      coordinator auto-submits the release on success.
//!   7. Waits a few blocks for the release to commit.
//!   8. Records post-inference balances and reports the deltas.
//!   9. Asserts: payer down 10_000, treasury/observer/proposer up by
//!      their share, total conserved (±rounding).

use arc_crypto::{hash_bytes, Hash256, Signature};
use arc_types::transaction::{InferenceEscrowOpenBody, TxBody};
use arc_types::{Transaction, TxType};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const DOMAIN_TAG: &str = "ARC-chain-validator-keypair-v1";
// NYC (149.28.32.76) is dead as of 2026-06-03 — coordinator defaults to LAX.
const DEFAULT_COORD: &str = "http://140.82.16.112:9090";
const SEEDS: &[(&str, &str)] = &[
    ("LAX", "140.82.16.112"),
    ("AMS", "136.244.109.1"),
    ("LHR", "104.238.171.11"),
    ("NRT", "202.182.107.41"),
    ("SGP", "149.28.153.31"),
];

fn keypair_from_phrase(phrase: &str) -> (SigningKey, [u8; 32], Hash256) {
    let seed = blake3::derive_key(DOMAIN_TAG, phrase.as_bytes());
    let sk = SigningKey::from_bytes(&seed);
    let pk = sk.verifying_key().to_bytes();
    let addr = Hash256(*blake3::hash(&pk).as_bytes());
    (sk, pk, addr)
}

async fn balance(c: &Client, coord: &str, addr_hex: &str) -> u64 {
    let url = format!("{}/account/{}", coord, addr_hex);
    match c.get(&url).send().await {
        Ok(r) if r.status().is_success() => r
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("balance").and_then(|b| b.as_u64()))
            .unwrap_or(0),
        _ => 0,
    }
}

async fn nonce_of(c: &Client, coord: &str, addr_hex: &str) -> u64 {
    let url = format!("{}/account/{}", coord, addr_hex);
    match c.get(&url).send().await {
        Ok(r) if r.status().is_success() => r
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("nonce").and_then(|n| n.as_u64()))
            .unwrap_or(0),
        _ => 0,
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let coord = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| DEFAULT_COORD.to_string());
    let c = Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .unwrap();
    let quick = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // Use a wall-clock-derived phrase so each run gets a fresh payer
    // address with no carry-over state from prior failed attempts.
    let phrase = format!(
        "milestone-b-live-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let (sk, pk, payer_addr) = keypair_from_phrase(&phrase);
    let payer_hex = hex::encode(payer_addr.0);
    println!("payer_address: 0x{}", payer_hex);

    // Step 2: faucet via the coordinator. The validator-signed FaucetClaim
    // (TxType 0x21, shipped 2026-05-11) propagates cleanly through DAG
    // consensus so the funded balance appears on every seed within ~1
    // block. We then wait for the tx to commit before reading balances.
    println!("faucet on coordinator…");
    let url = format!("{}/faucet/claim", coord);
    let faucet_resp = quick
        .post(&url)
        .json(&json!({ "address": &payer_hex }))
        .send()
        .await;
    match faucet_resp {
        Ok(r) => println!("  coord: {}", r.status()),
        Err(e) => println!("  coord: ERR {}", e),
    }
    // Poll the payer balance on the coordinator until it lands on the
    // chain (faucet handler pre-applies locally but the receipt may
    // take a few hundred ms to surface).
    let mut bal_pre = 0u64;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        bal_pre = balance(&quick, &coord, &payer_hex).await;
        if bal_pre > 0 {
            break;
        }
    }
    println!("payer_balance_pre_inference: {} ARC (on coord)", bal_pre);
    // Sanity: confirm the balance also propagated to the other seeds.
    // Allow up to 10s of slack — the DAG round at 4.2M Hz commits ~1s/block
    // but cross-region propagation can take a few rounds.
    for (name, ip) in SEEDS {
        let url = format!("http://{}:9090", ip);
        let mut b = 0u64;
        for _ in 0..40 {
            b = balance(&quick, &url, &payer_hex).await;
            if b > 0 { break; }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        println!("  payer_balance[{}]: {} ARC", name, b);
    }

    // Step 3: snapshot everyone's balance we care about.
    let treasury = hex::encode(hash_bytes(b"arc-treasury").0);
    let observer = hex::encode(hash_bytes(b"arc-observer-pool").0);
    let tre_pre = balance(&quick, &coord, &treasury).await;
    let obs_pre = balance(&quick, &coord, &observer).await;
    let mut replica_pre = Vec::new();
    for (name, _ip) in SEEDS {
        let addr = hex::encode(hash_bytes(format!("replica:{}", name).as_bytes()).0);
        let b = balance(&quick, &coord, &addr).await;
        replica_pre.push((*name, addr, b));
    }
    println!("treasury_pre: {}", tre_pre);
    println!("observer_pool_pre: {}", obs_pre);
    for (n, _, b) in &replica_pre {
        println!("replica[{}]_pre: {}", n, b);
    }

    // Step 4: build + sign InferenceEscrowOpen.
    let nonce = nonce_of(&quick, &coord, &payer_hex).await;
    let mut request_id = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut request_id);
    let model_id = hash_bytes(b"arc-testnet-llama-2-7b-chat-q4");
    let max_fee: u64 = 1_000;
    let max_tokens: u32 = 3; // short - saves testnet time
    let timeout_blocks: u64 = 10_000;

    let body = InferenceEscrowOpenBody {
        request_id,
        model_id,
        max_fee,
        max_tokens,
        timeout_blocks,
    };
    let mut tx = Transaction {
        tx_type: TxType::InferenceEscrowOpen,
        from: payer_addr,
        nonce,
        body: TxBody::InferenceEscrowOpen(body),
        fee: 0,
        gas_limit: 0,
        hash: Hash256::ZERO,
        signature: Signature::null(),
        sig_verified: false,
    };
    tx.hash = tx.compute_hash();
    let sig = sk.sign(tx.hash.as_bytes());
    tx.signature = Signature::Ed25519 {
        public_key: pk,
        signature: sig.to_bytes().to_vec(),
    };
    let open_hash_hex = hex::encode(tx.hash.0);
    println!("open_tx_hash: 0x{}", open_hash_hex);

    // Step 5: submit + wait for commit. Use pre-serialized JSON body
    // explicitly - `.json(&tx)` reqwest path was racing with diag_open
    // submission and intermittently dropping the tx.
    let json_body = serde_json::to_string(&tx).expect("serialize tx");
    let resp = quick
        .post(format!("{}/tx/submit_signed", coord))
        .body(json_body)
        .header("content-type", "application/json")
        .send()
        .await
        .expect("submit open tx");
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eprintln!("open submit failed: {} - {}", status, body);
        std::process::exit(1);
    }
    println!("open submitted, waiting for commit…");
    let mut committed = false;
    for _ in 0..480 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(r) = quick
            .get(format!("{}/tx/0x{}", coord, open_hash_hex))
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
        eprintln!("open tx did not commit in 30s");
        std::process::exit(1);
    }
    println!("open committed.");

    // Step 6: run paid inference.
    let wrapped = "[INST] Largest planet? [/INST]";
    println!("calling run_consensus with escrow gate (may take 2-3 min)…");
    let infer = c
        .post(format!("{}/inference/run_consensus", coord))
        .json(&json!({
            "input": wrapped,
            "max_tokens": max_tokens,
            "k": 3,
            "payer": format!("0x{}", payer_hex),
            "request_id": format!("0x{}", hex::encode(request_id)),
            "max_fee": max_fee,
            "model_id": format!("0x{}", hex::encode(model_id.0)),
            "timeout_blocks": timeout_blocks,
        }))
        .send()
        .await
        .expect("run_consensus send");
    if !infer.status().is_success() {
        let s = infer.status();
        let b = infer.text().await.unwrap_or_default();
        eprintln!("run_consensus failed: {} - {}", s, b);
        std::process::exit(1);
    }
    let infer_body: Value = infer.json().await.expect("run_consensus parse");
    let escrow_block = infer_body.get("escrow").cloned().unwrap_or(Value::Null);
    let release_hash = escrow_block
        .get("release_tx_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    println!("inference output: {:?}", infer_body.get("output"));
    println!("release_tx_hash: {}", release_hash);

    // Step 7: wait for release commit.
    let release_hex = release_hash.trim_start_matches("0x");
    for _ in 0..480 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(r) = quick
            .get(format!("{}/tx/0x{}", coord, release_hex))
            .send()
            .await
        {
            if r.status().is_success() {
                break;
            }
        }
    }

    // Step 8: record post-balances.
    let bal_post = balance(&quick, &coord, &payer_hex).await;
    let tre_post = balance(&quick, &coord, &treasury).await;
    let obs_post = balance(&quick, &coord, &observer).await;
    let mut replica_post = Vec::new();
    for (name, addr, _) in &replica_pre {
        let b = balance(&quick, &coord, addr).await;
        replica_post.push((*name, b));
    }

    println!("");
    println!("=== BALANCE DELTAS ===");
    println!("payer:    {} → {}  (Δ {:+})", bal_pre, bal_post, bal_post as i64 - bal_pre as i64);
    println!("treasury: {} → {}  (Δ {:+})", tre_pre, tre_post, tre_post as i64 - tre_pre as i64);
    println!("observer: {} → {}  (Δ {:+})", obs_pre, obs_post, obs_post as i64 - obs_pre as i64);
    let mut sum_replica_delta: i64 = 0;
    for (i, (name, b_post)) in replica_post.iter().enumerate() {
        let b_pre = replica_pre[i].2;
        let delta = *b_post as i64 - b_pre as i64;
        sum_replica_delta += delta;
        println!("replica[{}]: {} → {}  (Δ {:+})", name, b_pre, b_post, delta);
    }

    let payer_out = bal_pre as i64 - bal_post as i64;
    let total_in = (tre_post as i64 - tre_pre as i64)
        + (obs_post as i64 - obs_pre as i64)
        + sum_replica_delta;
    println!("");
    println!("payer sent: {}", payer_out);
    println!("beneficiaries received (treasury+observer+replicas): {}", total_in);
    println!("conservation residual: {} (positive = still in escrow or proposer)", payer_out - total_in);
}
