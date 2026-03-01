//! Consensus manager — wires arc-consensus into the node.
//!
//! Wraps the DAG `ConsensusEngine` and drives the propose → commit loop,
//! draining the mempool and feeding committed blocks into `StateDB`.

use arc_consensus::{ConsensusEngine, StakeTier, Validator, ValidatorSet};
use arc_crypto::Hash256;
use arc_mempool::Mempool;
use arc_state::StateDB;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
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
    pub fn new(validator_address: Hash256, stake: u64, num_shards: u16) -> Self {
        let tier = StakeTier::from_stake(stake)
            .expect("stake must be >= 500_000 ARC (Spark threshold)");

        // Assign the local validator to shard 0 by default.
        let validator = Validator::new(validator_address, stake, 0)
            .expect("validator creation failed — stake below minimum");

        // Bootstrap with a single-validator set at epoch 0.
        let validator_set = ValidatorSet::new(vec![validator], 0);

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
    /// This drains transactions from the mempool, proposes DAG blocks via the
    /// consensus engine, executes committed transactions against the state
    /// database, and produces on-chain blocks with Merkle roots.
    ///
    /// # Consensus Paths
    ///
    /// - **Single-validator (fast path)**: When only one validator is in the
    ///   set, transactions are executed directly against state for instant
    ///   finality. This avoids the multi-round DAG self-reference overhead.
    ///
    /// - **Multi-validator (DAG commit path)**: When the ValidatorSet has
    ///   more than one validator, blocks must go through the full DAG commit
    ///   rule (propose → certify → commit) before execution. Committed
    ///   transactions are resolved from the pending index and executed.
    pub async fn run_consensus_loop(
        &self,
        state: Arc<StateDB>,
        mempool: Arc<Mempool>,
    ) {
        use arc_types::Transaction;
        use dashmap::DashMap;

        let multi_validator = self.is_multi_validator();

        info!(
            tier = ?self.tier,
            address = %self.validator_address,
            multi_validator = multi_validator,
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

        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            // ── 1. Propose a block if we have pending txs and can produce ────
            if can_produce && mempool.len() > 0 {
                let transactions = mempool.drain(100_000);
                if !transactions.is_empty() {
                    let tx_hashes: Vec<Hash256> =
                        transactions.iter().map(|tx| tx.hash).collect();

                    // Index the full transactions so we can look them up at commit time
                    for tx in &transactions {
                        pending_txs.insert(tx.hash.0, tx.clone());
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
                        }
                        Err(e) => {
                            warn!("Failed to propose block: {}", e);
                        }
                    }

                    // After proposing, try to advance the round.
                    let _ = self.engine.advance_round();

                    if multi_validator {
                        // ── Multi-validator: DAG commit path ─────────────
                        // Do NOT execute directly. Wait for DAG commit rule
                        // (step 2 below) to finalize blocks before execution.
                        debug!(
                            pending = pending_txs.len(),
                            "Multi-validator mode: waiting for DAG commit"
                        );
                    } else {
                        // ── Fast path: single-validator mode ─────────────
                        // With only one validator, DAG commit requires multiple
                        // rounds of self-references which is slow. Execute the
                        // transactions directly against state for instant finality.
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
        let mgr = ConsensusManager::new(addr, 50_000_000, 4);
        assert_eq!(mgr.tier, StakeTier::Core);
        assert_eq!(mgr.stake, 50_000_000);
    }

    #[test]
    fn test_consensus_manager_arc_tier() {
        let addr = hash_bytes(b"arc-validator");
        let mgr = ConsensusManager::new(addr, 5_000_000, 4);
        assert_eq!(mgr.tier, StakeTier::Arc);
    }

    #[test]
    fn test_consensus_manager_spark_tier() {
        let addr = hash_bytes(b"spark-validator");
        let mgr = ConsensusManager::new(addr, 500_000, 4);
        assert_eq!(mgr.tier, StakeTier::Spark);
        // Spark validators cannot produce blocks
        assert!(!mgr.tier.can_produce_blocks());
    }

    #[test]
    #[should_panic(expected = "stake must be >= 500_000")]
    fn test_consensus_manager_below_minimum() {
        let addr = hash_bytes(b"too-poor");
        ConsensusManager::new(addr, 100_000, 4);
    }
}
