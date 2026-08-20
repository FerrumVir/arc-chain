//! Quick diagnostic: submit a Transfer tx, see if it lands.
//! Verifies block production is actually happening + tx-submission path
//! works, before debugging the C/D/E specific tx types.
use arc_crypto::{Hash256, Signature};
use arc_types::transaction::{TransferBody, TxBody};
use arc_types::{Transaction, TxType};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

const DOMAIN_TAG: &str = "ARC-chain-validator-keypair-v1";
const COORD: &str = "http://140.82.16.112:9090";

fn keypair(phrase: &str) -> (SigningKey, [u8; 32], Hash256) {
    let seed = blake3::derive_key(DOMAIN_TAG, phrase.as_bytes());
    let sk = SigningKey::from_bytes(&seed);
    let pk = sk.verifying_key().to_bytes();
    let addr = Hash256(*blake3::hash(&pk).as_bytes());
    (sk, pk, addr)
}

#[tokio::main]
async fn main() {
    let c = Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let (sk, pk, from) = keypair("milestone-b-live-test");
    let from_hex = hex::encode(from.0);
    let (_to_sk, _to_pk, to) = keypair("milestone-c-test-recipient");

    // Get current nonce + initial balance.
    let info: Value = c.get(format!("{}/account/{}", COORD, from_hex)).send().await.unwrap().json().await.unwrap();
    let nonce = info.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0);
    let bal_pre = info.get("balance").and_then(|b| b.as_u64()).unwrap_or(0);
    println!("from={}  nonce={}  balance={}", from_hex, nonce, bal_pre);

    let mut tx = Transaction {
        tx_type: TxType::Transfer,
        from,
        nonce,
        body: TxBody::Transfer(TransferBody { to, amount: 1, amount_commitment: None }),
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
    println!("submitting transfer tx: 0x{}", h);

    let r = c.post(format!("{}/tx/submit_signed", COORD)).json(&tx).send().await.unwrap();
    println!("submit status: {}  body: {}", r.status(), r.text().await.unwrap_or_default());

    // Poll info every 2s for 60s.
    let h0 = c.get(format!("{}/info", COORD)).send().await.unwrap().json::<Value>().await.unwrap();
    let height0 = h0.get("block_height").and_then(|x| x.as_u64()).unwrap_or(0);
    let mempool0 = h0.get("mempool_size").and_then(|x| x.as_u64()).unwrap_or(0);
    println!("h0={}  mempool0={}", height0, mempool0);
    for i in 1..=30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let h = c.get(format!("{}/info", COORD)).send().await.unwrap().json::<Value>().await.unwrap();
        let height = h.get("block_height").and_then(|x| x.as_u64()).unwrap_or(0);
        let mempool = h.get("mempool_size").and_then(|x| x.as_u64()).unwrap_or(0);
        let acc = c.get(format!("{}/account/{}", COORD, from_hex)).send().await.unwrap().json::<Value>().await.unwrap();
        let n = acc.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0);
        let b = acc.get("balance").and_then(|n| n.as_u64()).unwrap_or(0);
        println!("[{}s] h={} mempool={} from.nonce={} from.balance={}", i*2, height, mempool, n, b);
        if n > nonce { println!("  ✓ tx executed (nonce advanced)"); break; }
    }
}
