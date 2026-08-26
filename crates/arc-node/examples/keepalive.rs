//! Keepalive: submits a Transfer every 20s from a dedicated keypair.
//! Keeps chain block production alive without colliding with other tests.
use arc_crypto::{Hash256, Signature};
use arc_types::transaction::{TransferBody, TxBody};
use arc_types::{Transaction, TxType};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Duration;
const DOMAIN_TAG: &str = "ARC-chain-validator-keypair-v1";
const COORD: &str = "http://140.82.16.112:9090";
fn keypair(p: &str) -> (SigningKey, [u8; 32], Hash256) {
    let s = blake3::derive_key(DOMAIN_TAG, p.as_bytes());
    let sk = SigningKey::from_bytes(&s);
    let pk = sk.verifying_key().to_bytes();
    (sk, pk, Hash256(*blake3::hash(&pk).as_bytes()))
}
#[tokio::main]
async fn main() {
    let c = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let (sk, pk, from) = keypair("arc-keepalive-bot-v1");
    let from_hex = hex::encode(from.0);
    let (_, _, to) = keypair("arc-keepalive-recipient-v1");
    // Faucet up if low.
    let _ = c
        .post(format!("{}/faucet/claim", COORD))
        .json(&json!({"address": &from_hex}))
        .send()
        .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    loop {
        let acc: Value = match c
            .get(format!("{}/account/{}", COORD, from_hex))
            .send()
            .await
        {
            Ok(r) => r.json().await.unwrap_or(Value::Null),
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let nonce = acc.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0);
        let bal = acc.get("balance").and_then(|n| n.as_u64()).unwrap_or(0);
        if bal < 100 {
            let _ = c
                .post(format!("{}/faucet/claim", COORD))
                .json(&json!({"address": &from_hex}))
                .send()
                .await;
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }
        let mut tx = Transaction {
            tx_type: TxType::Transfer,
            from,
            nonce,
            body: TxBody::Transfer(TransferBody {
                to,
                amount: 1,
                amount_commitment: None,
            }),
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
        let _ = c
            .post(format!("{}/tx/submit_signed", COORD))
            .json(&tx)
            .send()
            .await;
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}
