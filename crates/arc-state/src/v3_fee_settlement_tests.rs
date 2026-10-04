//! Fixtures for protocol-v3 block-level fee settlement.
//!
//! `v3_launch_rule_goldens` pins the block hashes, state roots, treasury
//! effects and exact state-WAL operation stream of a short recovered-v3
//! history executed under the launch rule, where every transfer credits the
//! shared fee treasury itself. The pinned values were recorded by CI on a
//! commit whose execution and admission code was unmodified `main`
//! (4070114c). Any later change must reproduce them byte for byte whenever
//! block-level fee settlement is not active.
//!
//! `reference_greedy_selection` is a verbatim copy of the quadratic greedy
//! loop the consensus proposal and commit paths used on that commit. It is the
//! oracle for the incremental admission builder that replaces it.

use super::*;
use arc_crypto::KeyPair;

const FIXTURE_CHAIN_ID: &str = "0x415243";
const FIXTURE_GENESIS: &[u8] = b"arc-v3-fee-settlement-fixture-genesis";
const FIXTURE_TIMESTAMP_MS: u64 = 1_790_000_000_000;

/// Launch-rule transcript recorded on unmodified `main` logic. See the module
/// documentation; never edit these lines by hand to make a change pass.
const V3_LAUNCH_RULE_GOLDENS: &[&str] = &[];

fn fixture_key(label: &str, index: u64) -> KeyPair {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-v3-fee-settlement-fixture-key-v1");
    hasher.update(label.as_bytes());
    hasher.update(&index.to_le_bytes());
    KeyPair::from_ed25519_secret_bytes(hasher.finalize().as_bytes())
}

fn fixture_address(label: &str, index: u64) -> Address {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-v3-fee-settlement-fixture-address-v1");
    hasher.update(label.as_bytes());
    hasher.update(&index.to_le_bytes());
    Hash256(*hasher.finalize().as_bytes())
}

fn fixture_dir(name: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "arc-state-v3-fees-{name}-{}-{unique}",
        std::process::id()
    ))
}

/// Bind a state to the fixed fixture recovery domain, which makes it execute
/// and admit under protocol v3 exactly as a recovered chain does.
fn bind_fixture_recovery_domain(state: &StateDB) {
    *state.recovery_context.write() = Some(recovery::RecoveryContext::new(
        FIXTURE_CHAIN_ID,
        hash_bytes(FIXTURE_GENESIS),
        1,
        1,
    ));
}

fn v3_fixture_state(prefunded: &[(Address, u64)]) -> StateDB {
    let state = StateDB::with_genesis(prefunded);
    bind_fixture_recovery_domain(&state);
    state
}

fn signed_transfer(
    state: &StateDB,
    from: &KeyPair,
    to: Address,
    amount: u64,
    fee: u64,
    nonce: u64,
) -> Transaction {
    let mut transaction = Transaction::new_transfer(from.address(), to, amount, nonce);
    transaction.fee = fee;
    state
        .sign_transaction(&mut transaction, from)
        .expect("fixture transfer signs in the recovery domain");
    transaction
}

fn fixture_decision(height: u64) -> Hash256 {
    hash_bytes(format!("arc-v3-fee-settlement-fixture-decision-{height}").as_bytes())
}

/// The canonical consensus commit path with a deterministic timestamp.
fn commit_adaptive(
    state: &StateDB,
    transactions: &[Transaction],
    producer: Address,
) -> (Block, Vec<TxReceipt>) {
    let height = state.height() + 1;
    state
        .execute_block_adaptive_at_with_proof(
            transactions,
            producer,
            FIXTURE_TIMESTAMP_MS + height,
            fixture_decision(height),
        )
        .expect("fixture block executes")
}

fn commit_verified(
    state: &StateDB,
    transactions: &[Transaction],
    producer: Address,
) -> (Block, Vec<TxReceipt>) {
    let height = state.height() + 1;
    state
        .execute_block_verified_at(transactions, producer, FIXTURE_TIMESTAMP_MS + height)
        .expect("fixture block executes")
}

fn record_block(transcript: &mut Vec<String>, block: &Block, receipts: &[TxReceipt]) {
    let outcomes: String = receipts
        .iter()
        .map(|receipt| if receipt.success { '1' } else { '0' })
        .collect();
    transcript.push(format!(
        "h{} block={} root={} txs={} ok={outcomes}",
        block.header.height,
        block.hash.to_hex(),
        block.header.state_root.to_hex(),
        block.header.tx_count,
    ));
}

/// Digest of every state-WAL operation (with its block height) in order.
fn wal_digest(directory: &Path) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-v3-fee-settlement-fixture-wal-v1");
    for entry in read_wal(directory.join("state.wal")) {
        hasher.update(&entry.block_height.to_le_bytes());
        hasher.update(&bincode::serialize(&entry.op).expect("WAL operation serializes"));
    }
    hasher.finalize().to_hex().to_string()
}

/// Reference copy of the pre-builder greedy v3 selection in
/// `arc-node/src/consensus.rs` (proposal loop 4137-4155, commit loop
/// 4550-4568 on main 4070114c). A complete valid candidate is kept as is;
/// otherwise every transaction is retried against the admitted prefix and
/// either admitted or (proposal) deferred / (commit) omitted. Quadratic in
/// the candidate count, including signature verification: tests only.
fn reference_greedy_selection(
    state: &StateDB,
    candidates: &[Transaction],
) -> (Vec<Transaction>, Vec<Transaction>) {
    if state.validate_v3_block_admission(candidates).is_ok() {
        return (candidates.to_vec(), Vec::new());
    }
    let mut admitted: Vec<Transaction> = Vec::with_capacity(candidates.len());
    let mut deferred = Vec::new();
    for transaction in candidates {
        let mut candidate = admitted.clone();
        candidate.push(transaction.clone());
        if state.validate_v3_block_admission(&candidate).is_ok() {
            admitted.push(transaction.clone());
        } else {
            deferred.push(transaction.clone());
        }
    }
    (admitted, deferred)
}

fn render_transcript(transcript: &[String]) -> String {
    transcript
        .iter()
        .map(|line| format!("    \"{line}\","))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_matches_goldens(transcript: &[String], goldens: &[&str]) {
    let rendered = render_transcript(transcript);
    assert!(
        !goldens.is_empty(),
        "record these v3 launch-rule goldens from unmodified main logic:\n{rendered}"
    );
    assert_eq!(
        transcript, goldens,
        "the v3 launch-rule history changed; actual transcript:\n{rendered}"
    );
}

/// Execute the launch-rule fixture history on a fresh persistent recovered-v3
/// state. `configure` runs after the recovery domain is bound and before the
/// first block.
fn launch_rule_transcript(configure: impl FnOnce(&StateDB)) -> Vec<String> {
    let directory = fixture_dir("launch-rule");
    let senders: Vec<KeyPair> = (0..4)
        .map(|index| fixture_key("launch-sender", index))
        .collect();
    let sender_addresses: Vec<Address> = senders.iter().map(KeyPair::address).collect();
    let existing = fixture_address("launch-existing", 0);
    let mut prefunded: Vec<(Address, u64)> = sender_addresses
        .iter()
        .map(|address| (*address, 1_000_000))
        .collect();
    prefunded.push((existing, 5));
    let state =
        StateDB::with_genesis_persistent(&prefunded, &directory, hash_bytes(FIXTURE_GENESIS))
            .expect("fixture state opens");
    bind_fixture_recovery_domain(&state);
    configure(&state);
    let producer = fixture_key("launch-producer", 0).address();
    let recipient = |index| fixture_address("launch-recipient", index);
    let mut transcript = Vec::new();

    let first = signed_transfer(&state, &senders[0], recipient(0), 100, 1, 0);
    let (block, receipts) = commit_adaptive(&state, std::slice::from_ref(&first), producer);
    record_block(&mut transcript, &block, &receipts);

    let (block, receipts) = commit_adaptive(&state, &[], producer);
    record_block(&mut transcript, &block, &receipts);

    let to_existing = signed_transfer(&state, &senders[1], existing, 250, 7, 0);
    let (block, receipts) = commit_verified(&state, std::slice::from_ref(&to_existing), producer);
    record_block(&mut transcript, &block, &receipts);

    let second_from_first = signed_transfer(&state, &senders[0], recipient(1), 33, 100, 1);
    let (block, receipts) =
        commit_adaptive(&state, std::slice::from_ref(&second_from_first), producer);
    record_block(&mut transcript, &block, &receipts);

    // Disjoint transfers still share the fee treasury under the launch rule,
    // and a shared recipient conflicts under every rule. Neither candidate
    // block may mutate anything.
    let disjoint = signed_transfer(&state, &senders[2], recipient(2), 1, 1, 0);
    let other_disjoint = signed_transfer(&state, &senders[3], recipient(3), 1, 1, 0);
    let shared_recipient = signed_transfer(&state, &senders[3], recipient(2), 1, 1, 0);
    state.wal.sync().expect("fixture WAL syncs");
    let before = (
        state.height(),
        state.get_state_root(),
        std::fs::metadata(directory.join("state.wal"))
            .expect("fixture WAL exists")
            .len(),
    );
    for candidate in [
        vec![disjoint.clone(), other_disjoint],
        vec![disjoint.clone(), shared_recipient],
    ] {
        assert!(state.validate_v3_block_admission(&candidate).is_err());
        let height = state.height() + 1;
        assert!(
            state
                .execute_block_adaptive_at_with_proof(
                    &candidate,
                    producer,
                    FIXTURE_TIMESTAMP_MS + height,
                    fixture_decision(height),
                )
                .is_err()
        );
        state.wal.sync().expect("fixture WAL syncs");
        assert_eq!(
            (
                state.height(),
                state.get_state_root(),
                std::fs::metadata(directory.join("state.wal"))
                    .expect("fixture WAL exists")
                    .len(),
            ),
            before,
            "a refused v3 block left a trace"
        );
    }

    let (block, receipts) = commit_adaptive(&state, std::slice::from_ref(&disjoint), producer);
    record_block(&mut transcript, &block, &receipts);

    let (block, receipts) = commit_verified(&state, &[], producer);
    record_block(&mut transcript, &block, &receipts);

    let to_sender = signed_transfer(&state, &senders[3], sender_addresses[0], 500, 2, 0);
    let (block, receipts) = commit_adaptive(&state, std::slice::from_ref(&to_sender), producer);
    record_block(&mut transcript, &block, &receipts);

    let treasury = v3_fee_treasury_address();
    transcript.push(format!(
        "treasury balance={} history={}",
        state
            .get_account(&treasury)
            .map(|account| account.balance)
            .unwrap_or_default(),
        state.get_account_txs(&treasury.0).len(),
    ));
    for (index, address) in sender_addresses.iter().enumerate() {
        let account = state.get_account(address).expect("fixture sender exists");
        transcript.push(format!(
            "sender{index} balance={} nonce={}",
            account.balance, account.nonce
        ));
    }

    // The consensus selection over one hash-ordered candidate batch: a shared
    // recipient, a second spend by one sender, a below-minimum fee and an
    // exact duplicate next to ordinary transfers.
    let duplicated = signed_transfer(&state, &senders[1], recipient(11), 10, 3, 1);
    let mut candidates = vec![
        signed_transfer(&state, &senders[0], recipient(10), 10, 1, 2),
        duplicated.clone(),
        signed_transfer(&state, &senders[2], recipient(10), 10, 1, 1),
        signed_transfer(&state, &senders[0], recipient(12), 10, 1, 2),
        signed_transfer(&state, &senders[3], recipient(13), 10, 0, 1),
        duplicated,
        signed_transfer(&state, &senders[3], recipient(14), 10, 4, 1),
    ];
    candidates.sort_by_key(|transaction| transaction.hash.0);
    let (admitted, deferred) = reference_greedy_selection(&state, &candidates);
    assert_eq!(admitted.len(), 1, "the launch rule admits one transfer");
    for transaction in &admitted {
        transcript.push(format!("select {}", transaction.hash.to_hex()));
    }
    for transaction in &deferred {
        transcript.push(format!("defer {}", transaction.hash.to_hex()));
    }

    state.wal.sync().expect("fixture WAL syncs");
    drop(state);
    transcript.push(format!("wal={}", wal_digest(&directory)));
    std::fs::remove_dir_all(&directory).expect("fixture directory is removable");
    transcript
}

#[test]
fn v3_launch_rule_goldens() {
    let transcript = launch_rule_transcript(|_| {});
    assert_matches_goldens(&transcript, V3_LAUNCH_RULE_GOLDENS);
}

#[test]
fn reference_greedy_selection_keeps_one_launch_rule_transfer_per_block() {
    let senders: Vec<KeyPair> = (0..3)
        .map(|index| fixture_key("reference-sender", index))
        .collect();
    let state = v3_fixture_state(
        &senders
            .iter()
            .map(|key| (key.address(), 1_000))
            .collect::<Vec<_>>(),
    );
    let mut candidates: Vec<Transaction> = senders
        .iter()
        .enumerate()
        .map(|(index, key)| {
            signed_transfer(
                &state,
                key,
                fixture_address("reference-recipient", index as u64),
                5,
                1,
                0,
            )
        })
        .collect();
    candidates.sort_by_key(|transaction| transaction.hash.0);
    let (admitted, deferred) = reference_greedy_selection(&state, &candidates);
    assert_eq!(admitted.len(), 1, "the fee treasury admits one transfer");
    assert_eq!(admitted[0].hash, candidates[0].hash, "first in hash order");
    assert_eq!(deferred.len(), candidates.len() - 1);
    state
        .validate_v3_block_admission(&admitted)
        .expect("the reference selection is a valid block");
}
