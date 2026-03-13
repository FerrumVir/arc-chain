//! ARC Chain — Production Pipeline Benchmark (HONEST)
//!
//! This benchmark measures REAL production throughput through the actual
//! 4-stage pipeline: Receive → Verify → Execute → Commit.
//!
//! Unlike other benchmarks that skip signature verification or measure
//! components in isolation, this runs the COMPLETE production path:
//!   1. Pre-sign transactions with real Ed25519 keypairs
//!   2. Push signed batches through the Pipeline
//!   3. Pipeline verifies hash integrity
//!   4. Pipeline batch-verifies Ed25519 signatures (CPU-parallel, NOT GPU)
//!   5. Pipeline executes state transitions (balance transfers)
//!   6. Pipeline commits blocks (Merkle tree, WAL)
//!   7. Measure end-to-end throughput
//!
//! Multi-proposer mode simulates N independent proposer nodes, each with
//! its own state and pipeline, processing non-overlapping transaction sets.
//! This is the propose-verify architecture: each proposer executes its own
//! partition; in production, verifiers would check state diffs.
//!
//! Usage:
//!   arc-bench-production [--txs 100000] [--batch 10000] [--proposers 1]

use arc_crypto::signature::{benchmark_address, benchmark_keypair};
use arc_crypto::Hash256;
use arc_node::pipeline::{Pipeline, PipelineBatch};
use arc_state::StateDB;
use arc_types::Transaction;
use clap::Parser;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "arc-bench-production",
    about = "ARC Chain — Honest Production Pipeline Benchmark"
)]
struct Args {
    /// Total transactions per proposer to process.
    #[arg(long, default_value = "100000")]
    txs: usize,

    /// Batch size (transactions per pipeline submission).
    #[arg(long, default_value = "10000")]
    batch: usize,

    /// Number of parallel proposer nodes to simulate.
    #[arg(long, default_value = "1")]
    proposers: usize,

    /// Number of Rayon threads (0 = auto-detect).
    #[arg(long, default_value = "0")]
    threads: usize,
}

fn format_tps(tps: f64) -> String {
    if tps >= 1_000_000_000.0 {
        format!("{:.2}B", tps / 1_000_000_000.0)
    } else if tps >= 1_000_000.0 {
        format!("{:.2}M", tps / 1_000_000.0)
    } else if tps >= 1_000.0 {
        format!("{:.1}K", tps / 1_000.0)
    } else {
        format!("{:.0}", tps)
    }
}

/// Pre-sign a batch of transactions for a given proposer's sender partition.
/// Each proposer gets senders [start..start+count), receivers from a separate range.
fn presign_transactions(
    sender_start: u8,
    sender_count: u8,
    total_txs: usize,
) -> (Vec<Transaction>, Vec<(Hash256, u64)>) {
    let keypairs: Vec<_> = (sender_start..sender_start + sender_count)
        .map(|i| (benchmark_keypair(i), benchmark_address(i)))
        .collect();

    // Receiver addresses (don't overlap with senders)
    let receivers: Vec<Hash256> = (200u8..=255)
        .map(benchmark_address)
        .collect();

    // Genesis accounts: fund all senders and receivers
    let mut genesis: Vec<(Hash256, u64)> = keypairs
        .iter()
        .map(|(_, addr)| (*addr, u64::MAX / 4))
        .collect();
    for r in &receivers {
        genesis.push((*r, 0));
    }

    let mut transactions = Vec::with_capacity(total_txs);
    let mut nonces = vec![0u64; keypairs.len()];

    for tx_idx in 0..total_txs {
        let kp_idx = tx_idx % keypairs.len();
        let (sk, sender) = &keypairs[kp_idx];
        let receiver = receivers[tx_idx % receivers.len()];
        let nonce = nonces[kp_idx];

        let mut tx = Transaction::new_transfer(*sender, receiver, 1, nonce);

        // Real Ed25519 signing
        use ed25519_dalek::Signer;
        let sig = sk.sign(tx.hash.as_bytes());
        let vk = sk.verifying_key();
        tx.signature = arc_crypto::signature::Signature::Ed25519 {
            public_key: *vk.as_bytes(),
            signature: sig.to_bytes().to_vec(),
        };

        nonces[kp_idx] += 1;
        transactions.push(tx);
    }

    (transactions, genesis)
}

/// Run a single proposer's pipeline benchmark.
/// Returns (total_txs_processed, total_success, elapsed).
fn run_single_proposer(
    proposer_id: usize,
    total_txs: usize,
    batch_size: usize,
) -> (usize, usize, Duration) {
    // Each proposer gets its own sender partition (non-overlapping)
    let senders_per_proposer = 10u8;
    let sender_start = (proposer_id as u8) * senders_per_proposer;

    // Pre-sign all transactions
    let sign_start = Instant::now();
    let (transactions, genesis) = presign_transactions(sender_start, senders_per_proposer, total_txs);
    let sign_elapsed = sign_start.elapsed();

    println!(
        "  Proposer {}: Pre-signed {} txs in {:.2}s ({} sigs/sec)",
        proposer_id,
        transactions.len(),
        sign_elapsed.as_secs_f64(),
        format_tps(transactions.len() as f64 / sign_elapsed.as_secs_f64()),
    );

    // Create state with genesis accounts
    let state = Arc::new(StateDB::with_genesis(&genesis));

    // Create the actual production pipeline
    let pipeline = Pipeline::new(Arc::clone(&state));
    let producer = benchmark_address(255);

    // Feed batches into the pipeline and collect results
    let pipeline_start = Instant::now();
    let mut batches_submitted = 0usize;
    let mut total_processed = 0usize;
    let mut total_success = 0usize;

    // Submit all batches
    let num_batches = (transactions.len() + batch_size - 1) / batch_size;
    let mut tx_iter = transactions.into_iter();

    for _ in 0..num_batches {
        let batch: Vec<Transaction> = tx_iter.by_ref().take(batch_size).collect();
        if batch.is_empty() {
            break;
        }
        pipeline
            .submit(PipelineBatch {
                transactions: batch,
                producer,
            })
            .expect("pipeline submit");
        batches_submitted += 1;
    }

    // Collect all results (blocking with timeout)
    let timeout = Duration::from_secs(120);
    let deadline = Instant::now() + timeout;
    let mut results_received = 0usize;

    while results_received < batches_submitted {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            eprintln!(
                "  Proposer {}: TIMEOUT — got {}/{} results",
                proposer_id, results_received, batches_submitted
            );
            break;
        }

        // Poll with short sleep to avoid busy-waiting
        if let Some(result) = pipeline.try_recv() {
            total_processed += result.tx_count;
            total_success += result.success_count;
            results_received += 1;
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    let pipeline_elapsed = pipeline_start.elapsed();

    (total_processed, total_success, pipeline_elapsed)
}

fn main() {
    let args = Args::parse();

    if args.threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(args.threads)
            .build_global()
            .ok();
    }

    let num_threads = rayon::current_num_threads();
    let gpu = arc_gpu::probe_gpu();

    println!();
    println!("================================================================");
    println!(" ARC Chain — PRODUCTION Pipeline Benchmark (HONEST)");
    println!("================================================================");
    println!();
    println!("  WHAT THIS MEASURES:");
    println!("    Full 4-stage pipeline: Receive → Verify → Execute → Commit");
    println!("    - Real Ed25519 signatures (sign + verify)");
    println!("    - Hash integrity checks (BLAKE3)");
    println!("    - State execution (balance transfers with nonce)");
    println!("    - Merkle tree + WAL commit");
    println!();
    println!("  WHAT IS HONEST:");
    println!("    - Ed25519 verification is CPU-parallel (rayon), NOT GPU");
    println!("    - BLAKE3 hashing is CPU-parallel, GPU shader NOT wired in");
    println!("    - State execution is sequential per-account");
    println!("    - Numbers are MEASURED, not projected");
    println!();
    println!("  System:");
    println!("    CPU cores (Rayon):  {}", num_threads);
    println!("    GPU:                {} ({})", gpu.name, gpu.backend);
    println!("    GPU in pipeline:    NO (not wired into hot path)");
    println!("    Proposers:          {}", args.proposers);
    println!("    TXs per proposer:   {}", args.txs);
    println!("    Batch size:         {}", args.batch);
    println!();

    if args.proposers == 1 {
        // ── Single Proposer ────────────────────────────────────────────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  SINGLE PROPOSER (Full Pipeline)");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        let (processed, success, elapsed) = run_single_proposer(0, args.txs, args.batch);
        let tps = processed as f64 / elapsed.as_secs_f64();

        println!();
        println!("  Results:");
        println!("    Transactions:  {} submitted, {} processed, {} success",
            args.txs, processed, success);
        println!("    Pipeline time: {:.2}s", elapsed.as_secs_f64());
        println!("    Throughput:    {} TPS", format_tps(tps));
        println!();

        // ── Honest projections ─────────────────────────────────────────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  PROJECTIONS (based on measured single-proposer TPS)");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();
        println!("  Your 3 machines as proposers (propose-verify architecture):");
        println!("  Each proposer runs full pipeline on its own tx partition.");
        println!("  Verifiers only check state diffs (O(k), nearly free).");
        println!();

        // Conservative: same TPS per machine (reality varies by hardware)
        let per_machine = tps;
        let three_machine = per_machine * 3.0 * 0.90; // 90% efficiency for network overhead
        println!("    1 machine (this):    {:>12} TPS (measured)", format_tps(per_machine));
        println!("    3 machines (yours):   {:>12} TPS (projected, 90% efficiency)", format_tps(three_machine));
        println!();

        let gap_to_1b = 1_000_000_000.0 / three_machine;
        if three_machine >= 1_000_000_000.0 {
            println!("    STATUS: 1B TPS ACHIEVED");
        } else {
            println!("    Gap to 1B TPS:       {:.0}x more needed", gap_to_1b);
            println!();
            println!("  WHAT WOULD CLOSE THE GAP:");
            println!("    GPU Ed25519 MSM (Metal/CUDA):  ~20-40x on sig verification");
            println!("    GPU BLAKE3 in pipeline:        ~2-5x on hashing");
            println!("    Block-STM parallel execution:  ~2-4x on state execution");
            println!("    Combined theoretical:          ~80-800x");
            println!();
            let gpu_optimistic = three_machine * 80.0;
            let gpu_aggressive = three_machine * 300.0;
            println!("    With GPU optimizations (conservative): {} TPS", format_tps(gpu_optimistic));
            println!("    With GPU optimizations (aggressive):   {} TPS", format_tps(gpu_aggressive));
            println!();
            if gpu_optimistic >= 1_000_000_000.0 {
                println!("    VERDICT: 1B TPS is ACHIEVABLE with GPU compute kernels");
                println!("    BUT those kernels don't exist yet in this codebase.");
                println!("    GPU Ed25519 MSM shader = months of specialized crypto work.");
            } else if gpu_aggressive >= 1_000_000_000.0 {
                println!("    VERDICT: 1B TPS is POSSIBLE but requires aggressive GPU optimization");
                println!("    AND all three optimizations (GPU sigs + GPU hash + Block-STM).");
                println!("    This is best-case, not guaranteed.");
            } else {
                println!("    VERDICT: 1B TPS NOT achievable with 3 consumer machines.");
                println!("    Would need datacenter GPUs (H100s) or more nodes.");
            }
        }

    } else {
        // ── Multi-Proposer Simulation ──────────────────────────────────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  MULTI-PROPOSER SIMULATION ({} proposers)", args.proposers);
        println!("  Each proposer has its own state + pipeline + tx partition.");
        println!("  This simulates the propose-verify architecture locally.");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();

        let overall_start = Instant::now();

        // Run proposers in parallel threads
        let handles: Vec<_> = (0..args.proposers)
            .map(|id| {
                let txs = args.txs;
                let batch = args.batch;
                std::thread::Builder::new()
                    .name(format!("proposer-{}", id))
                    .spawn(move || run_single_proposer(id, txs, batch))
                    .expect("spawn proposer thread")
            })
            .collect();

        let mut results: Vec<(usize, usize, Duration)> = Vec::new();
        for handle in handles {
            results.push(handle.join().expect("proposer thread"));
        }

        let overall_elapsed = overall_start.elapsed();

        println!();
        println!("  Per-Proposer Results:");
        let mut total_processed = 0usize;
        let mut total_success = 0usize;
        let mut per_proposer_tps: Vec<f64> = Vec::new();
        for (id, (processed, success, elapsed)) in results.iter().enumerate() {
            let tps = *processed as f64 / elapsed.as_secs_f64();
            per_proposer_tps.push(tps);
            total_processed += processed;
            total_success += success;
            println!(
                "    Proposer {}: {} processed, {} success, {:.2}s, {} TPS",
                id, processed, success, elapsed.as_secs_f64(), format_tps(tps),
            );
        }

        // Aggregate TPS = total txs processed / wall-clock time
        let aggregate_tps = total_processed as f64 / overall_elapsed.as_secs_f64();
        let sum_individual_tps: f64 = per_proposer_tps.iter().sum();
        let avg_individual_tps = sum_individual_tps / args.proposers as f64;

        println!();
        println!("  Aggregate Results:");
        println!("    Total processed:     {}", total_processed);
        println!("    Total success:       {}", total_success);
        println!("    Wall-clock time:     {:.2}s", overall_elapsed.as_secs_f64());
        println!("    Aggregate TPS:       {} (total / wall-clock)", format_tps(aggregate_tps));
        println!("    Sum individual TPS:  {} (sum of per-proposer)", format_tps(sum_individual_tps));
        println!("    Avg per proposer:    {}", format_tps(avg_individual_tps));
        println!();

        let scaling = aggregate_tps / per_proposer_tps.get(0).copied().unwrap_or(1.0);
        println!("    Scaling efficiency:  {:.1}x from {} proposers ({:.0}% of linear)",
            scaling, args.proposers, (scaling / args.proposers as f64) * 100.0);
    }

    println!();
    println!("================================================================");
    println!(" HONESTY REPORT");
    println!("================================================================");
    println!("  What was REAL in this benchmark:");
    println!("    [x] Ed25519 keypair generation (deterministic)");
    println!("    [x] Ed25519 transaction signing");
    println!("    [x] BLAKE3 hash integrity verification");
    println!("    [x] Ed25519 signature verification (CPU rayon parallel)");
    println!("    [x] State execution (real balance/nonce mutations)");
    println!("    [x] Merkle tree construction");
    println!("    [x] Block commitment");
    println!();
    println!("  What was NOT accelerated:");
    println!("    [ ] GPU Ed25519 verification (function says 'gpu' but runs on CPU)");
    println!("    [ ] GPU BLAKE3 hashing (shader exists, not wired into pipeline)");
    println!("    [ ] GPU Merkle tree construction");
    println!("    [ ] Block-STM optimistic parallel execution");
    println!();
    println!("================================================================");
    println!();
}
