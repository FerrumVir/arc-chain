//! Adversarial check: does the Dense layer AIR actually bind the output?
//!
//! Proves a correct matrix-multiply, then tries to prove a forged one where a
//! single output element is off by one. A sound AIR must reject the forgery.
//!
//! Usage: cargo run --release --example soundness_check --features stwo-prover

use arc_crypto::inference_proof::dense_forward_i64;
use arc_crypto::stwo_air::try_prove_dense_stark;

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
    let in_size = 256;
    let out_size = 64;

    let weights = make_data("sound-w", in_size * out_size);
    let bias = make_data("sound-b", out_size);
    let input = make_data("sound-x", in_size);
    let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

    println!("=== Dense layer AIR soundness check ===");
    println!("Layer: {out_size}x{in_size} = {} MACs\n", in_size * out_size);

    // ── 1. Honest proof ─────────────────────────────────────────────────
    print!("1. Honest output                    ... ");
    match try_prove_dense_stark(&weights, &input, &output, &bias, in_size, out_size) {
        Ok((data, size, ms)) => {
            println!("PROVED   {size} B in {ms}ms");
            println!("   receipt: 0x{}", hex::encode(&data[..16]));
        }
        Err(e) => {
            println!("FAILED (this is a bug): {e}");
            std::process::exit(1);
        }
    }

    // ── 2. Forged: one output off by one ────────────────────────────────
    let mut forged = output.clone();
    forged[0] += 1;
    print!("\n2. output[0] off by one             ... ");
    match try_prove_dense_stark(&weights, &input, &forged, &bias, in_size, out_size) {
        Ok(_) => {
            println!("PROVED  <-- UNSOUND: the AIR does not bind the output");
            std::process::exit(1);
        }
        Err(_) => println!("REJECTED"),
    }

    // ── 3. Forged: a middle neuron's output swapped ─────────────────────
    let mut swapped = output.clone();
    swapped.swap(3, 7);
    print!("3. two neuron outputs swapped       ... ");
    match try_prove_dense_stark(&weights, &input, &swapped, &bias, in_size, out_size) {
        Ok(_) => {
            println!("PROVED  <-- UNSOUND: accumulator not bound per neuron");
            std::process::exit(1);
        }
        Err(_) => println!("REJECTED"),
    }

    // ── 4. Forged: bias tampered ────────────────────────────────────────
    let mut bad_bias = bias.clone();
    bad_bias[1] += 1000;
    print!("4. bias[1] tampered                 ... ");
    match try_prove_dense_stark(&weights, &input, &output, &bad_bias, in_size, out_size) {
        Ok(_) => {
            println!("PROVED  <-- UNSOUND: bias not bound");
            std::process::exit(1);
        }
        Err(_) => println!("REJECTED"),
    }

    // ── 5. Forged: one weight changed, output left alone ────────────────
    let mut bad_w = weights.clone();
    bad_w[0] += 1;
    print!("5. weight[0][0] changed             ... ");
    match try_prove_dense_stark(&bad_w, &input, &output, &bias, in_size, out_size) {
        Ok(_) => {
            println!("PROVED  <-- UNSOUND: products not bound to the sum");
            std::process::exit(1);
        }
        Err(_) => println!("REJECTED"),
    }

    println!("\nAll four forgeries rejected. The AIR binds output to the dot product.");
}
