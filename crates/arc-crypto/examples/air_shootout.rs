//! Unpacked vs packed Dense AIR: same statement, different trace shape.
//!
//! Usage: cargo run --release --example air_shootout --features stwo-prover

use arc_crypto::inference_proof::dense_forward_i64;
use arc_crypto::stwo_air::{
    packed_log_size, try_prove_dense_packed, try_prove_dense_stark, compute_log_size,
    PACK_K, PACKED_STARK_COLS, DENSE_STARK_COLS,
};
use std::io::Write;

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
    println!("=== Dense AIR shootout: 1 MAC/row vs {PACK_K} MACs/row ===");
    println!("unpacked: {DENSE_STARK_COLS} trace cols + 3 preprocessed");
    println!("packed:   {PACKED_STARK_COLS} trace cols + 3 preprocessed\n");

    println!(
        "{:<14}{:>10}{:>9}{:>9}{:>11}{:>11}{:>10}",
        "layer", "MACs", "old rows", "new rows", "old ms", "new ms", "speedup"
    );
    println!("{}", "-".repeat(74));
    let _ = std::io::stdout().flush();

    for (out_size, in_size) in [(256usize, 1024usize), (512, 2048), (1024, 4096), (4096, 4096)] {
        let n = out_size * in_size;
        let weights = make_data("w", n);
        let bias = vec![0i64; out_size];
        let input = make_data("x", in_size);
        let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

        let old_log = compute_log_size(n);
        let new_log = packed_log_size(in_size, out_size);

        let old_ms = try_prove_dense_stark(&weights, &input, &output, &bias, in_size, out_size)
            .map(|(_, _, ms)| ms)
            .unwrap_or(0);
        let new_ms = try_prove_dense_packed(&weights, &input, &output, &bias, in_size, out_size)
            .map(|(_, _, ms)| ms)
            .unwrap_or(0);

        let speedup = if new_ms > 0 {
            format!("{:.1}x", old_ms as f64 / new_ms as f64)
        } else {
            "-".into()
        };
        println!(
            "{:<14}{:>10}{:>9}{:>9}{:>11}{:>11}{:>10}",
            format!("{}x{}", out_size, in_size),
            n,
            format!("2^{old_log}"),
            format!("2^{new_log}"),
            old_ms,
            new_ms,
            speedup
        );
        let _ = std::io::stdout().flush();
    }

    // ── soundness: the packed AIR must reject the same forgeries ─────────
    println!("\n=== packed AIR soundness ===");
    let (out_size, in_size) = (64usize, 256usize);
    let weights = make_data("sw", out_size * in_size);
    let bias = make_data("sb", out_size);
    let input = make_data("sx", in_size);
    let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

    let honest = try_prove_dense_packed(&weights, &input, &output, &bias, in_size, out_size);
    println!(
        "1. honest output            ... {}",
        if honest.is_ok() { "PROVED" } else { "FAILED (bug)" }
    );

    let checks: Vec<(&str, Vec<i64>, Vec<i64>, Vec<i64>)> = vec![
        ("output[0] off by one", weights.clone(), bias.clone(), {
            let mut o = output.clone();
            o[0] += 1;
            o
        }),
        ("two outputs swapped", weights.clone(), bias.clone(), {
            let mut o = output.clone();
            o.swap(3, 7);
            o
        }),
        ("bias tampered", weights.clone(), {
            let mut b = bias.clone();
            b[1] += 1000;
            b
        }, output.clone()),
        ("one weight changed", {
            let mut w = weights.clone();
            w[0] += 1;
            w
        }, bias.clone(), output.clone()),
    ];

    let mut all_rejected = true;
    for (i, (name, w, b, o)) in checks.iter().enumerate() {
        let r = try_prove_dense_packed(w, &input, o, b, in_size, out_size);
        let rejected = r.is_err();
        all_rejected &= rejected;
        println!(
            "{}. {:<26} ... {}",
            i + 2,
            name,
            if rejected { "REJECTED" } else { "PROVED  <-- UNSOUND" }
        );
    }

    if honest.is_ok() && all_rejected {
        println!("\nPacked AIR is sound: honest proves, all four forgeries rejected.");
    } else {
        println!("\nFAILED - do not ship this.");
        std::process::exit(1);
    }
}
