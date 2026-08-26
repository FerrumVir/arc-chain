//! End-to-end safety test for Tier 1 on-chain inference identity.
//!
//! Pipeline exercised:
//!   1. apply `InferenceRequest` → escrow locks max_reward, status=Open
//!   2. spawn `InferenceValidatorTask` without a committed model artifact
//!   3. task derives committee membership but abstains instead of fabricating
//!      a deterministic stub vote for an artifact it did not execute
//!
//! Real-model execution is covered by the Candle path with an operator-provided
//! artifact; CI intentionally does not download a multi-gigabyte GGUF.

use std::sync::Arc;

use arc_crypto::{Hash256, KeyPair, hash_bytes};
use arc_mempool::Mempool;
use arc_node::inference_validator::InferenceValidatorTask;
use arc_state::{StateDB, TIER1_STATUS_OPEN};
use arc_types::Address;
use arc_types::transaction::{InferenceRequestBody, Transaction, TxBody, TxType};

fn build_request_tx(
    from: Address,
    nonce: u64,
    request_id: [u8; 32],
    max_reward: u64,
    committee_size: u8,
    deadline_blocks: u64,
) -> Transaction {
    let input_blob = b"[INST] hello [/INST]".to_vec();
    let input_hash = hash_bytes(&input_blob);
    let body = TxBody::InferenceRequest(InferenceRequestBody {
        request_id,
        model_id: hash_bytes(b"exact-synthetic-model-artifact-bytes"),
        input_hash,
        input_blob,
        max_tokens: 32,
        tier: 1,
        max_reward,
        deadline_blocks,
        committee_size,
    });
    let mut tx = Transaction {
        tx_type: TxType::InferenceRequest,
        from,
        nonce,
        body,
        fee: 0,
        gas_limit: 0,
        hash: Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: true,
    };
    tx.hash = tx.compute_hash();
    tx
}

#[tokio::test]
async fn tier1_without_an_exact_loaded_artifact_abstains() {
    // ── 1. Set up state with one validator who is also the requester ──
    let kp = KeyPair::generate_ed25519();
    let validator = kp.address();
    let state = Arc::new(StateDB::with_genesis(&[(validator, 1_000_000)]));
    state.seed_genesis_validators(&[(validator, StateDB::MIN_VALIDATOR_STAKE)]);

    let mempool = Arc::new(Mempool::new(1024));

    // ── 2. Apply the InferenceRequest tx directly via execute_block ──
    let req_id = [42u8; 32];
    let tx = build_request_tx(validator, 0, req_id, 100, 1, 20);
    let (_, receipts) = state.execute_block(&[tx], validator).unwrap();
    assert!(receipts[0].success, "request must apply");
    let escrow_addr = hash_bytes(&[b"arc-infreq", req_id.as_ref()].concat());
    let escrow = state.get_account(&escrow_addr).unwrap();
    assert_eq!(escrow.code_hash.0[0], TIER1_STATUS_OPEN);

    // ── 3. Spawn without an engine/commitment. This must never vote. ──
    let task = InferenceValidatorTask::new(
        state.clone(),
        mempool.clone(),
        validator,
        kp.clone(),
        None, // no candle engine
        None, // no tokenizer
        None, // no model_id
    );
    let task = Arc::new(task);

    let tick_task = task.clone();
    let handle = tokio::spawn(async move { InferenceValidatorTask::run_arc(tick_task).await });

    // Allow several 500ms ticks. A legacy implementation inserted a synthetic
    // InferenceVote here despite having no model bytes.
    let vote_tx = wait_for_mempool_tx(
        &mempool,
        |tx| matches!(tx.tx_type, TxType::InferenceVote),
        std::time::Duration::from_secs(2),
    )
    .await;
    handle.abort();
    assert!(vote_tx.is_none(), "model-less validator must abstain");
    let escrow_after_ticks = state.get_account(&escrow_addr).unwrap();
    assert_eq!(escrow_after_ticks.code_hash.0[0], TIER1_STATUS_OPEN);
    assert_eq!(escrow_after_ticks.balance, 100);
}

/// Drain the mempool until a tx matching `predicate` is observed or the
/// deadline elapses. Returns the matched tx; other txs are silently re-inserted.
async fn wait_for_mempool_tx(
    mempool: &Arc<Mempool>,
    predicate: impl Fn(&Transaction) -> bool,
    timeout: std::time::Duration,
) -> Option<Transaction> {
    let start = std::time::Instant::now();
    let mut others: Vec<Transaction> = Vec::new();
    let mut found: Option<Transaction> = None;
    'outer: while start.elapsed() <= timeout {
        let drained = mempool.drain(64);
        for tx in drained {
            if predicate(&tx) {
                found = Some(tx);
                break 'outer;
            } else {
                others.push(tx);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Restore non-matching txs so the caller can keep looking for more.
    for o in others.drain(..) {
        let _ = mempool.insert(o);
    }
    found
}
