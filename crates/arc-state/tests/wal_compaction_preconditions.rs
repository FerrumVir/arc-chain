//! Characterization of the v0.8.11 storage contract, NOT a compactor.
//! These tests pin why deleting a snapshotted prefix is not a safe migration.

use arc_crypto::{Hash256, hash_bytes};
use arc_state::{StateDB, WalEntry, read_wal_strict};
use std::{io::Write, path::Path};

fn open(path: &Path) -> Result<StateDB, arc_state::StateError> {
    StateDB::with_genesis_persistent(&[(hash_bytes(b"funded"), 1_000_000)], path, Hash256::ZERO)
}

fn advance(state: &StateDB) {
    let height = state.height() + 1;
    state
        .execute_block_adaptive_at_with_proof(
            &[],
            hash_bytes(b"funded"),
            1_700_000_000_000 + height,
            hash_bytes(&height.to_le_bytes()),
        )
        .unwrap();
}

fn write_entries(path: &Path, entries: &[WalEntry], renumber: bool) {
    let mut file = std::fs::File::create(path).unwrap();
    for (sequence, entry) in entries.iter().enumerate() {
        let mut entry = entry.clone();
        if renumber {
            entry.sequence = sequence as u64;
            entry.checksum = crc32fast::hash(
                &bincode::serialize(&(entry.block_height, entry.sequence, &entry.op)).unwrap(),
            );
        }
        let bytes = bincode::serialize(&entry).unwrap();
        file.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
        file.write_all(&bytes).unwrap();
    }
    file.sync_all().unwrap();
}

#[test]
fn durable_snapshots_do_not_retire_any_state_wal_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path).unwrap();
    let wal = path.join("state.wal");
    let mut previous = std::fs::metadata(&wal).unwrap().len();
    for _ in 0..16 {
        advance(&state);
        state.try_sync_wal().unwrap();
        let before = std::fs::read(&wal).unwrap();
        state.publish_durable_snapshot().unwrap();
        let after = std::fs::read(&wal).unwrap();
        assert_eq!(before, after, "snapshot publication changed WAL bytes");
        assert!(after.len() as u64 > previous);
        previous = after.len() as u64;
    }
}

#[test]
fn a_snapshot_does_not_make_a_prefixless_wal_reopenable() {
    // Check both tempting approaches: preserve the original sequences, or
    // renumber and rechecksum the retained tail to start at zero.
    for renumber in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        let state = open(&path).unwrap();
        advance(&state);
        let manifest = state.publish_durable_snapshot().unwrap();
        advance(&state);
        drop(state);
        let wal = path.join("state.wal");
        let entries = read_wal_strict(&wal).unwrap();
        let tail: Vec<_> = entries
            .into_iter()
            .filter(|entry| entry.sequence >= manifest.resume_from_sequence)
            .collect();
        assert!(!tail.is_empty());
        write_entries(&wal, &tail, renumber);
        if renumber {
            read_wal_strict(&wal).expect("renumbered tail is physically valid");
        } else {
            let error = read_wal_strict(&wal).unwrap_err();
            assert!(error.to_string().contains("WAL sequence gap"), "{error}");
        }
        let error = match open(&path) {
            Ok(_) => panic!("legacy startup accepted a prefixless state WAL"),
            Err(error) => error,
        };
        let expected = if renumber { "genesis" } else { "sequence gap" };
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn a_tip_snapshot_does_not_make_an_empty_normal_wal_reopenable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let state = open(&path).unwrap();
    advance(&state);
    state.publish_durable_snapshot().unwrap();
    drop(state);
    write_entries(&path.join("state.wal"), &[], false);
    let error = match open(&path) {
        Ok(_) => panic!("normal startup accepted an empty WAL with a tip snapshot"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("incomplete genesis prefix"),
        "{error}"
    );
}
