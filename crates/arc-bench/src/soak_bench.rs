//! ARC Chain — Soak / Stress Test Benchmark
//!
//! Runs a sustained stress test for a configurable duration, continuously
//! submitting batches through the full pipeline and reporting live metrics
//! every interval:
//!   - Current / Average / Peak / Min TPS
//!   - Total transactions processed
//!   - Success rate
//!   - Memory usage (RSS)
//!   - Progress bar
//!
//! Usage:
//!   cargo run --release --bin arc-bench-soak -- --duration 300 --batch-size 10000

use arc_crypto::signature::{benchmark_address, benchmark_keypair};
use arc_crypto::Hash256;
use arc_node::pipeline::{ExecutionMode, Pipeline, PipelineBatch, PipelineConfig, VerifyMode};
use arc_state::StateDB;
use arc_types::Transaction;
use clap::Parser;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "arc-bench-soak",
    about = "ARC Chain — Sustained soak / stress test"
)]
struct Args {
    /// Total duration of the soak test in seconds.
    #[arg(long, default_value = "300")]
    duration: u64,

    /// Batch size (transactions per pipeline submission).
    #[arg(long, default_value = "10000")]
    batch_size: usize,

    /// Number of unique senders.
    #[arg(long, default_value = "100")]
    senders: u8,

    /// How often (seconds) to print the live dashboard.
    #[arg(long, default_value = "10")]
    report_interval: u64,
}

// ── Formatting helpers ───────────────────────────────────────────────────────

fn format_tps(tps: f64) -> String {
    if tps >= 1_000_000.0 {
        format!("{:.1}M", tps / 1_000_000.0)
    } else if tps >= 1_000.0 {
        format!("{:.1}K", tps / 1_000.0)
    } else {
        format!("{:.0}", tps)
    }
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{},{:03},{:03}", n / 1_000_000, (n / 1_000) % 1_000, n % 1_000)
    } else if n >= 1_000 {
        format!("{},{:03}", n / 1_000, n % 1_000)
    } else {
        format!("{}", n)
    }
}

fn progress_bar(fraction: f64, width: usize) -> String {
    let filled = (fraction * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!(
        "{}{}  {:.0}%",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
        fraction * 100.0
    )
}

/// Resident set size in bytes (macOS / Linux).
fn rss_bytes() -> u64 {
#[allow(deprecated)]
    #[cfg(target_os = "macos")]
    {
        use std::mem::MaybeUninit;
        unsafe {
            let mut info = MaybeUninit::<libc::mach_task_basic_info_data_t>::zeroed().assume_init();
            let mut count = (std::mem::size_of::<libc::mach_task_basic_info_data_t>()
                / std::mem::size_of::<libc::natural_t>()) as libc::mach_msg_type_number_t;
            let kr = libc::task_info(
                libc::mach_task_self(),
                libc::MACH_TASK_BASIC_INFO,
                &mut info as *mut _ as libc::task_info_t,
                &mut count,
            );
            if kr == libc::KERN_SUCCESS {
                return info.resident_size as u64;
            }
        }
        0
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/self/statm: columns are in pages
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
            if let Some(rss_pages) = statm.split_whitespace().nth(1) {
                if let Ok(pages) = rss_pages.parse::<u64>() {
                    return pages * 4096;
                }
            }
        }
        0
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

fn format_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.1} GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else if b >= 1_024 {
        format!("{:.1} KB", b as f64 / 1_024.0)
    } else {
        format!("{} B", b)
    }
}

// ── Transaction pre-signing ──────────────────────────────────────────────────

fn presign_transactions(
    sender_count: u8,
    total_txs: usize,
) -> (Vec<Transaction>, Vec<(Hash256, u64)>) {
    let keypairs: Vec<_> = (0..sender_count)
        .map(|i| (benchmark_keypair(i), benchmark_address(i)))
        .collect();

    let receivers: Vec<Hash256> = (200u8..=255).map(benchmark_address).collect();

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

// ── Interval metrics ─────────────────────────────────────────────────────────

struct IntervalSnapshot {
    tps: f64,
}

struct SoakMetrics {
    total_txs: u64,
    total_success: u64,
    interval_snapshots: Vec<IntervalSnapshot>,
    peak_tps: f64,
    min_tps: f64,
}

impl SoakMetrics {
    fn new() -> Self {
        Self {
            total_txs: 0,
            total_success: 0,
            interval_snapshots: Vec::new(),
            peak_tps: 0.0,
            min_tps: f64::MAX,
        }
    }

    fn record_interval(&mut self, interval_txs: u64, interval_success: u64, interval_secs: f64) {
        self.total_txs += interval_txs;
        self.total_success += interval_success;

        let tps = interval_txs as f64 / interval_secs;
        if tps > self.peak_tps {
            self.peak_tps = tps;
        }
        if tps < self.min_tps {
            self.min_tps = tps;
        }
        self.interval_snapshots.push(IntervalSnapshot { tps });
    }

    fn avg_tps(&self, elapsed_secs: f64) -> f64 {
        if elapsed_secs > 0.0 {
            self.total_txs as f64 / elapsed_secs
        } else {
            0.0
        }
    }

    fn success_rate(&self) -> f64 {
        if self.total_txs == 0 {
            100.0
        } else {
            self.total_success as f64 / self.total_txs as f64 * 100.0
        }
    }

    fn std_dev(&self) -> f64 {
        if self.interval_snapshots.len() < 2 {
            return 0.0;
        }
        let mean: f64 =
            self.interval_snapshots.iter().map(|s| s.tps).sum::<f64>()
                / self.interval_snapshots.len() as f64;
        let variance: f64 = self
            .interval_snapshots
            .iter()
            .map(|s| {
                let diff = s.tps - mean;
                diff * diff
            })
            .sum::<f64>()
            / (self.interval_snapshots.len() - 1) as f64;
        variance.sqrt()
    }
}

// ── Dashboard printing ───────────────────────────────────────────────────────

const SEPARATOR: &str =
    "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}";

fn print_dashboard(
    elapsed: Duration,
    total_duration: Duration,
    current_tps: f64,
    metrics: &SoakMetrics,
) {
    let elapsed_secs = elapsed.as_secs_f64();
    let total_secs = total_duration.as_secs_f64();
    let fraction = (elapsed_secs / total_secs).min(1.0);

    println!();
    println!("{}", SEPARATOR);
    println!(
        " ARC Chain \u{2014} Soak Test (elapsed: {:.0}s / {:.0}s)",
        elapsed_secs, total_secs
    );
    println!("{}", SEPARATOR);
    println!("  Current TPS:    {}", format_tps(current_tps));
    println!(
        "  Average TPS:    {}",
        format_tps(metrics.avg_tps(elapsed_secs))
    );
    println!("  Peak TPS:       {}", format_tps(metrics.peak_tps));
    println!(
        "  Min TPS:        {}",
        if metrics.min_tps == f64::MAX {
            "N/A".to_string()
        } else {
            format_tps(metrics.min_tps)
        }
    );
    println!(
        "  Total Txs:      {}",
        format_count(metrics.total_txs)
    );
    println!("  Success Rate:   {:.1}%", metrics.success_rate());
    println!("  Memory (RSS):   {}", format_bytes(rss_bytes()));
    println!("  Progress:       {}", progress_bar(fraction, 20));
    println!("{}", SEPARATOR);
}

fn print_final_report(total_duration: Duration, metrics: &SoakMetrics) {
    let elapsed_secs = total_duration.as_secs_f64();
    let avg_tps = metrics.avg_tps(elapsed_secs);
    let std_dev = metrics.std_dev();

    // Pass if success rate >= 99.9% and we processed at least *some* transactions
    let passed = metrics.success_rate() >= 99.9 && metrics.total_txs > 0;

    println!();
    println!("{}", SEPARATOR);
    println!(" SOAK TEST COMPLETE");
    println!("{}", SEPARATOR);
    println!("  Duration:       {:.1}s", elapsed_secs);
    println!(
        "  Total Txs:      {}",
        format_count(metrics.total_txs)
    );
    println!("  Average TPS:    {}", format_tps(avg_tps));
    println!("  Peak TPS:       {}", format_tps(metrics.peak_tps));
    println!(
        "  Min TPS:        {}",
        if metrics.min_tps == f64::MAX {
            "N/A".to_string()
        } else {
            format_tps(metrics.min_tps)
        }
    );
    println!("  Std Dev:        {}", format_tps(std_dev));
    println!("  Success Rate:   {:.1}%", metrics.success_rate());
    println!("  Memory (RSS):   {}", format_bytes(rss_bytes()));
    println!(
        "  Status:         {}",
        if passed {
            "PASS \u{2713}"
        } else {
            "FAIL \u{2717}"
        }
    );
    println!("{}", SEPARATOR);
    println!();
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    let total_duration = Duration::from_secs(args.duration);
    let report_interval = Duration::from_secs(args.report_interval);

    println!();
    println!("{}", SEPARATOR);
    println!(" ARC Chain \u{2014} Soak Test Configuration");
    println!("{}", SEPARATOR);
    println!("  Duration:         {}s", args.duration);
    println!("  Batch size:       {}", args.batch_size);
    println!("  Senders:          {}", args.senders);
    println!("  Report interval:  {}s", args.report_interval);
    println!("  Execution mode:   Block-STM + Coalesce");
    println!("  Verify mode:      CPU (rayon)");
    println!("  CPU cores:        {}", rayon::current_num_threads());
    println!("{}", SEPARATOR);

    // ── Pre-sign one batch worth of transactions (reused every iteration) ────
    println!();
    print!("  Pre-signing {} transactions with Ed25519... ", args.batch_size);
    let sign_start = Instant::now();
    let (transactions, genesis) = presign_transactions(args.senders, args.batch_size);
    let sign_elapsed = sign_start.elapsed();
    println!(
        "done in {:.2}s ({} sigs/sec)",
        sign_elapsed.as_secs_f64(),
        format_tps(args.batch_size as f64 / sign_elapsed.as_secs_f64())
    );

    // ── Build pipeline ───────────────────────────────────────────────────────
    let state = Arc::new(StateDB::with_genesis(&genesis));
    let config = PipelineConfig {
        execution_mode: ExecutionMode::BlockSTM,
        verify_mode: VerifyMode::Cpu,
        coalesce_enabled: true,
        batch_size: args.batch_size,
    };
    let pipeline = Pipeline::with_config(Arc::clone(&state), config);
    let producer = benchmark_address(255);

    // ── Soak loop ────────────────────────────────────────────────────────────
    println!();
    println!("  Starting soak test...");

    let mut metrics = SoakMetrics::new();
    let soak_start = Instant::now();
    let mut last_report = Instant::now();
    let mut interval_submitted: u64 = 0;
    let mut interval_success: u64 = 0;
    let mut pending_batches: u64 = 0;

    loop {
        let elapsed = soak_start.elapsed();
        if elapsed >= total_duration {
            break;
        }

        // Submit a batch
        let submit_result = pipeline.submit(PipelineBatch {
            transactions: transactions.clone(),
            producer,
        });

        if submit_result.is_ok() {
            interval_submitted += args.batch_size as u64;
            pending_batches += 1;
        }

        // Drain available results (non-blocking)
        while let Some(result) = pipeline.try_recv() {
            interval_success += result.success_count as u64;
            pending_batches = pending_batches.saturating_sub(1);
        }

        // Report at interval
        if last_report.elapsed() >= report_interval {
            let interval_secs = last_report.elapsed().as_secs_f64();
            let current_tps = interval_submitted as f64 / interval_secs;

            metrics.record_interval(interval_submitted, interval_success, interval_secs);

            print_dashboard(elapsed, total_duration, current_tps, &metrics);

            interval_submitted = 0;
            interval_success = 0;
            last_report = Instant::now();
        }
    }

    // ── Drain remaining pipeline results ─────────────────────────────────────
    if interval_submitted > 0 || pending_batches > 0 {
        let drain_deadline = Instant::now() + Duration::from_secs(30);
        while pending_batches > 0 && Instant::now() < drain_deadline {
            if let Some(result) = pipeline.try_recv() {
                interval_success += result.success_count as u64;
                pending_batches = pending_batches.saturating_sub(1);
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        // Record final partial interval
        if interval_submitted > 0 {
            let interval_secs = last_report.elapsed().as_secs_f64();
            metrics.record_interval(interval_submitted, interval_success, interval_secs);
        }
    }

    // ── Final report ─────────────────────────────────────────────────────────
    let actual_duration = soak_start.elapsed();
    print_final_report(actual_duration, &metrics);
}
