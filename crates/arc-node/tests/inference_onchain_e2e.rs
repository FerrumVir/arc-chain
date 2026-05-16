//! End-to-end integration test for Tier 1 on-chain inference.
//!
//! Pipeline exercised:
//!   1. apply `InferenceRequest` → escrow locks max_reward, status=Open
//!   2. spawn `InferenceValidatorTask` (1 validator = entire committee)
//!   3. task tick → derives committee, runs (stub) inference, submits vote
//!   4. apply vote → status=Voting → ReadyToFinalize
//!   5. task tick → submits finalize
//!   6. apply finalize → status=Finalized, reward paid out 70/20/10
//!
//! Uses the stub inference path (no candle engine loaded) so the test is
//! fast (no GGUF load) and deterministic. The candle path is exercised
//! manually with a real model in Phase B deployment.

use std::sync::Arc;

use arc_crypto::{hash_bytes, Hash256, KeyPair};
use arc_mempool::Mempool;
use arc_node::inference_validator::InferenceValidatorTask;
use arc_state::{
    StateDB, TIER1_STATUS_FINALIZED, TIER1_STATUS_OPEN, TIER1_STATUS_VOTING,
};
use arc_types::transaction::{
    InferenceRequestBody, Transaction, TxBody, TxType,
};
use arc_types::Address;

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
        model_id: hash_bytes(b"arc-32L-test"),
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
async fn tier1_full_flow_single_validator() {
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

    // ── 3. Spawn the validator task. Stub inference (no candle) is fine. ──
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

    // Call tick() directly (bypassing the 500ms loop) so the test is fast.
    // First tick: should detect the request, run stub inference, submit vote.
    // We can't call private tick(), so use the public run() in a brief spawn.
    let tick_task = task.clone();
    let handle = tokio::spawn(async move {
        InferenceValidatorTask::run_arc(tick_task).await
    });

    // Wait until the mempool sees the vote tx (with a generous timeout to
    // tolerate runtime scheduler variance). 500ms tick + spawn detached vote
    // task means total budget under 3 seconds is comfortable.
    let vote_tx = wait_for_mempool_tx(
        &mempool,
        |tx| matches!(tx.tx_type, TxType::InferenceVote),
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("vote tx must land in mempool within timeout");

    // ── 4. Apply the vote tx to advance escrow status to Voting ──
    let (_, vote_receipts) = state.execute_block(&[vote_tx], validator).unwrap();
    assert!(vote_receipts[0].success, "vote must apply successfully");
    let escrow_after_vote = state.get_account(&escrow_addr).unwrap();
    assert_eq!(escrow_after_vote.code_hash.0[0], TIER1_STATUS_VOTING);

    // ── 5. Wait for the validator task to submit finalize ──
    let finalize_tx = wait_for_mempool_tx(
        &mempool,
        |tx| matches!(tx.tx_type, TxType::InferenceFinalize),
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("finalize tx must land in mempool within timeout");

    handle.abort();

    // ── 6. Apply finalize and verify payout + state ──
    let (_, final_receipts) =
        state.execute_block(&[finalize_tx], validator).unwrap();
    assert!(
        final_receipts[0].success,
        "finalize must apply successfully"
    );
    let escrow_final = state.get_account(&escrow_addr).unwrap();
    assert_eq!(escrow_final.balance, 0, "escrow must be drained");
    assert_eq!(escrow_final.code_hash.0[0], TIER1_STATUS_FINALIZED);

    // Validator earnings:
    //   - started 1_000_000
    //   - paid 100 in the request → 999_900
    //   - earned voters share = 70 (100 * 70%)
    //   - earned requester rebate = 20 (100 * 20%)
    //   - total = 999_990
    let final_acct = state.get_account(&validator).unwrap();
    assert_eq!(final_acct.balance, 999_990);

    let treasury =
        state.get_account(&arc_types::transaction::faucet_pool_address()).unwrap();
    assert_eq!(treasury.balance, 10, "treasury must receive 10% cut");

    // Pending index must be cleared.
    assert!(state.tier1_pending_requests().is_empty());
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
