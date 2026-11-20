use crate::block_stm::BlockSTM;
use arc_consensus::encode_block_data;
use arc_state::StateDB;
use arc_state::block_stm::execute_speculative;
use arc_types::{Address, Transaction, TxBody};
use crossbeam::channel::{self, Receiver, Sender};
use rayon::prelude::*;
use std::sync::Arc;
use std::thread;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    #[default]
    Sequential,
    BlockSTM,
    SpeculativeSTM,
    GpuResident,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VerifyMode {
    #[default]
    Cpu,
    GpuMetal,
    GpuCuda,
    CpuAvx512,
    CpuNeon,
}

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

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub execution_mode: ExecutionMode,
    pub verify_mode: VerifyMode,
    pub coalesce_enabled: bool,
    pub batch_size: usize,
    pub gpu_state_enabled: bool,
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

pub struct PipelineBatch {
    pub transactions: Vec<Transaction>,
    pub producer: Address,
}

pub struct PipelineResult {
    pub height: u64,
    pub tx_count: usize,
    pub success_count: usize,
    pub elapsed_ms: u128,
    pub execution_mode: ExecutionMode,
    pub coalesce_reads_saved: Option<usize>,
}

struct VerifiedBatch {
    transactions: Vec<Transaction>,
    sig_valid: Vec<bool>,
    producer: Address,
}

struct ExecutedBatch {
    transactions: Vec<Transaction>,
    receipt_success: Vec<bool>,
    producer: Address,
    execution_mode: ExecutionMode,
    coalesce_reads_saved: Option<usize>,
    transfer_pre: Vec<Option<(u64, u64, u64)>>,
}

pub struct Pipeline {
    tx_in: Sender<PipelineBatch>,
    rx_out: Receiver<PipelineResult>,
    config: PipelineConfig,
    sig_cache: Arc<arc_gpu::metal_verify::SigVerifyCache>,
}

impl Pipeline {
    pub fn new(state: Arc<StateDB>) -> Self {
        Self::with_config(state, PipelineConfig::default())
    }

    pub fn with_config(mut state: Arc<StateDB>, mut config: PipelineConfig) -> Self {
        if config.coalesce_enabled {
            warn!(
                "Pipeline coalescing is disabled until its writes and per-transaction results are canonical and restart-durable"
            );
            config.coalesce_enabled = false;
        }
        if config.gpu_state_enabled || config.execution_mode == ExecutionMode::GpuResident {
            let gpu_config = arc_state::gpu_state::GpuStateCacheConfig {
                max_gpu_accounts: config.gpu_state_capacity,
                ..Default::default()
            };
            if let Some(state_mut) = Arc::get_mut(&mut state) {
                state_mut.enable_gpu_cache(gpu_config);
            } else {
                warn!("Cannot enable GPU cache: Arc has multiple owners (startup ordering bug)");
            }
            info!(
                capacity = config.gpu_state_capacity,
                "Pipeline: GPU-resident state cache enabled"
            );
        }

        let (tx_in, rx_receive) = channel::bounded::<PipelineBatch>(2);
        let (tx_verified, rx_verified) = channel::bounded::<VerifiedBatch>(2);
        let (tx_executed, rx_executed) = channel::bounded::<ExecutedBatch>(2);
        let (tx_out, rx_out) = channel::bounded::<PipelineResult>(2);

        let sig_cache = Arc::new(arc_gpu::metal_verify::SigVerifyCache::new());

        let verify_mode = config.verify_mode;
        let verify_cache = Arc::clone(&sig_cache);
        let recovery_domain = state.transaction_domain_hash();
        thread::Builder::new()
            .name("pipeline-verify".into())
            .spawn(move || {
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

                    let hash_ok: Vec<bool> = batch
                        .transactions
                        .par_iter()
                        .map(|tx| {
                            let expected = match recovery_domain {
                                Some(domain) => tx.compute_hash_in_domain(&domain),
                                None => tx.compute_hash(),
                            };
                            expected == tx.hash
                        })
                        .collect();

                    let mut ed_indices: Vec<usize> = Vec::new();
                    let mut ed_msgs: Vec<Vec<u8>> = Vec::new();
                    let mut ed_sigs: Vec<ed25519_dalek::Signature> = Vec::new();
                    let mut ed_vks: Vec<ed25519_dalek::VerifyingKey> = Vec::new();
                    let mut other_indices: Vec<usize> = Vec::new();

                    for (i, tx) in batch.transactions.iter().enumerate() {
                        if !hash_ok[i] {
                            continue;
                        }
                        match &tx.signature {
                            arc_crypto::Signature::Ed25519 {
                                public_key,
                                signature,
                            } => {
                                let derived = arc_crypto::address_from_ed25519_pubkey(public_key);
                                if derived != tx.from {
                                    continue;
                                }
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
                                    continue;
                                }
                            }
                            _ => {
                                other_indices.push(i);
                            }
                        }
                    }

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
                            debug!(
                                cached = cached_count,
                                uncached = uncached_task_indices.len(),
                                "Pipeline: pre-verified cache hits"
                            );
                        }

                        if !uncached_task_indices.is_empty() {
                            let cpu_verify =
                                |indices: &[usize],
                                 msgs: &[Vec<u8>],
                                 sigs: &[ed25519_dalek::Signature],
                                 vks: &[ed25519_dalek::VerifyingKey],
                                 ed_idx: &[usize],
                                 out: &mut [bool]| {
                                    let msg_refs: Vec<&[u8]> =
                                        indices.iter().map(|&j| msgs[j].as_slice()).collect();
                                    let s: Vec<ed25519_dalek::Signature> =
                                        indices.iter().map(|&j| sigs[j]).collect();
                                    let v: Vec<ed25519_dalek::VerifyingKey> =
                                        indices.iter().map(|&j| vks[j]).collect();
                                    let results =
                                        arc_gpu::cpu_batch_verify_ed25519(&msg_refs, &s, &v);
                                    for (k, &valid) in results.iter().enumerate() {
                                        out[ed_idx[indices[k]]] = valid;
                                    }
                                };

                            match verify_mode {
                                VerifyMode::GpuMetal if metal_verifier.is_some() => {
                                    let verifier = metal_verifier.as_mut().unwrap();
                                    let tasks: Vec<arc_gpu::metal_verify::VerifyTask> =
                                        uncached_task_indices
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
                                    let verifier = cuda_verifier.as_mut().unwrap();
                                    let tasks: Vec<arc_gpu::metal_verify::VerifyTask> =
                                        uncached_task_indices
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
                                    let verifier = avx512_verifier.as_mut().unwrap();
                                    let tasks: Vec<arc_gpu::metal_verify::VerifyTask> =
                                        uncached_task_indices
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
                                    let verifier = neon_verifier.as_mut().unwrap();
                                    let tasks: Vec<arc_gpu::metal_verify::VerifyTask> =
                                        uncached_task_indices
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
                                    cpu_verify(
                                        &uncached_task_indices,
                                        &ed_msgs,
                                        &ed_sigs,
                                        &ed_vks,
                                        &ed_indices,
                                        &mut sig_valid,
                                    );
                                }
                            }
                        }
                    }

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

        let exec_mode = config.execution_mode;
        let exec_state = Arc::clone(&state);
        thread::Builder::new()
            .name("pipeline-execute".into())
            .spawn(move || {
                while let Ok(vbatch) = rx_verified.recv() {
                    let n = vbatch.transactions.len();

                    let valid_txs: Vec<Transaction> = vbatch
                        .transactions
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| vbatch.sig_valid[*i])
                        .map(|(_, tx)| tx.clone())
                        .collect();

                    let orig_indices: Vec<usize> = vbatch
                        .transactions
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| vbatch.sig_valid[*i])
                        .map(|(i, _)| i)
                        .collect();

                    let mut receipt_success = vec![false; n];
                    let mut actual_mode = exec_mode;
                    let coalesce_reads_saved: Option<usize> = None;

                    let transfer_pre: Vec<Option<(u64, u64, u64)>> = {
                        let mut touches: std::collections::HashMap<Address, usize> =
                            std::collections::HashMap::new();
                        for tx in &vbatch.transactions {
                            if let TxBody::Transfer(ref b) = tx.body {
                                *touches.entry(tx.from).or_insert(0) += 1;
                                *touches.entry(b.to).or_insert(0) += 1;
                            }
                        }
                        vbatch
                            .transactions
                            .iter()
                            .enumerate()
                            .map(|(i, tx)| {
                                if !vbatch.sig_valid[i] {
                                    return None;
                                }
                                let body = match tx.body {
                                    TxBody::Transfer(ref b) => b,
                                    _ => return None,
                                };
                                if touches.get(&tx.from) != Some(&1)
                                    || touches.get(&body.to) != Some(&1)
                                {
                                    return None;
                                }
                                let sender = exec_state.get_account(&tx.from)?;
                                let recv_bal = exec_state
                                    .get_account(&body.to)
                                    .map(|a| a.balance)
                                    .unwrap_or(0);
                                Some((sender.balance, sender.nonce, recv_bal))
                            })
                            .collect()
                    };

                    if exec_mode == ExecutionMode::BlockSTM && !valid_txs.is_empty() {
                        let stm = BlockSTM::new(Arc::clone(&exec_state));
                        let stm_result = stm.execute(&valid_txs);

                        if stm_result.rounds > 3 {
                            warn!(
                                rounds = stm_result.rounds,
                                reexecutions = stm_result.reexecutions,
                                "Block-STM: too many rounds, falling back to sequential"
                            );
                            actual_mode = ExecutionMode::Sequential;
                        } else {
                            for (j, &ok) in stm_result.success.iter().enumerate() {
                                receipt_success[orig_indices[j]] = ok;
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
                                    transfer_pre: transfer_pre.clone(),
                                })
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                    }

                    if exec_mode == ExecutionMode::SpeculativeSTM && !valid_txs.is_empty() {
                        let account_snapshot = dashmap::DashMap::new();
                        for tx in &valid_txs {
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
                                    if body.validator != sender_addr
                                        && let Some(acct) = exec_state.get_account(&body.validator)
                                    {
                                        account_snapshot.insert(body.validator.0, acct);
                                    }
                                }
                                TxBody::FaucetClaim(body) => {
                                    let pool_addr = arc_types::transaction::faucet_pool_address();
                                    if let Some(acct) = exec_state.get_account(&pool_addr) {
                                        account_snapshot.insert(pool_addr.0, acct);
                                    }
                                    if let Some(acct) = exec_state.get_account(&body.recipient) {
                                        account_snapshot.insert(body.recipient.0, acct);
                                    }
                                }
                                TxBody::DeployContract(_)
                                | TxBody::RegisterAgent(_)
                                | TxBody::MultiSig(_)
                                | TxBody::JoinValidator(_)
                                | TxBody::LeaveValidator
                                | TxBody::ClaimRewards
                                | TxBody::UpdateStake(_)
                                | TxBody::Governance(_)
                                | TxBody::BridgeLock(_)
                                | TxBody::BridgeMint(_)
                                | TxBody::BatchSettle(_)
                                | TxBody::ChannelOpen(_)
                                | TxBody::ChannelClose(_)
                                | TxBody::ChannelDispute(_)
                                | TxBody::ShardProof(_)
                                | TxBody::InferenceAttestation(_)
                                | TxBody::CommunityInferenceReward(_)
                                | TxBody::InferenceChallenge(_)
                                | TxBody::InferenceRegister(_)
                                | TxBody::InferenceEscrowOpen(_)
                                | TxBody::InferenceEscrowRelease(_)
                                | TxBody::InferenceEscrowRefund(_)
                                | TxBody::ModelRegistration(_)
                                | TxBody::ModelRequest(_)
                                | TxBody::ShardCoverageClaim(_)
                                | TxBody::CapacityAdvertisement(_)
                                | TxBody::ShardAssignmentProposal(_)
                                | TxBody::InferenceRequest(_)
                                | TxBody::InferenceVote(_)
                                | TxBody::InferenceFinalize(_)
                                | TxBody::NativeInferenceRequest(_)
                                | TxBody::NativeInferenceFinalize(_)
                                | TxBody::NativeInferenceRefund(_) => {}
                            }
                        }

                        let (spec_results, unresolved) =
                            execute_speculative(&valid_txs, &account_snapshot);

                        for res in &spec_results {
                            let j = res.tx_index;
                            if res.success {
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

                        for &idx in &unresolved {
                            exec_state.mark_tx_accounts_dirty_pub(&valid_txs[idx]);
                            let tx_ok = exec_state.execute_tx_pub(&valid_txs[idx]).is_ok();
                            receipt_success[orig_indices[idx]] = tx_ok;
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
                                transfer_pre: transfer_pre.clone(),
                            })
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }

                    if exec_mode == ExecutionMode::GpuResident && !valid_txs.is_empty() {
                        if let Some(cache) = exec_state.gpu_cache() {
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

                        let stm = BlockSTM::new(Arc::clone(&exec_state));
                        let stm_result = stm.execute(&valid_txs);

                        if stm_result.rounds <= 3 {
                            for (j, &ok) in stm_result.success.iter().enumerate() {
                                receipt_success[orig_indices[j]] = ok;
                            }

                            if let Some(cache) = exec_state.gpu_cache() {
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
                                    transfer_pre: transfer_pre.clone(),
                                })
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                        warn!(
                            rounds = stm_result.rounds,
                            "GPU-Resident Block-STM: too many rounds, falling back to sequential"
                        );
                        actual_mode = ExecutionMode::Sequential;
                    }

                    for (i, tx) in vbatch.transactions.iter().enumerate() {
                        if vbatch.sig_valid[i] {
                            exec_state.mark_tx_accounts_dirty_pub(tx);
                            let tx_ok = exec_state.execute_tx_pub(tx).is_ok();
                            receipt_success[i] = tx_ok;
                        }
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
                            transfer_pre: transfer_pre.clone(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("spawn execute thread");

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
                            let success_count =
                                ebatch.receipt_success.iter().filter(|&&v| v).count();

                            let mut state_diffs: Vec<([u8; 32], [u8; 32], [u8; 32])> = Vec::new();
                            let mut transfers: Vec<arc_crypto::stark::TransferWitness> = Vec::new();
                            let acct_digest = |addr: &Address, bal: u64, nonce: u64| -> [u8; 32] {
                                let mut buf = Vec::with_capacity(48);
                                buf.extend_from_slice(&addr.0);
                                buf.extend_from_slice(&bal.to_le_bytes());
                                buf.extend_from_slice(&nonce.to_le_bytes());
                                arc_crypto::hash_bytes(&buf).0
                            };
                            for (i, tx) in ebatch.transactions.iter().enumerate() {
                                if !ebatch.receipt_success.get(i).copied().unwrap_or(false) {
                                    continue;
                                }
                                let Some((sbb, snb, rbb)) =
                                    ebatch.transfer_pre.get(i).copied().flatten()
                                else {
                                    continue;
                                };
                                let TxBody::Transfer(ref body) = tx.body else {
                                    continue;
                                };
                                let Some(sender_post) = commit_state.get_account(&tx.from) else {
                                    continue;
                                };
                                let recv_post = commit_state
                                    .get_account(&body.to)
                                    .map(|a| a.balance)
                                    .unwrap_or(0);
                                let (Ok(nonce_before), Ok(nonce_after)) =
                                    (u32::try_from(snb), u32::try_from(sender_post.nonce))
                                else {
                                    continue;
                                };
                                let witness = arc_crypto::stark::TransferWitness {
                                    sender_bal_before: sbb,
                                    sender_bal_after: sender_post.balance,
                                    receiver_bal_before: rbb,
                                    receiver_bal_after: recv_post,
                                    amount: body.amount,
                                    sender_nonce_before: nonce_before,
                                    sender_nonce_after: nonce_after,
                                    fee: tx.fee,
                                };
                                if !witness.satisfies_air() {
                                    continue;
                                }
                                state_diffs.push((
                                    tx.from.0,
                                    acct_digest(&tx.from, sbb, snb),
                                    acct_digest(&tx.from, sender_post.balance, sender_post.nonce),
                                ));
                                transfers.push(witness);
                            }
                            let witness_rows = transfers.len();

                            let proof_input = arc_crypto::stark::BlockProofInput {
                                height: block.header.height,
                                block_hash: block.hash.0,
                                prev_state_root: block.header.parent_hash.0,
                                post_state_root: block.header.state_root.0,
                                tx_hashes: block.tx_hashes.iter().map(|h| h.0).collect(),
                                state_diffs,
                                transfers,
                            };
                            let proof = arc_crypto::stark::BlockProof::prove(&proof_input);
                            let vr = proof.verify();

                            let compressed =
                                arc_crypto::proof_compress::compress_proof(&proof.proof_data);
                            let ratio = if compressed.original_size > 0 {
                                compressed.compressed_data.len() as f64
                                    / compressed.original_size as f64
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
                                prover = if cfg!(feature = "stwo-prover") {
                                    "stwo-circle-stark"
                                } else {
                                    "mock-blake3"
                                },
                                witness_rows,
                                "Pipeline: STARK proof generated"
                            );

                            let da_input: Vec<u8> =
                                block.tx_hashes.iter().flat_map(|h| h.0.to_vec()).collect();
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

        Self {
            tx_in,
            rx_out,
            config,
            sig_cache,
        }
    }

    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    pub fn sig_cache(&self) -> &Arc<arc_gpu::metal_verify::SigVerifyCache> {
        &self.sig_cache
    }

    pub fn submit(&self, batch: PipelineBatch) -> Result<(), channel::SendError<PipelineBatch>> {
        self.tx_in.send(batch)
    }

    pub fn try_recv(&self) -> Option<PipelineResult> {
        self.rx_out.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::{KeyPair, hash_bytes};

    fn addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    #[test]
    fn test_pipeline_basic() {
        let kp = KeyPair::generate_ed25519();
        let sender = kp.address();

        let state = Arc::new(StateDB::with_genesis(&[(sender, 1_000_000), (addr(2), 0)]));

        let pipeline = Pipeline::new(Arc::clone(&state));

        let mut tx = Transaction::new_transfer(sender, addr(2), 100, 0);
        tx.sign(&kp).expect("signing must succeed");

        let txs = vec![tx];
        pipeline
            .submit(PipelineBatch {
                transactions: txs,
                producer: addr(99),
            })
            .unwrap();

        let result = pipeline
            .rx_out
            .recv_timeout(std::time::Duration::from_secs(5));
        assert!(result.is_ok(), "pipeline should produce a result");
        let result = result.unwrap();
        assert_eq!(result.tx_count, 1);
        assert_eq!(result.success_count, 1);
        assert_eq!(result.execution_mode, ExecutionMode::BlockSTM);
        assert!(result.coalesce_reads_saved.is_none());
    }

    #[test]
    fn test_pipeline_rejects_unsigned_tx() {
        let state = Arc::new(StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]));

        let pipeline = Pipeline::new(Arc::clone(&state));

        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 100, 0)];
        pipeline
            .submit(PipelineBatch {
                transactions: txs,
                producer: addr(99),
            })
            .unwrap();

        let result = pipeline
            .rx_out
            .recv_timeout(std::time::Duration::from_secs(5));
        assert!(result.is_ok(), "pipeline should produce a result");
        let result = result.unwrap();
        assert_eq!(result.tx_count, 1);
        assert_eq!(result.success_count, 0, "unsigned tx must not succeed");
    }

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

        let state = Arc::new(StateDB::with_genesis(&[(sender, 1_000_000), (addr(2), 0)]));

        let config = PipelineConfig {
            execution_mode: ExecutionMode::BlockSTM,
            verify_mode: VerifyMode::Cpu,
            coalesce_enabled: false,
            batch_size: 10_000,
            ..Default::default()
        };
        let pipeline = Pipeline::with_config(Arc::clone(&state), config);

        assert_eq!(pipeline.config().execution_mode, ExecutionMode::BlockSTM);

        let mut tx = Transaction::new_transfer(sender, addr(2), 100, 0);
        tx.sign(&kp).expect("signing must succeed");

        pipeline
            .submit(PipelineBatch {
                transactions: vec![tx],
                producer: addr(99),
            })
            .unwrap();

        let result = pipeline
            .rx_out
            .recv_timeout(std::time::Duration::from_secs(5));
        assert!(result.is_ok(), "pipeline should produce a result");
        let result = result.unwrap();
        assert_eq!(result.tx_count, 1);
        assert_eq!(result.success_count, 1);
        assert_eq!(result.execution_mode, ExecutionMode::BlockSTM);
        assert!(result.coalesce_reads_saved.is_none());
    }

    #[test]
    fn pipeline_cannot_mutate_state_after_rejected_evm_call() {
        let keypair = KeyPair::generate_ed25519();
        let sender = keypair.address();
        let contract = addr(87);
        let storage_key = hash_bytes(b"pipeline-evm-storage");
        let state = Arc::new(StateDB::with_genesis(&[(sender, 1_000_000)]));
        state.deploy_contract(&contract, vec![0x60, 0x00]);
        state.set_storage(&contract, storage_key, b"before".to_vec());

        let pipeline = Pipeline::with_config(
            Arc::clone(&state),
            PipelineConfig {
                execution_mode: ExecutionMode::BlockSTM,
                verify_mode: VerifyMode::Cpu,
                coalesce_enabled: false,
                ..Default::default()
            },
        );
        let mut transaction = Transaction::new_wasm_call(
            sender,
            contract,
            String::new(),
            vec![0xde, 0xad, 0xbe, 0xef],
            9,
            1_000_000,
            0,
        );
        transaction.sign(&keypair).unwrap();
        let transaction_hash = transaction.hash;

        pipeline
            .submit(PipelineBatch {
                transactions: vec![transaction],
                producer: addr(99),
            })
            .unwrap();
        let result = pipeline
            .rx_out
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("pipeline result");

        assert_eq!(result.success_count, 0);
        let sender_account = state.get_account(&sender).unwrap();
        assert_eq!(sender_account.balance, 1_000_000);
        assert_eq!(sender_account.nonce, 0);
        assert_eq!(
            state.get_storage(&contract, &storage_key),
            Some(b"before".to_vec())
        );
        assert!(state.event_logs.is_empty());
        assert!(
            !state
                .get_receipt(&transaction_hash.0)
                .expect("failed transaction still has a canonical receipt")
                .success
        );
    }

    #[test]
    fn configured_coalescing_is_forced_to_canonical_fallback() {
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

        assert!(
            !pipeline.config().coalesce_enabled,
            "unsafe coalescing must remain disabled in protocol v3"
        );

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

        let result = pipeline
            .rx_out
            .recv_timeout(std::time::Duration::from_secs(5));
        assert!(result.is_ok(), "pipeline should produce a result");
        let result = result.unwrap();
        assert_eq!(result.tx_count, 3);
        assert_eq!(result.success_count, 3);
        assert!(
            result.coalesce_reads_saved.is_none(),
            "canonical fallback must not claim coalescing savings"
        );
        assert_eq!(state.get_account(&sender).unwrap().nonce, 3);
        assert_eq!(state.get_account(&sender).unwrap().balance, 1_000_000 - 600);
        assert_eq!(state.get_account(&addr(2)).unwrap().balance, 600);
    }
}
