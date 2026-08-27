//! Consensus manager - wires arc-consensus into the node.
//!
//! Wraps the DAG `ConsensusEngine` and drives the propose → commit loop,
//! draining the mempool and feeding committed blocks into `StateDB`.

use crate::SharedValidators;
use crate::pipeline::{Pipeline, PipelineBatch};
use crate::vrf::ProposerSelector;
use arc_consensus::{ConsensusEngine, StakeTier, Validator, ValidatorSet};
use arc_crypto::{Hash256, KeyPair};
use arc_mempool::{EncryptedMempool, Mempool};
use arc_net::transport::{InboundMessage, OutboundMessage};
use arc_state::StateDB;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerDagTransactionError {
    AttachmentMismatch,
    InvalidTransaction(Hash256),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerStateDiffError {
    UnexpectedSource,
    HeightMismatch,
    StateRootMismatch,
}

/// Latest transport generation observed for one authenticated peer. A closed
/// generation is retained as a tombstone so a delayed `PeerConnected` event
/// cannot resurrect a connection whose disconnect was delivered first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerConnectionGeneration {
    connection_id: u64,
    connected: bool,
}

/// Record a live connection only when it is newer than the peer's last event.
/// An equal-generation disconnect wins over a reordered connect event.
fn record_peer_connected(
    peers: &mut std::collections::HashMap<Hash256, PeerConnectionGeneration>,
    address: Hash256,
    connection_id: u64,
) -> bool {
    match peers.get(&address) {
        Some(current) if connection_id <= current.connection_id => false,
        _ => {
            peers.insert(
                address,
                PeerConnectionGeneration {
                    connection_id,
                    connected: true,
                },
            );
            true
        }
    }
}

/// Record a disconnect for exactly this generation (or a newer event that
/// overtook an older live generation). Returns true only when the event
/// transitions the peer from live to disconnected.
fn record_peer_disconnected(
    peers: &mut std::collections::HashMap<Hash256, PeerConnectionGeneration>,
    address: Hash256,
    connection_id: u64,
) -> bool {
    let Some(current) = peers.get(&address).copied() else {
        peers.insert(
            address,
            PeerConnectionGeneration {
                connection_id,
                connected: false,
            },
        );
        return false;
    };

    if connection_id < current.connection_id
        || (connection_id == current.connection_id && !current.connected)
    {
        return false;
    }

    peers.insert(
        address,
        PeerConnectionGeneration {
            connection_id,
            connected: false,
        },
    );
    current.connected
}

#[cfg(test)]
mod peer_connection_generation_tests {
    use super::*;
    use arc_crypto::hash_bytes;

    #[test]
    fn connected_validator_generation_ignores_stale_disconnect() {
        let peer = hash_bytes(b"reconnecting-validator");
        let mut connected = std::collections::HashMap::new();

        assert!(record_peer_connected(&mut connected, peer, 41));
        assert!(record_peer_disconnected(&mut connected, peer, 41));
        assert!(record_peer_connected(&mut connected, peer, 42));

        // The old connection's reader exits after the replacement is live.
        // Its delayed cleanup must not remove the newer quorum member.
        assert!(!record_peer_disconnected(&mut connected, peer, 41));
        assert_eq!(
            connected.get(&peer),
            Some(&PeerConnectionGeneration {
                connection_id: 42,
                connected: true,
            })
        );

        assert!(record_peer_disconnected(&mut connected, peer, 42));
        assert!(!connected.get(&peer).unwrap().connected);
    }

    #[test]
    fn disconnected_generation_tombstone_rejects_reordered_connect() {
        let peer = hash_bytes(b"reordered-validator");
        let mut connected = std::collections::HashMap::new();

        // Transport removal can overtake the connect notification on the
        // shared channel. Retaining a tombstone prevents false live quorum.
        assert!(!record_peer_disconnected(&mut connected, peer, 7));
        assert!(!record_peer_connected(&mut connected, peer, 7));
        assert!(!connected.get(&peer).unwrap().connected);

        assert!(record_peer_connected(&mut connected, peer, 8));
        assert!(!record_peer_connected(&mut connected, peer, 7));
        assert_eq!(connected.get(&peer).unwrap().connection_id, 8);
        assert!(connected.get(&peer).unwrap().connected);
    }
}

/// A state diff is only a performance hint, never proof of execution. Bind it
/// to the authenticated author of the committed DAG block and compare it with
/// the result of local canonical execution before accepting it as corroborating
/// data. Callers must not apply `diff` to derive `executed_root`.
fn verify_peer_state_diff(
    source: Hash256,
    expected_author: Hash256,
    reported_height: u64,
    executed_height: u64,
    diff: &arc_types::StateDiff,
    executed_root: Hash256,
) -> Result<(), PeerStateDiffError> {
    if source != expected_author {
        return Err(PeerStateDiffError::UnexpectedSource);
    }
    if reported_height != executed_height {
        return Err(PeerStateDiffError::HeightMismatch);
    }
    if diff.new_root != executed_root {
        return Err(PeerStateDiffError::StateRootMismatch);
    }
    Ok(())
}

/// Verify that peer-supplied transaction bodies are the exact preimages for a
/// DAG block's canonical hash list. The returned copies carry a process-local
/// verification cache bit; that bit is never accepted from the wire.
#[cfg(test)]
fn verify_peer_dag_transactions(
    committed_hashes: &[Hash256],
    transactions: &[arc_types::Transaction],
) -> Result<Vec<arc_types::Transaction>, PeerDagTransactionError> {
    verify_peer_dag_transactions_in_domain(committed_hashes, transactions, None)
}

fn verify_peer_dag_transactions_in_domain(
    committed_hashes: &[Hash256],
    transactions: &[arc_types::Transaction],
    recovery_domain: Option<Hash256>,
) -> Result<Vec<arc_types::Transaction>, PeerDagTransactionError> {
    let mut attached_hashes: Vec<Hash256> = transactions.iter().map(|tx| tx.hash).collect();
    attached_hashes.sort_by_key(|hash| hash.0);
    let has_duplicates = attached_hashes.windows(2).any(|pair| pair[0] == pair[1]);
    if has_duplicates || attached_hashes != committed_hashes {
        return Err(PeerDagTransactionError::AttachmentMismatch);
    }

    transactions
        .iter()
        .map(|tx| {
            let mut verified = tx.clone();
            verified.sig_verified = false;
            match recovery_domain {
                Some(domain) => verified.verify_signature_in_domain(&domain),
                None => verified.verify_signature(),
            }
            .map_err(|_| PeerDagTransactionError::InvalidTransaction(verified.hash))?;
            verified.sig_verified = true;
            Ok(verified)
        })
        .collect()
}

/// The direct benchmark executor mutates canonical `StateDB` without a DAG
/// commit. That is valid only for a one-validator development chain. A
/// multi-validator benchmark must feed its signed transactions through the
/// mempool and normal DAG proposal/commit path so every validator executes the
/// same ordered block with the same consensus timestamp.
fn should_execute_local_benchmark(
    benchmark: bool,
    can_produce: bool,
    validator_count: usize,
) -> bool {
    benchmark && can_produce && validator_count == 1
}

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
    /// Pending state-diff hints keyed by block hash. The source is the peer
    /// identity authenticated by transport; diffs never replace local execution.
    pending_diffs: dashmap::DashMap<[u8; 32], (Hash256, arc_types::StateDiff, u64)>,
    /// VRF-based proposer selector (None = VRF disabled, backward compat).
    vrf_selector: Option<ProposerSelector>,
    /// Encrypted mempool for MEV-protected commit-reveal transactions.
    /// Runs alongside the regular mempool when `Some`.
    encrypted_mempool: Option<Arc<EncryptedMempool>>,
    /// Shared operator-approved validator authority list for RPC. Transport
    /// connection events must never add, remove, or reweight entries.
    pub dag_validators: Option<SharedValidators>,
    /// Shared DAG round counter for health endpoint.
    pub dag_round: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// Shared DAG committed block counter for health endpoint.
    pub dag_committed: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// WAL writer for DAG persistence - enables consensus recovery after restart.
    pub dag_wal: Option<Arc<arc_state::WalWriter>>,
    /// Registry for externally certified long-range checkpoints. It remains
    /// empty until canonical state-root signatures are actually collected.
    /// Behind Mutex for interior mutability in the consensus loop (takes &self).
    pub checkpoint_registry: std::sync::Mutex<arc_consensus::security::CheckpointRegistry>,
    /// Nothing-at-stake mitigation: double-vote tracker with graduated slashing.
    /// Behind Mutex for interior mutability in the consensus loop (takes &self).
    pub stake_tracker: std::sync::Mutex<arc_consensus::security::StakeTracker>,
}

impl ConsensusManager {
    /// Create a new consensus manager.
    ///
    /// # Arguments
    /// * `validator_address` - 256-bit address derived from the validator key.
    /// * `stake` - amount of ARC staked (must be >= STAKE_SPARK).
    /// * `num_shards` - number of sender-shards for the DAG.
    ///
    /// # Panics
    /// Panics if `stake` is below the minimum Spark threshold (500 000 ARC).
    pub fn new(
        validator_address: Hash256,
        stake: u64,
        num_shards: u16,
        benchmark: bool,
        peer_validators: &[(Hash256, u64)],
    ) -> Self {
        let (validator_set, tier) =
            Self::build_validator_set(validator_address, stake, peer_validators);
        let engine = Arc::new(ConsensusEngine::new(validator_set, validator_address));

        info!(
            address = %validator_address,
            stake = stake,
            tier = ?tier,
            shards = num_shards,
            "ConsensusManager initialized (strict legacy mode, no keypair)"
        );

        let vrf_selector = Self::build_vrf_selector(validator_address, stake, peer_validators);

        Self {
            engine,
            validator_address,
            stake,
            tier,
            num_shards,
            benchmark,
            proposer_mode: false,
            pending_diffs: dashmap::DashMap::new(),
            vrf_selector,
            encrypted_mempool: Some(Arc::new(EncryptedMempool::new(100_000))),
            dag_validators: None,
            dag_round: None,
            dag_committed: None,
            dag_wal: None,
            checkpoint_registry: std::sync::Mutex::new(
                arc_consensus::security::CheckpointRegistry::new(),
            ),
            stake_tracker: std::sync::Mutex::new(arc_consensus::security::StakeTracker::new()),
        }
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
        let (validator_set, tier) =
            Self::build_validator_set(validator_address, stake, peer_validators);
        let engine = Arc::new(ConsensusEngine::new_with_keypair(
            validator_set,
            validator_address,
            keypair,
        ));

        info!(
            address = %validator_address,
            stake = stake,
            tier = ?tier,
            shards = num_shards,
            "ConsensusManager initialized (signed block mode)"
        );

        let vrf_selector = Self::build_vrf_selector(validator_address, stake, peer_validators);

        Self {
            engine,
            validator_address,
            stake,
            tier,
            num_shards,
            benchmark,
            proposer_mode: false,
            pending_diffs: dashmap::DashMap::new(),
            vrf_selector,
            encrypted_mempool: Some(Arc::new(EncryptedMempool::new(100_000))),
            dag_validators: None,
            dag_round: None,
            dag_committed: None,
            dag_wal: None,
            checkpoint_registry: std::sync::Mutex::new(
                arc_consensus::security::CheckpointRegistry::new(),
            ),
            stake_tracker: std::sync::Mutex::new(arc_consensus::security::StakeTracker::new()),
        }
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
        // Observer mode: stake=0 means this node serves inference but
        // doesn't participate in consensus voting. We still need a tier
        // for display, so we default to Spark (lowest) for observers.
        let tier = StakeTier::from_stake(stake).unwrap_or(StakeTier::Spark);

        // Build validator set from peers only - observer isn't added.
        // This keeps observers out of the quorum calculation so they
        // don't break consensus when they go offline.
        let mut validators = Vec::new();
        if stake > 0
            && let Some(v) = Validator::new(validator_address, stake, 0)
        {
            validators.push(v);
        }
        for (addr, peer_stake) in peer_validators {
            if let Some(v) = Validator::new(*addr, *peer_stake, 0) {
                validators.push(v);
            }
        }
        // This fixed, operator-approved set is active from epoch 1. Transport
        // peer metadata never changes its identities or voting power.
        let validator_set = ValidatorSet::new(validators, 1);
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
            info!("Validator is Spark tier - observing only (cannot produce blocks)");
        }

        // Pending transaction index: tx_hash → Transaction
        // Transactions live here between drain from mempool and execution.
        let pending_txs: DashMap<[u8; 32], Transaction> = DashMap::new();

        // Track last proposed round to avoid double-proposing.
        let mut last_proposed_round: Option<u64> = None;

        // Genesis membership and live transport connectivity are different
        // facts. The validator set must be known/frozen before networking,
        // but proposing before enough of that set is connected strands this
        // node's round-0 block before peers can receive it. Track authenticated
        // connections and wait for live quorum before proposing.
        let mut connected_validators =
            std::collections::HashMap::<Hash256, PeerConnectionGeneration>::new();

        // Pending encrypted transaction batches, keyed by DAG block hash.
        // Stored at proposal time, revealed after DAG commit.
        let pending_encrypted: DashMap<[u8; 32], Vec<arc_mempool::EncryptedTx>> = DashMap::new();

        // ── Pipeline for single-validator pipelined execution ────────────
        let pipeline = Pipeline::new(Arc::clone(&state));

        loop {
            // Single-validator: 1ms tight loop for max TPS.
            // Multi-validator: 50ms to give peer blocks time to arrive
            // before re-checking quorum parents. This amortizes the
            // cross-continent latency (~100-300ms) without sacrificing
            // throughput - rounds advance when peers are ready, not on
            // a fixed timer.
            // Multi-validator: 200ms normal, 50ms benchmark (fast but peers can keep up).
            // Single-validator: 1ms for max local TPS.
            // 50ms tick for all multi-validator modes.
            // advance_round() returns immediately if quorum exists.
            // At 50ms, rounds advance as fast as blocks propagate
            // (~100-200ms cross-continent = 5-10 rounds/sec actual).
            let tick = if self.is_multi_validator() { 50 } else { 1 };
            tokio::time::sleep(tokio::time::Duration::from_millis(tick)).await;

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
                        InboundMessage::PeerConnected {
                            address,
                            stake,
                            connection_id,
                        } => {
                            if !record_peer_connected(
                                &mut connected_validators,
                                address,
                                connection_id,
                            ) {
                                debug!(
                                    peer = %address,
                                    connection_id,
                                    "Ignored stale or reordered peer-connected event"
                                );
                                continue;
                            }
                            // Transport authentication proves the peer identity,
                            // but its advertised stake is still self-asserted.
                            // Only the operator-approved genesis/checkpoint set
                            // grants voting membership and voting power.
                            let configured_stake = self
                                .engine
                                .validator_set()
                                .get_validator(&address)
                                .map(|validator| validator.stake);
                            if let Some(configured_stake) = configured_stake {
                                if stake != configured_stake {
                                    warn!(
                                        peer = %address,
                                        advertised_stake = stake,
                                        configured_stake,
                                        "Ignored validator's self-reported stake; using configured voting power"
                                    );
                                }
                                info!(
                                    peer = %address,
                                    configured_stake,
                                    "Configured validator connected; fixed membership unchanged"
                                );
                            } else {
                                warn!(
                                    peer = %address,
                                    advertised_stake = stake,
                                    "Unknown peer connected without consensus voting authority; fixed membership unchanged"
                                );
                            }
                        }
                        InboundMessage::PeerDisconnected {
                            address,
                            connection_id,
                        } => {
                            if !record_peer_disconnected(
                                &mut connected_validators,
                                address,
                                connection_id,
                            ) {
                                debug!(
                                    peer = %address,
                                    connection_id,
                                    "Ignored stale or duplicate peer-disconnected event"
                                );
                                continue;
                            }
                            // Connectivity is liveness metadata, never a
                            // membership transaction. Keep the fixed voting set
                            // unchanged for both configured and unknown peers.
                            if self.engine.validator_set().is_validator(&address) {
                                info!(
                                    peer = %address,
                                    "Configured validator disconnected; fixed membership unchanged"
                                );
                            } else {
                                debug!(peer = %address, "Unknown non-voting peer disconnected");
                            }
                        }
                        InboundMessage::DagBlockWithTxs {
                            block,
                            transactions,
                        } => {
                            // Verify peer-supplied envelopes locally before
                            // they can reach committed execution. The
                            // `sig_verified` bit is a process-local cache and
                            // is deliberately forced false across serde; a peer
                            // cannot confer trust by setting it.
                            // The attachment must be an exact preimage set of
                            // the transaction hashes committed by the DAG
                            // block. Accepting a partial or unrelated
                            // vector can finalize a block whose transactions
                            // are unavailable (or populate pending state with
                            // transactions the author never committed to).
                            let verified = match verify_peer_dag_transactions_in_domain(
                                &block.transactions,
                                &transactions,
                                state.transaction_domain_hash(),
                            ) {
                                Ok(verified) => verified,
                                Err(PeerDagTransactionError::AttachmentMismatch) => {
                                    warn!(
                                        author = %block.author,
                                        round = block.round,
                                        committed = block.transactions.len(),
                                        attached = transactions.len(),
                                        "Rejected DAG block with mismatched transaction attachment"
                                    );
                                    continue;
                                }
                                Err(PeerDagTransactionError::InvalidTransaction(tx_hash)) => {
                                    warn!(
                                        %tx_hash,
                                        author = %block.author,
                                        round = block.round,
                                        "Rejected entire DAG block containing an invalid transaction"
                                    );
                                    continue;
                                }
                            };
                            for tx in &verified {
                                pending_txs.insert(tx.hash.0, tx.clone());
                            }
                            // Feed block into consensus engine
                            match self.engine.receive_block(&block) {
                                Ok(()) => {
                                    // Persist DAG block to WAL for crash recovery
                                    if let Some(ref wal) = self.dag_wal
                                        && let Ok(bytes) = bincode::serialize(&block)
                                    {
                                        for tx in &verified {
                                            wal.append(
                                                arc_state::WalOp::SetFullTransaction(
                                                    tx.hash,
                                                    tx.clone(),
                                                ),
                                                block.round,
                                            );
                                        }
                                        wal.append(
                                            arc_state::WalOp::SetDagBlock(block.hash, bytes),
                                            block.round,
                                        );
                                    }
                                    debug!(
                                        author = %block.author,
                                        round = block.round,
                                        txs = block.transactions.len(),
                                        "Received DAG block from peer"
                                    );
                                    let round_before = self.engine.current_round();
                                    let advanced = self.engine.advance_round();
                                    // Only reset the view-change timer if the
                                    // round actually advanced or the block is
                                    // for our current round. Resetting on every
                                    // received block prevented view-change from
                                    // ever firing when stuck (blocks arrive but
                                    // don't form quorum in our round).
                                    if advanced || block.round == round_before {
                                        self.engine.reset_round_timer();
                                    }
                                    let peer_round = self.engine.current_round();
                                    if last_proposed_round != Some(peer_round) {
                                        // Will propose on the very next tick
                                        // (no need to duplicate propose logic here)
                                    }
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
                        InboundMessage::StateDiff {
                            source,
                            block_hash,
                            diff,
                            block_height,
                        } => {
                            // State diffs are optional hints, not execution
                            // proofs. Reject unsolicited hashes and bind the
                            // authenticated transport identity to the DAG block
                            // author before retaining bounded pending state.
                            let Some(block) = self.engine.get_block(&block_hash) else {
                                warn!(
                                    source = %source,
                                    block = %block_hash,
                                    "Rejected unsolicited state diff for unknown DAG block"
                                );
                                continue;
                            };
                            if block.author != source {
                                warn!(
                                    source = %source,
                                    expected = %block.author,
                                    block = %block_hash,
                                    "Rejected state diff from non-author peer"
                                );
                                continue;
                            }
                            self.pending_diffs
                                .insert(block_hash.0, (source, diff, block_height));
                            debug!(
                                block = %block_hash,
                                source = %source,
                                height = block_height,
                                "Retained authenticated state-diff hint"
                            );
                        }
                        InboundMessage::Transactions(txs) => {
                            let mut inserted = 0usize;
                            for tx_bytes in txs {
                                if let Ok(mut tx) = bincode::deserialize::<Transaction>(&tx_bytes) {
                                    // Gossip is an untrusted ingress boundary.
                                    // Verify before consuming bounded mempool
                                    // capacity and cache the result only in
                                    // this process.
                                    tx.sig_verified = false;
                                    if state.verify_transaction_signature(&tx).is_err() {
                                        continue;
                                    }
                                    tx.sig_verified = true;
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
                            debug!("State sync message (handled by RPC layer)");
                        }
                        InboundMessage::InferenceRequest {
                            request_id,
                            input: _,
                            max_tokens,
                            requester: _,
                        } => {
                            // Community GPU node received an inference request.
                            // TODO: Run model locally and send response via outbound_tx.
                            info!(
                                request_id = %request_id,
                                tokens = max_tokens,
                                "Received inference request from network"
                            );
                        }
                        InboundMessage::InferenceResponse {
                            request_id,
                            output: _,
                            output_hash: _,
                            model_hash: _,
                            ms_per_token,
                            responder,
                        } => {
                            // Seed node received inference result from community GPU.
                            // Store for the waiting RPC handler to pick up.
                            info!(
                                request_id = %request_id,
                                responder = %responder,
                                ms_per_token = ms_per_token,
                                "Received inference response from community node"
                            );
                        }
                        // ── Partition detection via heartbeat round info ───
                        InboundMessage::HeartbeatWithRound {
                            peer,
                            dag_round,
                            committed_round,
                        } => {
                            let my_round = self.engine.current_round();
                            if dag_round.saturating_sub(my_round) > 10_000 {
                                warn!(
                                    "PARTITION DETECTED: peer {} at round {} but we are at {} (gap: {}). Authenticated checkpoint sync is required.",
                                    peer,
                                    dag_round,
                                    my_round,
                                    dag_round - my_round
                                );
                                self.engine
                                    .observe_untrusted_round_hint(dag_round, committed_round);
                            } else if my_round.saturating_sub(dag_round) > 10_000 {
                                debug!(
                                    "Peer {} is behind us by {} rounds (them: {}, us: {})",
                                    peer,
                                    my_round - dag_round,
                                    dag_round,
                                    my_round
                                );
                            }
                        }
                        // ── Round sync request - respond with our state ───
                        InboundMessage::RoundSyncRequest {
                            peer,
                            their_round,
                            their_committed,
                        } => {
                            let my_round = self.engine.current_round();
                            let my_committed = self.engine.last_committed_round();
                            let vs = self.engine.validator_set();
                            if let Some(ref tx) = outbound_tx {
                                // Use try_send to avoid blocking the consensus loop when
                                // the outbound channel is full (root cause of P2P deadlock).
                                let _ = tx.try_send(
                                    arc_net::transport::OutboundMessage::SendRoundSyncResponse {
                                        target: peer,
                                        current_round: my_round,
                                        last_committed_round: my_committed,
                                        validator_count: vs.len() as u32,
                                        total_stake: vs.total_stake,
                                    },
                                );
                            }
                            if their_round.saturating_sub(my_round) > 10_000 {
                                warn!(
                                    "Peer {} ahead by {} rounds. Recording hint; authenticated checkpoint sync is required (local {}).",
                                    peer,
                                    their_round - my_round,
                                    my_round,
                                );
                                self.engine
                                    .observe_untrusted_round_hint(their_round, their_committed);
                            }
                        }
                        // ── Round sync response - update our round if behind ───
                        InboundMessage::RoundSyncResponse {
                            current_round,
                            last_committed_round,
                        } => {
                            let my_round = self.engine.current_round();
                            if current_round.saturating_sub(my_round) > 1000 {
                                info!(
                                    "Round sync hint: peer at round {}, we are at {}; authenticated checkpoint sync required",
                                    current_round, my_round
                                );
                                self.engine.observe_untrusted_round_hint(
                                    current_round,
                                    last_committed_round,
                                );
                            }
                        }
                        // ── Shard messages (forwarded to inference engine) ───
                        InboundMessage::ShardForward {
                            request_id,
                            model_id: _,
                            next_layer,
                            total_layers: _,
                            token_position: _,
                            activations: _,
                            activation_hash: _,
                        } => {
                            info!(
                                request_id = %request_id,
                                layer = next_layer,
                                "Received shard forward - processing layers"
                            );
                            // Shard processing handled by inference coordinator (Phase 2)
                        }
                        InboundMessage::ShardResult {
                            request_id,
                            token_id,
                            logits_hash: _,
                            responder,
                        } => {
                            info!(
                                request_id = %request_id,
                                token_id = token_id,
                                responder = %responder,
                                "Received shard result"
                            );
                        }
                        InboundMessage::ShardAnnounce {
                            model_id,
                            start_layer,
                            end_layer,
                            expert_indices,
                            node_address,
                            available_memory: _,
                            gpu_tier,
                        } => {
                            info!(
                                node = %node_address,
                                model = %model_id,
                                layers = %format!("[{}, {})", start_layer, end_layer),
                                experts = expert_indices.len(),
                                gpu_tier = gpu_tier,
                                "Shard announcement received"
                            );
                        }
                    }
                }
            }

            // Check multi-validator EACH iteration (validator set is dynamic).
            let multi_validator = self.is_multi_validator();
            let current_round = self.engine.current_round();
            let already_proposed = last_proposed_round == Some(current_round);
            let has_connected_quorum = if multi_validator {
                let vs = self.engine.validator_set();
                let mut connected_stake = 0u64;
                let mut seen_validators = std::collections::HashSet::new();
                seen_validators.insert(self.validator_address);
                if let Some(validator) = vs.get_validator(&self.validator_address) {
                    connected_stake = validator.stake;
                }
                for (address, generation) in connected_validators.iter() {
                    if generation.connected
                        && seen_validators.insert(*address)
                        && let Some(validator) = vs.get_validator(address)
                    {
                        connected_stake = connected_stake
                            .checked_add(validator.stake)
                            .expect("unique connected stake cannot exceed validator-set total");
                    }
                }
                connected_stake >= vs.quorum
            } else {
                true
            };

            // ── Pre-feed benchmark transactions into mempool ──────────────
            // Do this BEFORE the propose check so transactions are always
            // available regardless of round/parent state.
            // Cap mempool at 50K to prevent unbounded memory growth.
            if self.benchmark
                && multi_validator
                && mempool.len() < 5_000
                && let Some(ref pool) = benchmark_pool
            {
                let signed_txs = pool.drain(200);
                let fed = signed_txs.len();
                for tx in signed_txs {
                    let _ = mempool.insert(tx);
                }
                if fed > 0 && mempool.len() % 10_000 < 2_000 {
                    info!(
                        "Benchmark pre-feed: {} txs (mempool: {})",
                        fed,
                        mempool.len()
                    );
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
            let has_quorum_parents =
                if current_round == 0 || self.engine.is_recovery_bootstrap_round(current_round) {
                    true // Genesis of a legacy or signed recovery DAG domain
                } else {
                    let vs = self.engine.validator_set();
                    let prev_blocks = self.engine.blocks_in_round(current_round - 1);
                    let mut parent_stake = 0u64;
                    let mut seen_authors = std::collections::HashSet::new();
                    for hash in &prev_blocks {
                        if let Some(block) = self.engine.get_block(hash)
                            && let Some(validator) = vs.get_validator(&block.author)
                            && seen_authors.insert(block.author)
                        {
                            parent_stake = parent_stake
                                .checked_add(validator.stake)
                                .expect("unique parent stake cannot exceed validator-set total");
                        }
                    }
                    parent_stake >= vs.quorum
                };

            // ── VRF proposer eligibility check ──────────────────────────
            // In DAG consensus, ALL validators propose every round - that's
            // what builds the DAG. The leader is selected at commit time, not
            // at proposal time. VRF selection (EXPECTED_PROPOSERS_PER_SLOT=1)
            // would filter out 7/8 validators per round, preventing quorum.
            // Skip VRF in multi-validator DAG mode.
            let vrf_approved = if multi_validator {
                true // DAG: all validators propose every round
            } else if let Some(ref selector) = self.vrf_selector {
                let mut vrf_input = [0u8; 40];
                vrf_input[..8].copy_from_slice(&current_round.to_le_bytes());
                vrf_input[8..40].copy_from_slice(&self.validator_address.0);
                let vrf_hash = blake3::hash(&vrf_input);
                let vrf_output = crate::vrf::VrfOutput {
                    value: *vrf_hash.as_bytes(),
                };
                selector.is_proposer(self.stake, &vrf_output)
            } else {
                true // No VRF = always allowed (backward compat)
            };

            // Multi-validator proposals require both connected stake and a
            // unique-author parent quorum. A timeout is not a parent
            // certificate. Single-validator mode uses VRF scheduling.
            let allow_propose = if multi_validator {
                has_connected_quorum // DAG: wait until peers can receive round 0
            } else {
                vrf_approved // Single-validator: VRF gates block production
            };
            // ── Single-validator benchmark fast path ─────────────────────
            // Direct StateDB execution is intentionally forbidden once the
            // configured set has multiple validators. Multi-validator benchmark
            // traffic was pre-fed into the mempool above and must be DAG-committed.
            if should_execute_local_benchmark(
                self.benchmark,
                can_produce,
                self.engine.validator_set().len(),
            ) && let Some(ref pool) = benchmark_pool
            {
                let signed_txs = pool.drain(1_000_000);
                if !signed_txs.is_empty() {
                    let tx_count = signed_txs.len() as u64;
                    let start = std::time::Instant::now();
                    match state.execute_block_signed_benchmark(&signed_txs, self.validator_address)
                    {
                        Ok(block) => {
                            let elapsed = start.elapsed();
                            let tps = if elapsed.as_secs_f64() > 0.0 {
                                tx_count as f64 / elapsed.as_secs_f64()
                            } else {
                                tx_count as f64
                            };
                            debug!(
                                height = block.header.height,
                                txs = tx_count,
                                elapsed_ms = elapsed.as_millis(),
                                tps = format!("{:.0}", tps),
                                "Benchmark block"
                            );
                        }
                        Err(e) => {
                            warn!("Benchmark block failed: {}", e);
                        }
                    }
                }
            }

            if can_produce
                && !already_proposed
                && allow_propose
                && vrf_approved
                && has_quorum_parents
            {
                if !multi_validator && self.benchmark {
                    // Single-validator: just advance DAG round for tracking
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let _ = self.engine.propose_block(vec![], timestamp);
                    let _ = self.engine.advance_round();
                } else {
                    // ── Normal path: drain mempool ──────────────────────────────
                    // In benchmark mode, drain aggressively for max TPS.
                    // In normal mode, 100 per block keeps QUIC payload small.
                    // Multi-validator benchmark: 1000 per block (RPC-friendly).
                    // Single-validator benchmark: 50K (max local TPS).
                    // 500 txs per block in normal mode (was 100).
                    // At 10 rounds/sec = 5,000 tx/sec consensus throughput.
                    let drain_limit = if self.benchmark && multi_validator {
                        1_000
                    } else if self.benchmark {
                        50_000
                    } else {
                        500
                    };
                    let mempool_len_pre = mempool.len();
                    let transactions = mempool.drain(drain_limit);
                    if !transactions.is_empty() {
                        info!(
                            "Drained {} txs from mempool for DAG proposal",
                            transactions.len()
                        );
                        for tx in &transactions {
                            debug!(
                                tx_hash = %tx.hash,
                                tx_type = ?tx.tx_type,
                                from = %tx.from,
                                nonce = tx.nonce,
                                "Drained transaction for DAG proposal"
                            );
                        }
                    } else if mempool_len_pre > 0 {
                        warn!(
                            mempool_len = mempool_len_pre,
                            "Mempool reported entries but drain returned none"
                        );
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
                            // Re-enabled: gossip txs so peers can include them
                            // in THEIR proposals too. The leader-only commit
                            // rule (lib.rs:1452) means a tx in only OUR block
                            // is orphaned 5/6 of the time. Letting peers
                            // re-include it gives every leader a chance to
                            // commit it. Duplicate-execution is dedup'd by
                            // state.receipts at execute time, so cost is just
                            // DAG bandwidth.
                            if let Some(ref tx_chan) = outbound_tx {
                                let serialized: Vec<Vec<u8>> = transactions
                                    .iter()
                                    .filter_map(|tx| bincode::serialize(tx).ok())
                                    .collect();
                                if !serialized.is_empty() {
                                    let _ = tx_chan.try_send(
                                        OutboundMessage::BroadcastTransactions(serialized),
                                    );
                                }
                            }
                        }

                        let timestamp = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;

                        match self.engine.propose_block(tx_hashes, timestamp) {
                            Ok(block) => {
                                // Persist our own proposed block to WAL
                                if let Some(ref wal) = self.dag_wal
                                    && let Ok(bytes) = bincode::serialize(&block)
                                {
                                    for tx in &transactions {
                                        wal.append(
                                            arc_state::WalOp::SetFullTransaction(
                                                tx.hash,
                                                tx.clone(),
                                            ),
                                            block.round,
                                        );
                                    }
                                    wal.append(
                                        arc_state::WalOp::SetDagBlock(block.hash, bytes),
                                        block.round,
                                    );
                                }
                                info!(
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
                                    match tx_chan.try_send(OutboundMessage::BroadcastDagBlock {
                                        block: block.clone(),
                                        transactions: transactions.clone(),
                                    }) {
                                        Ok(()) => {}
                                        Err(e) => warn!(
                                            "Failed to broadcast DAG block: {} (channel full or closed)",
                                            e
                                        ),
                                    }
                                } else {
                                    warn!("No outbound channel - cannot broadcast DAG block");
                                }
                            }
                            Err(e) => {
                                warn!("Failed to propose block: {}", e);
                            }
                        }

                        // After proposing, advance the round ONLY if we have enough
                        // peer blocks in the current round. Without this gate, the node
                        // races ahead of its peers (advancing every 1ms) while peer
                        // blocks take 100-300ms to arrive across continents. The 2-round
                        // commit rule then can't fire because parent references are stale.
                        //
                        // A single-validator chain can advance immediately. A
                        // multi-validator benchmark uses the same quorum-paced
                        // round transition as every other production DAG.
                        if !multi_validator {
                            let _ = self.engine.advance_round();
                        }
                        // Multi-validator: round advancement happens below when
                        // has_quorum_parents becomes true on the NEXT iteration.

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
                            // Filter out transactions already applied via RPC
                            // (faucet/submit direct-apply). Without this filter,
                            // the pipeline re-executes them → double nonce
                            // increment and double balance deduction.
                            let fresh_txs: Vec<Transaction> = transactions
                                .iter()
                                .filter(|tx| !state.receipts.contains_key(&tx.hash.0))
                                .cloned()
                                .collect();

                            if !fresh_txs.is_empty() {
                                pipeline
                                    .submit(PipelineBatch {
                                        transactions: fresh_txs,
                                        producer: self.validator_address,
                                    })
                                    .unwrap_or_else(|e| {
                                        warn!("Pipeline submit failed: {:?}", e);
                                    });
                            }

                            // Clean up pending index - pipeline owns them now
                            for tx in &transactions {
                                pending_txs.remove(&tx.hash.0);
                            }
                        }
                    }
                }
            }

            // Advance round ONLY when quorum parents exist. If peer blocks
            // haven't arrived yet (100-300ms cross-continent), wait. The 200ms
            // tick gives them time. force_advance_round() was causing nodes to
            // race ahead of their peers, breaking parent references needed for
            // the 2-round commit rule.
            if already_proposed {
                let _ = self.engine.advance_round();
            }

            // ── 2. Try to commit finalized DAG blocks (multi-validator) ──────
            let mut committed = self.engine.try_commit();
            // Sort by round to ensure all nodes process in the same order.
            // Without this, nodes discover committed blocks at different times
            // and produce chain blocks in different sequences.
            committed.sort_by_key(|b| b.round);
            if !committed.is_empty() {
                for dag_block in &committed {
                    // Persist commit to WAL
                    if let Some(ref wal) = self.dag_wal {
                        wal.append(
                            arc_state::WalOp::CommitDagBlock(dag_block.hash),
                            dag_block.round,
                        );
                    }
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
                    if let Some(ref emp) = self.encrypted_mempool
                        && let Some((_, enc_batch)) = pending_encrypted.remove(&dag_block.hash.0)
                        && !enc_batch.is_empty()
                    {
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

                    // In multi-validator mode, process committed transactions.
                    // Proposer: full execution + export state diff.
                    // Verifier: apply received state diff + verify root.
                    if multi_validator {
                        let mut committed_txs: Vec<Transaction> = Vec::new();
                        for tx_hash in &dag_block.transactions {
                            if let Some((_, tx)) = pending_txs.remove(&tx_hash.0) {
                                // Skip transactions already applied via direct RPC path
                                // (faucet claims, /tx/submit). They're already in receipts.
                                if state.receipts.contains_key(&tx.hash.0) {
                                    continue;
                                }
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
                                let recovery_domain = state.transaction_domain_hash();
                                tokio::spawn(async move {
                                    for tx in txs.iter_mut() {
                                        if !tx.is_unsigned()
                                            && !tx.sig_verified
                                            && match recovery_domain {
                                                Some(domain) => {
                                                    tx.verify_signature_in_domain(&domain).is_ok()
                                                }
                                                None => tx.verify_signature().is_ok(),
                                            }
                                        {
                                            tx.sig_verified = true;
                                        }
                                    }
                                    txs
                                })
                            };
                            // Await pre-verification with timeout to prevent deadlock.
                            // If the spawned task hangs (runtime starvation), fall
                            // back to unverified txs after 5 seconds.
                            // All validators must execute the same tx set to agree on
                            // state root. NEVER truncate - different validators could
                            // truncate at different points, causing a consensus fork.
                            committed_txs = match tokio::time::timeout(
                                tokio::time::Duration::from_secs(5),
                                pre_verify_handle,
                            )
                            .await
                            {
                                Ok(Ok(verified_txs)) => verified_txs,
                                Ok(Err(e)) => {
                                    warn!("Pre-verify error: {e} - proceeding with unverified txs");
                                    committed_txs
                                }
                                Err(_) => {
                                    warn!("Pre-verify timeout - proceeding with unverified txs");
                                    committed_txs
                                }
                            };

                            // Cross-shard: lock cross-shard transactions before execution.
                            // Single-shard txs execute directly. Cross-shard txs use
                            // the 2-phase lock protocol for atomicity across shards.
                            // Cross-shard: identify and lock cross-shard transactions
                            let cross_shard_hashes: Vec<Hash256> = committed_txs
                                .iter()
                                .filter(|tx| {
                                    if let arc_types::TxBody::Transfer(ref body) = tx.body {
                                        arc_consensus::is_cross_shard(
                                            &tx.from,
                                            &body.to,
                                            self.num_shards,
                                        )
                                    } else {
                                        false
                                    }
                                })
                                .map(|tx| tx.hash)
                                .collect();
                            if !cross_shard_hashes.is_empty() {
                                for tx in committed_txs.iter() {
                                    if let arc_types::TxBody::Transfer(ref body) = tx.body {
                                        let src =
                                            arc_consensus::assign_shard(&tx.from, self.num_shards);
                                        let tgt =
                                            arc_consensus::assign_shard(&body.to, self.num_shards);
                                        if src != tgt {
                                            let _ = self.engine.lock_cross_shard(
                                                tx.hash,
                                                src,
                                                tgt,
                                                dag_block.hash,
                                                dag_block.round,
                                            );
                                        }
                                    }
                                }
                                debug!("{} cross-shard txs locked", cross_shard_hashes.len());
                            }

                            let start = std::time::Instant::now();

                            // A peer diff is optional corroborating data only.
                            // Every validator executes the exact committed
                            // transaction bodies locally; no network payload may
                            // replace this state transition.
                            let received_diff = self.pending_diffs.remove(&dag_block.hash.0);

                            {
                                // ── CANONICAL PATH: local adaptive execution ──
                                match state.execute_block_adaptive_at(
                                    &committed_txs,
                                    dag_block.author,
                                    dag_block.timestamp,
                                ) {
                                    Ok((block, receipts)) => {
                                        let elapsed = start.elapsed();
                                        let success = receipts.iter().filter(|r| r.success).count();
                                        let tps = if elapsed.as_secs_f64() > 0.0 {
                                            committed_txs.len() as f64 / elapsed.as_secs_f64()
                                        } else {
                                            committed_txs.len() as f64
                                        };

                                        // Run EVM execution for any EVM contract calls.
                                        let mut block_logs: Vec<arc_types::EventLog> = Vec::new();
                                        for (i, tx) in committed_txs.iter().enumerate() {
                                            if receipts[i].success
                                                && let arc_types::TxBody::WasmCall(ref body) =
                                                    tx.body
                                                && state.is_evm_contract(&body.contract)
                                            {
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
                                        if !block_logs.is_empty() {
                                            state.store_event_logs(block.header.height, block_logs);
                                        }

                                        // Commit cross-shard locks after successful execution
                                        for cs_hash in &cross_shard_hashes {
                                            let _ = self.engine.commit_cross_shard(*cs_hash);
                                        }

                                        // Validate an authenticated hint only
                                        // after local execution produced the
                                        // canonical height/root. A forged but
                                        // self-consistent diff cannot affect
                                        // state because it is never applied.
                                        if let Some((_, (source, diff, reported_height))) =
                                            received_diff.as_ref()
                                        {
                                            match verify_peer_state_diff(
                                                *source,
                                                dag_block.author,
                                                *reported_height,
                                                block.header.height,
                                                diff,
                                                block.header.state_root,
                                            ) {
                                                Ok(()) => debug!(
                                                    block = %dag_block.hash,
                                                    source = %source,
                                                    "State-diff hint corroborated local execution"
                                                ),
                                                Err(reason) => warn!(
                                                    block = %dag_block.hash,
                                                    source = %source,
                                                    ?reason,
                                                    declared_root = %diff.new_root,
                                                    executed_root = %block.header.state_root,
                                                    "Rejected state-diff hint after local execution"
                                                ),
                                            }
                                        }

                                        // Only a DAG block's authenticated
                                        // author may broadcast its optional
                                        // diff. Other validators already have
                                        // the locally executed result.
                                        if self.proposer_mode
                                            && dag_block.author == self.validator_address
                                        {
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
                                            mode = if self.proposer_mode {
                                                "proposer"
                                            } else {
                                                "full"
                                            },
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

            // ── Update shared health counters for /health endpoint ─────────
            if let Some(ref r) = self.dag_round {
                r.store(current_round, std::sync::atomic::Ordering::Relaxed);
            }
            if !committed.is_empty() {
                if let Some(ref c) = self.dag_committed {
                    c.fetch_add(committed.len() as u64, std::sync::atomic::Ordering::Relaxed);
                }

                // ── Security: track votes + create checkpoints ──────────────
                if let Ok(mut tracker) = self.stake_tracker.lock() {
                    for dag_block in &committed {
                        // Track the proposer's vote for double-vote detection
                        tracker.report_vote(dag_block.author, dag_block.round, dag_block.hash.0);

                        // Check for double voting in this round
                        let evidence = tracker.detect_double_voting(dag_block.round);
                        for ev in &evidence {
                            let slash = arc_consensus::security::calculate_slash_amount(
                                &arc_consensus::security::SlashableOffense::DoubleVote,
                                self.stake,
                            );
                            warn!(
                                validator = %ev.validator,
                                round = ev.round,
                                slash_amount = slash,
                                "DOUBLE VOTE DETECTED - slashing evidence recorded"
                            );
                            tracker.record_penalty(arc_consensus::security::PenaltyRecord {
                                validator: ev.validator,
                                offense: arc_consensus::security::SlashableOffense::DoubleVote,
                                slash_amount: slash,
                                round: ev.round,
                                timestamp: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0),
                            });
                        }
                    }

                    // Prune old security data every 10000 rounds to bound memory
                    if current_round.is_multiple_of(10_000) && current_round > 10_000 {
                        tracker.prune_votes(current_round - 10_000);
                    }
                }

                // Do not turn a local wall-clock timestamp and a hash of the
                // height into a fake long-range trust anchor. The trusted
                // checkpoint registry accepts only a canonical state-root
                // transcript carrying strict validator identity + stake
                // supermajorities. This loop does not yet collect that
                // evidence, so checkpoint intervals remain explicitly dark.
                for dag_block in &committed {
                    if dag_block.round > 0
                        && dag_block.round % arc_consensus::security::CHECKPOINT_INTERVAL == 0
                    {
                        warn!(
                            round = dag_block.round,
                            height = state.height(),
                            "Checkpoint interval reached without canonical state root and validator signature quorum; no trust anchor registered"
                        );
                    }
                }
            }

            // ── 3. Liveness: certified view-change check ──────────────────────
            // A local wall-clock timeout is not a quorum view-change
            // certificate. Until the network collects and verifies such a
            // certificate, preserve the current view and fail closed instead
            // of letting each node advance on a different timer.
            if multi_validator && self.engine.needs_view_change() {
                warn!(
                    round = current_round,
                    "Round stalled, but no authenticated quorum view-change certificate is available; consensus view unchanged"
                );
                self.engine.reset_round_timer();
            }

            // ── 3b. Broadcast round-info heartbeat every ~30 seconds ─────
            // Peers compare rounds to detect partitions early.
            static HEARTBEAT_COUNTER: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let hb_count = HEARTBEAT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let hb_interval = if self.is_multi_validator() { 600 } else { 6000 }; // ~30s at 50ms/600 or 1ms/6000
            if hb_count.is_multiple_of(hb_interval)
                && let Some(ref tx) = outbound_tx
            {
                // Use try_send to avoid blocking the consensus loop when
                // the outbound channel is full (root cause of P2P deadlock).
                let _ = tx.try_send(
                    arc_net::transport::OutboundMessage::BroadcastHeartbeatWithRound {
                        dag_round: current_round,
                        committed_round: self.engine.last_committed_round(),
                    },
                );
            }

            // Membership remains the fixed, operator-approved epoch-1 set.
            // A future epoch transition must be driven by an authenticated
            // on-chain governance certificate, never transport discovery.

            // ── 4. Periodic memory eviction ──────────────────────────────────
            // Cap in-memory data to prevent OOM in long-running nodes.
            // Run every ~100 iterations to amortize overhead.
            static EVICTION_COUNTER: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let count = EVICTION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count.is_multiple_of(100) {
                state.evict_transactions(1_000_000); // Keep last ~1M tx bodies

                // Evict stale pending data (txs, diffs, encrypted batches).
                // These accumulate when blocks are proposed but never committed
                // (e.g., during network partitions). Cap at 50K entries each.
                if pending_txs.len() > 50_000 {
                    let excess = pending_txs.len() - 25_000;
                    let keys: Vec<[u8; 32]> =
                        pending_txs.iter().take(excess).map(|e| *e.key()).collect();
                    for k in keys {
                        pending_txs.remove(&k);
                    }
                }
                if pending_encrypted.len() > 10_000 {
                    let keys: Vec<[u8; 32]> = pending_encrypted
                        .iter()
                        .take(5_000)
                        .map(|e| *e.key())
                        .collect();
                    for k in keys {
                        pending_encrypted.remove(&k);
                    }
                }
                if self.pending_diffs.len() > 10_000 {
                    let keys: Vec<[u8; 32]> = self
                        .pending_diffs
                        .iter()
                        .take(5_000)
                        .map(|e| *e.key())
                        .collect();
                    for k in keys {
                        self.pending_diffs.remove(&k);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::{KeyPair, Signature, hash_bytes};
    use arc_types::{Account, AccountChange, StateDiff, Transaction, TxType};

    fn signed_transfer(key: &KeyPair, recipient: u8, nonce: u64) -> Transaction {
        let mut tx =
            Transaction::new_transfer(key.address(), hash_bytes(&[recipient]), nonce + 1, nonce);
        tx.sign(key).unwrap();
        tx
    }

    #[test]
    fn direct_benchmark_execution_is_single_validator_only() {
        assert!(should_execute_local_benchmark(true, true, 1));
        assert!(!should_execute_local_benchmark(false, true, 1));
        assert!(!should_execute_local_benchmark(true, false, 1));
        assert!(!should_execute_local_benchmark(true, true, 0));
        assert!(!should_execute_local_benchmark(true, true, 2));
        assert!(!should_execute_local_benchmark(true, true, 8));
    }

    #[test]
    fn peer_dag_attachment_accepts_exact_set_in_noncanonical_body_order() {
        let key = KeyPair::generate_ed25519();
        let first = signed_transfer(&key, 1, 0);
        let second = signed_transfer(&key, 2, 1);
        let mut committed = vec![first.hash, second.hash];
        committed.sort_by_key(|hash| hash.0);

        let verified = verify_peer_dag_transactions(&committed, &[second, first]).unwrap();
        assert_eq!(verified.len(), 2);
        assert!(verified.iter().all(|tx| tx.sig_verified));
    }

    #[test]
    fn peer_dag_attachment_rejects_partial_duplicate_and_invalid_transactions() {
        let key = KeyPair::generate_ed25519();
        let first = signed_transfer(&key, 1, 0);
        let second = signed_transfer(&key, 2, 1);
        let mut committed = vec![first.hash, second.hash];
        committed.sort_by_key(|hash| hash.0);

        assert!(matches!(
            verify_peer_dag_transactions(&committed, std::slice::from_ref(&first)),
            Err(PeerDagTransactionError::AttachmentMismatch)
        ));
        assert!(matches!(
            verify_peer_dag_transactions(
                &[first.hash, first.hash],
                &[first.clone(), first.clone()]
            ),
            Err(PeerDagTransactionError::AttachmentMismatch)
        ));

        let mut invalid = second;
        invalid.signature = Signature::null();
        assert!(matches!(
            verify_peer_dag_transactions(&committed, &[first, invalid.clone()]),
            Err(PeerDagTransactionError::InvalidTransaction(hash)) if hash == invalid.hash
        ));
    }

    #[test]
    fn peer_dag_attachment_ignores_forged_trust_bit_and_rejects_type_mismatch() {
        let key = KeyPair::generate_ed25519();
        let mut unsigned = Transaction::new_transfer(key.address(), hash_bytes(b"recipient"), 1, 0);
        unsigned.sig_verified = true;
        assert!(matches!(
            verify_peer_dag_transactions(&[unsigned.hash], &[unsigned.clone()]),
            Err(PeerDagTransactionError::InvalidTransaction(hash)) if hash == unsigned.hash
        ));

        let mut mismatch = signed_transfer(&key, 3, 0);
        mismatch.tx_type = TxType::InferenceAttestation;
        mismatch.sign(&key).unwrap();
        assert!(matches!(
            verify_peer_dag_transactions(&[mismatch.hash], &[mismatch.clone()]),
            Err(PeerDagTransactionError::InvalidTransaction(hash)) if hash == mismatch.hash
        ));
    }

    #[test]
    fn peer_state_diff_requires_authenticated_block_author_and_exact_height() {
        let author = hash_bytes(b"author");
        let outsider = hash_bytes(b"outsider");
        let root = hash_bytes(b"executed-root");
        let diff = StateDiff {
            changes: Vec::new(),
            new_root: root,
        };

        assert_eq!(
            verify_peer_state_diff(outsider, author, 8, 8, &diff, root),
            Err(PeerStateDiffError::UnexpectedSource)
        );
        assert_eq!(
            verify_peer_state_diff(author, author, 7, 8, &diff, root),
            Err(PeerStateDiffError::HeightMismatch)
        );
        assert!(verify_peer_state_diff(author, author, 8, 8, &diff, root).is_ok());
    }

    #[test]
    fn self_consistent_forged_state_diff_cannot_replace_local_execution() {
        let author = hash_bytes(b"author");
        let funded = hash_bytes(b"funded");
        let attacker = hash_bytes(b"attacker");

        // Build a root that is internally consistent for an attacker-chosen
        // balance mutation. This is exactly what the old apply-and-compare path
        // accepted as proof.
        let forged_state = StateDB::with_genesis(&[(funded, 1_000)]);
        let forged_account = Account::new(attacker, u64::MAX);
        forged_state.update_account(&attacker, forged_account.clone());
        let forged_root = forged_state.get_state_root();
        let forged_diff = StateDiff {
            changes: vec![AccountChange {
                address: attacker,
                account: forged_account,
            }],
            new_root: forged_root,
        };

        // Canonical local execution did not create the attacker account, so
        // its independently computed root wins and the hint is rejected.
        let canonical_state = StateDB::with_genesis(&[(funded, 1_000)]);
        let canonical_root = canonical_state.get_state_root();
        assert_ne!(forged_root, canonical_root);
        assert_eq!(
            verify_peer_state_diff(author, author, 1, 1, &forged_diff, canonical_root,),
            Err(PeerStateDiffError::StateRootMismatch)
        );
        assert!(canonical_state.get_account(&attacker).is_none());
        assert_eq!(canonical_state.get_state_root(), canonical_root);
    }

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
    fn test_consensus_manager_below_minimum_is_spark_observer() {
        // After observer mode was added (so community nodes can join with
        // stake=0), small-stake validators are no longer rejected. They get
        // the lowest tier (Spark) and cannot produce blocks, but they can
        // still observe consensus and serve inference. Verify that contract.
        let addr = hash_bytes(b"observer");
        let mgr = ConsensusManager::new(addr, 100_000, 4, false, &[]);
        assert_eq!(
            mgr.tier,
            StakeTier::Spark,
            "Small stake should get Spark tier"
        );
        assert!(
            !mgr.tier.can_produce_blocks(),
            "Spark tier should not produce blocks"
        );
        // Even stake=0 should work (community observer mode)
        let observer_addr = hash_bytes(b"zero-stake-observer");
        let observer = ConsensusManager::new(observer_addr, 0, 4, false, &[]);
        assert_eq!(observer.tier, StakeTier::Spark);
        assert!(!observer.tier.can_produce_blocks());
    }

    #[tokio::test]
    async fn forged_transport_stake_cannot_join_fixed_validator_set() {
        let local_key = KeyPair::generate_ed25519();
        let configured_peer = hash_bytes(b"configured-peer");
        let forged_peer = hash_bytes(b"forged-peer");
        let configured_stake = 5_000_000;
        let mut manager = ConsensusManager::new_with_keypair(
            local_key.address(),
            configured_stake,
            4,
            false,
            &[(configured_peer, configured_stake)],
            local_key,
        );
        let shared_validators = Arc::new(parking_lot::RwLock::new(vec![
            (manager.validator_address, configured_stake),
            (configured_peer, configured_stake),
        ]));
        manager.dag_validators = Some(shared_validators.clone());

        let before = manager.engine.validator_set();
        let (inbound_tx, inbound_rx) = mpsc::channel(4);
        inbound_tx
            .send(InboundMessage::PeerConnected {
                address: forged_peer,
                stake: u64::MAX,
                connection_id: 1,
            })
            .await
            .unwrap();

        let state = Arc::new(StateDB::with_genesis(&[]));
        let mempool = Arc::new(Mempool::new(16));
        let result = tokio::time::timeout(
            tokio::time::Duration::from_millis(120),
            manager.run_consensus_loop(state, mempool, Some(inbound_rx), None, None),
        )
        .await;
        assert!(result.is_err(), "consensus loop should still be running");

        let after = manager.engine.validator_set();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.total_stake, before.total_stake);
        assert_eq!(after.quorum, before.quorum);
        assert!(!after.is_validator(&forged_peer));
        assert_eq!(
            *shared_validators.read(),
            vec![
                (manager.validator_address, configured_stake),
                (configured_peer, configured_stake)
            ],
            "transport discovery must not enter the RPC validator authority list"
        );
    }
}
