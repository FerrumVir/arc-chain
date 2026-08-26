//! Does the block AIR actually constrain balances and nonces?
//!
//! Builds a valid block witness, proves it, then tries three forgeries that a
//! chain must never accept.
//!
//! Usage: cargo run --release --example block_witness_check --features stwo-prover

use arc_crypto::stark::{BlockProofInput, TransferWitness};
use arc_crypto::stwo_air::try_prove_block;

fn witness(bal: u64, amount: u64, fee: u64, recv: u64, nonce: u32) -> TransferWitness {
    TransferWitness {
        sender_bal_before: bal,
        sender_bal_after: bal - amount - fee,
        receiver_bal_before: recv,
        receiver_bal_after: recv + amount,
        amount,
        sender_nonce_before: nonce,
        sender_nonce_after: nonce + 1,
        fee,
    }
}

fn input(transfers: Vec<TransferWitness>) -> BlockProofInput {
    let diffs = transfers
        .iter()
        .enumerate()
        .map(|(i, _)| ([i as u8; 32], [(i + 1) as u8; 32], [(i + 2) as u8; 32]))
        .collect();
    BlockProofInput {
        height: 1_337,
        block_hash: [7u8; 32],
        prev_state_root: [1u8; 32],
        post_state_root: [2u8; 32],
        tx_hashes: vec![[3u8; 32]; transfers.len()],
        state_diffs: diffs,
        transfers,
    }
}

fn main() {
    println!("=== Block AIR witness check ===\n");

    let good = vec![
        witness(1_000_000, 250_000, 500, 42, 7),
        witness(880_000, 12_345, 500, 900_000, 0),
        witness(5_000, 5_000, 0, 1, 99),
    ];

    print!("1. Three honest transfers          ... ");
    match try_prove_block(&input(good.clone())) {
        Ok((data, size, ms)) => {
            println!("PROVED   {size} B in {ms}ms  0x{}", hex::encode(&data[..8]))
        }
        Err(e) => {
            println!("FAILED (bug): {e}");
            std::process::exit(1);
        }
    }

    // Money printed from nothing: receiver credited without sender debit.
    let mut minted = good.clone();
    minted[0].receiver_bal_after += 1;
    print!("\n2. Receiver credited an extra unit ... ");
    check_rejected(&minted);

    // Sender keeps the fee.
    let mut free_fee = good.clone();
    free_fee[1].sender_bal_after += free_fee[1].fee;
    print!("3. Sender does not pay the fee     ... ");
    check_rejected(&free_fee);

    // Replay: nonce does not advance.
    let mut replay = good.clone();
    replay[2].sender_nonce_after = replay[2].sender_nonce_before;
    print!("4. Nonce not incremented (replay)  ... ");
    check_rejected(&replay);

    println!("\nAll three forgeries rejected by satisfies_air() and by the prover.");
}

fn check_rejected(ws: &[TransferWitness]) {
    let filtered = ws.iter().all(|w| w.satisfies_air());
    let proved = try_prove_block(&input(ws.to_vec())).is_ok();
    if filtered || proved {
        println!("ACCEPTED  <-- UNSOUND (filter={filtered}, prover={proved})");
        std::process::exit(1);
    }
    println!("REJECTED");
}
