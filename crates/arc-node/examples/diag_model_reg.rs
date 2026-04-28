//! Diagnostic: submit a ModelRegistration and watch what happens to it
//! at every observable layer (mempool, block queue, /tx endpoint).
use arc_crypto::{hash_bytes, Hash256, Signature};
use arc_types::transaction::{ModelRegistrationBody, TxBody, MIN_MODEL_REGISTRATION_FEE};
use arc_types::{Transaction, TxType};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

const DOMAIN_TAG: &str = "ARC-chain-validator-keypair-v1";
const COORD: &str = "http://149.28.32.76:9090";

fn keypair(phrase: &str) -> (SigningKey, [u8; 32], Hash256) {
    let seed = blake3::derive_key(DOMAIN_TAG, phrase.as_bytes());
    let sk = SigningKey::from_bytes(&seed);
    let pk = sk.verifying_key().to_bytes();
    let addr = Hash256(*blake3::hash(&pk).as_bytes());
    (sk, pk, addr)
}

#[tokio::main]
async fn main() {
    let c = Client::builder().timeout(Duration::from_secs(15)).build().unwrap();
    let (sk, pk, from) = keypair("milestone-c-publisher");
    let from_hex = hex::encode(from.0);
    let acc: Value = c.get(format!("{}/account/{}", COORD, from_hex)).send().await.unwrap().json().await.unwrap();
    let nonce = acc.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0);
    let bal = acc.get("balance").and_then(|n| n.as_u64()).unwrap_or(0);
    println!("from={}  nonce={}  balance={}", from_hex, nonce, bal);

    let body = ModelRegistrationBody {
        model_id: hash_bytes(b"diag-model-id"),
        metadata_hash: hash_bytes(b"diag-meta"),
        chunk_tree_root: hash_bytes(b"diag-chunks"),
        n_layers: 32,
        d_model: 4096,
        quantization: "q4".into(),
        registration_fee: MIN_MODEL_REGISTRATION_FEE,
        royalty_recipient: from,
    };
    let mut tx = Transaction {
        tx_type: TxType::ModelRegistration,
        from,
        nonce,
        body: TxBody::ModelRegistration(body),
        fee: 0,
        gas_limit: 0,
        hash: Hash256::ZERO,
        signature: Signature::null(),
        sig_verified: false,
    };
    tx.hash = tx.compute_hash();
    let sig = sk.sign(tx.hash.as_bytes());
    tx.signature = Signature::Ed25519 { public_key: pk, signature: sig.to_bytes().to_vec() };
    let h = hex::encode(tx.hash.0);
    let json_body = serde_json::to_string(&tx).unwrap();
    println!("\n--- tx JSON (first 200 chars) ---");
    println!("{}", &json_body[..json_body.len().min(200)]);
    println!("\nsubmitting...");
    let r = c.post(format!("{}/tx/submit_signed", COORD)).body(json_body).header("content-type","application/json").send().await.unwrap();
    println!("submit status: {}", r.status());
    println!("submit body: {}", r.text().await.unwrap_or_default());
    println!("tx_hash: 0x{}", h);

    // Watch mempool + tx endpoint every 1s for 10s, then every 5s for 60s.
    for i in 0..70 {
        let dt = if i < 10 { 1 } else { 5 };
        tokio::time::sleep(Duration::from_secs(dt)).await;
        let info = c.get(format!("{}/info", COORD)).send().await.unwrap().json::<Value>().await.unwrap();
        let height = info.get("block_height").and_then(|x| x.as_u64()).unwrap_or(0);
        let mempool = info.get("mempool_size").and_then(|x| x.as_u64()).unwrap_or(0);
        let tx_resp = c.get(format!("{}/tx/0x{}", COORD, h)).send().await;
        let tx_status = match tx_resp { Ok(r) => format!("{}", r.status()), Err(e) => format!("err:{}", e) };
        let acc: Value = c.get(format!("{}/account/{}", COORD, from_hex)).send().await.unwrap().json().await.unwrap();
        let n = acc.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0);
        println!("[t+{}s] h={} mempool={} tx_endpoint={} from.nonce={}", (if i<10 {i+1} else {10+(i-9)*5}), height, mempool, tx_status, n);
        if n > nonce { println!("  ✓ tx EXECUTED"); break; }
    }
}
