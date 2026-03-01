//! Consensus manager — wires arc-consensus into the node.
//!
//! Wraps the DAG `ConsensusEngine` and drives the propose → commit loop,
//! draining the mempool and feeding committed blocks into `StateDB`.

use arc_consensus::{ConsensusEngine, StakeTier, Validator, ValidatorSet};
use arc_crypto::Hash256;
use arc_mempool::Mempool;
use arc_net::transport::{InboundMessage, OutboundMessage};
use arc_state::StateDB;
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
        let tier = StakeTier::from_stake(stake)
            .expect("stake must be >= 500_000 ARC (Spark threshold)");

        // Assign the local validator to shard 0 by default.
        let validator = Validator::new(validator_address, stake, 0)
            .expect("validator creation failed — stake below minimum");

        // Build validator set: local + any known peers.
        let mut validators = vec![validator];
        for (addr, peer_stake) in peer_validators {
            if let Some(v) = Validator::new(*addr, *peer_stake, 0) {
                validators.push(v);
            }
        }
        let validator_set = ValidatorSet::new(validators, 0);

        let engine = Arc::new(ConsensusEngine::new(validator_set, validator_address));

        info!(
            address = %validator_address,
            stake = stake,
            tier = ?tier,
            shards = num_shards,
            "ConsensusManager initialized"
        );

        Self {
            engine,
            validator_address,
            stake,
            tier,
            num_shards,
            benchmark,
        }
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

        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

            // ── 0. Process inbound network messages ─────────────────────
            if let Some(ref mut rx) = inbound_rx {
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        InboundMessage::PeerConnected { address, stake } => {
                            // Build new ValidatorSet with both validators
                            let local_validator =
                                Validator::new(self.validator_address, self.stake, 0)
                                    .expect("local validator");
                            let remote_validator = Validator::new(address, stake, 0)
                                .expect("remote validator stake too low");
                            let new_set = ValidatorSet::new(
                                vec![local_validator, remote_validator],
                                0,
                            );
                            self.engine.update_validator_set(new_set);
                            // Reset DAG so both nodes start fresh at round 0.
                            // Previous single-validator blocks were already executed
                            // via fast path, so nothing is lost.
                            self.engine.reset_dag();
                            pending_txs.clear();
                            last_proposed_round = None;
                            info!(
                                peer = %address,
                                validators = self.engine.validator_set().len(),
                                "Peer connected — ValidatorSet updated, DAG reset"
                            );
                        }
                        InboundMessage::PeerDisconnected { address } => {
                            // Revert to single-validator set
                            let local_validator =
                                Validator::new(self.validator_address, self.stake, 0)
                                    .expect("local validator");
                            let new_set = ValidatorSet::new(vec![local_validator], 0);
                            self.engine.update_validator_set(new_set);
                            // Reset DAG for clean single-validator operation.
                            self.engine.reset_dag();
                            pending_txs.clear();
                            last_proposed_round = None;
                            info!(
                                peer = %address,
                                "Peer disconnected — reverting to single-validator mode, DAG reset"
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
                    }
                }
            }

            // Check multi-validator EACH iteration (validator set is dynamic).
            let multi_validator = self.is_multi_validator();
            let current_round = self.engine.current_round();
            let already_proposed = last_proposed_round == Some(current_round);

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

            if can_produce && !already_proposed && has_quorum_parents {
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
                        .unwrap()
                        .as_millis() as u64;
                    let _ = self.engine.propose_block(vec![], timestamp);
                    let _ = self.engine.advance_round();
                } else {
                    // ── Benchmark multi-validator: feed signed txs into mempool ──
                    if self.benchmark {
                        if let Some(ref pool) = benchmark_pool {
                            let signed_txs = pool.drain(5_000);
                            for tx in signed_txs {
                                let _ = mempool.insert(tx);
                            }
                        }
                    }

                    // ── Normal path: drain mempool ──────────────────────────────
                    let transactions = mempool.drain(5_000);
                    let has_txs = !transactions.is_empty();

                    if has_txs || multi_validator {
                        let tx_hashes: Vec<Hash256> =
                            transactions.iter().map(|tx| tx.hash).collect();

                        // Index and gossip only when we have transactions
                        if has_txs {
                            for tx in &transactions {
                                pending_txs.insert(tx.hash.0, tx.clone());
                            }
                            if let Some(ref tx_chan) = outbound_tx {
                                let tx_bytes: Vec<Vec<u8>> = transactions
                                    .iter()
                                    .filter_map(|t| bincode::serialize(t).ok())
                                    .collect();
                                let _ = tx_chan
                                    .try_send(OutboundMessage::BroadcastTransactions(tx_bytes));
                            }
                        }

                        let timestamp = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
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

                        if multi_validator {
                            // ── Multi-validator: DAG commit path ─────────────
                            if has_txs {
                                debug!(
                                    pending = pending_txs.len(),
                                    "Multi-validator mode: waiting for DAG commit"
                                );
                            }
                        } else if has_txs {
                            // ── Fast path: single-validator mode ─────────────
                            let start = std::time::Instant::now();
                            match state.execute_block(&transactions, self.validator_address) {
                                Ok((block, receipts)) => {
                                    let elapsed = start.elapsed();
                                    let success = receipts.iter().filter(|r| r.success).count();
                                    let tps = if elapsed.as_secs_f64() > 0.0 {
                                        transactions.len() as f64 / elapsed.as_secs_f64()
                                    } else {
                                        transactions.len() as f64
                                    };
                                    info!(
                                        height = block.header.height,
                                        txs = transactions.len(),
                                        success = success,
                                        elapsed_ms = elapsed.as_millis(),
                                        tps = format!("{:.0}", tps),
                                        root = %block.header.tx_root,
                                        "Block produced (fast path)"
                                    );
                                }
                                Err(e) => {
                                    warn!("Block execution failed: {}", e);
                                }
                            }

                            // Clean up executed transactions from the pending index
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

                    // In multi-validator mode, execute committed transactions
                    // against state now that they are finalized.
                    if multi_validator {
                        let mut committed_txs: Vec<Transaction> = Vec::new();
                        for tx_hash in &dag_block.transactions {
                            if let Some((_, tx)) = pending_txs.remove(&tx_hash.0) {
                                committed_txs.push(tx);
                            }
                        }
                        if !committed_txs.is_empty() {
                            let start = std::time::Instant::now();
                            match state.execute_block(&committed_txs, self.validator_address)
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
                                    info!(
                                        height = block.header.height,
                                        txs = committed_txs.len(),
                                        success = success,
                                        elapsed_ms = elapsed.as_millis(),
                                        tps = format!("{:.0}", tps),
                                        root = %block.header.tx_root,
                                        "Block produced (DAG commit)"
                                    );
                                }
                                Err(e) => {
                                    warn!("DAG commit block execution failed: {}", e);
                                }
                            }
                        }
                    }
                }
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
