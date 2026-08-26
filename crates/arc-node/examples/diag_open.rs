//! Diagnose InferenceEscrowOpen tx submit + landing.
use arc_crypto::{Hash256, Signature, hash_bytes};
use arc_types::transaction::{InferenceEscrowOpenBody, TxBody};
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
    let phrase = format!(
        "diag-open-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    let (sk, pk, from) = keypair(&phrase);
    let from_hex = hex::encode(from.0);
    let _ = c
        .post(format!("{}/faucet/claim", COORD))
        .json(&json!({"address":&from_hex}))
        .send()
        .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let acc: Value = c
        .get(format!("{}/account/{}", COORD, from_hex))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let nonce = acc.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0);
    let bal = acc.get("balance").and_then(|n| n.as_u64()).unwrap_or(0);
    println!("from={} nonce={} bal={}", from_hex, nonce, bal);

    let mut request_id = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut request_id);
    let body = InferenceEscrowOpenBody {
        request_id,
        model_id: hash_bytes(b"test-model"),
        max_fee: 1000,
        max_tokens: 3,
        timeout_blocks: 10000,
    };
    let mut tx = Transaction {
        tx_type: TxType::InferenceEscrowOpen,
        from,
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
    let h = hex::encode(tx.hash.0);
    let json_body = serde_json::to_string(&tx).unwrap();
    println!(
        "tx JSON ({} chars): {}",
        json_body.len(),
        &json_body[..json_body.len().min(300)]
    );
    println!("tx_hash: 0x{}", h);
    let r = c
        .post(format!("{}/tx/submit_signed", COORD))
        .body(json_body)
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    println!("submit status: {}", r.status());
    println!("submit body: {}", r.text().await.unwrap_or_default());

    for i in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let info: Value = c
            .get(format!("{}/info", COORD))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let height = info
            .get("block_height")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let mp = info
            .get("mempool_size")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let txr = c.get(format!("{}/tx/0x{}", COORD, h)).send().await;
        let txs = match txr {
            Ok(r) => format!("{}", r.status()),
            Err(e) => format!("err:{}", e),
        };
        let acc: Value = c
            .get(format!("{}/account/{}", COORD, from_hex))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let n = acc.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0);
        println!(
            "[{}s] h={} mp={} tx={} from.n={}",
            (i + 1) * 2,
            height,
            mp,
            txs,
            n
        );
        if n > nonce {
            println!("✓ EXEC");
            break;
        }
    }
}
