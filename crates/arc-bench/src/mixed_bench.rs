mod ephemeral_keys;

use arc_crypto::Hash256;
use arc_node::pipeline::{Pipeline, PipelineBatch};
use arc_state::StateDB;
use arc_types::Transaction;
use clap::Parser;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "arc-bench-mixed",
    about = "ARC Chain - Mixed Workload Benchmark (ETH-weighted TPS)"
)]
struct Args {
    /// Total transactions to process.
    #[arg(long, default_value = "100000")]
    txs: usize,

    /// Batch size (transactions per pipeline submission).
    #[arg(long = "batch-size", default_value = "10000")]
    batch_size: usize,

    /// Number of Rayon threads (0 = auto-detect).
    #[arg(long, default_value = "0")]
    threads: usize,
}

#[derive(Clone, Copy, Debug)]
enum SimTxType {
    SimpleTransfer,
    Erc20Transfer,
    DexSwap,
    NftOperation,
    LendingBorrowing,
    MevBot,
    Bridge,
    ContractDeploy,
    Other,
}

impl SimTxType {
    fn weight(self) -> f64 {
        match self {
            Self::SimpleTransfer => 1.0,
            Self::Erc20Transfer => 2.7,
            Self::DexSwap => 6.0,
            Self::NftOperation => 7.1,
            Self::LendingBorrowing => 13.1,
            Self::MevBot => 4.8,
            Self::Bridge => 3.8,
            Self::ContractDeploy => 61.9,
            Self::Other => 3.8,
        }
    }

    fn gas(self) -> u64 {
        match self {
            Self::SimpleTransfer => 21_000,
            Self::Erc20Transfer => 57_500,
            Self::DexSwap => 125_000,
            Self::NftOperation => 150_000,
            Self::LendingBorrowing => 275_000,
            Self::MevBot => 100_000,
            Self::Bridge => 80_000,
            Self::ContractDeploy => 1_300_000,
            Self::Other => 80_000,
        }
    }

    fn extra_reads(self) -> usize {
        match self {
            Self::SimpleTransfer => 0,
            Self::Erc20Transfer => 2,
            Self::DexSwap => 6,
            Self::NftOperation => 4,
            Self::LendingBorrowing => 10,
            Self::MevBot => 4,
            Self::Bridge => 3,
            Self::ContractDeploy => 48,
            Self::Other => 3,
        }
    }

    fn extra_writes(self) -> usize {
        match self {
            Self::SimpleTransfer => 0,
            Self::Erc20Transfer => 1,
            Self::DexSwap => 4,
            Self::NftOperation => 3,
            Self::LendingBorrowing => 6,
            Self::MevBot => 2,
            Self::Bridge => 1,
            Self::ContractDeploy => 28,
            Self::Other => 1,
        }
    }
}

const ETH_WEIGHT_FACTOR: f64 = 3.93;

fn assign_tx_type(index: usize) -> SimTxType {
    let bucket = (index % 1000) as u16;
    match bucket {
        0..=379 => SimTxType::SimpleTransfer,
        380..=589 => SimTxType::Erc20Transfer,
        590..=739 => SimTxType::DexSwap,
        740..=819 => SimTxType::NftOperation,
        820..=869 => SimTxType::LendingBorrowing,
        870..=909 => SimTxType::MevBot,
        910..=939 => SimTxType::Bridge,
        940..=944 => SimTxType::ContractDeploy,
        _ => SimTxType::Other,
    }
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

fn format_gas(gas: f64) -> String {
    if gas >= 1_000_000_000.0 {
        format!("{:.2}B", gas / 1_000_000_000.0)
    } else if gas >= 1_000_000.0 {
        format!("{:.1}M", gas / 1_000_000.0)
    } else if gas >= 1_000.0 {
        format!("{:.1}K", gas / 1_000.0)
    } else {
        format!("{:.0}", gas)
    }
}

fn presign_transactions(total_txs: usize) -> (Vec<Transaction>, Vec<(Hash256, u64)>, Vec<Hash256>) {
    let sender_count = 10usize;
    let keypairs = ephemeral_keys::signing_keypairs(sender_count);

    let receivers = ephemeral_keys::addresses(56);
    let aux_addrs = ephemeral_keys::addresses(100);

    let mut genesis: Vec<(Hash256, u64)> = keypairs
        .iter()
        .map(|(_, addr)| (*addr, u64::MAX / 4))
        .collect();
    for r in &receivers {
        genesis.push((*r, 0));
    }
    for addr in &aux_addrs {
        genesis.push((*addr, u64::MAX / 8));
    }

    let mut transactions = Vec::with_capacity(total_txs);
    let mut nonces = vec![0u64; keypairs.len()];

    for tx_idx in 0..total_txs {
        let kp_idx = tx_idx % keypairs.len();
        let (sk, sender) = &keypairs[kp_idx];
        let receiver = receivers[tx_idx % receivers.len()];
        let nonce = nonces[kp_idx];

        let mut tx = Transaction::new_transfer(*sender, receiver, 1, nonce);

        tx.sign(sk)
            .expect("ephemeral benchmark signing must succeed");

        nonces[kp_idx] += 1;
        transactions.push(tx);
    }

    (transactions, genesis, aux_addrs)
}

fn simulate_extra_state_ops(state: &StateDB, tx_types: &[SimTxType], aux_addrs: &[Hash256]) {
    let storage_contract = aux_addrs[0];

    for (i, &sim_type) in tx_types.iter().enumerate() {
        let reads = sim_type.extra_reads();
        let writes = sim_type.extra_writes();

        for r in 0..reads {
            let addr = aux_addrs[(i + r) % aux_addrs.len()];
            let _ = state.get_account(&addr);
        }

        for w in 0..writes {
            let key_bytes = ((i as u64) * 1000 + (w as u64)).to_le_bytes();
            let key = arc_crypto::hash_bytes(&key_bytes);
            let value = vec![0u8; 32];
            state.set_storage(&storage_contract, key, value);
        }
    }
}

struct MixStats {
    counts: [(SimTxType, usize); 9],
    total: usize,
    total_gas: u64,
    weighted_sum: f64,
}

fn compute_mix_stats(tx_types: &[SimTxType]) -> MixStats {
    let mut counts = [0usize; 9];
    let mut total_gas = 0u64;
    let mut weighted_sum = 0.0f64;

    for &t in tx_types {
        let idx = match t {
            SimTxType::SimpleTransfer => 0,
            SimTxType::Erc20Transfer => 1,
            SimTxType::DexSwap => 2,
            SimTxType::NftOperation => 3,
            SimTxType::LendingBorrowing => 4,
            SimTxType::MevBot => 5,
            SimTxType::Bridge => 6,
            SimTxType::ContractDeploy => 7,
            SimTxType::Other => 8,
        };
        counts[idx] += 1;
        total_gas += t.gas();
        weighted_sum += t.weight();
    }

    let types = [
        SimTxType::SimpleTransfer,
        SimTxType::Erc20Transfer,
        SimTxType::DexSwap,
        SimTxType::NftOperation,
        SimTxType::LendingBorrowing,
        SimTxType::MevBot,
        SimTxType::Bridge,
        SimTxType::ContractDeploy,
        SimTxType::Other,
    ];

    let result: [(SimTxType, usize); 9] = std::array::from_fn(|i| (types[i], counts[i]));

    MixStats {
        counts: result,
        total: tx_types.len(),
        total_gas,
        weighted_sum,
    }
}

fn type_name(t: SimTxType) -> &'static str {
    match t {
        SimTxType::SimpleTransfer => "Simple transfer",
        SimTxType::Erc20Transfer => "ERC-20 transfer",
        SimTxType::DexSwap => "DEX swap",
        SimTxType::NftOperation => "NFT operation",
        SimTxType::LendingBorrowing => "Lending/borrowing",
        SimTxType::MevBot => "MEV bot",
        SimTxType::Bridge => "Bridge",
        SimTxType::ContractDeploy => "Contract deploy",
        SimTxType::Other => "Other",
    }
}

fn run_transfer_only(total_txs: usize, batch_size: usize) -> (usize, usize, Duration) {
    let (transactions, genesis, _) = presign_transactions(total_txs);
    let state = Arc::new(StateDB::with_genesis(&genesis));
    let pipeline = Pipeline::new(Arc::clone(&state));
    let producer = genesis[0].0;

    let start = Instant::now();
    let num_batches = transactions.len().div_ceil(batch_size);
    let mut tx_iter = transactions.into_iter();
    let mut batches_submitted = 0usize;

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

    let mut total_processed = 0usize;
    let mut total_success = 0usize;
    let timeout = Duration::from_secs(120);
    let deadline = Instant::now() + timeout;
    let mut results_received = 0usize;

    while results_received < batches_submitted {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            eprintln!("  TIMEOUT waiting for transfer-only results");
            break;
        }
        if let Some(result) = pipeline.try_recv() {
            total_processed += result.tx_count;
            total_success += result.success_count;
            results_received += 1;
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    let elapsed = start.elapsed();
    (total_processed, total_success, elapsed)
}

fn run_mixed_workload(
    total_txs: usize,
    batch_size: usize,
) -> (usize, usize, Duration, Vec<SimTxType>) {
    let tx_types: Vec<SimTxType> = (0..total_txs).map(assign_tx_type).collect();

    let (transactions, genesis, aux_addrs) = presign_transactions(total_txs);
    let state = Arc::new(StateDB::with_genesis(&genesis));
    let pipeline = Pipeline::new(Arc::clone(&state));
    let producer = genesis[0].0;

    let start = Instant::now();
    let num_batches = transactions.len().div_ceil(batch_size);
    let mut tx_iter = transactions.into_iter();
    let mut type_iter = tx_types.chunks(batch_size);
    let mut batches_submitted = 0usize;

    for _ in 0..num_batches {
        let batch: Vec<Transaction> = tx_iter.by_ref().take(batch_size).collect();
        if batch.is_empty() {
            break;
        }
        let batch_types = type_iter.next().unwrap_or(&[]);

        pipeline
            .submit(PipelineBatch {
                transactions: batch,
                producer,
            })
            .expect("pipeline submit");
        batches_submitted += 1;

        simulate_extra_state_ops(&state, batch_types, &aux_addrs);
    }

    let mut total_processed = 0usize;
    let mut total_success = 0usize;
    let timeout = Duration::from_secs(120);
    let deadline = Instant::now() + timeout;
    let mut results_received = 0usize;

    while results_received < batches_submitted {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            eprintln!("  TIMEOUT waiting for mixed workload results");
            break;
        }
        if let Some(result) = pipeline.try_recv() {
            total_processed += result.tx_count;
            total_success += result.success_count;
            results_received += 1;
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    let elapsed = start.elapsed();
    (total_processed, total_success, elapsed, tx_types)
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
    println!(" ARC Chain - MIXED WORKLOAD Benchmark (ETH-weighted TPS)");
    println!("================================================================");
    println!();
    println!("  WHAT THIS MEASURES:");
    println!("    Full 4-stage pipeline: Receive -> Verify -> Execute -> Commit");
    println!("    + Additional state reads/writes per tx type to simulate");
    println!("      real Ethereum mainnet workload distribution.");
    println!();
    println!("  WHY ETH-WEIGHTED TPS:");
    println!("    Raw TPS counts every tx equally, but a DEX swap is 6x");
    println!("    heavier than a simple transfer.  ETH-weighted TPS divides");
    println!(
        "    by the composite weight factor ({:.2}x) to give a fair",
        ETH_WEIGHT_FACTOR
    );
    println!("    comparison to Ethereum's gas-based throughput.");
    println!();
    println!("  System:");
    println!("    CPU cores (Rayon):  {}", num_threads);
    println!("    GPU:                {} ({})", gpu.name, gpu.backend);
    println!("    Total TXs:          {}", args.txs);
    println!("    Batch size:         {}", args.batch_size);
    println!();

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PHASE 1: Transfer-only baseline (simple transfers only)");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    let sign_start = Instant::now();
    let (tf_processed, tf_success, tf_elapsed) = run_transfer_only(args.txs, args.batch_size);
    let _sign_time = sign_start.elapsed();
    let tf_tps = tf_processed as f64 / tf_elapsed.as_secs_f64();

    println!(
        "    Processed:         {} / {} submitted",
        tf_processed, args.txs
    );
    println!("    Successful:        {}", tf_success);
    println!("    Pipeline time:     {:.2}s", tf_elapsed.as_secs_f64());
    println!("    Raw TPS:           {}", format_tps(tf_tps));
    println!(
        "    Gas throughput:    {} gas/sec (at 21,000 gas each)",
        format_gas(tf_tps * 21_000.0)
    );
    println!();

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PHASE 2: Mixed workload (Ethereum mainnet distribution)");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    let (mx_processed, mx_success, mx_elapsed, tx_types) =
        run_mixed_workload(args.txs, args.batch_size);
    let mx_raw_tps = mx_processed as f64 / mx_elapsed.as_secs_f64();
    let mx_weighted_tps = mx_raw_tps / ETH_WEIGHT_FACTOR;

    let stats = compute_mix_stats(&tx_types);

    println!("  Transaction Mix:");
    println!(
        "    {:<20} {:>7} {:>7}  {:>6}  {:>6}  {:>8}",
        "Type", "Count", "Pct", "Weight", "Reads", "Writes"
    );
    println!("    {}", "-".repeat(62));
    for (sim_type, count) in &stats.counts {
        let pct = (*count as f64 / stats.total as f64) * 100.0;
        println!(
            "    {:<20} {:>7} {:>6.1}%  {:>5.1}x  {:>3}+{:<3}  {:>3}+{:<3}",
            type_name(*sim_type),
            count,
            pct,
            sim_type.weight(),
            2 + sim_type.extra_reads(),
            2 + sim_type.extra_writes(),
            sim_type.extra_reads(),
            sim_type.extra_writes(),
        );
    }
    println!();

    let total_extra_reads: usize = tx_types.iter().map(|t| t.extra_reads()).sum();
    let total_extra_writes: usize = tx_types.iter().map(|t| t.extra_writes()).sum();
    let actual_weight = stats.weighted_sum / stats.total as f64;

    println!("  Extra state operations (beyond baseline transfers):");
    println!("    Additional reads:  {}", total_extra_reads);
    println!("    Additional writes: {}", total_extra_writes);
    println!(
        "    Actual weight:     {:.2}x (expected {:.2}x)",
        actual_weight, ETH_WEIGHT_FACTOR
    );
    println!();

    println!("  Results:");
    println!(
        "    Processed:         {} / {} submitted",
        mx_processed, args.txs
    );
    println!("    Successful:        {}", mx_success);
    println!("    Pipeline time:     {:.2}s", mx_elapsed.as_secs_f64());
    println!(
        "    Total gas:         {}",
        format_gas(stats.total_gas as f64)
    );
    println!(
        "    Gas throughput:    {} gas/sec",
        format_gas(stats.total_gas as f64 / mx_elapsed.as_secs_f64())
    );
    println!();

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  RESULTS COMPARISON");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!(
        "                        {:>12}  {:>12}",
        "Transfer-only", "Mixed"
    );
    println!("    {}", "-".repeat(42));
    println!(
        "    Raw TPS:            {:>12}  {:>12}",
        format_tps(tf_tps),
        format_tps(mx_raw_tps)
    );
    println!(
        "    ETH-weighted TPS:   {:>12}  {:>12}",
        format_tps(tf_tps),
        format_tps(mx_weighted_tps)
    );
    println!(
        "    Pipeline time:      {:>11.2}s  {:>11.2}s",
        tf_elapsed.as_secs_f64(),
        mx_elapsed.as_secs_f64()
    );
    println!();

    let slowdown = if mx_raw_tps > 0.0 {
        tf_tps / mx_raw_tps
    } else {
        f64::INFINITY
    };
    println!(
        "    Mixed workload overhead: {:.1}x slowdown vs transfer-only",
        slowdown
    );
    println!(
        "    ETH weight factor:       {:.2}x (gas-weighted tx complexity)",
        ETH_WEIGHT_FACTOR
    );
    println!();

    let eth_tps = 15.0;
    let eth_gas_per_sec = 2_500_000.0;
    let arc_gas_per_sec = stats.total_gas as f64 / mx_elapsed.as_secs_f64();

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  ETHEREUM COMPARISON");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!(
        "    Ethereum L1:            ~{} TPS / ~{}  gas/sec",
        format_tps(eth_tps),
        format_gas(eth_gas_per_sec)
    );
    println!(
        "    ARC (transfer-only):     {} TPS / {} gas/sec",
        format_tps(tf_tps),
        format_gas(tf_tps * 21_000.0)
    );
    println!(
        "    ARC (mixed workload):    {} TPS / {} gas/sec",
        format_tps(mx_weighted_tps),
        format_gas(arc_gas_per_sec)
    );
    println!();
    println!(
        "    ARC vs ETH (weighted):   {:.0}x faster",
        mx_weighted_tps / eth_tps
    );
    println!(
        "    ARC vs ETH (gas/sec):    {:.0}x faster",
        arc_gas_per_sec / eth_gas_per_sec
    );
    println!();

    println!("================================================================");
    println!(" HONESTY REPORT");
    println!("================================================================");
    println!("  What was REAL:");
    println!("    [x] Ed25519 keypair generation + signing");
    println!("    [x] BLAKE3 hash integrity verification");
    println!("    [x] Ed25519 signature verification (CPU rayon parallel)");
    println!("    [x] State execution (real balance/nonce mutations)");
    println!("    [x] Merkle tree + block commit");
    println!("    [x] Additional state reads/writes for heavy tx types");
    println!();
    println!("  What is SIMULATED:");
    println!("    [~] Heavy tx types modelled as extra state ops, not actual");
    println!("        EVM/WASM execution (no contract bytecode runs)");
    println!("    [~] Distribution is deterministic, not random");
    println!();
    println!("  What is NOT accelerated:");
    println!("    [ ] GPU Ed25519 verification (runs on CPU)");
    println!("    [ ] GPU BLAKE3 hashing");
    println!("    [ ] Block-STM parallel execution");
    println!();
    println!("================================================================");
    println!();
}
