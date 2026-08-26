//! How large a single Circle STARK can this machine actually prove?
//!
//! Walks a ladder of Dense-layer sizes up to a full Llama-2-7B attention
//! projection in one proof. Every entry is a real proof, verified inline.
//!
//! Usage: cargo run --release --example stark_scale --features stwo-prover

use arc_crypto::inference_proof::dense_forward_i64;
use arc_crypto::stwo_air::try_prove_dense_stark;
use std::io::Write;
use std::time::Instant;

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
    let ladder: Vec<(usize, usize, &str)> = vec![
        (64, 256, ""),
        (256, 1024, ""),
        (512, 2048, ""),
        (1024, 4096, "7B attn shard"),
        (2048, 4096, ""),
        (4096, 4096, "full 7B attn projection"),
    ];

    println!("=== Circle STARK scale ladder (one machine, one proof each) ===\n");
    println!(
        "{:<14} {:>12} {:>9} {:>11} {:>9}  {}",
        "layer", "MACs", "log rows", "prove ms", "receipt", "note"
    );
    println!("{}", "-".repeat(78));
    let _ = std::io::stdout().flush();

    for (out_size, in_size, note) in ladder {
        let n = out_size * in_size;
        let log_rows = (n as f64).log2().ceil() as u32;

        let weights = make_data("scale-w", n);
        let bias = vec![0i64; out_size];
        let input = make_data("scale-x", in_size);
        let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

        let t = Instant::now();
        let res = try_prove_dense_stark(&weights, &input, &output, &bias, in_size, out_size);
        let ms = t.elapsed().as_millis();
        let label = format!("{}x{}", out_size, in_size);

        match res {
            Ok((_data, size, _)) => println!(
                "{:<14} {:>12} {:>9} {:>11} {:>8}B  {}",
                label, n, log_rows, ms, size, note
            ),
            Err(e) => {
                println!("{:<14} {:>12} {:>9} {:>11}   FAILED", label, n, log_rows, ms);
                println!("   {e}");
                break;
            }
        }
        let _ = std::io::stdout().flush();
    }
}
