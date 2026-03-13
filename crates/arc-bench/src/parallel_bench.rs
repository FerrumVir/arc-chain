//! ARC Chain — Parallel Execution Benchmark
//!
//! Compares all three execution modes side-by-side:
//!   1. Sequential (baseline)
//!   2. Block-STM (optimistic parallel)
//!   3. Block-STM + State Coalescing (parallel + batched I/O)
//!
//! Also benchmarks GPU Ed25519 verification vs CPU.

use arc_crypto::signature::{benchmark_address, benchmark_keypair};
use arc_crypto::Hash256;
use arc_node::pipeline::{Pipeline, PipelineBatch, PipelineConfig, ExecutionMode, VerifyMode};
use arc_state::StateDB;
use arc_types::Transaction;
use clap::Parser;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "arc-bench-parallel",
    about = "ARC Chain — Sequential vs Block-STM vs Coalesced Benchmark"
)]
struct Args {
    /// Total transactions to process per mode.
    #[arg(long, default_value = "100000")]
    txs: usize,

    /// Batch size (transactions per pipeline submission).
    #[arg(long, default_value = "10000")]
    batch: usize,

    /// Number of unique senders (more senders = less contention = more parallelism).
    #[arg(long, default_value = "50")]
    senders: u8,
}

fn format_tps(tps: f64) -> String {
    if tps >= 1_000_000.0 {
        format!("{:.2}M", tps / 1_000_000.0)
    } else if tps >= 1_000.0 {
        format!("{:.1}K", tps / 1_000.0)
    } else {
        format!("{:.0}", tps)
    }
}

/// Pre-sign a batch of transactions using real Ed25519 keypairs.
fn presign_transactions(
    sender_count: u8,
    total_txs: usize,
) -> (Vec<Transaction>, Vec<(Hash256, u64)>) {
    let keypairs: Vec<_> = (0..sender_count)
        .map(|i| (benchmark_keypair(i), benchmark_address(i)))
        .collect();

    let receivers: Vec<Hash256> = (200u8..=255)
        .map(benchmark_address)
        .collect();

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

/// Run one benchmark pass with a given execution mode.
/// Returns (success_count, elapsed, mode_name).
fn run_mode(
    mode_name: &str,
    config: PipelineConfig,
    transactions: &[Transaction],
    genesis: &[(Hash256, u64)],
    batch_size: usize,
) -> (usize, Duration) {
    let state = Arc::new(StateDB::with_genesis(genesis));
    let pipeline = Pipeline::with_config(Arc::clone(&state), config);
    let producer = benchmark_address(255);

    let pipeline_start = Instant::now();
    let mut batches_submitted = 0usize;
    let mut total_success = 0usize;

    let num_batches = (transactions.len() + batch_size - 1) / batch_size;

    for chunk in transactions.chunks(batch_size) {
        pipeline
            .submit(PipelineBatch {
                transactions: chunk.to_vec(),
                producer,
            })
            .expect("pipeline submit");
        batches_submitted += 1;
    }

    // Collect results
    let timeout = Duration::from_secs(120);
    let deadline = Instant::now() + timeout;
    let mut results_received = 0usize;

    while results_received < batches_submitted {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            eprintln!("  {}: TIMEOUT — got {}/{} results", mode_name, results_received, batches_submitted);
            break;
        }

        if let Some(result) = pipeline.try_recv() {
            total_success += result.success_count;
            results_received += 1;
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    let elapsed = pipeline_start.elapsed();
    (total_success, elapsed)
}

/// Benchmark GPU (Metal) vs CPU Ed25519 batch verification.
fn bench_gpu_verify(count: usize) {
    use arc_crypto::signature::benchmark_keypair;
    use arc_gpu::metal_verify::{MetalVerifier, VerifyTask};
    use ed25519_dalek::Signer;

    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  GPU vs CPU Ed25519 Batch Verification ({} signatures)", count);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // Generate signed messages as VerifyTasks
    let mut tasks = Vec::with_capacity(count);

    for i in 0..count {
        let sk = benchmark_keypair((i % 200) as u8);
        let msg = format!("benchmark-message-{}", i);
        let sig = sk.sign(msg.as_bytes());
        let vk = sk.verifying_key();

        tasks.push(VerifyTask {
            message: msg.into_bytes(),
            public_key: *vk.as_bytes(),
            signature: sig.to_bytes(),
        });
    }

    let mut verifier = MetalVerifier::new();
    println!("    Metal GPU:    {}", if verifier.is_gpu_available() { "DETECTED" } else { "not available (CPU fallback)" });

    // CPU-only path
    let cpu_result = verifier.batch_verify_cpu(&tasks);
    let cpu_rate = count as f64 / (cpu_result.elapsed_us as f64 / 1_000_000.0);
    println!("    CPU (rayon):  {} sigs in {:.3}s = {} verif/sec  ({} valid)",
        count, cpu_result.elapsed_us as f64 / 1_000_000.0,
        format_tps(cpu_rate), cpu_result.valid);

    // GPU path (Metal on Apple Silicon, CPU fallback otherwise)
    verifier.reset_stats();
    let gpu_result = verifier.batch_verify(&tasks);
    let gpu_rate = count as f64 / (gpu_result.elapsed_us as f64 / 1_000_000.0);
    println!("    GPU (Metal):  {} sigs in {:.3}s = {} verif/sec  ({} valid, gpu_used={})",
        count, gpu_result.elapsed_us as f64 / 1_000_000.0,
        format_tps(gpu_rate), gpu_result.valid, gpu_result.used_gpu);

    let speedup = gpu_rate / cpu_rate;
    println!("    Speedup:      {:.2}x {}", speedup,
        if speedup > 1.0 { "(GPU faster)" } else { "(CPU faster)" }
    );
    println!("    All valid:    {}", if cpu_result.valid == count && gpu_result.valid == count { "YES" } else { "MISMATCH" });

    let stats = verifier.stats();
    println!("    Stats:        GPU batches={}, CPU batches={}", stats.gpu_batches, stats.cpu_batches);
}

fn main() {
    let args = Args::parse();

    println!("================================================================");
    println!(" ARC Chain — FULL PIPELINE Benchmark (CPU + GPU)");
    println!("================================================================");
    println!();
    println!("  Tests EVERY combination of verify + execute mode:");
    println!("    Verify:  CPU (rayon)  vs  GPU (Metal)");
    println!("    Execute: Sequential   vs  Block-STM   vs  Block-STM+Coalesce");
    println!();
    println!("  Full pipeline: Receive → Verify → Execute → Commit");
    println!();
    println!("  Config:");
    println!("    Transactions:   {}", args.txs);
    println!("    Batch size:     {}", args.batch);
    println!("    Senders:        {} (less = more contention)", args.senders);
    println!("    CPU cores:      {}", rayon::current_num_threads());

    // Pre-sign transactions once
    println!();
    print!("  Pre-signing {} transactions with Ed25519... ", args.txs);
    let sign_start = Instant::now();
    let (transactions, genesis) = presign_transactions(args.senders, args.txs);
    let sign_elapsed = sign_start.elapsed();
    println!("done in {:.2}s ({} sigs/sec)",
        sign_elapsed.as_secs_f64(),
        format_tps(args.txs as f64 / sign_elapsed.as_secs_f64()));

    // Define all 6 mode combinations
    struct BenchMode {
        name: &'static str,
        verify: VerifyMode,
        exec: ExecutionMode,
        coalesce: bool,
    }

    let modes = vec![
        BenchMode { name: "CPU verify + Sequential exec",          verify: VerifyMode::Cpu,      exec: ExecutionMode::Sequential, coalesce: false },
        BenchMode { name: "CPU verify + Block-STM exec",           verify: VerifyMode::Cpu,      exec: ExecutionMode::BlockSTM,   coalesce: false },
        BenchMode { name: "CPU verify + Block-STM + Coalesce",     verify: VerifyMode::Cpu,      exec: ExecutionMode::BlockSTM,   coalesce: true },
        BenchMode { name: "GPU verify + Sequential exec",          verify: VerifyMode::GpuMetal, exec: ExecutionMode::Sequential, coalesce: false },
        BenchMode { name: "GPU verify + Block-STM exec",           verify: VerifyMode::GpuMetal, exec: ExecutionMode::BlockSTM,   coalesce: false },
        BenchMode { name: "GPU verify + Block-STM + Coalesce",     verify: VerifyMode::GpuMetal, exec: ExecutionMode::BlockSTM,   coalesce: true },
    ];

    let mut results: Vec<(&str, usize, f64)> = Vec::new();

    for (i, mode) in modes.iter().enumerate() {
        println!();
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  MODE {}: {}", i + 1, mode.name);
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        let config = PipelineConfig {
            execution_mode: mode.exec,
            verify_mode: mode.verify,
            coalesce_enabled: mode.coalesce,
            batch_size: args.batch,
            ..Default::default()
        };
        let (success, elapsed) = run_mode(mode.name, config, &transactions, &genesis, args.batch);
        let tps = success as f64 / elapsed.as_secs_f64();

        println!("    Success:     {}/{}", success, args.txs);
        println!("    Time:        {:.3}s", elapsed.as_secs_f64());
        println!("    Throughput:  {} TPS", format_tps(tps));

        results.push((mode.name, success, tps));
    }

    // ── Standalone GPU vs CPU verification ───────────────────────────────
    bench_gpu_verify(args.txs);

    // ── Summary ─────────────────────────────────────────────────────────────
    let baseline_tps = results[0].2;

    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  SUMMARY");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!("    Mode                                     TPS      Speedup   ETH-weighted");
    println!("    ──────────────────────────────────────────────────────────────────────────");
    for (name, _success, tps) in &results {
        let speedup = tps / baseline_tps;
        let weighted = tps / 3.93;
        println!("    {:<42} {:>8}  {:.2}x      {:>8}",
            name, format_tps(*tps), speedup, format_tps(weighted));
    }
    println!();

    let best_tps = results.iter().map(|r| r.2).fold(0.0f64, f64::max);
    let best_weighted = best_tps / 3.93;
    println!("    Best single-node (raw):       {} TPS", format_tps(best_tps));
    println!("    Best single-node (weighted):  {} TPS", format_tps(best_weighted));
    println!("    vs Ethereum (~15 TPS):        {:.0}x faster", best_weighted / 15.0);
    println!();

    // Projections using best mode
    println!("    MULTI-NODE PROJECTIONS (propose-verify, ETH-weighted):");
    println!("      10 nodes:   {} TPS", format_tps(best_tps * 10.0 * 0.9 / 3.93));
    println!("      50 nodes:   {} TPS", format_tps(best_tps * 50.0 * 0.85 / 3.93));
    println!("      100 nodes:  {} TPS", format_tps(best_tps * 100.0 * 0.8 / 3.93));
    println!("      500 nodes:  {} TPS", format_tps(best_tps * 500.0 * 0.7 / 3.93));

    println!();
    println!("================================================================");
}
