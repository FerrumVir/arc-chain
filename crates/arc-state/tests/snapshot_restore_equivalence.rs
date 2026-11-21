use arc_crypto::{Hash256, hash_bytes};
use arc_state::StateDB;
use arc_state::snapshot::{self, SnapshotError};
use arc_types::Address;

fn addr(n: u8) -> Address {
    hash_bytes(&[n])
}

fn prefunded() -> Vec<(Address, u64)> {
    vec![(addr(1), 1_000_000), (addr(2), 500_000)]
}

fn open(dir: &std::path::Path) -> StateDB {
    StateDB::with_genesis_persistent(&prefunded(), dir, Hash256::ZERO).expect("persistent state")
}

fn advance(state: &StateDB, count: u64) {
    for i in 0..count {
        state
            .execute_block_adaptive_at_with_proof(
                &[],
                addr(1),
                1_700_000_000_000 + i,
                hash_bytes(&i.to_le_bytes()),
            )
            .expect("canonical execution");
    }
}

type ObservedChain = Vec<(u64, Hash256, Hash256)>;
type ObservableState = (u64, Hash256, ObservedChain, Vec<u64>);

fn observable(state: &StateDB) -> ObservableState {
    let height = state.height();
    let chain = (0..=height)
        .filter_map(|h| {
            state
                .get_block(h)
                .map(|b| (h, b.hash, b.header.parent_hash))
        })
        .collect();
    let balances = prefunded()
        .iter()
        .map(|(a, _)| state.get_account(a).map(|acct| acct.balance).unwrap_or(0))
        .collect();
    (height, state.get_state_root(), chain, balances)
}

#[test]
fn a_snapshot_plus_the_tail_equals_a_full_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");

    let state = open(&path);
    advance(&state, 12);
    let manifest = state
        .publish_durable_snapshot()
        .expect("snapshot published");
    assert_eq!(manifest.identity.height, state.height());
    advance(&state, 9);
    let expected = observable(&state);
    drop(state);

    let with_snapshot = open(&path);
    let from_snapshot = observable(&with_snapshot);
    drop(with_snapshot);

    snapshot::remove(&path).expect("snapshot removed");
    let full_replay = open(&path);
    let from_replay = observable(&full_replay);

    assert_eq!(
        from_snapshot, from_replay,
        "a snapshot plus its tail reached a different state than a full replay"
    );
    assert_eq!(
        from_snapshot, expected,
        "recovery did not reproduce the state that was durable before the restart"
    );
}

fn transfer(from: Address, to: Address, amount: u64, nonce: u64) -> arc_types::Transaction {
    let mut tx = arc_types::Transaction::new_transfer(from, to, amount, nonce);
    tx.sig_verified = true;
    tx
}

fn history(state: &StateDB, hashes: &[Hash256]) -> Vec<(Hash256, Option<bool>, bool)> {
    hashes
        .iter()
        .map(|h| {
            (
                *h,
                state.get_receipt(&h.0).map(|r| r.success),
                state.get_transaction(&h.0).is_some(),
            )
        })
        .collect()
}

#[test]
fn a_snapshot_carries_only_a_recent_history_window_and_open_rebuilds_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path);
    state.set_snapshot_history_window(4);
    let mut hashes = Vec::new();
    let mut execute = |state: &StateDB, nonce: u64| {
        let tx = transfer(addr(1), addr(3), 10, nonce);
        hashes.push(tx.hash);
        state
            .execute_block_adaptive_at_with_proof(
                &[tx],
                addr(1),
                1_700_000_000_000 + nonce,
                hash_bytes(&nonce.to_le_bytes()),
            )
            .expect("canonical execution");
    };
    for nonce in 0..12 {
        execute(&state, nonce);
    }
    state
        .publish_durable_snapshot()
        .expect("snapshot published");
    let verified = snapshot::load(&path).expect("the snapshot verifies");
    assert!(
        verified.payload.blocks.len() <= 4
            && verified.payload.receipts.len() <= 4
            && verified.payload.full_transactions.len() <= 4,
        "the snapshot carried more than its window: {} blocks, {} receipts, {} bodies",
        verified.payload.blocks.len(),
        verified.payload.receipts.len(),
        verified.payload.full_transactions.len()
    );
    assert_eq!(
        verified.payload.blocks.last().map(|(h, _)| *h),
        Some(state.height()),
        "the window ends at the tip, whose block anchors what comes next"
    );
    for nonce in 12..15 {
        execute(&state, nonce);
    }
    let expected = (observable(&state), history(&state, &hashes));
    drop(state);

    let with_snapshot = open(&path);
    let from_snapshot = (observable(&with_snapshot), history(&with_snapshot, &hashes));
    drop(with_snapshot);
    snapshot::remove(&path).expect("snapshot removed");
    let full_replay = open(&path);
    let from_replay = (observable(&full_replay), history(&full_replay, &hashes));

    assert_eq!(
        from_snapshot, from_replay,
        "snapshot + prefix history + tail != full replay"
    );
    assert_eq!(
        from_snapshot, expected,
        "recovery lost something that was durable"
    );
    assert!(
        from_snapshot
            .1
            .iter()
            .all(|(_, receipt, body)| *receipt == Some(true) && *body),
        "a receipt or body below the window was not rebuilt: {:?}",
        from_snapshot.1
    );
}

#[test]
fn a_snapshot_taken_at_the_tip_needs_no_tail_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path);
    advance(&state, 7);
    let expected = observable(&state);
    state
        .publish_durable_snapshot()
        .expect("snapshot published");
    drop(state);

    let reopened = open(&path);
    assert_eq!(observable(&reopened), expected);
}

#[test]
fn repeated_snapshots_and_restarts_stay_equivalent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let mut expected;
    {
        let state = open(&path);
        advance(&state, 5);
        state.publish_durable_snapshot().unwrap();
        expected = observable(&state);
    }
    for round in 0..4 {
        let state = open(&path);
        assert_eq!(
            observable(&state),
            expected,
            "restart {round} did not reproduce the previous state"
        );
        advance(&state, 3);
        state.publish_durable_snapshot().unwrap();
        expected = observable(&state);
    }
    let final_state = open(&path);
    assert_eq!(observable(&final_state), expected);
}

#[test]
fn a_corrupt_snapshot_is_refused_and_the_node_still_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path);
    advance(&state, 10);
    state.publish_durable_snapshot().unwrap();
    advance(&state, 4);
    let expected = observable(&state);
    drop(state);

    let payload = path.join(snapshot::PAYLOAD_FILE);
    let mut bytes = std::fs::read(&payload).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&payload, &bytes).unwrap();
    assert!(matches!(
        snapshot::load(&path),
        Err(SnapshotError::DigestMismatch { .. })
    ));

    let recovered = open(&path);
    assert_eq!(
        observable(&recovered),
        expected,
        "a corrupt snapshot must cost recovery time, not correctness"
    );
}

#[test]
fn an_interrupted_snapshot_write_leaves_the_previous_state_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path);
    advance(&state, 8);
    state.publish_durable_snapshot().unwrap();
    advance(&state, 5);
    let expected = observable(&state);
    drop(state);

    std::fs::remove_file(path.join(snapshot::MANIFEST_FILE)).unwrap();
    assert!(matches!(snapshot::load(&path), Err(SnapshotError::Absent)));

    let recovered = open(&path);
    assert_eq!(observable(&recovered), expected);
}

#[test]
fn a_snapshot_that_does_not_reproduce_its_state_root_is_backed_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path);
    advance(&state, 6);
    state.publish_durable_snapshot().unwrap();
    advance(&state, 3);
    let expected = observable(&state);
    drop(state);

    let loaded = snapshot::load(&path).expect("snapshot loads");
    snapshot::publish(
        &path,
        loaded.payload.clone(),
        hash_bytes(b"not-the-real-root"),
        loaded.manifest.resume_from_sequence,
        loaded.manifest.block_hash,
    )
    .expect("republished with a wrong root");

    let recovered = open(&path);
    assert_eq!(
        observable(&recovered),
        expected,
        "a snapshot whose root does not verify must be backed out, and full \
         replay must still reach the right state"
    );
}

#[test]
fn a_snapshot_from_an_unknown_version_is_ignored_not_guessed_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path);
    advance(&state, 4);
    state.publish_durable_snapshot().unwrap();
    let expected = observable(&state);
    drop(state);

    let manifest_path = path.join(snapshot::MANIFEST_FILE);
    let mut bytes = std::fs::read(&manifest_path).unwrap();
    bytes[b"ARC-SNAPSHOT".len()] = 99;
    std::fs::write(&manifest_path, &bytes).unwrap();
    assert!(matches!(
        snapshot::load(&path),
        Err(SnapshotError::UnknownVersion(99))
    ));

    let recovered = open(&path);
    assert_eq!(observable(&recovered), expected);
}

#[test]
fn a_torn_wal_tail_after_a_snapshot_still_recovers_to_the_repaired_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");

    let state = open(&path);
    advance(&state, 10);
    state
        .publish_durable_snapshot()
        .expect("snapshot published");
    advance(&state, 6);
    drop(state);

    let wal = path.join("state.wal");
    let bytes = std::fs::read(&wal).unwrap();
    assert!(bytes.len() > 64);
    std::fs::write(&wal, &bytes[..bytes.len() - 17]).unwrap();

    let with_snapshot = open(&path);
    let from_snapshot = observable(&with_snapshot);
    assert!(
        from_snapshot.0 >= 10,
        "recovery fell back below the snapshot's own height: {}",
        from_snapshot.0
    );
    drop(with_snapshot);

    snapshot::remove(&path).expect("snapshot removed");
    let full_replay = open(&path);
    assert_eq!(
        from_snapshot,
        observable(&full_replay),
        "a snapshot changed what a torn WAL tail recovers to"
    );
}

#[test]
fn a_snapshot_ahead_of_the_wal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path);
    advance(&state, 6);
    let expected = observable(&state);
    let loaded_root = state.get_state_root();
    drop(state);

    let mut forged = snapshot::SnapshotPayload {
        height: 9_999,
        ..Default::default()
    };
    forged.canonicalize();
    snapshot::publish(&path, forged, loaded_root, u64::MAX, Hash256::ZERO)
        .expect("forged snapshot written");

    let recovered = open(&path);
    assert_eq!(
        observable(&recovered),
        expected,
        "a snapshot that does not reproduce the state it claims must be backed \
         out, and full replay must still reach the right state"
    );
}
