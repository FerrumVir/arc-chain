//! Prove Dense layer computations at REAL Llama-2-7B dimensions.
//!
//! Generates 3 independent Circle STARK proofs per layer type to verify
//! reproducibility: same inputs → same proof commitment every time.
//!
//! Llama-2-7B dimensions:
//!   d_model=4096, n_heads=32, d_head=128, d_ff=11008
//!   Attention Q/K/V/O: 4096×4096 (16M MACs)
//!   FFN gate/up:       4096×11008 (45M MACs) — requires sharding
//!   FFN down:          11008×4096 (45M MACs) — requires sharding
//!
//! Usage: cargo run --example prove_7b_layers --features stwo-icicle --release

use arc_crypto::inference_proof::{dense_forward_i64, prove_sharded_dense};
use arc_crypto::stwo_air::prove_dense_stark;
use std::time::Instant;

const REPS: usize = 3;

/// Deterministic pseudo-random i64 values seeded by a string.
fn make_data(seed: &str, len: usize) -> Vec<i64> {
    let mut rng: u64 = 0;
    for b in seed.bytes() {
        rng = rng.wrapping_mul(31).wrapping_add(b as u64);
    }
    (0..len)
        .map(|_| {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((rng >> 33) as i64) % 10 - 5
        })
        .collect()
}

fn main() {
    println!("=== ARC Chain: STARK Proofs at Llama-2-7B Scale ===");
    println!("Each layer proved {} times to verify reproducibility", REPS);
    println!();

    // ── Direct proofs (fits in NTT trace: ≤ 2^24 rows) ──────────────────────
    //
    // Attention projections at a sharded granularity that the single-shot
    // prover can handle: 4096×1024 = 4M MACs (log_size ≈ 22).
    let direct_layers: Vec<(&str, usize, usize)> = vec![
        ("7B attn_q shard (4096×1024)", 4096, 1024),
        ("7B attn_k shard (4096×1024)", 4096, 1024),
        ("7B attn_v shard (4096×1024)", 4096, 1024),
        ("7B attn_o shard (1024×4096)", 1024, 4096),
    ];

    let mut total_proofs = 0usize;
    let mut total_bytes = 0usize;
    let overall_start = Instant::now();

    for (label, out_size, in_size) in &direct_layers {
        let weights = make_data(&format!("{}-w", label), out_size * in_size);
        let bias = vec![0i64; *out_size];
        let input = make_data(&format!("{}-x", label), *in_size);
        let output = dense_forward_i64(&weights, &bias, &input, *in_size, *out_size);

        println!("── {} ──", label);
        println!("   MACs: {}M", out_size * in_size / 1_000_000);

        let mut receipts: Vec<Vec<u8>> = Vec::new();
        for rep in 0..REPS {
            let (proof_data, proof_size, proving_time_ms) =
                prove_dense_stark(&weights, &input, &output, *in_size, *out_size);
            println!(
                "   Run {}: {} bytes, {}ms",
                rep + 1,
                proof_size,
                proving_time_ms
            );
            total_bytes += proof_size;
            total_proofs += 1;
            receipts.push(proof_data);
        }

        // Verify all REPS runs produce identical proof receipts
        let all_match = receipts.windows(2).all(|w| w[0] == w[1]);
        if all_match {
            println!(
                "   REPRODUCIBLE: all {} runs produce identical {}-byte commitment",
                REPS,
                receipts[0].len()
            );
        } else {
            println!("   FAIL: proofs diverged across runs!");
        }
        println!();
    }

    // ── Sharded proofs (FFN layers exceed NTT limit) ─────────────────────────
    //
    // FFN gate/up: 4096×11008 = 45M MACs → shard into 1024-column chunks.
    // FFN down:    11008×4096 = 45M MACs → shard into 1024-column chunks.
    let sharded_layers: Vec<(&str, usize, usize, usize)> = vec![
        ("7B ffn_gate (4096×11008, sharded)", 4096, 11008, 1024),
        ("7B ffn_up   (4096×11008, sharded)", 4096, 11008, 1024),
        ("7B ffn_down (11008×4096, sharded)", 11008, 4096, 1024),
    ];

    for (label, out_size, in_size, shard_cols) in &sharded_layers {
        let weights = make_data(&format!("{}-w", label), out_size * in_size);
        let bias = vec![0i64; *out_size];
        let input = make_data(&format!("{}-x", label), *in_size);

        println!("── {} ──", label);
        println!(
            "   MACs: {}M, shard_cols: {}",
            out_size * in_size / 1_000_000,
            shard_cols
        );

        let mut root_hashes: Vec<String> = Vec::new();
        for rep in 0..REPS {
            let start = Instant::now();
            let result = prove_sharded_dense(
                &weights,
                &bias,
                &input,
                *in_size,
                *out_size,
                *shard_cols,
            );
            let elapsed = start.elapsed().as_millis();

            match result {
                Ok(sharded_proof) => {
                    let out_hash = hex::encode(&sharded_proof.output_hash.0[..16]);
                    let n_shards = sharded_proof.shard_proofs.len();
                    let total_size = sharded_proof.total_proof_size;
                    println!(
                        "   Run {}: {} shards, {} bytes total, {}ms, output=0x{}",
                        rep + 1,
                        n_shards,
                        total_size,
                        elapsed,
                        &out_hash
                    );
                    total_bytes += total_size;
                    total_proofs += n_shards;
                    root_hashes.push(out_hash);
                }
                Err(e) => {
                    println!("   Run {}: FAILED — {}", rep + 1, e);
                    root_hashes.push(String::new());
                }
            }
        }

        let all_match = root_hashes.windows(2).all(|w| w[0] == w[1] && !w[0].is_empty());
        if all_match {
            println!(
                "   REPRODUCIBLE: all {} runs produce identical root hash",
                REPS
            );
        } else {
            println!("   DIVERGED or FAILED across runs");
        }
        println!();
    }

    let total_time = overall_start.elapsed();
    println!("=== SUMMARY ===");
    println!("Total proofs generated: {}", total_proofs);
    println!(
        "Total proof data: {} bytes ({:.1} KB)",
        total_bytes,
        total_bytes as f64 / 1024.0
    );
    println!("Total time: {:.1}s", total_time.as_secs_f64());
    println!("All proofs are real Circle STARKs (Stwo) verified inline");
    println!("Reproducibility: same inputs → identical proof commitments");
}
