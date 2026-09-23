//! Checkpoint adoption (C11, design v2.1) on the parts a plain transfer chain
//! can show: the history window is verified from the certified tip by
//! recomputed hashes, and a chain that is not protocol 4 never adopts. The
//! full adoption flow runs on a native-inference fixture in
//! `inference_contract_state.rs` (`checkpoint_adoption_takes_only_committed_native_rows`).

use arc_crypto::{Hash256, hash_bytes};
use arc_state::StateDB;
use arc_state::checkpoint_adoption::verify_history_window;
use arc_state::snapshot::SnapshotPayload;
use arc_types::{Address, Block, Transaction};

fn addr(n: u8) -> Address {
    hash_bytes(&[n])
}

fn open(dir: &std::path::Path) -> StateDB {
    StateDB::with_genesis_persistent(
        &[(addr(1), 1_000_000), (addr(2), 500_000)],
        dir,
        Hash256::ZERO,
    )
    .expect("persistent state")
}

fn advance(state: &StateDB, first_nonce: u64, count: u64) {
    for i in 0..count {
        let nonce = first_nonce + i;
        let mut tx = Transaction::new_transfer(addr(1), addr(3), 10, nonce);
        tx.sig_verified = true;
        state
            .execute_block_adaptive_at_with_proof(
                &[tx],
                addr(1),
                1_700_000_000_000 + nonce,
                hash_bytes(&nonce.to_le_bytes()),
            )
            .expect("canonical execution");
    }
}

fn peer_checkpoint(dir: &std::path::Path) -> (SnapshotPayload, Block) {
    let peer = open(dir);
    advance(&peer, 0, 30);
    let tip = peer.get_block(30).expect("tip");
    (peer.export_durable_snapshot(), tip)
}

fn wal(state: &StateDB) -> Vec<u8> {
    std::fs::read(state.persistence_dir().unwrap().join("state.wal")).unwrap()
}

#[test]
fn the_window_is_verified_from_the_certified_tip_by_recomputed_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let (payload, tip) = peer_checkpoint(&dir.path().join("peer"));
    assert!(verify_history_window(&payload.blocks, &tip).is_ok());
    let below_tip = payload.blocks.len() - 2;

    // A forged header whose `hash` field is left as the ORIGINAL: linkage by
    // declared hashes (the v1 check) would accept it; recomputing refuses.
    let mut kept_hash = payload.blocks.clone();
    kept_hash[below_tip].1.header.proof_hash = hash_bytes(b"another anchor");
    assert!(verify_history_window(&kept_hash, &tip).is_err());

    // The same forgery with a self-consistent hash breaks linkage instead.
    let mut rehashed = kept_hash.clone();
    rehashed[below_tip].1.hash = Block::compute_hash(&rehashed[below_tip].1.header);
    assert!(verify_history_window(&rehashed, &tip).is_err());

    // Listed transactions the header's tx_root does not commit.
    let mut listed = payload.blocks.clone();
    let last = listed.len() - 1;
    listed[last].1.tx_hashes = vec![hash_bytes(b"not in this block")];
    let mut swapped_tip = tip.clone();
    swapped_tip.tx_hashes = listed[last].1.tx_hashes.clone();
    assert!(verify_history_window(&listed, &swapped_tip).is_err());

    // A gap, and a block filed under the wrong height.
    let mut gap = payload.blocks.clone();
    gap.remove(below_tip);
    assert!(verify_history_window(&gap, &tip).is_err());
    let mut misfiled = payload.blocks.clone();
    misfiled[below_tip].0 += 1_000;
    assert!(verify_history_window(&misfiled, &tip).is_err());

    // A last block that is not the certified tip.
    assert!(verify_history_window(&payload.blocks[..payload.blocks.len() - 1], &tip).is_err());
}

#[test]
fn a_chain_that_is_not_protocol_4_never_adopts_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (payload, tip) = peer_checkpoint(&dir.path().join("peer"));
    let node = open(&dir.path().join("node"));
    advance(&node, 0, 10);
    let before = wal(&node);
    // Its uncovered domains (contract storage, stake) can change, so "equal
    // to this node's" would adopt stale state: refused before anything.
    assert!(node.plan_checkpoint_adoption(&payload, &tip).is_err());
    assert_eq!(wal(&node), before);
    assert_eq!(node.height(), 10);
}

#[test]
fn an_unplanned_payload_is_never_written() {
    let dir = tempfile::tempdir().unwrap();
    let (payload, tip) = peer_checkpoint(&dir.path().join("peer"));
    assert!(
        !payload.receipts.is_empty(),
        "the raw export carries receipts"
    );
    let node = open(&dir.path().join("node"));
    advance(&node, 0, 10);
    let before = wal(&node);
    assert!(
        node.rebase_onto_checkpoint(&payload, &tip, Hash256::ZERO, 7)
            .is_err()
    );
    assert_eq!(wal(&node), before, "nothing was written");
    assert_eq!(node.rebase_anchor_round(), None);
}

#[test]
fn a_checkpoint_never_moves_a_node_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let (payload, tip) = peer_checkpoint(&dir.path().join("peer"));
    let node = open(&dir.path().join("node"));
    advance(&node, 0, 40);
    assert!(node.plan_checkpoint_adoption(&payload, &tip).is_err());
    assert_eq!(node.height(), 40);
}
