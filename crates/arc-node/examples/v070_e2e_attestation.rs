// v0.7.0 end-to-end attestation test against a live arc-node.
//
// Exercises the full community-worker flow without needing a real GGUF
// model loaded: generate a fresh keypair, register against the seed,
// have the seed dispatch a job (via /inference/run), claim it, sign a
// fake-output InferenceAttestation, submit it, and verify
// /worker/earnings reflects the on-chain credit.
//
// This mutating example accepts only a numeric loopback IP. ARC_SEED may
// select another local port, but hostnames and non-loopback origins fail
// before client construction.

use arc_crypto::{Hash256, KeyPair, Signature, hash_bytes};
use arc_types::{Transaction, TxBody, TxType, transaction::InferenceAttestationBody};
use serde_json::Value;
use std::time::Duration;

#[path = "support/local_rpc.rs"]
mod local_rpc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let requested_seed =
        std::env::var("ARC_SEED").unwrap_or_else(|_| "http://127.0.0.1:9090".to_string());
    let seed = local_rpc::require_loopback_rpc(&requested_seed)?;

    println!("=== v0.7.0 end-to-end attestation test ===");
    println!("Seed: {}", seed);
    println!();

    // 1. Generate a fresh keypair so we know /worker/earnings starts at 0
    //    for this address.
    let kp = KeyPair::generate_ed25519();
    let address = kp.address();
    let address_hex = format!("0x{}", hex::encode(address.0));
    println!("Generated worker address: {}", address_hex);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    // 2. Faucet the worker so it has balance (in case bond>0 in a later
    //    release; v0.7.0 phase 1 uses bond=0 so this is precautionary).
    println!("\n[1/7] Faucet claim ...");
    let faucet_resp: Value = client
        .post(format!("{}/faucet/claim", seed))
        .json(&serde_json::json!({ "address": address_hex.trim_start_matches("0x") }))
        .send()
        .await?
        .json()
        .await?;
    println!("  ↳ {}", faucet_resp);

    // Wait for faucet to commit on-chain (a few blocks).
    tokio::time::sleep(Duration::from_secs(5)).await;

    // 3. Verify starting earnings = 0
    println!("\n[2/7] Initial /worker/earnings ...");
    let initial: Value = client
        .get(format!(
            "{}/worker/earnings/{}",
            seed,
            address_hex.trim_start_matches("0x")
        ))
        .send()
        .await?
        .json()
        .await?;
    println!("  ↳ {}", initial);
    assert_eq!(
        initial.get("total_attestations").and_then(|v| v.as_u64()),
        Some(0),
        "fresh worker must start at 0 attestations",
    );

    // 4. Register the worker against the seed's /community/register
    println!("\n[3/7] Registering worker ...");
    let reg_body = serde_json::json!({
        "worker_id": address_hex,
        "name": "v070-e2e-test",
        "capabilities": ["inference"],
        "model": "test-stub",
        "platform": "darwin-arm64",
    });
    let reg_resp: Value = client
        .post(format!("{}/community/register", seed))
        .json(&reg_body)
        .send()
        .await?
        .json()
        .await?;
    println!("  ↳ {}", reg_resp);
    assert_eq!(reg_resp.get("ok").and_then(|v| v.as_bool()), Some(true));

    // 5. Kick off /inference/run in the background; it will queue a job
    //    on the community work queue.
    println!("\n[4/7] Triggering /inference/run (background) ...");
    let seed_c = seed.clone();
    let client_c = client.clone();
    let inference_handle = tokio::spawn(async move {
        let r = client_c
            .post(format!("{}/inference/run", seed_c))
            .json(&serde_json::json!({
                "input": "v0.7.0 e2e test prompt",
                "max_tokens": 4,
            }))
            .send()
            .await;
        match r {
            Ok(resp) => resp.json::<Value>().await.ok(),
            Err(_) => None,
        }
    });

    // Give the seed a moment to enqueue.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // 6. Claim the work
    println!("\n[5/7] /community/claim_work ...");
    let claim: Value = client
        .post(format!("{}/community/claim_work", seed))
        .json(&serde_json::json!({
            "worker_id": address_hex,
            "capabilities": ["inference"],
        }))
        .send()
        .await?
        .json()
        .await?;
    println!("  ↳ {}", claim);
    let job_id = claim
        .get("job_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no job_id in claim response"))?
        .to_string();
    let input = claim
        .get("input")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    println!("  ↳ Got job_id={}, input={:?}", job_id, input);

    // 7. Build a fake-output InferenceAttestation and sign it
    println!("\n[6/7] Building + signing InferenceAttestation tx ...");
    let model_id = hash_bytes(b"v070-e2e-test-stub-model");
    let input_hash = hash_bytes(input.as_bytes());
    let output_text = "stub output";
    let output_hash = hash_bytes(output_text.as_bytes());

    // Worker nonce: query the chain
    let acct: Value = client
        .get(format!(
            "{}/account/{}",
            seed,
            address_hex.trim_start_matches("0x")
        ))
        .send()
        .await?
        .json()
        .await
        .unwrap_or(Value::Null);
    let chain_nonce = acct.get("nonce").and_then(|v| v.as_u64()).unwrap_or(0);

    let mut tx = Transaction {
        tx_type: TxType::InferenceAttestation,
        from: address,
        nonce: chain_nonce,
        body: TxBody::InferenceAttestation(InferenceAttestationBody {
            model_id,
            input_hash,
            output_hash,
            challenge_period: 100,
            bond: 0,
            beneficiary: None,
        }),
        fee: 0,
        gas_limit: 0,
        hash: Hash256::ZERO,
        signature: Signature::null(),
        sig_verified: false,
    };
    tx.sign(&kp)?;
    let bytes = bincode::serialize(&tx)?;
    let signed_hex = format!("0x{}", hex::encode(&bytes));
    println!("  ↳ tx_hash {}", hex::encode(tx.hash.0));
    println!("  ↳ from    {}", hex::encode(tx.from.0));
    println!("  ↳ nonce   {}", tx.nonce);
    println!("  ↳ {} bytes signed", bytes.len());

    // 8. Submit work with the signed attestation
    println!("\n[7/7] /community/submit_work with signed attestation ...");
    let submit: Value = client
        .post(format!("{}/community/submit_work", seed))
        .json(&serde_json::json!({
            "job_id": job_id,
            "worker_id": address_hex,
            "success": true,
            "output": output_text,
            "output_hash": format!("0x{}", hex::encode(output_hash.0)),
            "tokens_generated": 1,
            "total_ms": 50,
            "ms_per_token": 50,
            "engine": "v070-e2e-test-stub",
            "signed_attestation_hex": signed_hex,
        }))
        .send()
        .await?
        .json()
        .await?;
    println!("  ↳ {}", submit);
    let attestation_status = submit
        .get("attestation")
        .and_then(|a| a.get("status"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    if attestation_status != "submitted_to_mempool" {
        anyhow::bail!(
            "attestation NOT accepted: status={:?}, full response={}",
            attestation_status,
            submit
        );
    }
    println!("  ✓ attestation accepted: {}", attestation_status);

    // The /inference/run waiter should now return success.
    let inference_result = inference_handle.await.ok().flatten();
    println!("\n  /inference/run returned: {:?}", inference_result);

    // 9. Wait for the attestation to commit on-chain, then check earnings
    println!("\n[verification] Waiting 10s for attestation to commit ...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    let final_earnings: Value = client
        .get(format!(
            "{}/worker/earnings/{}",
            seed,
            address_hex.trim_start_matches("0x")
        ))
        .send()
        .await?
        .json()
        .await?;
    println!("Final /worker/earnings: {}", final_earnings);

    let final_count = final_earnings
        .get("total_attestations")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let final_arc = final_earnings
        .get("total_arc")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    if final_count == 0 {
        anyhow::bail!(
            "earnings still 0 after 10s — attestation tx didn't commit. Check seed mempool."
        );
    }

    println!();
    println!("=========================================");
    println!("  ✓ v0.7.0 end-to-end test PASSED");
    println!("    worker_address: {}", address_hex);
    println!("    attestations:   {}", final_count);
    println!("    total_arc:      {}", final_arc);
    println!("=========================================");

    Ok(())
}
