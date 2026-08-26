//! v0.7.9 smoke test for the caller-signed `signed_tx` path on
//! `/inference/onchain/submit`.
//!
//! Builds a real ed25519-signed InferenceRequest Transaction from a
//! deterministic phrase-derived keypair, faucets the address, bincode-
//! serializes the tx, hex-encodes, and submits via the new `signed_tx`
//! field added in commit 34e1fd05. Then polls
//! `/inference/onchain/result/<request_id>` until Finalized or timeout.
//!
//! Usage:
//!     cargo run --release --example v079_signed_inference -p arc-node -- \
//!         [coord_url] [phrase]
//!
//! Default coord is LAX (140.82.16.112:9090). Phrase defaults to a
//! wall-clock-derived string so reruns produce a fresh address with
//! no carry-over nonce state.

use arc_crypto::{Hash256, Signature, hash_bytes};
use arc_types::transaction::{InferenceRequestBody, TxBody};
use arc_types::{Transaction, TxType};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Duration;

const DOMAIN_TAG: &str = "ARC-chain-validator-keypair-v1";
const DEFAULT_COORD: &str = "http://140.82.16.112:9090";

fn keypair(phrase: &str) -> (SigningKey, [u8; 32], Hash256) {
    let seed = blake3::derive_key(DOMAIN_TAG, phrase.as_bytes());
    let sk = SigningKey::from_bytes(&seed);
    let pk = sk.verifying_key().to_bytes();
    let addr = Hash256(*blake3::hash(&pk).as_bytes());
    (sk, pk, addr)
}

async fn balance(c: &Client, coord: &str, addr_hex: &str) -> u64 {
    let url = format!("{}/account/{}", coord, addr_hex);
    if let Ok(r) = c.get(&url).send().await
        && r.status().is_success()
        && let Ok(v) = r.json::<Value>().await
    {
        return v.get("balance").and_then(|b| b.as_u64()).unwrap_or(0);
    }
    0
}

async fn nonce_of(c: &Client, coord: &str, addr_hex: &str) -> u64 {
    let url = format!("{}/account/{}", coord, addr_hex);
    if let Ok(r) = c.get(&url).send().await
        && r.status().is_success()
        && let Ok(v) = r.json::<Value>().await
    {
        return v.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0);
    }
    0
}

async fn faucet_and_wait(c: &Client, coord: &str, addr_hex: &str, target: u64) {
    if balance(c, coord, addr_hex).await >= target {
        return;
    }
    let _ = c
        .post(format!("{}/faucet/claim", coord))
        .json(&json!({ "address": addr_hex }))
        .send()
        .await;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if balance(c, coord, addr_hex).await >= target {
            eprintln!("  faucet ok: {} ARC", balance(c, coord, addr_hex).await);
            return;
        }
    }
    eprintln!("  WARN: faucet did not reach target {}", target);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let coord = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| DEFAULT_COORD.to_string());
    let phrase = args.get(2).cloned().unwrap_or_else(|| {
        format!(
            "v079-signed-inference-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        )
    });
    println!("=== v0.7.9 signed_tx smoke ===");
    println!("coord:  {}", coord);
    println!("phrase: {}", phrase);

    let c = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    let (sk, pk, addr) = keypair(&phrase);
    let addr_hex = hex::encode(addr.0);
    println!("address: 0x{}", addr_hex);

    // 1. Faucet the address so it has balance for the escrow lock.
    println!();
    println!("--- step 1: faucet keypair ---");
    faucet_and_wait(&c, &coord, &addr_hex, 100).await;

    // 2. Read nonce.
    let nonce = nonce_of(&c, &coord, &addr_hex).await;
    println!("nonce: {}", nonce);

    // 3. Build the InferenceRequest body + tx.
    println!();
    println!("--- step 2: build + sign InferenceRequest ---");
    let input_blob = b"v079 signed_tx smoke: tell me a one-line joke about determinism".to_vec();
    let input_hash = hash_bytes(&input_blob);
    let request_id = {
        let mut h = blake3::Hasher::new();
        h.update(addr.as_ref());
        h.update(&nonce.to_le_bytes());
        h.update(input_hash.as_ref());
        *h.finalize().as_bytes()
    };
    let body = TxBody::InferenceRequest(InferenceRequestBody {
        request_id,
        // Canonical testnet model_id — must match
        // arc_node::inference_validator::canonical_testnet_model_id(),
        // otherwise committee members won't run inference for the request.
        model_id: hash_bytes(b"arc-32L-test"),
        input_hash,
        input_blob,
        max_tokens: 16,
        tier: 1,
        max_reward: 10,
        deadline_blocks: 50,
        committee_size: 15,
    });
    let mut tx = Transaction {
        tx_type: TxType::InferenceRequest,
        from: addr,
        nonce,
        body,
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
    println!("request_id: 0x{}", hex::encode(request_id));
    println!("tx_hash:    0x{}", hex::encode(tx.hash.0));

    // 4. Bincode-serialize, hex-encode, POST as signed_tx.
    println!();
    println!("--- step 3: POST /inference/onchain/submit (signed_tx) ---");
    let bin = bincode::serialize(&tx).expect("bincode serialize tx");
    let signed_hex = format!("0x{}", hex::encode(&bin));
    println!(
        "signed_tx_len: {} bytes ({} hex chars)",
        bin.len(),
        signed_hex.len()
    );
    let submit_resp = c
        .post(format!("{}/inference/onchain/submit", coord))
        .json(&json!({ "signed_tx": signed_hex }))
        .send()
        .await
        .expect("submit POST");
    let st = submit_resp.status();
    let body_txt = submit_resp.text().await.unwrap_or_default();
    println!("HTTP {}\n{}", st, body_txt);
    if !st.is_success() {
        eprintln!("submit failed — bailing");
        std::process::exit(2);
    }

    // 5. Poll the result.
    println!();
    println!("--- step 4: poll /inference/onchain/result ---");
    let req_id_hex = hex::encode(request_id);
    let mut last_status = String::new();
    let mut finalized = false;
    for i in 0..36 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let url = format!("{}/inference/onchain/result/0x{}", coord, req_id_hex);
        match c.get(&url).send().await {
            Ok(r) => {
                let s = r.status();
                let txt = r.text().await.unwrap_or_default();
                if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                    let status = v
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("?")
                        .to_string();
                    if status != last_status {
                        println!("[{:>2}s] status={} HTTP {}", (i + 1) * 5, status, s);
                        println!("       full: {}", txt);
                        last_status = status.clone();
                    } else {
                        println!("[{:>2}s] status={} (unchanged)", (i + 1) * 5, status);
                    }
                    if status == "Finalized" {
                        finalized = true;
                        println!("\n=== FINAL PAYLOAD ===\n{}", txt);
                        break;
                    }
                } else {
                    println!("[{:>2}s] HTTP {} body={}", (i + 1) * 5, s, txt);
                }
            }
            Err(e) => println!("[{:>2}s] poll err: {}", (i + 1) * 5, e),
        }
    }

    // 6. Recheck nonce.
    let final_nonce = nonce_of(&c, &coord, &addr_hex).await;
    println!();
    println!(
        "--- final nonce of signer: {} (was {} pre-submit) ---",
        final_nonce, nonce
    );

    if !finalized {
        eprintln!("\nFAIL: never reached Finalized after 180s");
        std::process::exit(1);
    }
    println!("\nOK: signed_tx path Finalized");
}
