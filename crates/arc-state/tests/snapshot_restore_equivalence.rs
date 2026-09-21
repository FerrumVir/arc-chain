//! A snapshot plus the WAL tail after it must reach exactly the state that
//! replaying the whole WAL reaches.
//!
//! That equivalence is the entire justification for skipping records at open.
//! It is asserted here against real `StateDB` instances over real files rather
//! than argued for, and the failure modes a snapshot introduces - a corrupt
//! payload, a lost manifest, an interrupted write, a snapshot that does not
//! reproduce its own state root - each get a test that the node still recovers.

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

/// Produce `count` canonical blocks, so the WAL has a tail worth skipping.
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

/// Everything an observer can read back, used to compare two recovered states.
fn observable(state: &StateDB) -> (u64, Hash256, Vec<(u64, Hash256, Hash256)>, Vec<u64>) {
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

    // Build a chain, snapshot partway, then keep going.
    let state = open(&path);
    advance(&state, 12);
    let manifest = state.publish_durable_snapshot().expect("snapshot published");
    assert_eq!(manifest.identity.height, state.height());
    advance(&state, 9);
    let expected = observable(&state);
    drop(state);

    // Reopen WITH the snapshot: only the tail is applied.
    let with_snapshot = open(&path);
    let from_snapshot = observable(&with_snapshot);
    drop(with_snapshot);

    // Reopen WITHOUT it: the whole WAL is applied.
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

/// Every transaction's receipt outcome and whether its body is held.
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
    // A snapshot used to capture every block, receipt and body, so each one
    // grew with the chain. It now carries a recent window; the rest must come
    // back from the WAL records below it - with every receipt and body, not
    // just the block headers.
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
    state.publish_durable_snapshot().expect("snapshot published");
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

    assert_eq!(from_snapshot, from_replay, "snapshot + prefix history + tail != full replay");
    assert_eq!(from_snapshot, expected, "recovery lost something that was durable");
    assert!(
        from_snapshot.1.iter().all(|(_, receipt, body)| *receipt == Some(true) && *body),
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
    state.publish_durable_snapshot().expect("snapshot published");
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

    // Flip a byte in the payload. The digest no longer matches.
    let payload = path.join(snapshot::PAYLOAD_FILE);
    let mut bytes = std::fs::read(&payload).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&payload, &bytes).unwrap();
    assert!(matches!(
        snapshot::load(&path),
        Err(SnapshotError::DigestMismatch { .. })
    ));

    // The node opens anyway, by replaying everything.
    let recovered = open(&path);
    assert_eq!(
        observable(&recovered),
        expected,
        "a corrupt snapshot must cost recovery time, not correctness"
    );
}

#[test]
fn an_interrupted_snapshot_write_leaves_the_previous_state_recoverable() {
    // The shape a crash between the payload write and the manifest write
    // leaves: a payload with no manifest naming it.
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
    // A digest proves the bytes are the ones written. It does not prove they
    // describe the state they claim to - so the root is recomputed after
    // installing, and a mismatch must discard the install rather than build
    // the tail on top of it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path);
    advance(&state, 6);
    state.publish_durable_snapshot().unwrap();
    advance(&state, 3);
    let expected = observable(&state);
    drop(state);

    // Rewrite the manifest with a state root that is not the payload's, and
    // re-point its digest at the untouched payload so only the ROOT is wrong.
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
    // The version byte sits immediately after the magic.
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
    // A snapshot bounds what replay applies; it must not change what replay
    // DECIDES. If the tail beyond the snapshot is torn, recovery has to reach
    // the same state a full replay of the repaired log reaches - not the
    // snapshot's own height, and not a mixture.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");

    let state = open(&path);
    advance(&state, 10);
    state.publish_durable_snapshot().expect("snapshot published");
    advance(&state, 6);
    drop(state);

    // Tear the final frame, the way a crash mid-append leaves it.
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

    // The same torn log, replayed in full, must agree.
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
    // A snapshot naming a resume point beyond anything in the log would skip
    // every remaining record. The state root check is what catches it: the
    // installed payload describes a height the log never reached.
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
