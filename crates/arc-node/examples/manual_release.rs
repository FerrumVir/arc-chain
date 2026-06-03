//! Submit an InferenceEscrowRelease for the open already on-chain.

use arc_crypto::{hash_bytes, Hash256, Signature};
use arc_types::transaction::{InferenceEscrowReleaseBody, TxBody};
use arc_types::{Transaction, TxType};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

const COORD: &str = "http://140.82.16.112:9090";
const PAYER_PHRASE: &str = "milestone-b-live-test";
const REQUEST_ID_HEX: &str =
    "f404a52ae155907183b428fdac2601a08dbf003416dc16ef7c073e93c2c94d56";
const MODEL_ID_HEX: &str =
    "2c66ccd2ebaa35b1031efb18e1af8b946339a6b31a3c718cbd3beb3f4281156d";

async fn balance(c: &Client, hex_addr: &str) -> u64 {
    let url = format!("{}/account/{}", COORD, hex_addr);
    let resp = match c.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return 0,
    };
    resp.json::<Value>()
        .await
        .ok()
        .and_then(|v| v.get("balance").and_then(|b| b.as_u64()))
        .unwrap_or(0)
}

async fn nonce_of(c: &Client, hex_addr: &str) -> u64 {
    let url = format!("{}/account/{}", COORD, hex_addr);
    let resp = match c.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return 0,
    };
    resp.json::<Value>()
        .await
        .ok()
        .and_then(|v| v.get("nonce").and_then(|n| n.as_u64()))
        .unwrap_or(0)
}

#[tokio::main]
async fn main() {
    let c = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    let seed = blake3::derive_key("ARC-chain-validator-keypair-v1", PAYER_PHRASE.as_bytes());
    let sk = SigningKey::from_bytes(&seed);
    let pk = sk.verifying_key().to_bytes();
    let payer_addr = Hash256(*blake3::hash(&pk).as_bytes());

    let request_id_bytes: [u8; 32] = {
        let raw = hex::decode(REQUEST_ID_HEX).unwrap();
        let mut a = [0u8; 32];
        a.copy_from_slice(&raw);
        a
    };
    let model_id = Hash256({
        let raw = hex::decode(MODEL_ID_HEX).unwrap();
        let mut a = [0u8; 32];
        a.copy_from_slice(&raw);
        a
    });

    let payer_pre = balance(&c, &hex::encode(payer_addr.0)).await;
    let treasury = hash_bytes(b"arc-treasury");
    let observer = hash_bytes(b"arc-observer-pool");
    let proposer = payer_addr;
    let replicas: Vec<Hash256> = ["NYC", "LAX", "AMS", "LHR", "NRT", "SGP"]
        .iter()
        .map(|n| hash_bytes(format!("replica:{}", n).as_bytes()))
        .collect();

    let tre_pre = balance(&c, &hex::encode(treasury.0)).await;
    let obs_pre = balance(&c, &hex::encode(observer.0)).await;
    let mut rep_pre: Vec<u64> = Vec::new();
    for r in &replicas {
        rep_pre.push(balance(&c, &hex::encode(r.0)).await);
    }

    let nonce = nonce_of(&c, &hex::encode(payer_addr.0)).await;
    let body = InferenceEscrowReleaseBody {
        request_id: request_id_bytes,
        payer: payer_addr,
        model_id,
        max_tokens: 3,
        timeout_blocks: 10000,
        output_hash: hash_bytes(b"manual-release-test"),
        proposer,
        replicas: replicas.clone(),
        observer_pool: observer,
        treasury,
    };
    let mut tx = Transaction {
        tx_type: TxType::InferenceEscrowRelease,
        from: payer_addr,
        nonce,
        body: TxBody::InferenceEscrowRelease(body),
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
    let release_hash = hex::encode(tx.hash.0);
    println!("release_tx_hash: 0x{}", release_hash);

    let resp = c
        .post(format!("{}/tx/submit_signed", COORD))
        .json(&tx)
        .send()
        .await
        .expect("submit");
    println!("submit status: {}", resp.status());

    let mut committed = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(r) = c
            .get(format!("{}/tx/0x{}", COORD, release_hash))
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
        eprintln!("release tx did not commit in 60s");
        std::process::exit(1);
    }
    println!("release committed.");

    let payer_post = balance(&c, &hex::encode(payer_addr.0)).await;
    let tre_post = balance(&c, &hex::encode(treasury.0)).await;
    let obs_post = balance(&c, &hex::encode(observer.0)).await;
    let mut rep_post: Vec<u64> = Vec::new();
    for r in &replicas {
        rep_post.push(balance(&c, &hex::encode(r.0)).await);
    }

    println!();
    println!("=== BALANCE DELTAS (release) ===");
    println!(
        "payer/proposer: {} → {}  (Δ {:+})",
        payer_pre, payer_post, payer_post as i64 - payer_pre as i64
    );
    println!(
        "treasury:       {} → {}  (Δ {:+})",
        tre_pre, tre_post, tre_post as i64 - tre_pre as i64
    );
    println!(
        "observer pool:  {} → {}  (Δ {:+})",
        obs_pre, obs_post, obs_post as i64 - obs_pre as i64
    );
    let names = ["NYC", "LAX", "AMS", "LHR", "NRT", "SGP"];
    let mut sum_replica: i64 = 0;
    for i in 0..6 {
        let d = rep_post[i] as i64 - rep_pre[i] as i64;
        sum_replica += d;
        println!(
            "replica[{}]:    {} → {}  (Δ {:+})",
            names[i], rep_pre[i], rep_post[i], d
        );
    }
    let credited = (tre_post as i64 - tre_pre as i64)
        + (obs_post as i64 - obs_pre as i64)
        + sum_replica
        + (payer_post as i64 - payer_pre as i64);
    println!();
    println!("Total credited (treasury+observer+replicas+proposer-self): {}", credited);
    println!("Expected: 10000 ARC released from escrow");
    if credited == 10000 {
        println!("✓ TOTAL CONSERVED");
    } else {
        println!("Δ vs expected: {}", credited - 10000);
    }
}
