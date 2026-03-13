//! GPU-Resident State Benchmark
//!
//! Compares state execution throughput with and without GPU-resident state cache.
//! Tests: DashMap-only (baseline) vs GPU-backed state (Metal unified / CPU fallback).

use arc_crypto::Hash256;
use arc_state::StateDB;
use arc_state::gpu_state::{GpuStateCacheConfig, GpuStateCache};
use arc_types::{Account, Address, Transaction, TxBody, TxType};
use std::sync::Arc;
use std::time::Instant;

fn benchmark_address(seed: u8) -> Address {
    let mut addr = [0u8; 32];
    addr[0] = seed;
    Hash256(addr)
}

fn benchmark_address_u16(seed: u16) -> Address {
    let mut addr = [0u8; 32];
    addr[0] = (seed & 0xFF) as u8;
    addr[1] = (seed >> 8) as u8;
    Hash256(addr)
}

pub fn run_gpu_state_benchmark() {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║           ARC Chain — GPU-Resident State Benchmark              ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();

    let num_accounts: usize = 50_000;
    let num_lookups: usize = 1_000_000;
    let num_transfers: usize = 500_000;

    // ── Phase 1: Setup ──────────────────────────────────────────────────────
    println!("Setting up {} accounts...", num_accounts);

    let genesis: Vec<(Address, u64)> = (0..num_accounts as u16)
        .map(|i| (benchmark_address_u16(i), 1_000_000))
        .collect();

    // ── Phase 2: Baseline — DashMap only ────────────────────────────────────
    println!("\n━━━ Phase 2: Baseline (DashMap only) ━━━");

    let state_baseline = Arc::new(StateDB::with_genesis(&genesis));

    // Random account lookups
    let t0 = Instant::now();
    let mut hits = 0u64;
    for i in 0..num_lookups {
        let addr = benchmark_address_u16((i % num_accounts) as u16);
        if state_baseline.get_account(&addr).is_some() {
            hits += 1;
        }
    }
    let baseline_lookup_elapsed = t0.elapsed();
    let baseline_lookup_rate = num_lookups as f64 / baseline_lookup_elapsed.as_secs_f64();
    println!(
        "  Lookups:    {num_lookups} in {:.3}s → {:.0} lookups/sec (hits: {})",
        baseline_lookup_elapsed.as_secs_f64(),
        baseline_lookup_rate,
        hits
    );

    // Transfer execution
    let t1 = Instant::now();
    let mut success_count = 0u64;
    for i in 0..num_transfers {
        let sender_idx = (i % num_accounts) as u16;
        let receiver_idx = ((i + 1) % num_accounts) as u16;
        let sender = benchmark_address_u16(sender_idx);
        let receiver = benchmark_address_u16(receiver_idx);

        if let Some(mut acct) = state_baseline.get_account(&sender) {
            if acct.balance >= 1 {
                acct.balance -= 1;
                acct.nonce += 1;
                state_baseline.update_account(&sender, acct);

                let mut recv_acct = state_baseline.get_or_create_account(&receiver);
                recv_acct.balance += 1;
                state_baseline.update_account(&receiver, recv_acct);
                success_count += 1;
            }
        }
    }
    let baseline_transfer_elapsed = t1.elapsed();
    let baseline_tps = success_count as f64 / baseline_transfer_elapsed.as_secs_f64();
    println!(
        "  Transfers:  {} in {:.3}s → {:.0} TPS",
        success_count,
        baseline_transfer_elapsed.as_secs_f64(),
        baseline_tps
    );

    // ── Phase 3: GPU-Resident State ─────────────────────────────────────────
    println!("\n━━━ Phase 3: GPU-Resident State ━━━");

    let gpu_config = GpuStateCacheConfig {
        max_gpu_accounts: num_accounts,
        ..Default::default()
    };
    let state_gpu = StateDB::with_genesis_gpu(&genesis, gpu_config);
    let state_gpu = Arc::new(state_gpu);

    // Report memory model
    if let Some(cache) = state_gpu.gpu_cache() {
        println!(
            "  Memory model: {:?}",
            cache.memory_model()
        );
        let stats = cache.stats();
        println!(
            "  GPU accounts: {}, Warm accounts: {}",
            stats.gpu_accounts, stats.warm_accounts
        );
    }

    // Random account lookups (GPU-backed)
    let t2 = Instant::now();
    let mut gpu_hits = 0u64;
    for i in 0..num_lookups {
        let addr = benchmark_address_u16((i % num_accounts) as u16);
        if state_gpu.get_account(&addr).is_some() {
            gpu_hits += 1;
        }
    }
    let gpu_lookup_elapsed = t2.elapsed();
    let gpu_lookup_rate = num_lookups as f64 / gpu_lookup_elapsed.as_secs_f64();
    println!(
        "  Lookups:    {num_lookups} in {:.3}s → {:.0} lookups/sec (hits: {})",
        gpu_lookup_elapsed.as_secs_f64(),
        gpu_lookup_rate,
        gpu_hits
    );

    // Transfer execution (GPU-backed)
    let t3 = Instant::now();
    let mut gpu_success = 0u64;
    for i in 0..num_transfers {
        let sender_idx = (i % num_accounts) as u16;
        let receiver_idx = ((i + 1) % num_accounts) as u16;
        let sender = benchmark_address_u16(sender_idx);
        let receiver = benchmark_address_u16(receiver_idx);

        if let Some(mut acct) = state_gpu.get_account(&sender) {
            if acct.balance >= 1 {
                acct.balance -= 1;
                acct.nonce += 1;
                state_gpu.update_account(&sender, acct);

                let mut recv_acct = state_gpu.get_or_create_account(&receiver);
                recv_acct.balance += 1;
                state_gpu.update_account(&receiver, recv_acct);
                gpu_success += 1;
            }
        }
    }
    let gpu_transfer_elapsed = t3.elapsed();
    let gpu_tps = gpu_success as f64 / gpu_transfer_elapsed.as_secs_f64();
    println!(
        "  Transfers:  {} in {:.3}s → {:.0} TPS",
        gpu_success,
        gpu_transfer_elapsed.as_secs_f64(),
        gpu_tps
    );

    // GPU cache stats
    if let Some(cache) = state_gpu.gpu_cache() {
        let stats = cache.stats();
        println!(
            "  Cache stats: GPU hits={}, CPU hits={}, misses={}, hit_rate={:.1}%",
            stats.gpu_hits, stats.cpu_hits, stats.misses,
            stats.gpu_hit_rate * 100.0
        );
    }

    // ── Summary ─────────────────────────────────────────────────────────────
    println!();
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║  RESULTS                                                        ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    let lookup_speedup = gpu_lookup_rate / baseline_lookup_rate;
    let transfer_speedup = gpu_tps / baseline_tps;
    println!(
        "║  Lookup throughput:  Baseline {:.0}/s → GPU {:.0}/s  ({:.1}x)",
        baseline_lookup_rate, gpu_lookup_rate, lookup_speedup
    );
    println!(
        "║  Transfer TPS:      Baseline {:.0} → GPU {:.0}  ({:.1}x)",
        baseline_tps, gpu_tps, transfer_speedup
    );
    println!(
        "║  Accounts:          {}",
        num_accounts
    );
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();

    // Verify correctness
    assert_eq!(hits, num_lookups as u64, "baseline lookups should all hit");
    assert_eq!(gpu_hits, num_lookups as u64, "GPU lookups should all hit");
}
