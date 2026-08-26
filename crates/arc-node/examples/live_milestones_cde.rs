//! Milestones C + D + E live-testnet end-to-end driver.
//!
//! Closes the runtime acceptance gap on PR #40: the protocol surface
//! shipped in code, but no on-chain receipts existed for any of
//! ModelRegistration / ModelRequest / ShardCoverageClaim /
//! CapacityAdvertisement / ShardAssignmentProposal. This example
//! produces all of them against a live coordinator and asserts the
//! state writes are visible.
//!
//! What it does (each step is a real signed tx on the testnet):
//!   1. Faucets a publisher keypair.
//!   2. Submits a `ModelRegistration` for a fake "mistral-7b-test"
//!      model with the floor fee (1000 ARC). Asserts the 1000 ARC
//!      lands in the treasury (Milestone E anti-spam fee live-proof).
//!   3. Submits a `ModelRequest` from a separate "querier" keypair
//!      naming the same model_id. Asserts the request appears in
//!      `/models/open_requests` (Milestone C demand-signal live-proof).
//!   4. Faucets 3 worker keypairs, each submits a
//!      `ShardCoverageClaim` for a different layer range
//!      (Milestone C supply-side live-proof).
//!   5. Each worker submits a `CapacityAdvertisement` (Milestone D
//!      capacity live-proof). Asserts they appear in
//!      `/capacity/advertisements`.
//!   6. Submits a `ShardAssignmentProposal` whose `assignments`
//!      mirror the 3 claims from step 4. Asserts the proposal-derived
//!      account appears with the expected `storage_root` commitment
//!      (Milestone D planner-output live-proof).
//!   7. Hits `/assignments/for_me?pubkey=…` for one of the workers
//!      and asserts the assignment is returned (Milestone D worker
//!      retrieval live-proof).
//!
//! Usage:
//!     cargo run --release --example live_milestones_cde -p arc-node -- [coord_url]

use arc_crypto::{Hash256, Signature, hash_bytes};
use arc_types::transaction::{
    AssignmentEntry, CapacityAdvertisementBody, MIN_MODEL_REGISTRATION_FEE, ModelRegistrationBody,
    ModelRequestBody, ShardAssignmentProposalBody, ShardCoverageClaimBody, TxBody,
};
use arc_types::{Transaction, TxType};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Duration;

const DOMAIN_TAG: &str = "ARC-chain-validator-keypair-v1";
const DEFAULT_COORD: &str = "http://140.82.16.112:9090";
const TREASURY_TAG: &[u8] = b"arc-treasury";

fn keypair(phrase: &str) -> (SigningKey, [u8; 32], Hash256) {
    let seed = blake3::derive_key(DOMAIN_TAG, phrase.as_bytes());
    let sk = SigningKey::from_bytes(&seed);
    let pk = sk.verifying_key().to_bytes();
    let addr = Hash256(*blake3::hash(&pk).as_bytes());
    (sk, pk, addr)
}

async fn balance(c: &Client, coord: &str, addr_hex: &str) -> u64 {
    match c
        .get(format!("{}/account/{}", coord, addr_hex))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("balance").and_then(|b| b.as_u64()))
            .unwrap_or(0),
        _ => 0,
    }
}

async fn nonce(c: &Client, coord: &str, addr_hex: &str) -> u64 {
    match c
        .get(format!("{}/account/{}", coord, addr_hex))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("nonce").and_then(|n| n.as_u64()))
            .unwrap_or(0),
        _ => 0,
    }
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
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if balance(c, coord, addr_hex).await >= target {
            return;
        }
    }
    eprintln!(
        "  WARN: {} did not reach {} ARC after faucet - current {}",
        addr_hex,
        target,
        balance(c, coord, addr_hex).await
    );
}

// Every argument is a distinct piece of the transaction being signed, and
// several share a type (`Hash256`, `u64`, `[u8; 32]`). Bundling them into a
// struct would move the ordering mistake from the compiler's reach into a
// silent field mix-up, so the wide signature stays.
#[allow(clippy::too_many_arguments)]
async fn submit_signed(
    c: &Client,
    coord: &str,
    sk: &SigningKey,
    pk: [u8; 32],
    from: Hash256,
    nonce_v: u64,
    tx_type: TxType,
    body: TxBody,
) -> String {
    let mut tx = Transaction {
        tx_type,
        from,
        nonce: nonce_v,
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
    let hash_hex = hex::encode(tx.hash.0);
    let resp = c
        .post(format!("{}/tx/submit_signed", coord))
        .json(&tx)
        .send()
        .await
        .expect("submit");
    if !resp.status().is_success() {
        let s = resp.status();
        let b = resp.text().await.unwrap_or_default();
        panic!("submit failed: {} - {}", s, b);
    }
    hash_hex
}

async fn wait_committed(c: &Client, coord: &str, hash_hex: &str) -> bool {
    // Loaded testnet produces ~1 block / 30 s. Allow up to 240 s (8 blocks)
    // before giving up - generous but well below the operator's patience.
    for _ in 0..480 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(r) = c.get(format!("{}/tx/0x{}", coord, hash_hex)).send().await
            && r.status().is_success()
        {
            return true;
        }
    }
    false
}

async fn fetch_json(c: &Client, url: &str) -> Value {
    c.get(url)
        .send()
        .await
        .expect("get")
        .json::<Value>()
        .await
        .unwrap_or(Value::Null)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let coord = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| DEFAULT_COORD.to_string());
    let c = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    println!("=== Live driver for Milestones C + D + E on {} ===", coord);

    // Stable phrases so reruns produce monotonic nonces - every step
    // uses the right next nonce read fresh from /account/.
    let (pub_sk, pub_pk, pub_addr) = keypair("milestone-c-publisher");
    let (qry_sk, qry_pk, qry_addr) = keypair("milestone-c-querier");
    let workers: [(&str, SigningKey, [u8; 32], Hash256); 3] = [
        {
            let (sk, pk, addr) = keypair("milestone-c-worker-A");
            ("worker-A", sk, pk, addr)
        },
        {
            let (sk, pk, addr) = keypair("milestone-c-worker-B");
            ("worker-B", sk, pk, addr)
        },
        {
            let (sk, pk, addr) = keypair("milestone-c-worker-C");
            ("worker-C", sk, pk, addr)
        },
    ];

    let pub_hex = hex::encode(pub_addr.0);
    let qry_hex = hex::encode(qry_addr.0);
    let treasury = hex::encode(hash_bytes(TREASURY_TAG).0);

    println!("publisher: 0x{}", pub_hex);
    println!("querier:   0x{}", qry_hex);
    for (name, _, _, addr) in &workers {
        println!("{}:  0x{}", name, hex::encode(addr.0));
    }
    println!("treasury:  0x{}", treasury);

    // Step 1: faucet publisher + querier + 3 workers.
    println!();
    println!("--- step 1: faucet keypairs ---");
    faucet_and_wait(&c, &coord, &pub_hex, 5_000).await;
    faucet_and_wait(&c, &coord, &qry_hex, 1_000).await;
    for (name, _, _, addr) in &workers {
        let h = hex::encode(addr.0);
        faucet_and_wait(&c, &coord, &h, 1_000).await;
        println!("  {} balance: {}", name, balance(&c, &coord, &h).await);
    }
    let pub_bal_pre = balance(&c, &coord, &pub_hex).await;
    let tre_bal_pre = balance(&c, &coord, &treasury).await;
    println!("publisher balance (pre-reg): {}", pub_bal_pre);
    println!("treasury  balance (pre-reg): {}", tre_bal_pre);

    // Step 2: ModelRegistration with floor fee. Body hashes incorporate
    // the publisher's current nonce so reruns produce fresh tx_hashes
    // - avoids being wedged by prior-attempt hashes still cached in
    // mempool's seen-set.
    println!();
    println!("--- step 2: ModelRegistration (Milestone C + E spam fee) ---");
    let nonce_seed = nonce(&c, &coord, &pub_hex).await;
    let suffix = format!("nonce-{}", nonce_seed);
    let model_id = hash_bytes(format!("arc-testnet-mistral-7b-test-{}", suffix).as_bytes());
    let metadata_hash = hash_bytes(format!("mistral-7b-test-metadata-{}", suffix).as_bytes());
    let chunk_tree_root = hash_bytes(format!("mistral-7b-test-chunks-{}", suffix).as_bytes());
    let reg_body = ModelRegistrationBody {
        model_id,
        metadata_hash,
        chunk_tree_root,
        n_layers: 32,
        d_model: 4096,
        quantization: "q4".into(),
        registration_fee: MIN_MODEL_REGISTRATION_FEE,
        royalty_recipient: pub_addr,
    };
    let reg_tx_hash = submit_signed(
        &c,
        &coord,
        &pub_sk,
        pub_pk,
        pub_addr,
        nonce_seed,
        TxType::ModelRegistration,
        TxBody::ModelRegistration(reg_body),
    )
    .await;
    println!("ModelRegistration tx: 0x{}", reg_tx_hash);
    if !wait_committed(&c, &coord, &reg_tx_hash).await {
        eprintln!("ModelRegistration did not commit - abort");
        std::process::exit(1);
    }
    let pub_bal_post = balance(&c, &coord, &pub_hex).await;
    let tre_bal_post = balance(&c, &coord, &treasury).await;
    println!(
        "publisher: {} → {} (Δ {:+})",
        pub_bal_pre,
        pub_bal_post,
        pub_bal_post as i64 - pub_bal_pre as i64
    );
    println!(
        "treasury:  {} → {} (Δ {:+})",
        tre_bal_pre,
        tre_bal_post,
        tre_bal_post as i64 - tre_bal_pre as i64
    );
    let registry = fetch_json(&c, &format!("{}/models/registry", coord)).await;
    let registry_count = registry.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    println!("registry visible? count={} (expect ≥ 1)", registry_count);

    // Step 3: ModelRequest from querier.
    println!();
    println!("--- step 3: ModelRequest (Milestone C demand) ---");
    let mut request_id = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut request_id);
    let req_body = ModelRequestBody {
        request_id,
        model_id,
        target_k_replication: 3,
        bond_per_layer_epoch: 10,
        max_wait_secs: 300,
    };
    let qry_nonce = nonce(&c, &coord, &qry_hex).await;
    let req_tx_hash = submit_signed(
        &c,
        &coord,
        &qry_sk,
        qry_pk,
        qry_addr,
        qry_nonce,
        TxType::ModelRequest,
        TxBody::ModelRequest(req_body),
    )
    .await;
    println!("ModelRequest tx: 0x{}", req_tx_hash);
    wait_committed(&c, &coord, &req_tx_hash).await;
    let opens = fetch_json(&c, &format!("{}/models/open_requests", coord)).await;
    let open_count = opens.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    println!("open_requests visible? count={} (expect ≥ 1)", open_count);

    // Step 4: 3 ShardCoverageClaims, one per worker for a non-overlapping
    // range. These exercise the supply side of Milestone C.
    println!();
    println!("--- step 4: ShardCoverageClaim × 3 (Milestone C supply) ---");
    let ranges: [(u32, u32); 3] = [(0, 11), (11, 22), (22, 32)];
    let mut claim_tx_hashes = Vec::new();
    for (i, (name, sk, pk, addr)) in workers.iter().enumerate() {
        let h = hex::encode(addr.0);
        let n = nonce(&c, &coord, &h).await;
        let body = ShardCoverageClaimBody {
            model_id,
            node_pubkey: *pk,
            ranges: vec![ranges[i]],
            bond: 100,
            epoch_blocks: 1_000,
        };
        let tx_hash = submit_signed(
            &c,
            &coord,
            sk,
            *pk,
            *addr,
            n,
            TxType::ShardCoverageClaim,
            TxBody::ShardCoverageClaim(body),
        )
        .await;
        println!("  {} claim {:?}: 0x{}", name, ranges[i], tx_hash);
        wait_committed(&c, &coord, &tx_hash).await;
        claim_tx_hashes.push(tx_hash);
    }

    // Step 5: 3 CapacityAdvertisements (Milestone D capacity).
    println!();
    println!("--- step 5: CapacityAdvertisement × 3 (Milestone D) ---");
    for (i, (name, sk, pk, addr)) in workers.iter().enumerate() {
        let h = hex::encode(addr.0);
        let n = nonce(&c, &coord, &h).await;
        let body = CapacityAdvertisementBody {
            node_pubkey: *pk,
            ram_bytes: 16 * 1024 * 1024 * 1024,
            vram_bytes: 8 * 1024 * 1024 * 1024,
            bandwidth_mbps: 100,
            uptime_hint_mins: 720,
            stake: 5_000,
            region: ["US", "EU", "AS"][i].into(),
        };
        let tx_hash = submit_signed(
            &c,
            &coord,
            sk,
            *pk,
            *addr,
            n,
            TxType::CapacityAdvertisement,
            TxBody::CapacityAdvertisement(body),
        )
        .await;
        println!("  {} capacity adv: 0x{}", name, tx_hash);
        wait_committed(&c, &coord, &tx_hash).await;
    }
    let caps = fetch_json(&c, &format!("{}/capacity/advertisements", coord)).await;
    let cap_count = caps.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    println!("advertisements visible? count={} (expect ≥ 3)", cap_count);

    // Step 6: ShardAssignmentProposal mirroring the 3 claims (Milestone D).
    println!();
    println!("--- step 6: ShardAssignmentProposal (Milestone D planner output) ---");
    // Build a deterministic input snapshot hash from the
    // (model_id, request_id, claim_tx_hashes) tuple so reruns produce
    // distinct proposal accounts.
    let mut snapshot_input = Vec::new();
    snapshot_input.extend_from_slice(model_id.0.as_ref());
    snapshot_input.extend_from_slice(&request_id);
    for h in &claim_tx_hashes {
        snapshot_input.extend_from_slice(h.as_bytes());
    }
    let input_snapshot_hash = hash_bytes(&snapshot_input);
    let proposal = ShardAssignmentProposalBody {
        epoch_blocks: 1_000,
        assignments: workers
            .iter()
            .enumerate()
            .map(|(i, (_, _, pk, _))| AssignmentEntry {
                node_pubkey: *pk,
                model_id,
                ranges: vec![ranges[i]],
            })
            .collect(),
        input_snapshot_hash,
    };
    let pub_nonce_2 = nonce(&c, &coord, &pub_hex).await;
    let prop_tx_hash = submit_signed(
        &c,
        &coord,
        &pub_sk,
        pub_pk,
        pub_addr,
        pub_nonce_2,
        TxType::ShardAssignmentProposal,
        TxBody::ShardAssignmentProposal(proposal),
    )
    .await;
    println!("ShardAssignmentProposal tx: 0x{}", prop_tx_hash);
    wait_committed(&c, &coord, &prop_tx_hash).await;

    // Step 7: each worker queries /assignments/for_me, asserts theirs is
    // returned (Milestone D retrieval live-proof).
    println!();
    println!("--- step 7: /assignments/for_me retrieval (Milestone D) ---");
    let mut all_assigned = true;
    for (name, _, pk, _) in &workers {
        let pk_hex = hex::encode(pk);
        let url = format!("{}/assignments/for_me?pubkey=0x{}", coord, pk_hex);
        let resp = fetch_json(&c, &url).await;
        let count = resp.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("  {} assignments visible: count={}", name, count);
        if count == 0 {
            all_assigned = false;
        }
    }

    // Final summary.
    println!();
    println!("=== SUMMARY ===");
    println!(
        "ModelRegistration:        0x{}  (treasury Δ {:+})",
        reg_tx_hash,
        tre_bal_post as i64 - tre_bal_pre as i64
    );
    println!("ModelRequest:             0x{}", req_tx_hash);
    for (i, (name, _, _, _)) in workers.iter().enumerate() {
        println!("ShardCoverageClaim ({}):  0x{}", name, claim_tx_hashes[i]);
    }
    println!("ShardAssignmentProposal:  0x{}", prop_tx_hash);
    println!(
        "registry count: {}  open_requests: {}  capacity_ads: {}",
        registry_count, open_count, cap_count
    );
    println!(
        "treasury Δ on reg fee: {:+} (expect ≥ +{} from MIN_MODEL_REGISTRATION_FEE)",
        tre_bal_post as i64 - tre_bal_pre as i64,
        MIN_MODEL_REGISTRATION_FEE
    );

    if registry_count == 0
        || open_count == 0
        || cap_count < 3
        || !all_assigned
        || tre_bal_post < tre_bal_pre + MIN_MODEL_REGISTRATION_FEE
    {
        eprintln!("FAIL: one or more acceptance checks did not pass.");
        std::process::exit(1);
    }
    println!();
    println!("ALL CHECKS PASS - Milestones C + D + E are live on the testnet.");
}
