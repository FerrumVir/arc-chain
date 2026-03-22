//! Consensus manager — wires arc-consensus into the node.
//!
//! Wraps the DAG `ConsensusEngine` and drives the propose → commit loop,
//! draining the mempool and feeding committed blocks into `StateDB`.

use arc_consensus::{ConsensusEngine, StakeTier, Validator, ValidatorSet};
use arc_crypto::{Hash256, KeyPair};
use arc_mempool::{EncryptedMempool, Mempool};
use arc_net::transport::{InboundMessage, OutboundMessage};
use arc_state::StateDB;
use crate::pipeline::{Pipeline, PipelineBatch};
use crate::vrf::ProposerSelector;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Orchestrates DAG consensus for a single validator node.
pub struct ConsensusManager {
    /// The underlying DAG consensus engine.
    pub engine: Arc<ConsensusEngine>,
    /// This validator's address.
    pub validator_address: Hash256,
    /// This validator's staked ARC.
    pub stake: u64,
    /// Stake tier (Spark / Arc / Core).
    pub tier: StakeTier,
    /// Number of sender-shards.
    pub num_shards: u16,
    /// Whether benchmark mode is active (bypass mempool, generate txs directly).
    pub benchmark: bool,
    /// Whether this node runs in proposer mode (full execution + state diff export).
    /// When false, acts as a verifier (applies diffs, confirms roots).
    pub proposer_mode: bool,
    /// Pending state diffs received from proposer nodes, keyed by block hash.
    pending_diffs: dashmap::DashMap<[u8; 32], (arc_types::StateDiff, u64)>,
    /// VRF-based proposer selector (None = VRF disabled, backward compat).
    vrf_selector: Option<ProposerSelector>,
    /// Encrypted mempool for MEV-protected commit-reveal transactions.
    /// Runs alongside the regular mempool when `Some`.
    encrypted_mempool: Option<Arc<EncryptedMempool>>,
}

impl ConsensusManager {
    /// Create a new consensus manager.
    ///
    /// # Arguments
    /// * `validator_address` — 256-bit address derived from the validator key.
    /// * `stake` — amount of ARC staked (must be >= STAKE_SPARK).
    /// * `num_shards` — number of sender-shards for the DAG.
    ///
    /// # Panics
    /// Panics if `stake` is below the minimum Spark threshold (500 000 ARC).
    pub fn new(validator_address: Hash256, stake: u64, num_shards: u16, benchmark: bool, peer_validators: &[(Hash256, u64)]) -> Self {
        let (validator_set, tier) = Self::build_validator_set(validator_address, stake, peer_validators);
        let engine = Arc::new(ConsensusEngine::new(validator_set, validator_address));

        info!(
            address = %validator_address,
            stake = stake,
            tier = ?tier,
            shards = num_shards,
            "ConsensusManager initialized (no keypair — test mode)"
        );

        let vrf_selector = Self::build_vrf_selector(validator_address, stake, peer_validators);

        Self { engine, validator_address, stake, tier, num_shards, benchmark, proposer_mode: false, pending_diffs: dashmap::DashMap::new(), vrf_selector, encrypted_mempool: Some(Arc::new(EncryptedMempool::new(100_000))) }
    }

    /// Create a consensus manager with a signing keypair (production mode).
    /// Blocks proposed by this node will be signed with the keypair,
    /// and unsigned blocks from peers will be rejected.
    pub fn new_with_keypair(
        validator_address: Hash256,
        stake: u64,
        num_shards: u16,
        benchmark: bool,
        peer_validators: &[(Hash256, u64)],
        keypair: KeyPair,
    ) -> Self {
        let (validator_set, tier) = Self::build_validator_set(validator_address, stake, peer_validators);
        let engine = Arc::new(ConsensusEngine::new_with_keypair(validator_set, validator_address, keypair));

        info!(
            address = %validator_address,
            stake = stake,
            tier = ?tier,
            shards = num_shards,
            "ConsensusManager initialized (signed block mode)"
        );

        let vrf_selector = Self::build_vrf_selector(validator_address, stake, peer_validators);

        Self { engine, validator_address, stake, tier, num_shards, benchmark, proposer_mode: false, pending_diffs: dashmap::DashMap::new(), vrf_selector, encrypted_mempool: Some(Arc::new(EncryptedMempool::new(100_000))) }
    }

    /// Enable proposer mode: this node fully executes blocks and exports
    /// state diffs for verifier nodes.  Without proposer mode, the node
    /// acts as a verifier and applies diffs from proposers.
    pub fn set_proposer_mode(&mut self, enabled: bool) {
        self.proposer_mode = enabled;
        info!(proposer_mode = enabled, "Propose-Verify mode updated");
    }

    fn build_validator_set(
        validator_address: Hash256,
        stake: u64,
        peer_validators: &[(Hash256, u64)],
    ) -> (ValidatorSet, StakeTier) {
        let tier = StakeTier::from_stake(stake)
            .expect("stake must be >= 500_000 ARC (Spark threshold)");

        let validator = Validator::new(validator_address, stake, 0)
            .expect("validator creation failed — stake below minimum");

        let mut validators = vec![validator];
        for (addr, peer_stake) in peer_validators {
            if let Some(v) = Validator::new(*addr, *peer_stake, 0) {
                validators.push(v);
            }
        }
        let validator_set = ValidatorSet::new(validators, 0);
        (validator_set, tier)
    }

    /// Build a VRF ProposerSelector from the local validator + peers.
    fn build_vrf_selector(
        validator_address: Hash256,
        stake: u64,
        peer_validators: &[(Hash256, u64)],
    ) -> Option<ProposerSelector> {
        use crate::vrf::ValidatorInfo;

        let mut vrf_validators = vec![ValidatorInfo {
            public_key: validator_address.0, // Use address bytes as pubkey placeholder
            stake,
            address: validator_address,
        }];
        for (addr, peer_stake) in peer_validators {
            vrf_validators.push(ValidatorInfo {
                public_key: addr.0,
                stake: *peer_stake,
                address: *addr,
            });
        }
        Some(ProposerSelector::new(vrf_validators))
    }

    /// Returns whether the validator set has more than one validator,
    /// meaning multi-validator DAG commit should be used instead of
    /// the single-validator fast path.
    pub fn is_multi_validator(&self) -> bool {
        self.engine.validator_set().len() > 1
    }

    /// Run the consensus loop: propose blocks, advance rounds, commit, and
    /// execute against state.
    ///
    /// When `inbound_rx` and `outbound_tx` are provided, the loop integrates
    /// with the P2P transport layer for multi-node consensus. When `None`,
    /// it behaves as a single-node (backward compatible).
    pub async fn run_consensus_loop(
        &self,
        state: Arc<StateDB>,
        mempool: Arc<Mempool>,
        mut inbound_rx: Option<mpsc::Receiver<InboundMessage>>,
        outbound_tx: Option<mpsc::Sender<OutboundMessage>>,
        benchmark_pool: Option<Arc<crate::benchmark::BenchmarkPool>>,
    ) {
        use arc_types::Transaction;
        use dashmap::DashMap;

        info!(
            tier = ?self.tier,
            address = %self.validator_address,
            multi_validator = self.is_multi_validator(),
            validators = self.engine.validator_set().len(),
            "Consensus loop started"
        );

        let can_produce = self.tier.can_produce_blocks();
        if !can_produce {
            info!("Validator is Spark tier — observing only (cannot produce blocks)");
        }

        // Pending transaction index: tx_hash → Transaction
        // Transactions live here between drain from mempool and execution.
        let pending_txs: DashMap<[u8; 32], Transaction> = DashMap::new();

        // Track last proposed round to avoid double-proposing.
        let mut last_proposed_round: Option<u64> = None;

        // Pending encrypted transaction batches, keyed by DAG block hash.
        // Stored at proposal time, revealed after DAG commit.
        let pending_encrypted: DashMap<[u8; 32], Vec<arc_mempool::EncryptedTx>> = DashMap::new();

        // ── Pipeline for single-validator pipelined execution ────────────
        let pipeline = Pipeline::new(Arc::clone(&state));

        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

            // ── Drain pipeline results ──────────────────────────────────
            while let Some(result) = pipeline.try_recv() {
                info!(
                    height = result.height,
                    txs = result.tx_count,
                    success = result.success_count,
                    elapsed_ms = result.elapsed_ms,
                    "Block produced (pipeline)"
                );
            }

            // ── 0. Process inbound network messages ─────────────────────
            if let Some(ref mut rx) = inbound_rx {
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        InboundMessage::PeerConnected { address, stake } => {
                            // Check if this peer is already in our validator set.
                            // If so, this is a reconnect — do NOT reset the DAG,
                            // which would destroy all round progress and cause
                            // perpetual 0 TPS in soak tests with network jitter.
                            let already_known = self.engine.validator_set().is_validator(&address);
                            if already_known {
                                info!(
                                    peer = %address,
                                    "Peer reconnected — already in validator set, keeping DAG state"
                                );
                            } else {
                                // New peer: rebuild validator set including all
                                // existing validators plus the new one.
                                let current_vs = self.engine.validator_set();
                                let mut validators: Vec<Validator> = current_vs.validators.clone();
                                if let Some(v) = Validator::new(address, stake, 0) {
                                    validators.push(v);
                                }
                                // Ensure local validator is present
                                if !validators.iter().any(|v| v.address == self.validator_address) {
                                    if let Some(v) = Validator::new(self.validator_address, self.stake, 0) {
                                        validators.push(v);
                                    }
                                }
                                let was_single = current_vs.len() <= 1;
                                let new_set = ValidatorSet::new(validators, 0);
                                self.engine.update_validator_set(new_set);

                                // Only reset DAG when transitioning from single to
                                // multi-validator. Previous single-validator blocks
                                // were already executed via fast path, so nothing is lost.
                                // In multi-to-multi transitions (adding a 3rd validator),
                                // preserve existing DAG progress.
                                if was_single {
                                    self.engine.reset_dag();
                                    pending_txs.clear();
                                    last_proposed_round = None;
                                }
                                info!(
                                    peer = %address,
                                    validators = self.engine.validator_set().len(),
                                    was_single = was_single,
                                    "Peer connected — ValidatorSet updated"
                                );
                            }
                        }
                        InboundMessage::PeerDisconnected { address } => {
                            // Remove disconnected peer from validator set.
                            let current_vs = self.engine.validator_set();
                            let remaining: Vec<Validator> = current_vs
                                .validators
                                .iter()
                                .filter(|v| v.address != address)
                                .cloned()
                                .collect();
                            // Ensure local validator is present
                            let mut validators = remaining;
                            if !validators.iter().any(|v| v.address == self.validator_address) {
                                if let Some(v) = Validator::new(self.validator_address, self.stake, 0) {
                                    validators.push(v);
                                }
                            }
                            let now_single = validators.len() <= 1;
                            let new_set = ValidatorSet::new(validators, 0);
                            self.engine.update_validator_set(new_set);

                            // Only reset DAG when reverting to single-validator mode.
                            // The pending DAG blocks are no longer useful since the
                            // peer that produced them is gone and we can't reach quorum.
                            if now_single {
                                self.engine.reset_dag();
                                pending_txs.clear();
                                last_proposed_round = None;
                            }
                            info!(
                                peer = %address,
                                now_single = now_single,
                                "Peer disconnected — validator removed"
                            );
                        }
                        InboundMessage::DagBlockWithTxs {
                            block,
                            transactions,
                        } => {
                            // Insert the full transactions into pending_txs
                            // so we can resolve them when this block commits.
                            for tx in &transactions {
                                pending_txs.insert(tx.hash.0, tx.clone());
                            }
                            // Feed block into consensus engine
                            match self.engine.receive_block(&block) {
                                Ok(()) => {
                                    debug!(
                                        author = %block.author,
                                        round = block.round,
                                        txs = block.transactions.len(),
                                        "Received DAG block from peer"
                                    );
                                    let _ = self.engine.advance_round();
                                }
                                Err(e) => {
                                    warn!(
                                        author = %block.author,
                                        round = block.round,
                                        "Rejected DAG block: {}",
                                        e
                                    );
                                }
                            }
                        }
                        InboundMessage::StateDiff { block_hash, diff, block_height } => {
                            // Store the state diff for when this block commits.
                            self.pending_diffs.insert(block_hash.0, (diff, block_height));
                            debug!(
                                block = %block_hash,
                                height = block_height,
                                "Received state diff from proposer"
                            );
                        }
                        InboundMessage::Transactions(txs) => {
                            let mut inserted = 0usize;
                            for tx_bytes in txs {
                                if let Ok(tx) =
                                    bincode::deserialize::<Transaction>(&tx_bytes)
                                {
                                    // Skip if already proposed (prevents gossip loop:
                                    // drain removes from mempool.seen, so without this
                                    // check the same tx bounces between peers forever)
                                    if pending_txs.contains_key(&tx.hash.0) {
                                        continue;
                                    }
                                    if mempool.insert(tx).is_ok() {
                                        inserted += 1;
                                    }
                                }
                            }
                            if inserted > 0 {
                                debug!(count = inserted, "Inserted gossiped txs into mempool");
                            }
                        }
                        // State sync messages are handled by the RPC layer,
                        // not the consensus loop. Log and ignore at this layer.
                        InboundMessage::SnapshotManifestRequest { .. }
                        | InboundMessage::SnapshotChunkRequest { .. }
                        | InboundMessage::SnapshotManifestResponse { .. }
                        | InboundMessage::SnapshotChunkResponse { .. } => {
                            debug!("Received state sync message via P2P (handled by RPC layer)");
                        }
                    }
                }
            }

            // Check multi-validator EACH iteration (validator set is dynamic).
            let multi_validator = self.is_multi_validator();
            let current_round = self.engine.current_round();
            let already_proposed = last_proposed_round == Some(current_round);

            // ── Pre-feed benchmark transactions into mempool ──────────────
            // Do this BEFORE the propose check so transactions are always
            // available regardless of round/parent state.
            if self.benchmark && multi_validator {
                if let Some(ref pool) = benchmark_pool {
                    let signed_txs = pool.drain(10_000);
                    let fed = signed_txs.len();
                    for tx in signed_txs {
                        let _ = mempool.insert(tx);
                    }
                    if fed > 0 {
                        info!("Benchmark pre-feed: {} txs into mempool (size now: {})", fed, mempool.len());
                    }
                }
            }

            // ── 1. Propose a block ─────────────────────────────────────────
            // In multi-validator mode, propose every round (even empty) so the
            // DAG advances and the 2-round commit rule can fire.
            // In single-validator mode, only propose when there are transactions.
            //
            // IMPORTANT: Check parent readiness BEFORE draining the mempool.
            // If the peer's block from the previous round hasn't arrived yet,
            // we would fail to propose and lose the drained transactions.
            let has_quorum_parents = if current_round == 0 {
                true // Round 0 has no parent requirement
            } else if self.engine.is_force_advanced() {
                // After a view-change (force_advance_round), relax the parent
                // check. The force advance already decided to skip quorum for
                // the stalled round, so requiring quorum parents here would
                // re-deadlock immediately. Accept whatever parents exist.
                true
            } else {
                let vs = self.engine.validator_set();
                let prev_blocks = self.engine.blocks_in_round(current_round - 1);
                let mut parent_stake = 0u64;
                for hash in &prev_blocks {
                    if let Some(block) = self.engine.get_block(&hash) {
                        if let Some(validator) = vs.get_validator(&block.author) {
                            parent_stake += validator.stake;
                        }
                    }
                }
                parent_stake >= vs.quorum
            };

            // ── VRF proposer eligibility check ──────────────────────────
            // Use a deterministic hash of (round, validator_address) as a VRF
            // output proxy. Full VRF proofs (with keypair-based computation)
            // are verified when receiving blocks from peers; this local check
            // uses the same threshold arithmetic to decide "should I propose?".
            let vrf_approved = if let Some(ref selector) = self.vrf_selector {
                let mut vrf_input = [0u8; 40];
                vrf_input[..8].copy_from_slice(&current_round.to_le_bytes());
                vrf_input[8..40].copy_from_slice(&self.validator_address.0);
                let vrf_hash = blake3::hash(&vrf_input);
                let vrf_output = crate::vrf::VrfOutput { value: *vrf_hash.as_bytes() };
                selector.is_proposer(self.stake, &vrf_output)
            } else {
                true // No VRF = always allowed (backward compat)
            };

            // In benchmark/testnet mode, always allow proposals even without
            // quorum parents — the DAG is in catch-up and strict parent checks
            // would prevent any transactions from being included.
            let allow_propose = has_quorum_parents || self.benchmark;
            if can_produce && !already_proposed && allow_propose && vrf_approved {
                // ── Benchmark fast path: drain pre-signed txs, verify+execute ──
                if self.benchmark && !multi_validator {
                    if let Some(ref pool) = benchmark_pool {
                        let signed_txs = pool.drain(1_000_000);
                        if !signed_txs.is_empty() {
                            let tx_count = signed_txs.len() as u64;
                            let start = std::time::Instant::now();
                            match state.execute_block_signed_benchmark(
                                &signed_txs,
                                self.validator_address,
                            ) {
                                Ok(block) => {
                                    let elapsed = start.elapsed();
                                    let tps = if elapsed.as_secs_f64() > 0.0 {
                                        tx_count as f64 / elapsed.as_secs_f64()
                                    } else {
                                        tx_count as f64
                                    };
                                    info!(
                                        height = block.header.height,
                                        txs = tx_count,
                                        elapsed_ms = elapsed.as_millis(),
                                        tps = format!("{:.0}", tps),
                                        "Signed benchmark block produced"
                                    );
                                }
                                Err(e) => {
                                    warn!("Benchmark block failed: {}", e);
                                }
                            }
                        }
                    }
                    // Still advance DAG round for tracking
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let _ = self.engine.propose_block(vec![], timestamp);
                    let _ = self.engine.advance_round();
                } else {
                    // ── Benchmark multi-validator: feed signed txs into mempool ──
                    if self.benchmark {
                        if let Some(ref pool) = benchmark_pool {
                            let signed_txs = pool.drain(50_000);
                            for tx in signed_txs {
                                let _ = mempool.insert(tx);
                            }
                        }
                    }

                    // ── Normal path: drain mempool ──────────────────────────────
                    let transactions = mempool.drain(50_000);
                    if !transactions.is_empty() {
                        info!("Drained {} txs from mempool for DAG proposal", transactions.len());
                    }

                    // ── Encrypted mempool: drain encrypted txs in FIFO order ──
                    // Encrypted transactions are included alongside regular ones.
                    // They remain opaque until after DAG commit (reveal phase).
                    let encrypted_batch = if let Some(ref emp) = self.encrypted_mempool {
                        let batch = emp.drain_fifo(10_000);
                        if !batch.is_empty() {
                            debug!(
                                count = batch.len(),
                                slot = emp.current_slot(),
                                "Drained encrypted transactions (FIFO)"
                            );
                        }
                        batch
                    } else {
                        Vec::new()
                    };

                    let has_txs = !transactions.is_empty() || !encrypted_batch.is_empty();

                    if has_txs || multi_validator {
                        let tx_hashes: Vec<Hash256> =
                            transactions.iter().map(|tx| tx.hash).collect();

                        // Index transactions for later lookup on commit
                        if has_txs {
                            for tx in &transactions {
                                pending_txs.insert(tx.hash.0, tx.clone());
                            }
                            // NOTE: We do NOT gossip TX separately here.
                            // The DagBlockWithTxs broadcast below carries the
                            // full transactions, making BroadcastTransactions
                            // redundant. Separate gossip causes peers to insert
                            // our TX into their mempools, which they then
                            // re-propose, causing duplicate proposals and
                            // nonce conflicts on execution.
                        }

                        let timestamp = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;

                        match self.engine.propose_block(tx_hashes, timestamp) {
                            Ok(block) => {
                                debug!(
                                    round = block.round,
                                    txs = block.transactions.len(),
                                    hash = %block.hash,
                                    "Proposed DAG block"
                                );
                                last_proposed_round = Some(block.round);

                                // Store encrypted batch for reveal after commit.
                                if !encrypted_batch.is_empty() {
                                    pending_encrypted.insert(block.hash.0, encrypted_batch.clone());
                                }

                                // Broadcast to peers
                                if let Some(ref tx_chan) = outbound_tx {
                                    let _ =
                                        tx_chan.try_send(OutboundMessage::BroadcastDagBlock {
                                            block: block.clone(),
                                            transactions: transactions.clone(),
                                        });
                                }
                            }
                            Err(e) => {
                                warn!("Failed to propose block: {}", e);
                            }
                        }

                        // After proposing, try to advance the round.
                        let _ = self.engine.advance_round();

                        // Advance the encrypted mempool slot each round so that
                        // new encrypted transactions target the next slot key.
                        if let Some(ref emp) = self.encrypted_mempool {
                            emp.advance_slot();
                        }

                        if multi_validator {
                            // ── Multi-validator: DAG commit path ─────────────
                            if has_txs {
                                debug!(
                                    pending = pending_txs.len(),
                                    "Multi-validator mode: waiting for DAG commit"
                                );
                            }
                        } else if has_txs {
                            // ── Pipeline path: single-validator mode ─────────
                            pipeline.submit(PipelineBatch {
                                transactions: transactions.clone(),
                                producer: self.validator_address,
                            }).unwrap_or_else(|e| {
                                warn!("Pipeline submit failed: {:?}", e);
                            });

                            // Clean up pending index — pipeline owns them now
                            for tx in &transactions {
                                pending_txs.remove(&tx.hash.0);
                            }
                        }
                    }
                }
            }

            // ── 2. Try to commit finalized DAG blocks (multi-validator) ──────
            let committed = self.engine.try_commit();
            if !committed.is_empty() {
                for dag_block in &committed {
                    info!(
                        round = dag_block.round,
                        hash = %dag_block.hash,
                        txs = dag_block.transactions.len(),
                        "DAG block committed"
                    );

                    // ── Encrypted mempool: reveal phase (commit-reveal) ──────
                    // After DAG commit, decrypt encrypted transactions from
                    // the batch that was included in this block. Revealed
                    // transactions are fed back into pending_txs for execution.
                    if let Some(ref emp) = self.encrypted_mempool {
                        if let Some((_, enc_batch)) = pending_encrypted.remove(&dag_block.hash.0) {
                            if !enc_batch.is_empty() {
                                let revealed = emp.reveal_batch(&enc_batch, dag_block.round);
                                let revealed_count = revealed.len();
                                for rtx in revealed {
                                    pending_txs.insert(rtx.transaction.hash.0, rtx.transaction);
                                }
                                if revealed_count > 0 {
                                    info!(
                                        count = revealed_count,
                                        round = dag_block.round,
                                        block = %dag_block.hash,
                                        "Revealed encrypted transactions after DAG commit"
                                    );
                                }
                            }
                        }
                    }

                    // In multi-validator mode, process committed transactions.
                    // Proposer: full execution + export state diff.
                    // Verifier: apply received state diff + verify root.
                    if multi_validator {
                        let mut committed_txs: Vec<Transaction> = Vec::new();
                        for tx_hash in &dag_block.transactions {
                            if let Some((_, tx)) = pending_txs.remove(&tx_hash.0) {
                                committed_txs.push(tx);
                            }
                        }
                        if !committed_txs.is_empty() {
                            // ── Pipeline stage overlap: pre-verify signatures ──
                            // Verify all signatures in a background task before
                            // execution, so the next block's verification can
                            // overlap with this block's execution.
                            let pre_verify_handle = {
                                let mut txs = committed_txs.clone();
                                tokio::spawn(async move {
                                    for tx in txs.iter_mut() {
                                        if !tx.is_unsigned() && !tx.sig_verified {
                                            if tx.verify_signature().is_ok() {
                                                tx.sig_verified = true;
                                            }
                                        }
                                    }
                                    txs
                                })
                            };
                            // Await pre-verification (overlaps with any prior commit work)
                            committed_txs = match pre_verify_handle.await {
                                Ok(verified_txs) => verified_txs,
                                Err(_) => committed_txs, // fallback: use unverified
                            };

                            let start = std::time::Instant::now();

                            // Check if we have a state diff from a proposer.
                            let received_diff = self.pending_diffs.remove(&dag_block.hash.0);

                            if self.proposer_mode || received_diff.is_none() {
                                // ── PROPOSER PATH: adaptive execution (auto-selects Sequential vs BlockSTM) ──
                                match state.execute_block_adaptive(&committed_txs, self.validator_address)
                                {
                                    Ok((block, receipts)) => {
                                        let elapsed = start.elapsed();
                                        let success =
                                            receipts.iter().filter(|r| r.success).count();
                                        let tps = if elapsed.as_secs_f64() > 0.0 {
                                            committed_txs.len() as f64 / elapsed.as_secs_f64()
                                        } else {
                                            committed_txs.len() as f64
                                        };

                                        // Run EVM execution for any EVM contract calls.
                                        let mut block_logs: Vec<arc_types::EventLog> = Vec::new();
                                        for (i, tx) in committed_txs.iter().enumerate() {
                                            if receipts[i].success {
                                                if let arc_types::TxBody::WasmCall(ref body) = tx.body {
                                                    if state.is_evm_contract(&body.contract) {
                                                        let result = arc_vm::evm::evm_execute(
                                                            &state,
                                                            tx.from,
                                                            body.contract,
                                                            body.calldata.clone(),
                                                            body.value,
                                                            body.gas_limit.max(1_000_000),
                                                        );
                                                        for mut log in result.logs {
                                                            log.tx_hash = tx.hash;
                                                            log.block_height = block.header.height;
                                                            block_logs.push(log);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        if !block_logs.is_empty() {
                                            state.store_event_logs(block.header.height, block_logs);
                                        }

                                        // Export state diff and broadcast to verifiers.
                                        if self.proposer_mode {
                                            let dirty = state.drain_dirty_addresses();
                                            let diff = state.export_state_diff(&dirty);
                                            if let Some(ref tx_chan) = outbound_tx {
                                                let _ = tx_chan.try_send(
                                                    OutboundMessage::BroadcastStateDiff {
                                                        block_hash: dag_block.hash,
                                                        diff,
                                                        block_height: block.header.height,
                                                    },
                                                );
                                            }
                                        }

                                        info!(
                                            height = block.header.height,
                                            txs = committed_txs.len(),
                                            success = success,
                                            elapsed_ms = elapsed.as_millis(),
                                            tps = format!("{:.0}", tps),
                                            mode = if self.proposer_mode { "proposer" } else { "full" },
                                            "Block produced (DAG commit)"
                                        );

                                        if let Some(mut proof) = self.engine.finality_proofs.get_mut(&dag_block.hash) {
                                            proof.height = block.header.height;
                                        }
                                    }
                                    Err(e) => {
                                        warn!("DAG commit block execution failed: {}", e);
                                    }
                                }
                            } else {
                                // ── VERIFIER PATH: apply state diff ───────────
                                let Some((_, (diff, _height))) = received_diff else {
                                    warn!("Verifier path reached without state diff for {}", dag_block.hash);
                                    continue;
                                };
                                let verified_root = state.apply_state_diff(&diff);

                                if verified_root == diff.new_root {
                                    info!(
                                        hash = %dag_block.hash,
                                        txs = committed_txs.len(),
                                        elapsed_ms = start.elapsed().as_millis(),
                                        "Block verified (state diff applied)"
                                    );
                                } else {
                                    // FRAUD DETECTED: proposer's state diff doesn't match.
                                    warn!(
                                        hash = %dag_block.hash,
                                        expected = %diff.new_root,
                                        computed = %verified_root,
                                        "FRAUD: state diff root mismatch — proposer may be malicious"
                                    );
                                    // TODO: submit fraud proof, slash proposer
                                }

                                if let Some(mut proof) = self.engine.finality_proofs.get_mut(&dag_block.hash) {
                                    proof.height = state.height();
                                }
                            }
                        }
                    }
                }
            }

            // ── 3. Liveness: view-change check ────────────────────────────────
            // If the round has been stalled too long, force-advance to prevent
            // indefinite halts (e.g. from a crashed proposer).
            // force_advance_round() sets the force_advanced flag, which relaxes
            // parent quorum checks in propose_block() and receive_block(),
            // allowing the DAG to recover from stalls.
            if multi_validator && self.engine.needs_view_change() {
                warn!(
                    round = current_round,
                    "Round stalled — forcing view-change (advancing round)"
                );
                self.engine.force_advance_round();
                // force_advance_round() already resets the round timer internally
                last_proposed_round = None; // Allow proposing in the new round
            }

            // ── 4. Periodic memory eviction ──────────────────────────────────
            // Cap in-memory tx bodies to prevent OOM in long-running nodes.
            // Run every ~100 iterations to amortize overhead.
            static EVICTION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let count = EVICTION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count % 100 == 0 {
                state.evict_transactions(1_000_000); // Keep last ~1M tx bodies
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    #[test]
    fn test_consensus_manager_core_tier() {
        let addr = hash_bytes(b"core-validator");
        let mgr = ConsensusManager::new(addr, 50_000_000, 4, false, &[]);
        assert_eq!(mgr.tier, StakeTier::Core);
        assert_eq!(mgr.stake, 50_000_000);
    }

    #[test]
    fn test_consensus_manager_arc_tier() {
        let addr = hash_bytes(b"arc-validator");
        let mgr = ConsensusManager::new(addr, 5_000_000, 4, false, &[]);
        assert_eq!(mgr.tier, StakeTier::Arc);
    }

    #[test]
    fn test_consensus_manager_spark_tier() {
        let addr = hash_bytes(b"spark-validator");
        let mgr = ConsensusManager::new(addr, 500_000, 4, false, &[]);
        assert_eq!(mgr.tier, StakeTier::Spark);
        // Spark validators cannot produce blocks
        assert!(!mgr.tier.can_produce_blocks());
    }

    #[test]
    #[should_panic(expected = "stake must be >= 500_000")]
    fn test_consensus_manager_below_minimum() {
        let addr = hash_bytes(b"too-poor");
        ConsensusManager::new(addr, 100_000, 4, false, &[]);
    }
}
