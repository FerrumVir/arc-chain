//! Measured post-quantum signature throughput on this machine.
//!
//! Exercises the exact `KeyPair::sign` / `Signature::verify` path that the
//! mempool and block pipeline use, so the numbers reflect real transaction
//! verification cost - address derivation included.
//!
//! Usage: cargo run --release --example pq_bench

use arc_crypto::{KeyPair, hash_bytes};
use std::time::Instant;

const KEYGEN_N: usize = 100;
const SIGN_N: usize = 500;
const VERIFY_N: usize = 500;
/// Warm-up ops before timing, so the first-call cost of lazily initialised
/// tables doesn't land in the measurement.
const WARMUP: usize = 200;
/// Timing batches. We report the fastest batch: on a 24-core desktop the
/// scheduler adds tens of microseconds of noise to any single run, and the
/// minimum is the measurement least contaminated by it.
const BATCHES: usize = 7;

fn bench(label: &str, make: fn() -> KeyPair) {
    // ── keygen ──────────────────────────────────────────────────────────
    let t0 = Instant::now();
    let mut kps = Vec::with_capacity(KEYGEN_N);
    for _ in 0..KEYGEN_N {
        kps.push(make());
    }
    let keygen_us = t0.elapsed().as_secs_f64() * 1e6 / KEYGEN_N as f64;

    let kp = &kps[0];
    let addr = kp.address();
    let msg = hash_bytes(b"arc-chain pq benchmark message");

    // ── warm up both paths before timing anything ───────────────────────
    let mut sig = kp.sign(&msg).expect("sign");
    for _ in 0..WARMUP {
        sig = kp.sign(&msg).expect("sign");
        sig.verify(&msg, &addr).expect("verify");
    }

    // ── sign: fastest of BATCHES batches ────────────────────────────────
    let mut sign_us = f64::MAX;
    for _ in 0..BATCHES {
        let t = Instant::now();
        for _ in 0..SIGN_N {
            sig = kp.sign(&msg).expect("sign");
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / SIGN_N as f64;
        if us < sign_us {
            sign_us = us;
        }
    }

    // ── verify: full tx path (address derivation + signature check) ─────
    let mut verify_us = f64::MAX;
    for _ in 0..BATCHES {
        let t = Instant::now();
        for _ in 0..VERIFY_N {
            sig.verify(&msg, &addr).expect("verify");
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / VERIFY_N as f64;
        if us < verify_us {
            verify_us = us;
        }
    }

    // ── on-chain footprint ──────────────────────────────────────────────
    let wire_bytes = bincode_len(&sig);

    println!(
        "{:<14} {:>10.1} {:>10.1} {:>10.1} {:>12.0} {:>10}",
        label,
        keygen_us / 1000.0,
        sign_us,
        verify_us,
        1_000_000.0 / verify_us,
        wire_bytes
    );
}

/// Raw on-wire byte count: public key + signature material.
fn bincode_len(sig: &arc_crypto::Signature) -> usize {
    use arc_crypto::Signature as S;
    match sig {
        S::Ed25519 {
            public_key,
            signature,
        } => public_key.len() + signature.len(),
        S::Secp256k1 { signature } => signature.len(),
        S::MlDsa65 {
            public_key,
            signature,
        } => public_key.len() + signature.len(),
        S::Falcon512 {
            public_key,
            signature,
        } => public_key.len() + signature.len(),
    }
}

fn main() {
    println!("=== ARC Chain: signature scheme benchmark ===");
    println!(
        "keygen n={KEYGEN_N}, sign n={SIGN_N}, verify n={VERIFY_N}, {WARMUP} warm-up ops,\nfastest of {BATCHES} batches. Verify timing includes address derivation.\n"
    );
    println!(
        "{:<14} {:>10} {:>10} {:>10} {:>12} {:>10}",
        "scheme", "keygen ms", "sign us", "vrfy us", "vrfy/sec", "sig bytes"
    );
    println!("{}", "-".repeat(72));

    bench("Ed25519", KeyPair::generate_ed25519);
    bench("secp256k1", KeyPair::generate_secp256k1);
    bench("ML-DSA-65 (PQ)", KeyPair::generate_ml_dsa);
    bench("Falcon-512 (PQ)", KeyPair::generate_falcon512);

    println!("\nML-DSA-65  = NIST FIPS 204, standardized Aug 2024");
    println!("Falcon-512 = NIST FIPS 206 draft (FN-DSA), final standard expected ~2027");
    println!("Both verify through the same Signature::verify path used by the mempool.");
}
