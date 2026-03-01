//! ARC Chain — Signed Transaction Benchmark Suite
//!
//! Measures real TPS with signature verification: Ed25519 (classical) vs ML-DSA-65 (quantum-proof).
//!
//! Usage:
//!   arc-bench-signed [--txs N] [--mode unsigned|signed|batch-verified|ml-dsa|all] [--threads N]
//!
//! Modes:
//!   unsigned       — No signature checks (baseline)
//!   signed         — Generate real Ed25519-signed txs, verify each individually
//!   batch-verified — Generate signed txs, batch_verify_ed25519() then execute
//!   ml-dsa         — Generate ML-DSA-65 (post-quantum) signed txs + batch verify
//!   all            — Run all modes including quantum comparison (default)

use arc_crypto::{hash_bytes, batch_verify_ed25519, batch_verify_ml_dsa, Hash256, KeyPair};
use arc_crypto::signature::{ML_DSA_PK_LEN, ML_DSA_SIG_LEN};
use arc_state::StateDB;
use arc_types::Transaction;
use clap::Parser;
use rayon::prelude::*;
use std::time::Instant;

// ─────────────────────────────────────────────────────────────────────────────
//  CLI
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "arc-bench-signed",
    about = "ARC Chain — Signature Benchmark (Ed25519 + ML-DSA-65 Quantum-Proof)"
)]
struct Args {
    /// Number of transactions to benchmark.
    #[arg(long, default_value = "100000")]
    txs: usize,

    /// Benchmark mode: unsigned, signed, batch-verified, ml-dsa, all.
    #[arg(long, default_value = "all")]
    mode: String,

    /// Number of Rayon threads (0 = auto-detect).
    #[arg(long, default_value = "0")]
    threads: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn format_number(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_rate(rate: f64) -> String {
    if rate >= 1_000_000.0 {
        format!("{:.2}M", rate / 1_000_000.0)
    } else if rate >= 1_000.0 {
        format!("{:.1}K", rate / 1_000.0)
    } else {
        format!("{:.0}", rate)
    }
}

/// Generate keypairs and signed transactions.
/// Returns (transactions, keypairs, keygen_time, signing_time).
fn generate_signed_transactions(
    count: usize,
) -> (Vec<Transaction>, Vec<KeyPair>, std::time::Duration, std::time::Duration) {
    // Step 1: Generate keypairs (parallel)
    let keygen_start = Instant::now();
    let keypairs: Vec<KeyPair> = (0..count)
        .into_par_iter()
        .map(|_| KeyPair::generate_ed25519())
        .collect();
    let keygen_time = keygen_start.elapsed();

    // Step 2: Create and sign transactions
    // Each sender transfers to the next keypair (circular)
    let sign_start = Instant::now();
    let transactions: Vec<Transaction> = keypairs
        .par_iter()
        .enumerate()
        .map(|(i, kp)| {
            let from = kp.address();
            let to_idx = (i + 1) % count;
            let to = keypairs[to_idx].address();
            let mut tx = Transaction::new_transfer(from, to, 1, 0);
            tx.sign(kp).expect("signing must succeed");
            tx
        })
        .collect();
    let sign_time = sign_start.elapsed();

    (transactions, keypairs, keygen_time, sign_time)
}

/// Generate ML-DSA-65 (post-quantum) keypairs and signed transactions.
/// Returns (transactions, keypairs, keygen_time, signing_time).
fn generate_ml_dsa_transactions(
    count: usize,
) -> (Vec<Transaction>, Vec<KeyPair>, std::time::Duration, std::time::Duration) {
    // Step 1: Generate ML-DSA keypairs (parallel — heavier than Ed25519)
    let keygen_start = Instant::now();
    let keypairs: Vec<KeyPair> = (0..count)
        .into_par_iter()
        .map(|_| KeyPair::generate_ml_dsa())
        .collect();
    let keygen_time = keygen_start.elapsed();

    // Step 2: Create and sign transactions
    let sign_start = Instant::now();
    let transactions: Vec<Transaction> = keypairs
        .par_iter()
        .enumerate()
        .map(|(i, kp)| {
            let from = kp.address();
            let to_idx = (i + 1) % count;
            let to = keypairs[to_idx].address();
            let mut tx = Transaction::new_transfer(from, to, 1, 0);
            tx.sign(kp).expect("ML-DSA signing must succeed");
            tx
        })
        .collect();
    let sign_time = sign_start.elapsed();

    (transactions, keypairs, keygen_time, sign_time)
}

/// Benchmark: ML-DSA batch verification using Rayon parallel individual verification.
fn bench_ml_dsa_batch_verify(transactions: &[Transaction]) -> f64 {
    let mut messages: Vec<Vec<u8>> = Vec::with_capacity(transactions.len());
    let mut signatures: Vec<[u8; ML_DSA_SIG_LEN]> = Vec::with_capacity(transactions.len());
    let mut public_keys: Vec<[u8; ML_DSA_PK_LEN]> = Vec::with_capacity(transactions.len());

    for tx in transactions {
        messages.push(tx.hash.as_bytes().to_vec());
        match &tx.signature {
            arc_crypto::Signature::MlDsa65 {
                public_key,
                signature,
            } => {
                signatures.push(signature.as_slice().try_into().expect("correct sig len"));
                public_keys.push(public_key.as_slice().try_into().expect("correct pk len"));
            }
            _ => panic!("expected MlDsa65 signatures"),
        }
    }

    let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();

    let start = Instant::now();
    batch_verify_ml_dsa(&msg_refs, &signatures, &public_keys)
        .expect("ML-DSA batch verify must succeed");
    let elapsed = start.elapsed();

    transactions.len() as f64 / elapsed.as_secs_f64()
}

/// Benchmark: ML-DSA batch-verified block execution.
fn bench_ml_dsa_batch_execution(
    transactions: &[Transaction],
    keypairs: &[KeyPair],
) -> f64 {
    let state = state_for_signed(keypairs);
    let validator = keypairs[0].address();

    let start = Instant::now();

    // Step 1: Batch verify all ML-DSA signatures
    let mut messages: Vec<Vec<u8>> = Vec::with_capacity(transactions.len());
    let mut sigs: Vec<[u8; ML_DSA_SIG_LEN]> = Vec::with_capacity(transactions.len());
    let mut pks: Vec<[u8; ML_DSA_PK_LEN]> = Vec::with_capacity(transactions.len());

    for tx in transactions {
        messages.push(tx.hash.as_bytes().to_vec());
        match &tx.signature {
            arc_crypto::Signature::MlDsa65 {
                public_key,
                signature,
            } => {
                sigs.push(signature.as_slice().try_into().expect("correct sig len"));
                pks.push(public_key.as_slice().try_into().expect("correct pk len"));
            }
            _ => panic!("expected MlDsa65 signatures"),
        }
    }

    let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();
    batch_verify_ml_dsa(&msg_refs, &sigs, &pks)
        .expect("ML-DSA batch verify must succeed");

    // Step 2: Execute block (sigs already verified)
    let (_, receipts) = state.execute_block(transactions, validator).unwrap();

    let elapsed = start.elapsed();

    let success = receipts.iter().filter(|r| r.success).count();
    assert!(success > 0, "at least some ML-DSA transactions must succeed");

    transactions.len() as f64 / elapsed.as_secs_f64()
}

/// Generate unsigned transactions between random-ish accounts.
fn generate_unsigned_transactions(count: usize) -> Vec<Transaction> {
    let num_agents = 10_000u32.min(count as u32);
    let txs_per_agent = (count as u32) / num_agents;

    (0..num_agents)
        .flat_map(|agent_id| {
            let from = hash_bytes(&agent_id.to_le_bytes());
            let to = hash_bytes(&((agent_id + 1) % num_agents).to_le_bytes());
            (0..txs_per_agent as u64).map(move |nonce| {
                Transaction::new_transfer(from, to, 1, nonce)
            })
        })
        .collect()
}

/// Create a StateDB pre-funded for unsigned transactions.
fn state_for_unsigned(count: usize) -> StateDB {
    let num_agents = 10_000u32.min(count as u32);
    let accounts: Vec<(Hash256, u64)> = (0..num_agents)
        .map(|i| (hash_bytes(&i.to_le_bytes()), u64::MAX / 2))
        .collect();
    StateDB::with_genesis(&accounts)
}

/// Create a StateDB pre-funded for signed transactions.
fn state_for_signed(keypairs: &[KeyPair]) -> StateDB {
    let accounts: Vec<(Hash256, u64)> = keypairs
        .iter()
        .map(|kp| (kp.address(), u64::MAX / 2))
        .collect();
    StateDB::with_genesis(&accounts)
}

// ─────────────────────────────────────────────────────────────────────────────
//  Benchmark Scenarios
// ─────────────────────────────────────────────────────────────────────────────

struct BenchResults {
    tx_count: usize,
    num_threads: usize,
    // Ed25519 crypto micro-benchmarks
    keygen_rate: f64,
    signing_rate: f64,
    individual_verify_rate: f64,
    batch_verify_rate: f64,
    batch_speedup: f64,
    // ML-DSA-65 crypto micro-benchmarks
    ml_dsa_keygen_rate: f64,
    ml_dsa_signing_rate: f64,
    ml_dsa_batch_verify_rate: f64,
    // End-to-end execution
    unsigned_tps: f64,
    signed_tps: f64,
    batch_verified_tps: f64,
    ml_dsa_batch_tps: f64,
    sig_overhead_pct: f64,
    batch_overhead_pct: f64,
}

/// Benchmark: individual signature verification.
fn bench_individual_verify(transactions: &[Transaction]) -> f64 {
    let start = Instant::now();
    let verified: usize = transactions
        .par_iter()
        .filter(|tx| tx.verify_signature().is_ok())
        .count();
    let elapsed = start.elapsed();
    assert_eq!(
        verified,
        transactions.len(),
        "all signatures must verify"
    );
    transactions.len() as f64 / elapsed.as_secs_f64()
}

/// Benchmark: batch verification using ed25519_dalek batch_verify.
/// Uses parallel chunked batch verification for maximum throughput.
fn bench_batch_verify(transactions: &[Transaction]) -> f64 {
    // Extract components for batch verification
    let mut messages: Vec<Vec<u8>> = Vec::with_capacity(transactions.len());
    let mut signatures: Vec<ed25519_dalek::Signature> = Vec::with_capacity(transactions.len());
    let mut verifying_keys: Vec<ed25519_dalek::VerifyingKey> =
        Vec::with_capacity(transactions.len());

    for tx in transactions {
        messages.push(tx.hash.as_bytes().to_vec());
        match &tx.signature {
            arc_crypto::Signature::Ed25519 {
                public_key,
                signature,
            } => {
                let vk = ed25519_dalek::VerifyingKey::from_bytes(public_key)
                    .expect("valid public key");
                let sig_bytes: [u8; 64] = signature.as_slice().try_into().expect("64-byte sig");
                let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
                verifying_keys.push(vk);
                signatures.push(sig);
            }
            _ => panic!("expected Ed25519 signatures only"),
        }
    }

    let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();

    // Use larger chunks and parallelize across Rayon threads.
    // ed25519-dalek batch_verify uses multi-scalar multiplication internally,
    // giving ~2x speedup per batch. We chunk at 4096 and parallelize the chunks.
    let chunk_size = 4096;
    let n = transactions.len();
    let num_chunks = (n + chunk_size - 1) / chunk_size;

    let start = Instant::now();

    // Build chunk indices and verify in parallel
    let chunks: Vec<(usize, usize)> = (0..num_chunks)
        .map(|i| {
            let s = i * chunk_size;
            let e = (s + chunk_size).min(n);
            (s, e)
        })
        .collect();

    chunks.par_iter().for_each(|&(s, e)| {
        batch_verify_ed25519(
            &msg_refs[s..e],
            &signatures[s..e],
            &verifying_keys[s..e],
        )
        .expect("batch verify must succeed");
    });

    let elapsed = start.elapsed();
    n as f64 / elapsed.as_secs_f64()
}

/// Benchmark: unsigned block execution (baseline).
fn bench_unsigned_execution(count: usize) -> f64 {
    let state = state_for_unsigned(count);
    let transactions = generate_unsigned_transactions(count);

    let validator = hash_bytes(&[0u8]);
    let start = Instant::now();
    let (_, receipts) = state.execute_block(&transactions, validator).unwrap();
    let elapsed = start.elapsed();

    let success = receipts.iter().filter(|r| r.success).count();
    assert!(
        success > 0,
        "at least some unsigned transactions must succeed"
    );

    count as f64 / elapsed.as_secs_f64()
}

/// Benchmark: signed block execution (verify each individually during execution).
fn bench_signed_execution(transactions: &[Transaction], keypairs: &[KeyPair]) -> f64 {
    let state = state_for_signed(keypairs);
    let validator = keypairs[0].address();

    let start = Instant::now();
    let (_, receipts) = state
        .execute_block_verified(transactions, validator)
        .unwrap();
    let elapsed = start.elapsed();

    let success = receipts.iter().filter(|r| r.success).count();
    assert!(
        success > 0,
        "at least some signed transactions must succeed (got {}/{})",
        success,
        receipts.len()
    );

    transactions.len() as f64 / elapsed.as_secs_f64()
}

/// Benchmark: batch-verified block execution.
/// First batch-verify all signatures, then execute without per-tx sig checks.
fn bench_batch_verified_execution(
    transactions: &[Transaction],
    keypairs: &[KeyPair],
) -> f64 {
    let state = state_for_signed(keypairs);
    let validator = keypairs[0].address();

    let start = Instant::now();

    // Step 1: Batch verify all signatures up front
    let mut messages: Vec<Vec<u8>> = Vec::with_capacity(transactions.len());
    let mut sigs: Vec<ed25519_dalek::Signature> = Vec::with_capacity(transactions.len());
    let mut vks: Vec<ed25519_dalek::VerifyingKey> = Vec::with_capacity(transactions.len());

    for tx in transactions {
        messages.push(tx.hash.as_bytes().to_vec());
        match &tx.signature {
            arc_crypto::Signature::Ed25519 {
                public_key,
                signature,
            } => {
                let vk = ed25519_dalek::VerifyingKey::from_bytes(public_key)
                    .expect("valid public key");
                let sig_bytes: [u8; 64] = signature.as_slice().try_into().expect("64-byte sig");
                let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
                vks.push(vk);
                sigs.push(sig);
            }
            _ => panic!("expected Ed25519 signatures only"),
        }
    }

    let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();
    let chunk_size = 4096;
    let n = transactions.len();
    let num_chunks = (n + chunk_size - 1) / chunk_size;

    let chunks: Vec<(usize, usize)> = (0..num_chunks)
        .map(|i| {
            let s = i * chunk_size;
            let e = (s + chunk_size).min(n);
            (s, e)
        })
        .collect();

    chunks.par_iter().for_each(|&(s, e)| {
        batch_verify_ed25519(
            &msg_refs[s..e],
            &sigs[s..e],
            &vks[s..e],
        )
        .expect("batch verify must succeed");
    });

    // Step 2: Execute block WITHOUT per-tx signature verification
    // (signatures already verified in batch above)
    let (_, receipts) = state.execute_block(transactions, validator).unwrap();

    let elapsed = start.elapsed();

    let success = receipts.iter().filter(|r| r.success).count();
    assert!(
        success > 0,
        "at least some batch-verified transactions must succeed"
    );

    transactions.len() as f64 / elapsed.as_secs_f64()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Output
// ─────────────────────────────────────────────────────────────────────────────

fn print_results(results: &BenchResults) {
    let w = 72;
    let line = "=".repeat(w);
    let dash = "-".repeat(w);

    println!();
    println!("{}", line);
    println!(" ARC Chain Benchmark — Classical vs Quantum-Proof Signatures");
    println!("{}", line);
    println!(
        " Transactions     : {}",
        format_number(results.tx_count)
    );
    println!(" Threads          : {} (Rayon)", results.num_threads);
    println!("{}", dash);
    println!(" ED25519 (CLASSICAL)");
    println!("{}", dash);
    println!(
        " Key Generation   : {:>12} keys/sec",
        format_rate(results.keygen_rate)
    );
    println!(
        " Signing          : {:>12} sigs/sec",
        format_rate(results.signing_rate)
    );
    println!(
        " Individual Verify: {:>12} verifies/sec",
        format_rate(results.individual_verify_rate)
    );
    println!(
        " Batch Verify     : {:>12} verifies/sec  ({:.1}x speedup)",
        format_rate(results.batch_verify_rate),
        results.batch_speedup
    );
    println!("{}", dash);
    println!(" ML-DSA-65 (QUANTUM-PROOF — NIST FIPS 204)");
    println!("{}", dash);
    println!(
        " Key Generation   : {:>12} keys/sec",
        format_rate(results.ml_dsa_keygen_rate)
    );
    println!(
        " Signing          : {:>12} sigs/sec",
        format_rate(results.ml_dsa_signing_rate)
    );
    println!(
        " Batch Verify     : {:>12} verifies/sec  (Rayon parallel)",
        format_rate(results.ml_dsa_batch_verify_rate)
    );
    println!("{}", dash);
    println!(" END-TO-END BLOCK EXECUTION");
    println!("{}", dash);
    println!(
        " Unsigned (no sig)     : {:>12} TPS  (baseline)",
        format_rate(results.unsigned_tps)
    );
    println!(
        " Ed25519 (per-tx)      : {:>12} TPS",
        format_rate(results.signed_tps)
    );
    println!(
        " Ed25519 (batch)       : {:>12} TPS",
        format_rate(results.batch_verified_tps)
    );
    println!(
        " ML-DSA-65 (batch)     : {:>12} TPS  (QUANTUM-PROOF)",
        format_rate(results.ml_dsa_batch_tps)
    );
    println!("{}", dash);
    println!(" QUANTUM vs CLASSICAL");
    println!("{}", dash);
    let ratio = if results.ml_dsa_batch_tps > 0.0 && results.batch_verified_tps > 0.0 {
        results.batch_verified_tps / results.ml_dsa_batch_tps
    } else {
        0.0
    };
    println!(
        " Ed25519 batch / ML-DSA batch : {:.2}x  (classical is {:.2}x {})",
        ratio,
        if ratio >= 1.0 { ratio } else { 1.0 / ratio },
        if ratio >= 1.0 { "faster" } else { "slower" }
    );
    println!(
        " Quantum security cost        : {:.1}%  overhead vs Ed25519 batch",
        if results.batch_verified_tps > 0.0 {
            ((results.batch_verified_tps - results.ml_dsa_batch_tps) / results.batch_verified_tps) * 100.0
        } else {
            0.0
        }
    );
    println!("{}", dash);
    println!(" COMPARISON WITH OTHER L1 CHAINS");
    println!("{}", dash);
    let solana_tps = 65_000.0f64;
    println!(
        " Solana (theoretical max)      : {:>10} TPS",
        format_rate(solana_tps)
    );
    println!(
        " ARC (unsigned baseline)       : {:>10} TPS  ({:.1}x Solana)",
        format_rate(results.unsigned_tps),
        results.unsigned_tps / solana_tps
    );
    println!(
        " ARC (Ed25519 batch)           : {:>10} TPS  ({:.1}x Solana)",
        format_rate(results.batch_verified_tps),
        results.batch_verified_tps / solana_tps
    );
    println!(
        " ARC (ML-DSA-65 quantum-proof) : {:>10} TPS  ({:.1}x Solana)",
        format_rate(results.ml_dsa_batch_tps),
        results.ml_dsa_batch_tps / solana_tps
    );
    println!("{}", line);
    println!();
}

// ─────────────────────────────────────────────────────────────────────────────
//  Main
// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    // Configure thread pool
    if args.threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(args.threads)
            .build_global()
            .ok();
    }

    let num_threads = rayon::current_num_threads();
    let n = args.txs;

    println!();
    println!("================================================================");
    println!(" ARC Chain — Signature Benchmark (Classical + Quantum-Proof)");
    println!(" {} transactions, {} threads", format_number(n), num_threads);
    println!("================================================================");
    println!();

    match args.mode.as_str() {
        "unsigned" => {
            println!("[1/1] Unsigned block execution (baseline)...");
            let tps = bench_unsigned_execution(n);
            println!("  => {:>12} TPS (unsigned baseline)", format_rate(tps));
            println!();
        }

        "signed" => {
            println!("[1/3] Generating {} signed transactions...", format_number(n));
            let (txs, keypairs, keygen_time, sign_time) = generate_signed_transactions(n);
            println!(
                "  Key generation:  {:.2}s ({} keys/sec)",
                keygen_time.as_secs_f64(),
                format_rate(n as f64 / keygen_time.as_secs_f64())
            );
            println!(
                "  Signing:         {:.2}s ({} sigs/sec)",
                sign_time.as_secs_f64(),
                format_rate(n as f64 / sign_time.as_secs_f64())
            );
            println!();

            println!("[2/3] Individual signature verification...");
            let verify_rate = bench_individual_verify(&txs);
            println!("  => {} verifies/sec", format_rate(verify_rate));
            println!();

            println!("[3/3] Signed block execution (per-tx verification)...");
            let tps = bench_signed_execution(&txs, &keypairs);
            println!("  => {} TPS (with per-tx sig verification)", format_rate(tps));
            println!();
        }

        "batch-verified" => {
            println!("[1/3] Generating {} signed transactions...", format_number(n));
            let (txs, keypairs, keygen_time, sign_time) = generate_signed_transactions(n);
            println!(
                "  Key generation:  {:.2}s ({} keys/sec)",
                keygen_time.as_secs_f64(),
                format_rate(n as f64 / keygen_time.as_secs_f64())
            );
            println!(
                "  Signing:         {:.2}s ({} sigs/sec)",
                sign_time.as_secs_f64(),
                format_rate(n as f64 / sign_time.as_secs_f64())
            );
            println!();

            println!("[2/3] Batch signature verification...");
            let batch_rate = bench_batch_verify(&txs);
            println!("  => {} verifies/sec", format_rate(batch_rate));
            println!();

            println!("[3/3] Batch-verified block execution...");
            let tps = bench_batch_verified_execution(&txs, &keypairs);
            println!("  => {} TPS (batch verify + execute)", format_rate(tps));
            println!();
        }

        "ml-dsa" => {
            println!("[1/3] Generating {} ML-DSA-65 keypairs + signed transactions...", format_number(n));
            let (ml_txs, ml_keypairs, ml_keygen_time, ml_sign_time) = generate_ml_dsa_transactions(n);
            println!(
                "  Key generation:  {:.2}s ({} keys/sec)",
                ml_keygen_time.as_secs_f64(),
                format_rate(n as f64 / ml_keygen_time.as_secs_f64())
            );
            println!(
                "  Signing:         {:.2}s ({} sigs/sec)",
                ml_sign_time.as_secs_f64(),
                format_rate(n as f64 / ml_sign_time.as_secs_f64())
            );
            println!();

            println!("[2/3] ML-DSA batch verification (Rayon parallel)...");
            let ml_batch_rate = bench_ml_dsa_batch_verify(&ml_txs);
            println!("  => {} verifies/sec", format_rate(ml_batch_rate));
            println!();

            println!("[3/3] ML-DSA batch-verified block execution...");
            let ml_tps = bench_ml_dsa_batch_execution(&ml_txs, &ml_keypairs);
            println!("  => {} TPS (quantum-proof)", format_rate(ml_tps));
            println!();
        }

        "all" | _ => {
            // ── Ed25519: Generate signed transactions ────────────────
            println!("[1/9] Generating {} Ed25519 keypairs + signed transactions...", format_number(n));
            let (txs, keypairs, keygen_time, sign_time) = generate_signed_transactions(n);
            let keygen_rate = n as f64 / keygen_time.as_secs_f64();
            let signing_rate = n as f64 / sign_time.as_secs_f64();
            println!(
                "  Key generation:  {:.2}s ({} keys/sec)",
                keygen_time.as_secs_f64(),
                format_rate(keygen_rate)
            );
            println!(
                "  Signing:         {:.2}s ({} sigs/sec)",
                sign_time.as_secs_f64(),
                format_rate(signing_rate)
            );
            println!();

            // ── Ed25519: Individual verification ─────────────────────
            println!("[2/9] Ed25519 individual signature verification...");
            let individual_verify_rate = bench_individual_verify(&txs);
            println!(
                "  => {} verifies/sec",
                format_rate(individual_verify_rate)
            );
            println!();

            // ── Ed25519: Batch verification ──────────────────────────
            println!("[3/9] Ed25519 batch verification (multi-scalar multiplication)...");
            let batch_verify_rate = bench_batch_verify(&txs);
            let batch_speedup = batch_verify_rate / individual_verify_rate;
            println!(
                "  => {} verifies/sec ({:.1}x speedup vs individual)",
                format_rate(batch_verify_rate),
                batch_speedup
            );
            println!();

            // ── Unsigned block execution ─────────────────────────────
            println!("[4/9] Unsigned block execution (baseline, no sig checks)...");
            let unsigned_tps = bench_unsigned_execution(n);
            println!("  => {} TPS", format_rate(unsigned_tps));
            println!();

            // ── Ed25519: Signed block execution ──────────────────────
            println!("[5/9] Ed25519 signed block execution (per-tx verification)...");
            let signed_tps = bench_signed_execution(&txs, &keypairs);
            println!("  => {} TPS", format_rate(signed_tps));
            println!();

            // ── Ed25519: Batch-verified block execution ──────────────
            println!("[6/9] Ed25519 batch-verified block execution...");
            let batch_verified_tps = bench_batch_verified_execution(&txs, &keypairs);
            println!("  => {} TPS", format_rate(batch_verified_tps));
            println!();

            // ── ML-DSA-65: Generate quantum-proof transactions ───────
            println!("[7/9] Generating {} ML-DSA-65 keypairs + signed transactions...", format_number(n));
            let (ml_txs, ml_keypairs, ml_keygen_time, ml_sign_time) = generate_ml_dsa_transactions(n);
            let ml_dsa_keygen_rate = n as f64 / ml_keygen_time.as_secs_f64();
            let ml_dsa_signing_rate = n as f64 / ml_sign_time.as_secs_f64();
            println!(
                "  Key generation:  {:.2}s ({} keys/sec)",
                ml_keygen_time.as_secs_f64(),
                format_rate(ml_dsa_keygen_rate)
            );
            println!(
                "  Signing:         {:.2}s ({} sigs/sec)",
                ml_sign_time.as_secs_f64(),
                format_rate(ml_dsa_signing_rate)
            );
            println!();

            // ── ML-DSA-65: Batch verification ────────────────────────
            println!("[8/9] ML-DSA-65 batch verification (Rayon parallel)...");
            let ml_dsa_batch_verify_rate = bench_ml_dsa_batch_verify(&ml_txs);
            println!("  => {} verifies/sec", format_rate(ml_dsa_batch_verify_rate));
            println!();

            // ── ML-DSA-65: Batch-verified block execution ────────────
            println!("[9/9] ML-DSA-65 batch-verified block execution...");
            let ml_dsa_batch_tps = bench_ml_dsa_batch_execution(&ml_txs, &ml_keypairs);
            println!("  => {} TPS (quantum-proof)", format_rate(ml_dsa_batch_tps));
            println!();

            // ── Compute overhead ─────────────────────────────────────
            let sig_overhead_pct = if unsigned_tps > 0.0 {
                ((unsigned_tps - signed_tps) / unsigned_tps) * 100.0
            } else {
                0.0
            };
            let batch_overhead_pct = if unsigned_tps > 0.0 {
                ((unsigned_tps - batch_verified_tps) / unsigned_tps) * 100.0
            } else {
                0.0
            };

            let results = BenchResults {
                tx_count: n,
                num_threads,
                keygen_rate,
                signing_rate,
                individual_verify_rate,
                batch_verify_rate,
                batch_speedup,
                ml_dsa_keygen_rate,
                ml_dsa_signing_rate,
                ml_dsa_batch_verify_rate,
                unsigned_tps,
                signed_tps,
                batch_verified_tps,
                ml_dsa_batch_tps,
                sig_overhead_pct,
                batch_overhead_pct,
            };

            print_results(&results);
        }
    }
}
