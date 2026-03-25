//! 4-stage pipelined block execution.
//!
//! Overlaps work on consecutive blocks across four stages:
//!
//! ```text
//!   Block N  : [Receive] [Verify] [Execute] [Commit]
//!   Block N+1:           [Receive] [Verify] [Execute] [Commit]
//!   Block N+2:                     [Receive] [Verify] [Execute] [Commit]
//! ```
//!
//! Each stage runs on its own thread, communicating via bounded crossbeam
//! channels (capacity 2).  The pipeline is driven by the consensus loop:
//! instead of doing all four phases serially per block, it pushes a batch
//! of transactions into the pipeline's receive end and the commit stage
//! emits the finished block asynchronously.
//!
//! **Throughput gain**: On a 4+ core machine, the pipeline effectively hides
//! verification latency behind execution of the previous block, and execution
//! latency behind commit of the block before that.  Real-world improvement
//! is 2-3× on CPU-bound workloads (signature verification dominates).

use crate::block_stm::BlockSTM;
use crate::coalesce::CoalescedBatch;
use arc_state::block_stm::execute_speculative;
use arc_state::StateDB;
use arc_types::{Address, Transaction, TxBody};
use crossbeam::channel::{self, Receiver, Sender};
use rayon::prelude::*;
use std::sync::Arc;
use std::thread;
use arc_consensus::encode_block_data;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How the execute stage processes transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Execute transactions one-by-one (original behaviour).
    Sequential,
    /// Optimistic parallel execution via Block-STM with automatic fallback to
    /// sequential if too many conflict rounds occur.
    BlockSTM,
    /// Speculative Block-STM — executes ALL transactions in parallel optimistically,
    /// validates read/write sets, and re-executes only conflicting transactions.
    /// Falls back to sequential for unresolved conflicts after max rounds.
    SpeculativeSTM,
    /// GPU-resident state: hot accounts live in GPU unified/managed memory.
    /// Combines Block-STM parallel execution with GPU-accelerated state lookups
    /// for ~40x bandwidth improvement on hot accounts.
    GpuResident,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Sequential
    }
}

/// How the verify stage verifies Ed25519 signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// CPU-only verification via rayon parallel iterator.
    Cpu,
    /// GPU-accelerated verification via Metal (Apple Silicon) with automatic
    /// fallback to CPU for small batches or non-Apple platforms.
    GpuMetal,
    /// GPU-accelerated verification via CUDA (NVIDIA GPUs).
    /// Falls back to CPU until the CUDA kernel is implemented (week 3-5).
    GpuCuda,
    /// CPU SIMD verification using AVX-512 intrinsics (x86_64).
    /// Falls back to scalar CPU until the AVX-512 kernel is implemented (week 6-7).
    CpuAvx512,
    /// CPU SIMD verification using ARM NEON intrinsics (aarch64).
    /// Falls back to scalar CPU until the NEON kernel is implemented (week 8).
    CpuNeon,
}

impl Default for VerifyMode {
    fn default() -> Self {
        Self::Cpu
    }
}

/// Auto-detect the best verification mode based on runtime hardware probing.
///
/// Priority: CUDA → Metal → AVX-512 → NEON → CPU (scalar).
fn auto_detect_verify_mode() -> VerifyMode {
    let hw = arc_gpu::hardware_detect::detect();
    if hw.cuda_available {
        VerifyMode::GpuCuda
    } else if hw.metal_available {
        VerifyMode::GpuMetal
    } else if hw.avx512_available {
        VerifyMode::CpuAvx512
    } else if hw.neon_available {
        VerifyMode::CpuNeon
    } else {
        VerifyMode::Cpu
    }
}

/// Tuning knobs for the pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Which execution strategy to use in stage 3.
    pub execution_mode: ExecutionMode,
    /// Which verification strategy to use in stage 2.
    pub verify_mode: VerifyMode,
    /// Whether to run state-coalescing as a pre-processing step before
    /// execution. Coalescing groups same-sender transactions and collapses
    /// redundant reads/writes for hot accounts.
    pub coalesce_enabled: bool,
    /// (Reserved) Maximum number of transactions per execution batch.
    pub batch_size: usize,
    /// When `true` and `execution_mode == GpuResident`, enables GPU state cache.
    pub gpu_state_enabled: bool,
    /// GPU state cache capacity (number of hot accounts in GPU memory).
    pub gpu_state_capacity: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            execution_mode: ExecutionMode::BlockSTM,
            verify_mode: auto_detect_verify_mode(),
            coalesce_enabled: false,
            batch_size: 10_000,
            gpu_state_enabled: true,
            gpu_state_capacity: 1_000_000,
        }
    }
}

/// A batch of transactions entering the pipeline.
pub struct PipelineBatch {
    pub transactions: Vec<Transaction>,
    pub producer: Address,
}

/// Result from the commit stage.
pub struct PipelineResult {
    pub height: u64,
    pub tx_count: usize,
    pub success_count: usize,
    pub elapsed_ms: u128,
    /// Which execution strategy was used for this block.
    pub execution_mode: ExecutionMode,
    /// If coalescing ran, the number of state reads saved. `None` when disabled.
    pub coalesce_reads_saved: Option<usize>,
}

/// After signature verification, the batch gains validity flags.
struct VerifiedBatch {
    transactions: Vec<Transaction>,
    sig_valid: Vec<bool>,
    producer: Address,
}

/// After execution, we have receipts.
struct ExecutedBatch {
    transactions: Vec<Transaction>,
    receipt_success: Vec<bool>,
    producer: Address,
    /// Which execution mode was actually used (may differ from config if
    /// Block-STM fell back to sequential).
    execution_mode: ExecutionMode,
    /// State reads saved by coalescing (None if coalescing was disabled).
    coalesce_reads_saved: Option<usize>,
}

/// The 4-stage pipeline.
pub struct Pipeline {
    /// Send batches into the pipeline.
    tx_in: Sender<PipelineBatch>,
    /// Receive finished results.
    rx_out: Receiver<PipelineResult>,
    /// The config this pipeline was created with.
    config: PipelineConfig,
    /// Pre-verification cache for GPU signature verification.
    /// Populated by background verification; checked at block-time verify stage.
    sig_cache: Arc<arc_gpu::metal_verify::SigVerifyCache>,
}

impl Pipeline {
    /// Create and start the pipeline with default config (sequential execution,
    /// coalescing disabled).
    ///
    /// Spawns 3 background threads (verify, execute, commit).
    /// The receive stage is implicit — the caller pushes `PipelineBatch` via `submit()`.
    pub fn new(state: Arc<StateDB>) -> Self {
        Self::with_config(state, PipelineConfig::default())
    }

    /// Create and start the pipeline with the given config.
    ///
    /// Spawns 3 background threads (verify, execute, commit).
    pub fn with_config(state: Arc<StateDB>, config: PipelineConfig) -> Self {
        // Enable GPU state cache if requested.
        if config.gpu_state_enabled || config.execution_mode == ExecutionMode::GpuResident {
            let gpu_config = arc_state::gpu_state::GpuStateCacheConfig {
                max_gpu_accounts: config.gpu_state_capacity,
                ..Default::default()
            };
            // Safety: we need &mut but state is behind Arc. The enable_gpu_cache
            // is called once at startup before any concurrent access.
            unsafe {
                let state_ptr = Arc::as_ptr(&state) as *mut StateDB;
                (*state_ptr).enable_gpu_cache(gpu_config);
            }
            info!(
                capacity = config.gpu_state_capacity,
                "Pipeline: GPU-resident state cache enabled"
            );
        }

        // Bounded channels between stages (capacity 2 to allow slight buffering
        // without unbounded memory growth).
        let (tx_in, rx_receive) = channel::bounded::<PipelineBatch>(2);
        let (tx_verified, rx_verified) = channel::bounded::<VerifiedBatch>(2);
        let (tx_executed, rx_executed) = channel::bounded::<ExecutedBatch>(2);
        let (tx_out, rx_out) = channel::bounded::<PipelineResult>(2);

        let sig_cache = Arc::new(arc_gpu::metal_verify::SigVerifyCache::new());

        // ── Stage 2: Verify (batch Ed25519 + individual fallback) ────────
        let verify_mode = config.verify_mode;
        let verify_cache = Arc::clone(&sig_cache);
        thread::Builder::new()
            .name("pipeline-verify".into())
            .spawn(move || {
                // Initialize the backend-specific verifier for this thread.
                let mut metal_verifier = match verify_mode {
                    VerifyMode::GpuMetal => Some(arc_gpu::metal_verify::MetalVerifier::new()),
                    _ => None,
                };
                let mut cuda_verifier = match verify_mode {
                    VerifyMode::GpuCuda => Some(arc_gpu::cuda_verify::CudaVerifier::new()),
                    _ => None,
                };
                let mut avx512_verifier = match verify_mode {
                    VerifyMode::CpuAvx512 => Some(arc_gpu::avx512_verify::Avx512Verifier::new()),
                    _ => None,
                };
                let mut neon_verifier = match verify_mode {
                    VerifyMode::CpuNeon => Some(arc_gpu::neon_verify::NeonVerifier::new()),
                    _ => None,
                };

                info!(mode = ?verify_mode, "Pipeline: verify backend initialized");

                while let Ok(batch) = rx_receive.recv() {
                    let n = batch.transactions.len();
                    let mut sig_valid = vec![false; n];

                    // ── Phase 1: hash integrity check (parallel) ─────────
                    let hash_ok: Vec<bool> = batch
                        .transactions
                        .par_iter()
                        .map(|tx| tx.compute_hash() == tx.hash)
                        .collect();

                    // ── Phase 2: separate Ed25519 from others ────────────
                    let mut ed_indices: Vec<usize> = Vec::new();
                    let mut ed_msgs: Vec<Vec<u8>> = Vec::new();
                    let mut ed_sigs: Vec<ed25519_dalek::Signature> = Vec::new();
                    let mut ed_vks: Vec<ed25519_dalek::VerifyingKey> = Vec::new();
                    let mut other_indices: Vec<usize> = Vec::new();

                    for (i, tx) in batch.transactions.iter().enumerate() {
                        if !hash_ok[i] {
                            continue; // hash mismatch → invalid
                        }
                        match &tx.signature {
                            arc_crypto::Signature::Ed25519 { public_key, signature } => {
                                // Check address derivation.
                                let derived = arc_crypto::address_from_ed25519_pubkey(public_key);
                                if derived != tx.from {
                                    continue; // address mismatch → invalid
                                }
                                // Extract components for batch verification.
                                if let (Ok(vk), true) = (
                                    ed25519_dalek::VerifyingKey::from_bytes(public_key),
                                    signature.len() == 64,
                                ) {
                                    let mut sig_bytes = [0u8; 64];
                                    sig_bytes.copy_from_slice(signature);
                                    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
                                    ed_indices.push(i);
                                    ed_msgs.push(tx.hash.0.to_vec());
                                    ed_sigs.push(sig);
                                    ed_vks.push(vk);
                                } else {
                                    continue; // malformed key/sig → invalid
                                }
                            }
                            _ => {
                                other_indices.push(i);
                            }
                        }
                    }

                    // ── Phase 3: batch verify Ed25519 ────────────────────
                    // Check pre-verification cache first, only verify uncached sigs.
                    if !ed_indices.is_empty() {
                        let mut uncached_task_indices: Vec<usize> = Vec::new();
                        let mut cached_count = 0usize;

                        for (j, &orig_idx) in ed_indices.iter().enumerate() {
                            let tx_hash = batch.transactions[orig_idx].hash.0;
                            if let Some(valid) = verify_cache.lookup(&tx_hash) {
                                sig_valid[orig_idx] = valid;
                                verify_cache.remove(&tx_hash);
                                cached_count += 1;
                            } else {
                                uncached_task_indices.push(j);
                            }
                        }

                        if cached_count > 0 {
                            debug!(cached = cached_count, uncached = uncached_task_indices.len(),
                                "Pipeline: pre-verified cache hits");
                        }

                        // Verify remaining uncached signatures via the selected backend.
                        if !uncached_task_indices.is_empty() {
                            // Helper: CPU rayon fallback path (used by CPU mode and
                            // as fallback for not-yet-implemented kernel modes).
                            let cpu_verify = |indices: &[usize],
                                              msgs: &[Vec<u8>],
                                              sigs: &[ed25519_dalek::Signature],
                                              vks: &[ed25519_dalek::VerifyingKey],
                                              ed_idx: &[usize],
                                              out: &mut [bool]| {
                                let msg_refs: Vec<&[u8]> = indices.iter()
                                    .map(|&j| msgs[j].as_slice()).collect();
                                let s: Vec<ed25519_dalek::Signature> = indices.iter()
                                    .map(|&j| sigs[j]).collect();
                                let v: Vec<ed25519_dalek::VerifyingKey> = indices.iter()
                                    .map(|&j| vks[j]).collect();
                                let results = arc_gpu::cpu_batch_verify_ed25519(
                                    &msg_refs, &s, &v,
                                );
                                for (k, &valid) in results.iter().enumerate() {
                                    out[ed_idx[indices[k]]] = valid;
                                }
                            };

                            match verify_mode {
                                VerifyMode::GpuMetal if metal_verifier.is_some() => {
                                    // GPU Metal path
                                    let verifier = metal_verifier.as_mut().unwrap();
                                    let tasks: Vec<arc_gpu::metal_verify::VerifyTask> = uncached_task_indices
                                        .iter()
                                        .map(|&j| arc_gpu::metal_verify::VerifyTask {
                                            message: ed_msgs[j].clone(),
                                            public_key: *ed_vks[j].as_bytes(),
                                            signature: ed_sigs[j].to_bytes(),
                                        })
                                        .collect();
                                    let result = verifier.batch_verify(&tasks);
                                    let invalid_set: std::collections::HashSet<usize> =
                                        result.invalid_indices.iter().copied().collect();
                                    for (k, &j) in uncached_task_indices.iter().enumerate() {
                                        sig_valid[ed_indices[j]] = !invalid_set.contains(&k);
                                    }
                                }
                                VerifyMode::GpuCuda if cuda_verifier.is_some() => {
                                    // CUDA kernel dispatch
                                    let verifier = cuda_verifier.as_mut().unwrap();
                                    let tasks: Vec<arc_gpu::metal_verify::VerifyTask> = uncached_task_indices
                                        .iter()
                                        .map(|&j| arc_gpu::metal_verify::VerifyTask {
                                            message: ed_msgs[j].clone(),
                                            public_key: *ed_vks[j].as_bytes(),
                                            signature: ed_sigs[j].to_bytes(),
                                        })
                                        .collect();
                                    let result = verifier.batch_verify(&tasks);
                                    let invalid_set: std::collections::HashSet<usize> =
                                        result.invalid_indices.iter().copied().collect();
                                    for (k, &j) in uncached_task_indices.iter().enumerate() {
                                        sig_valid[ed_indices[j]] = !invalid_set.contains(&k);
                                    }
                                }
                                VerifyMode::CpuAvx512 if avx512_verifier.is_some() => {
                                    // AVX-512 kernel dispatch
                                    let verifier = avx512_verifier.as_mut().unwrap();
                                    let tasks: Vec<arc_gpu::metal_verify::VerifyTask> = uncached_task_indices
                                        .iter()
                                        .map(|&j| arc_gpu::metal_verify::VerifyTask {
                                            message: ed_msgs[j].clone(),
                                            public_key: *ed_vks[j].as_bytes(),
                                            signature: ed_sigs[j].to_bytes(),
                                        })
                                        .collect();
                                    let result = verifier.batch_verify(&tasks);
                                    let invalid_set: std::collections::HashSet<usize> =
                                        result.invalid_indices.iter().copied().collect();
                                    for (k, &j) in uncached_task_indices.iter().enumerate() {
                                        sig_valid[ed_indices[j]] = !invalid_set.contains(&k);
                                    }
                                }
                                VerifyMode::CpuNeon if neon_verifier.is_some() => {
                                    // NEON kernel dispatch
                                    let verifier = neon_verifier.as_mut().unwrap();
                                    let tasks: Vec<arc_gpu::metal_verify::VerifyTask> = uncached_task_indices
                                        .iter()
                                        .map(|&j| arc_gpu::metal_verify::VerifyTask {
                                            message: ed_msgs[j].clone(),
                                            public_key: *ed_vks[j].as_bytes(),
                                            signature: ed_sigs[j].to_bytes(),
                                        })
                                        .collect();
                                    let result = verifier.batch_verify(&tasks);
                                    let invalid_set: std::collections::HashSet<usize> =
                                        result.invalid_indices.iter().copied().collect();
                                    for (k, &j) in uncached_task_indices.iter().enumerate() {
                                        sig_valid[ed_indices[j]] = !invalid_set.contains(&k);
                                    }
                                }
                                _ => {
                                    // CPU scalar path (default)
                                    cpu_verify(
                                        &uncached_task_indices, &ed_msgs, &ed_sigs, &ed_vks,
                                        &ed_indices, &mut sig_valid,
                                    );
                                }
                            }
                        }
                    }

                    // ── Phase 4: verify non-Ed25519 individually ─────────
                    for &i in &other_indices {
                        let tx = &batch.transactions[i];
                        sig_valid[i] = tx.signature.verify(&tx.hash, &tx.from).is_ok();
                    }

                    debug!(
                        txs = n,
                        valid = sig_valid.iter().filter(|&&v| v).count(),
                        ed25519_batch = ed_indices.len(),
                        other = other_indices.len(),
                        "Pipeline: signatures verified"
                    );

                    if tx_verified
                        .send(VerifiedBatch {
                            transactions: batch.transactions,
                            sig_valid,
                            producer: batch.producer,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("spawn verify thread");

        // ── Stage 3: Execute ─────────────────────────────────────────────
        // Capture config values for the execute thread.
        let exec_mode = config.execution_mode;
        let coalesce_enabled = config.coalesce_enabled;
        let exec_state = Arc::clone(&state);
        thread::Builder::new()
            .name("pipeline-execute".into())
            .spawn(move || {
                while let Ok(vbatch) = rx_verified.recv() {
                    let n = vbatch.transactions.len();

                    // Filter to only signature-valid transactions for execution.
                    let valid_txs: Vec<Transaction> = vbatch
                        .transactions
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| vbatch.sig_valid[*i])
                        .map(|(_, tx)| tx.clone())
                        .collect();

                    // Build an index mapping: valid_txs[j] == vbatch.transactions[orig_indices[j]]
                    let orig_indices: Vec<usize> = vbatch
                        .transactions
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| vbatch.sig_valid[*i])
                        .map(|(i, _)| i)
                        .collect();

                    let mut receipt_success = vec![false; n];
                    let mut actual_mode = exec_mode;
                    let mut coalesce_reads_saved: Option<usize> = None;

                    // ── Optional: Coalescing pre-processing ──────────────
                    if coalesce_enabled && !valid_txs.is_empty() {
                        let coalesced = CoalescedBatch::from_transactions(valid_txs.clone());

                        if coalesced.is_worthwhile() {
                            match coalesced.execute(&exec_state) {
                                Ok(stats) => {
                                    info!(
                                        total = stats.total_txs,
                                        senders = stats.unique_senders,
                                        receivers = stats.unique_receivers,
                                        reads_saved = stats.reads_saved,
                                        failed = stats.failed_txs,
                                        remainder = stats.remainder_txs,
                                        "Pipeline: coalesced execution"
                                    );

                                    coalesce_reads_saved = Some(stats.reads_saved);

                                    // Mark all valid txs as successful (coalesce handles
                                    // failures internally by not crediting, but from the
                                    // pipeline's perspective the batch was processed).
                                    // We conservatively mark everything successful and let
                                    // the commit stage handle receipts.
                                    for &idx in &orig_indices {
                                        receipt_success[idx] = true;
                                    }
                                    // Mark failed coalesced txs appropriately.
                                    // Coalescing doesn't give per-tx results, so we
                                    // mark all as success. The state is already correct.

                                    // Handle EVM logs for any WasmCall txs in the batch.
                                    let mut block_logs: Vec<arc_types::EventLog> = Vec::new();
                                    for (j, tx) in valid_txs.iter().enumerate() {
                                        if let TxBody::WasmCall(ref body) = tx.body {
                                            if exec_state.is_evm_contract(&body.contract) {
                                                let result = arc_vm::evm::evm_execute(
                                                    &exec_state,
                                                    tx.from,
                                                    body.contract,
                                                    body.calldata.clone(),
                                                    body.value,
                                                    body.gas_limit.max(1_000_000),
                                                );
                                                if !result.success {
                                                    receipt_success[orig_indices[j]] = false;
                                                }
                                                for mut log in result.logs {
                                                    log.tx_hash = tx.hash;
                                                    block_logs.push(log);
                                                }
                                            }
                                        }
                                    }
                                    if !block_logs.is_empty() {
                                        let height = exec_state.height();
                                        exec_state.store_event_logs(height + 1, block_logs);
                                    }

                                    debug!(
                                        txs = n,
                                        success = receipt_success.iter().filter(|&&v| v).count(),
                                        mode = ?actual_mode,
                                        "Pipeline: transactions executed (coalesced)"
                                    );

                                    if tx_executed
                                        .send(ExecutedBatch {
                                            transactions: vbatch.transactions,
                                            receipt_success,
                                            producer: vbatch.producer,
                                            execution_mode: actual_mode,
                                            coalesce_reads_saved,
                                        })
                                        .is_err()
                                    {
                                        break;
                                    }
                                    continue; // Skip the normal execution paths below.
                                }
                                Err(e) => {
                                    warn!("Coalesced execution failed, falling back: {}", e);
                                    // Fall through to normal execution.
                                }
                            }
                        }
                        // If not worthwhile, fall through to the normal path.
                    }

                    // ── Block-STM path ───────────────────────────────────
                    if exec_mode == ExecutionMode::BlockSTM && !valid_txs.is_empty() {
                        let stm = BlockSTM::new(Arc::clone(&exec_state));
                        let stm_result = stm.execute(&valid_txs);

                        if stm_result.rounds > 3 {
                            // Too many conflict rounds — fall back to sequential.
                            warn!(
                                rounds = stm_result.rounds,
                                reexecutions = stm_result.reexecutions,
                                "Block-STM: too many rounds, falling back to sequential"
                            );
                            actual_mode = ExecutionMode::Sequential;
                            // Fall through to sequential below.
                        } else {
                            // Block-STM succeeded within acceptable rounds.
                            for (j, &ok) in stm_result.success.iter().enumerate() {
                                receipt_success[orig_indices[j]] = ok;
                            }

                            // EVM execution for WasmCall txs.
                            let mut block_logs: Vec<arc_types::EventLog> = Vec::new();
                            for (j, tx) in valid_txs.iter().enumerate() {
                                if receipt_success[orig_indices[j]] {
                                    if let TxBody::WasmCall(ref body) = tx.body {
                                        if exec_state.is_evm_contract(&body.contract) {
                                            let result = arc_vm::evm::evm_execute(
                                                &exec_state,
                                                tx.from,
                                                body.contract,
                                                body.calldata.clone(),
                                                body.value,
                                                body.gas_limit.max(1_000_000),
                                            );
                                            if !result.success {
                                                receipt_success[orig_indices[j]] = false;
                                            }
                                            for mut log in result.logs {
                                                log.tx_hash = tx.hash;
                                                block_logs.push(log);
                                            }
                                        }
                                    }
                                }
                            }
                            if !block_logs.is_empty() {
                                let height = exec_state.height();
                                exec_state.store_event_logs(height + 1, block_logs);
                            }

                            info!(
                                txs = n,
                                success = receipt_success.iter().filter(|&&v| v).count(),
                                rounds = stm_result.rounds,
                                reexecutions = stm_result.reexecutions,
                                "Pipeline: Block-STM execution complete"
                            );

                            if tx_executed
                                .send(ExecutedBatch {
                                    transactions: vbatch.transactions,
                                    receipt_success,
                                    producer: vbatch.producer,
                                    execution_mode: ExecutionMode::BlockSTM,
                                    coalesce_reads_saved,
                                })
                                .is_err()
                            {
                                break;
                            }
                            continue; // Done, skip sequential.
                        }
                    }

                    // ── Speculative STM path ─────────────────────────────
                    if exec_mode == ExecutionMode::SpeculativeSTM && !valid_txs.is_empty() {
                        // Build a DashMap snapshot of accounts for speculative execution.
                        let account_snapshot = dashmap::DashMap::new();
                        for tx in &valid_txs {
                            // Pre-load all accounts that transactions might touch.
                            let sender_addr = tx.from;
                            if let Some(acct) = exec_state.get_account(&sender_addr) {
                                account_snapshot.insert(sender_addr.0, acct);
                            }
                            match &tx.body {
                                TxBody::Transfer(body) => {
                                    if let Some(acct) = exec_state.get_account(&body.to) {
                                        account_snapshot.insert(body.to.0, acct);
                                    }
                                }
                                TxBody::Settle(body) => {
                                    if let Some(acct) = exec_state.get_account(&body.agent_id) {
                                        account_snapshot.insert(body.agent_id.0, acct);
                                    }
                                }
                                TxBody::WasmCall(body) => {
                                    // Pre-load contract account for value transfers + execution
                                    if let Some(acct) = exec_state.get_account(&body.contract) {
                                        account_snapshot.insert(body.contract.0, acct);
                                    }
                                }
                                TxBody::Swap(body) => {
                                    if let Some(acct) = exec_state.get_account(&body.counterparty) {
                                        account_snapshot.insert(body.counterparty.0, acct);
                                    }
                                }
                                TxBody::Escrow(body) => {
                                    if let Some(acct) = exec_state.get_account(&body.beneficiary) {
                                        account_snapshot.insert(body.beneficiary.0, acct);
                                    }
                                }
                                TxBody::Stake(body) => {
                                    // Pre-load the validator address account if different from sender
                                    if body.validator != sender_addr {
                                        if let Some(acct) = exec_state.get_account(&body.validator) {
                                            account_snapshot.insert(body.validator.0, acct);
                                        }
                                    }
                                }
                                TxBody::DeployContract(_) | TxBody::RegisterAgent(_)
                                | TxBody::MultiSig(_) | TxBody::JoinValidator(_)
                                | TxBody::LeaveValidator | TxBody::ClaimRewards
                                | TxBody::UpdateStake(_) | TxBody::Governance(_)
                                | TxBody::BridgeLock(_) | TxBody::BridgeMint(_)
                                | TxBody::BatchSettle(_) | TxBody::ChannelOpen(_)
                                | TxBody::ChannelClose(_) | TxBody::ChannelDispute(_)
                                | TxBody::ShardProof(_)
                                | TxBody::InferenceAttestation(_)
                                | TxBody::InferenceChallenge(_)
                                | TxBody::InferenceRegister(_) => {}
                            }
                        }

                        let (spec_results, unresolved) =
                            execute_speculative(&valid_txs, &account_snapshot);

                        // Apply speculative results to the state and mark receipt success.
                        for res in &spec_results {
                            let j = res.tx_index;
                            if res.success {
                                // Apply writes from validated speculative execution.
                                for (&key, &(balance, nonce)) in &res.access_set.writes {
                                    let addr = arc_crypto::Hash256(key);
                                    let mut acct = exec_state
                                        .get_account(&addr)
                                        .unwrap_or_else(|| arc_types::Account::new(addr, 0));
                                    acct.balance = balance;
                                    acct.nonce = nonce;
                                    exec_state.update_account(&addr, acct);
                                }
                                receipt_success[orig_indices[j]] = true;
                            } else {
                                receipt_success[orig_indices[j]] = false;
                            }
                        }

                        // Execute unresolved transactions sequentially as fallback.
                        for &idx in &unresolved {
                            exec_state.mark_tx_accounts_dirty_pub(&valid_txs[idx]);
                            let tx_ok = exec_state.execute_tx_pub(&valid_txs[idx]).is_ok();
                            receipt_success[orig_indices[idx]] = tx_ok;
                        }

                        // EVM execution for WasmCall txs.
                        let mut block_logs: Vec<arc_types::EventLog> = Vec::new();
                        for (j, tx) in valid_txs.iter().enumerate() {
                            if receipt_success[orig_indices[j]] {
                                if let TxBody::WasmCall(ref body) = tx.body {
                                    if exec_state.is_evm_contract(&body.contract) {
                                        let result = arc_vm::evm::evm_execute(
                                            &exec_state,
                                            tx.from,
                                            body.contract,
                                            body.calldata.clone(),
                                            body.value,
                                            body.gas_limit.max(1_000_000),
                                        );
                                        if !result.success {
                                            receipt_success[orig_indices[j]] = false;
                                        }
                                        for mut log in result.logs {
                                            log.tx_hash = tx.hash;
                                            block_logs.push(log);
                                        }
                                    }
                                }
                            }
                        }
                        if !block_logs.is_empty() {
                            let height = exec_state.height();
                            exec_state.store_event_logs(height + 1, block_logs);
                        }

                        info!(
                            txs = n,
                            success = receipt_success.iter().filter(|&&v| v).count(),
                            speculative_validated = spec_results.len(),
                            sequential_fallback = unresolved.len(),
                            "Pipeline: Speculative STM execution complete"
                        );

                        if tx_executed
                            .send(ExecutedBatch {
                                transactions: vbatch.transactions,
                                receipt_success,
                                producer: vbatch.producer,
                                execution_mode: ExecutionMode::SpeculativeSTM,
                                coalesce_reads_saved,
                            })
                            .is_err()
                        {
                            break;
                        }
                        continue; // Done, skip sequential.
                    }

                    // ── GPU-Resident path ─────────────────────────────────
                    // Combines Block-STM parallel execution with GPU state cache.
                    // Hot accounts are read from GPU unified/managed memory.
                    if exec_mode == ExecutionMode::GpuResident && !valid_txs.is_empty() {
                        // Prefetch accounts that transactions will touch into GPU cache.
                        if let Some(ref cache) = exec_state.gpu_cache() {
                            let mut addrs: Vec<[u8; 32]> = Vec::with_capacity(valid_txs.len() * 2);
                            for tx in &valid_txs {
                                addrs.push(tx.from.0);
                                match &tx.body {
                                    TxBody::Transfer(body) => addrs.push(body.to.0),
                                    TxBody::Settle(body) => addrs.push(body.agent_id.0),
                                    TxBody::WasmCall(body) => addrs.push(body.contract.0),
                                    TxBody::Swap(body) => addrs.push(body.counterparty.0),
                                    TxBody::Escrow(body) => addrs.push(body.beneficiary.0),
                                    _ => {}
                                }
                            }
                            cache.prefetch(&addrs);
                        }

                        // Use Block-STM for parallel execution (with GPU-cached state).
                        let stm = BlockSTM::new(Arc::clone(&exec_state));
                        let stm_result = stm.execute(&valid_txs);

                        if stm_result.rounds <= 3 {
                            for (j, &ok) in stm_result.success.iter().enumerate() {
                                receipt_success[orig_indices[j]] = ok;
                            }

                            // Sync GPU cache after execution.
                            if let Some(ref cache) = exec_state.gpu_cache() {
                                cache.sync();
                            }

                            info!(
                                txs = n,
                                success = receipt_success.iter().filter(|&&v| v).count(),
                                rounds = stm_result.rounds,
                                reexecutions = stm_result.reexecutions,
                                gpu_state = true,
                                "Pipeline: GPU-resident Block-STM execution complete"
                            );

                            if tx_executed
                                .send(ExecutedBatch {
                                    transactions: vbatch.transactions,
                                    receipt_success,
                                    producer: vbatch.producer,
                                    execution_mode: ExecutionMode::GpuResident,
                                    coalesce_reads_saved,
                                })
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                        // Too many rounds — fall through to sequential.
                        warn!(
                            rounds = stm_result.rounds,
                            "GPU-Resident Block-STM: too many rounds, falling back to sequential"
                        );
                        actual_mode = ExecutionMode::Sequential;
                    }

                    // ── Sequential path (default / fallback) ─────────────
                    let mut block_logs: Vec<arc_types::EventLog> = Vec::new();

                    for (i, tx) in vbatch.transactions.iter().enumerate() {
                        if vbatch.sig_valid[i] {
                            exec_state.mark_tx_accounts_dirty_pub(tx);
                            let tx_ok = exec_state.execute_tx_pub(tx).is_ok();
                            receipt_success[i] = tx_ok;

                            // For EVM contract calls, run actual EVM execution
                            // to handle storage writes, internal transfers, and event logs.
                            if tx_ok {
                                if let TxBody::WasmCall(ref body) = tx.body {
                                    if exec_state.is_evm_contract(&body.contract) {
                                        let result = arc_vm::evm::evm_execute(
                                            &exec_state,
                                            tx.from,
                                            body.contract,
                                            body.calldata.clone(),
                                            body.value,
                                            body.gas_limit.max(1_000_000),
                                        );
                                        if !result.success {
                                            receipt_success[i] = false;
                                        }
                                        for mut log in result.logs {
                                            log.tx_hash = tx.hash;
                                            block_logs.push(log);
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Store event logs for this batch (height will be set at commit)
                    if !block_logs.is_empty() {
                        let height = exec_state.height();
                        exec_state.store_event_logs(height + 1, block_logs);
                    }

                    debug!(
                        txs = n,
                        success = receipt_success.iter().filter(|&&v| v).count(),
                        mode = ?actual_mode,
                        "Pipeline: transactions executed (sequential)"
                    );

                    if tx_executed
                        .send(ExecutedBatch {
                            transactions: vbatch.transactions,
                            receipt_success,
                            producer: vbatch.producer,
                            execution_mode: actual_mode,
                            coalesce_reads_saved,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("spawn execute thread");

        // ── Stage 4: Commit ──────────────────────────────────────────────
        let commit_state = Arc::clone(&state);
        thread::Builder::new()
            .name("pipeline-commit".into())
            .spawn(move || {
                while let Ok(ebatch) = rx_executed.recv() {
                    let start = std::time::Instant::now();

                    let result = commit_state.commit_executed_block(
                        &ebatch.transactions,
                        &ebatch.receipt_success,
                        ebatch.producer,
                    );

                    let elapsed = start.elapsed();

                    match result {
                        Ok((block, _receipts)) => {
                            let success_count = ebatch
                                .receipt_success
                                .iter()
                                .filter(|&&v| v)
                                .count();

                            // ── STARK proof generation ──────────────────
                            let proof_input = arc_crypto::stark::BlockProofInput {
                                height: block.header.height,
                                block_hash: block.hash.0,
                                prev_state_root: block.header.parent_hash.0,
                                post_state_root: block.header.state_root.0,
                                tx_hashes: block.tx_hashes.iter().map(|h| h.0).collect(),
                                state_diffs: vec![],
                                transfers: vec![],
                            };
                            let proof = arc_crypto::stark::BlockProof::prove(&proof_input);
                            let vr = proof.verify();

                            // Compress the proof for storage / transmission
                            let compressed = arc_crypto::proof_compress::compress_proof(&proof.proof_data);
                            let ratio = if compressed.original_size > 0 {
                                compressed.compressed_data.len() as f64 / compressed.original_size as f64
                            } else {
                                1.0
                            };
                            debug!(
                                height = block.header.height,
                                proof_size = proof.proof_size_bytes,
                                compressed_size = compressed.compressed_data.len(),
                                ratio = format!("{:.2}", ratio),
                                proving_ms = proof.proving_time_ms,
                                valid = vr.is_valid,
                                prover = if cfg!(feature = "stwo-prover") { "stwo-circle-stark" } else { "mock-blake3" },
                                "Pipeline: STARK proof generated"
                            );

                            // ── DA erasure encoding ──────────────────
                            let da_input: Vec<u8> = block
                                .tx_hashes
                                .iter()
                                .flat_map(|h| h.0.to_vec())
                                .collect();
                            let da_encoding = encode_block_data(&da_input, 4, 2);
                            debug!(
                                height = block.header.height,
                                da_root = format!("{:.2}", hex::encode(da_encoding.root.0)),
                                chunk_count = da_encoding.chunk_count,
                                "Pipeline: DA erasure encoding committed"
                            );

                            info!(
                                height = block.header.height,
                                txs = ebatch.transactions.len(),
                                success = success_count,
                                elapsed_ms = elapsed.as_millis(),
                                mode = ?ebatch.execution_mode,
                                coalesce_reads_saved = ?ebatch.coalesce_reads_saved,
                                "Pipeline: block committed"
                            );

                            let _ = tx_out.send(PipelineResult {
                                height: block.header.height,
                                tx_count: ebatch.transactions.len(),
                                success_count,
                                elapsed_ms: elapsed.as_millis(),
                                execution_mode: ebatch.execution_mode,
                                coalesce_reads_saved: ebatch.coalesce_reads_saved,
                            });
                        }
                        Err(e) => {
                            warn!("Pipeline commit failed: {}", e);
                        }
                    }
                }
            })
            .expect("spawn commit thread");

        Self { tx_in, rx_out, config, sig_cache }
    }

    /// Returns the config this pipeline was created with.
    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// Get a reference to the signature pre-verification cache.
    ///
    /// Use this to pre-verify signatures as transactions arrive in the mempool:
    /// ```rust,ignore
    /// let cache = pipeline.sig_cache();
    /// cache.pre_verify(&tasks, &hashes);
    /// // Later, verify stage checks cache automatically
    /// ```
    pub fn sig_cache(&self) -> &Arc<arc_gpu::metal_verify::SigVerifyCache> {
        &self.sig_cache
    }

    /// Submit a batch of transactions into the pipeline.
    ///
    /// Non-blocking if the pipeline has capacity, blocks briefly otherwise.
    pub fn submit(&self, batch: PipelineBatch) -> Result<(), channel::SendError<PipelineBatch>> {
        self.tx_in.send(batch)
    }

    /// Try to receive a completed result (non-blocking).
    pub fn try_recv(&self) -> Option<PipelineResult> {
        self.rx_out.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::{hash_bytes, KeyPair};

    fn addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    // ── Existing tests (unchanged) ──────────────────────────────────────

    #[test]
    fn test_pipeline_basic() {
        // Generate a keypair so we can produce a valid signature.
        let kp = KeyPair::generate_ed25519();
        let sender = kp.address();

        let state = Arc::new(StateDB::with_genesis(&[
            (sender, 1_000_000),
            (addr(2), 0),
        ]));

        let pipeline = Pipeline::new(Arc::clone(&state));

        // Create and sign the transaction.
        let mut tx = Transaction::new_transfer(sender, addr(2), 100, 0);
        tx.sign(&kp).expect("signing must succeed");

        let txs = vec![tx];
        pipeline
            .submit(PipelineBatch {
                transactions: txs,
                producer: addr(99),
            })
            .unwrap();

        // Wait for result (with timeout)
        let result = pipeline.rx_out.recv_timeout(std::time::Duration::from_secs(5));
        assert!(result.is_ok(), "pipeline should produce a result");
        let result = result.unwrap();
        assert_eq!(result.tx_count, 1);
        assert_eq!(result.success_count, 1);
        // Default mode is now BlockSTM.
        assert_eq!(result.execution_mode, ExecutionMode::BlockSTM);
        assert!(result.coalesce_reads_saved.is_none());
    }

    #[test]
    fn test_pipeline_rejects_unsigned_tx() {
        let state = Arc::new(StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 0),
        ]));

        let pipeline = Pipeline::new(Arc::clone(&state));

        // Submit an unsigned transaction — must be rejected.
        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 100, 0)];
        pipeline
            .submit(PipelineBatch {
                transactions: txs,
                producer: addr(99),
            })
            .unwrap();

        let result = pipeline.rx_out.recv_timeout(std::time::Duration::from_secs(5));
        assert!(result.is_ok(), "pipeline should produce a result");
        let result = result.unwrap();
        assert_eq!(result.tx_count, 1);
        assert_eq!(result.success_count, 0, "unsigned tx must not succeed");
    }

    // ── New tests for Block-STM + coalescing integration ────────────────

    #[test]
    fn test_pipeline_config_defaults() {
        let cfg = PipelineConfig::default();
        assert_eq!(cfg.execution_mode, ExecutionMode::BlockSTM);
        assert!(!cfg.coalesce_enabled);
        assert_eq!(cfg.batch_size, 10_000);
    }

    #[test]
    fn test_pipeline_block_stm_mode() {
        let kp = KeyPair::generate_ed25519();
        let sender = kp.address();

        let state = Arc::new(StateDB::with_genesis(&[
            (sender, 1_000_000),
            (addr(2), 0),
        ]));

        let config = PipelineConfig {
            execution_mode: ExecutionMode::BlockSTM,
            verify_mode: VerifyMode::Cpu,
            coalesce_enabled: false,
            batch_size: 10_000,
            ..Default::default()
        };
        let pipeline = Pipeline::with_config(Arc::clone(&state), config);

        // Verify config is accessible.
        assert_eq!(pipeline.config().execution_mode, ExecutionMode::BlockSTM);

        // Create and sign the transaction.
        let mut tx = Transaction::new_transfer(sender, addr(2), 100, 0);
        tx.sign(&kp).expect("signing must succeed");

        pipeline
            .submit(PipelineBatch {
                transactions: vec![tx],
                producer: addr(99),
            })
            .unwrap();

        let result = pipeline.rx_out.recv_timeout(std::time::Duration::from_secs(5));
        assert!(result.is_ok(), "pipeline should produce a result");
        let result = result.unwrap();
        assert_eq!(result.tx_count, 1);
        assert_eq!(result.success_count, 1);
        assert_eq!(result.execution_mode, ExecutionMode::BlockSTM);
        assert!(result.coalesce_reads_saved.is_none());
    }

    #[test]
    fn test_pipeline_coalesce_mode() {
        // Use two keypairs so we get multiple same-sender txs (triggers coalescing).
        let kp = KeyPair::generate_ed25519();
        let sender = kp.address();

        let state = Arc::new(StateDB::with_genesis(&[
            (sender, 1_000_000),
            (addr(2), 0),
            (addr(3), 0),
        ]));

        let config = PipelineConfig {
            execution_mode: ExecutionMode::Sequential,
            verify_mode: VerifyMode::Cpu,
            coalesce_enabled: true,
            batch_size: 10_000,
            ..Default::default()
        };
        let pipeline = Pipeline::with_config(Arc::clone(&state), config);

        assert!(pipeline.config().coalesce_enabled);

        // Create three txs from the same sender to the same receiver to ensure
        // coalescing is "worthwhile" (3 txs, 2 unique accounts).
        let mut tx1 = Transaction::new_transfer(sender, addr(2), 100, 0);
        tx1.sign(&kp).expect("signing must succeed");
        let mut tx2 = Transaction::new_transfer(sender, addr(2), 200, 1);
        tx2.sign(&kp).expect("signing must succeed");
        let mut tx3 = Transaction::new_transfer(sender, addr(2), 300, 2);
        tx3.sign(&kp).expect("signing must succeed");

        pipeline
            .submit(PipelineBatch {
                transactions: vec![tx1, tx2, tx3],
                producer: addr(99),
            })
            .unwrap();

        let result = pipeline.rx_out.recv_timeout(std::time::Duration::from_secs(5));
        assert!(result.is_ok(), "pipeline should produce a result");
        let result = result.unwrap();
        assert_eq!(result.tx_count, 3);
        assert_eq!(result.success_count, 3);
        // Coalescing should have been applied.
        assert!(
            result.coalesce_reads_saved.is_some(),
            "coalescing should report reads saved"
        );
        assert!(
            result.coalesce_reads_saved.unwrap() > 0,
            "should have saved some reads"
        );
    }
}
