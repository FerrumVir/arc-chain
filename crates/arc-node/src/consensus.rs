//! Consensus manager - wires arc-consensus into the node.
//!
//! Wraps the DAG `ConsensusEngine` and drives the propose → commit loop,
//! draining the mempool and feeding committed blocks into `StateDB`.

use crate::SharedValidators;
use crate::recovery_dag_wal::{ActiveDurability, ActiveLogWriter, RetainedDagRecord};
use crate::vrf::ProposerSelector;
use arc_consensus::{ConsensusEngine, StakeTier, Validator, ValidatorSet};
use arc_crypto::{Hash256, KeyPair};
use arc_mempool::Mempool;
use arc_net::transport::{InboundMessage, OutboundMessage};
use arc_state::StateDB;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Transaction bodies retained between DAG availability and canonical commit.
/// Backpressure is safer than deleting an arbitrary body that a later leader
/// block still commits to.
const MAX_PENDING_DAG_PREIMAGES: usize = 100_000;
const RECOVERY_DAG_REBROADCAST_INTERVAL: Duration = Duration::from_secs(1);

/// Runtime policy that may replace a bounded recovery-DAG writer with a
/// compacted successor. The caller hands over exclusive ownership of the
/// writer so its advisory store lock is released before generation publish.
/// An error deliberately leaves the writer slot empty and is fatal to the
/// consensus loop; restart then follows the generation crash-recovery rules.
pub trait RecoveryDagRollover: Send + Sync {
    fn prepare_append(
        &self,
        state: &StateDB,
        engine: &ConsensusEngine,
        writer: ActiveLogWriter,
        upcoming: &[RetainedDagRecord],
    ) -> Result<ActiveLogWriter, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerDagTransactionError {
    AttachmentMismatch,
    InvalidTransaction(Hash256),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DagPreimageError {
    Capacity,
    Missing(Hash256),
    HashMismatch { expected: Hash256, actual: Hash256 },
    DuplicateLocalProposal { round: u64 },
}

fn pending_preimage_capacity_allows(
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    hashes: &[Hash256],
) -> bool {
    let new_hashes = hashes
        .iter()
        .filter(|hash| !pending.contains_key(&hash.0))
        .count();
    pending.len().saturating_add(new_hashes) <= MAX_PENDING_DAG_PREIMAGES
}

fn retain_dag_preimages(
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    latest_round: &dashmap::DashMap<[u8; 32], u64>,
    round: u64,
    transactions: &[arc_types::Transaction],
) -> Result<(), DagPreimageError> {
    let hashes: Vec<_> = transactions
        .iter()
        .map(|transaction| transaction.hash)
        .collect();
    if !pending_preimage_capacity_allows(pending, &hashes) {
        return Err(DagPreimageError::Capacity);
    }
    for transaction in transactions {
        pending
            .entry(transaction.hash.0)
            .or_insert_with(|| transaction.clone());
        latest_round
            .entry(transaction.hash.0)
            .and_modify(|latest| *latest = (*latest).max(round))
            .or_insert(round);
    }
    Ok(())
}

fn exact_dag_preimages(
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    hashes: &[Hash256],
) -> Result<Vec<arc_types::Transaction>, DagPreimageError> {
    hashes
        .iter()
        .map(|expected| {
            let transaction = pending
                .get(&expected.0)
                .ok_or(DagPreimageError::Missing(*expected))?
                .clone();
            if transaction.hash != *expected {
                return Err(DagPreimageError::HashMismatch {
                    expected: *expected,
                    actual: transaction.hash,
                });
            }
            Ok(transaction)
        })
        .collect()
}

/// Recover the exact local proposal for `round`, including every body needed
/// for a byte-identical re-broadcast. A protocol-v3 restart can occur after its
/// proposal crossed the DAG fsync barrier but before every peer received it.
/// Re-proposing would be a double vote, while failing to re-broadcast would
/// strand unanimous recovery-mode progress forever.
fn local_proposal_with_preimages(
    engine: &ConsensusEngine,
    local_address: Hash256,
    round: u64,
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
) -> Result<Option<(arc_consensus::DagBlock, Vec<arc_types::Transaction>)>, DagPreimageError> {
    let mut local = engine
        .blocks_in_round(round)
        .into_iter()
        .filter_map(|hash| engine.get_block(&hash))
        .filter(|block| block.author == local_address);
    let Some(block) = local.next() else {
        return Ok(None);
    };
    if local.next().is_some() {
        return Err(DagPreimageError::DuplicateLocalProposal { round });
    }
    let transactions = exact_dag_preimages(pending, &block.transactions)?;
    Ok(Some((block, transactions)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalProposalBroadcast {
    NotPresent,
    Enqueued,
    Backpressured,
}

fn try_rebroadcast_local_proposal(
    engine: &ConsensusEngine,
    local_address: Hash256,
    round: u64,
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    outbound: Option<&mpsc::Sender<OutboundMessage>>,
) -> Result<LocalProposalBroadcast, String> {
    let Some((block, transactions)) =
        local_proposal_with_preimages(engine, local_address, round, pending)
            .map_err(|error| format!("local proposal/preimage invariant failed: {error:?}"))?
    else {
        return Ok(LocalProposalBroadcast::NotPresent);
    };
    let outbound =
        outbound.ok_or_else(|| "recovery DAG proposal has no outbound transport".to_string())?;
    match outbound.try_send(OutboundMessage::BroadcastDagBlock {
        block,
        transactions,
    }) {
        Ok(()) => Ok(LocalProposalBroadcast::Enqueued),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            Ok(LocalProposalBroadcast::Backpressured)
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err("recovery DAG outbound transport channel is closed".to_string())
        }
    }
}

/// Queue the smallest recovery-DAG window that can heal a clean rolling
/// restart. A validator can stop after its current proposal was fsynced and
/// delivered to its peers but before their proposals for that round reached
/// its own WAL. The live peers then advance exactly one round and stall. Their
/// current blocks are not admissible on the restarted node until it receives
/// the missing parent-round blocks, so replay must be parent-first.
///
/// Only the node's exact, already-signed local proposals are queued. This does
/// not create a view-change certificate, relax parent quorum, or permit a
/// second proposal for either round.
fn queue_recovery_reconnect_replay(
    engine: &ConsensusEngine,
    pending_rounds: &mut std::collections::BTreeSet<u64>,
) {
    let current_round = engine.current_round();
    if !engine.is_recovery_bootstrap_round(current_round)
        && let Some(parent_round) = current_round.checked_sub(1)
    {
        pending_rounds.insert(parent_round);
    }
    pending_rounds.insert(current_round);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryReplayDrain {
    Complete,
    Backpressured { round: u64 },
}

/// Enqueue queued reconnect proposals in ascending round order and retain the
/// first unsent round on backpressure. Transport ordering therefore puts the
/// missing parent before a dependent current-round block whenever both use the
/// same connection, while the retained queue makes a full channel lossless.
fn try_drain_recovery_reconnect_replay(
    engine: &ConsensusEngine,
    local_address: Hash256,
    pending_rounds: &mut std::collections::BTreeSet<u64>,
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    outbound: Option<&mpsc::Sender<OutboundMessage>>,
) -> Result<RecoveryReplayDrain, String> {
    while let Some(round) = pending_rounds.first().copied() {
        match try_rebroadcast_local_proposal(engine, local_address, round, pending, outbound)? {
            LocalProposalBroadcast::Enqueued => {
                pending_rounds.remove(&round);
            }
            LocalProposalBroadcast::NotPresent => {
                let local_is_validator = engine.validator_set().can_produce_blocks(&local_address);
                if local_is_validator && round < engine.current_round() {
                    return Err(format!(
                        "recovery reconnect parent proposal for round {round} is missing"
                    ));
                }
                // The current proposal can legitimately be absent when a peer
                // connects before this node has collected all parents. An
                // observer also has no local proposal to replay.
                pending_rounds.remove(&round);
            }
            LocalProposalBroadcast::Backpressured => {
                return Ok(RecoveryReplayDrain::Backpressured { round });
            }
        }
    }
    Ok(RecoveryReplayDrain::Complete)
}

fn prune_irreversible_preimages(
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    latest_round: &dashmap::DashMap<[u8; 32], u64>,
    committed_round_exclusive: u64,
) -> usize {
    let obsolete: Vec<_> = latest_round
        .iter()
        .filter(|entry| *entry.value() < committed_round_exclusive)
        .map(|entry| *entry.key())
        .collect();
    for hash in &obsolete {
        latest_round.remove(hash);
        pending.remove(hash);
    }
    obsolete.len()
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

    let mut verified: Vec<_> = transactions
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
        .collect::<Result<_, _>>()?;
    // The DAG commits only the canonical hash sequence, not the sender's
    // attachment order. Canonicalize the bodies to that same order before any
    // state-admission decision so two honest peers cannot accept/reject the
    // same block differently after receiving permuted JSON/bincode vectors.
    verified.sort_by_key(|transaction| transaction.hash.0);
    Ok(verified)
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
    /// Shared operator-approved validator authority list for RPC. Transport
    /// connection events must never add, remove, or reweight entries.
    pub dag_validators: Option<SharedValidators>,
    /// Shared DAG round counter for health endpoint.
    pub dag_round: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// Shared DAG committed block counter for health endpoint.
    pub dag_committed: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// WAL writer for DAG persistence - enables consensus recovery after restart.
    pub dag_wal: Option<Arc<arc_state::WalWriter>>,
    /// Content-addressed, bounded recovery-domain DAG delta. Protocol v3 uses
    /// this instead of the legacy unbounded segmented WAL.
    pub recovery_dag_writer: Option<Arc<parking_lot::Mutex<Option<ActiveLogWriter>>>>,
    /// Synchronous rollover policy invoked at safe append boundaries. Protocol
    /// v3 requires this alongside its bounded writer so capacity cannot turn
    /// into periodic coordinated validator exits.
    pub recovery_dag_rollover: Option<Arc<dyn RecoveryDagRollover>>,
    /// Registry for externally certified long-range checkpoints. It remains
    /// empty until canonical state-root signatures are actually collected.
    /// Behind Mutex for interior mutability in the consensus loop (takes &self).
    pub checkpoint_registry: std::sync::Mutex<arc_consensus::security::CheckpointRegistry>,
    /// Nothing-at-stake mitigation: double-vote tracker with graduated slashing.
    /// Behind Mutex for interior mutability in the consensus loop (takes &self).
    pub stake_tracker: std::sync::Mutex<arc_consensus::security::StakeTracker>,
    /// Strictly verified transaction bodies restored from the bound recovery
    /// DAG WAL before the live loop starts.
    recovered_preimages: Vec<arc_types::Transaction>,
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
            dag_validators: None,
            dag_round: None,
            dag_committed: None,
            dag_wal: None,
            recovery_dag_writer: None,
            recovery_dag_rollover: None,
            checkpoint_registry: std::sync::Mutex::new(
                arc_consensus::security::CheckpointRegistry::new(),
            ),
            stake_tracker: std::sync::Mutex::new(arc_consensus::security::StakeTracker::new()),
            recovered_preimages: Vec::new(),
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
            dag_validators: None,
            dag_round: None,
            dag_committed: None,
            dag_wal: None,
            recovery_dag_writer: None,
            recovery_dag_rollover: None,
            checkpoint_registry: std::sync::Mutex::new(
                arc_consensus::security::CheckpointRegistry::new(),
            ),
            stake_tracker: std::sync::Mutex::new(arc_consensus::security::StakeTracker::new()),
            recovered_preimages: Vec::new(),
        }
    }

    /// Install preimages that were signature/domain checked during strict DAG
    /// WAL replay. They are retained directly; routing them through the live
    /// mempool could delay a previously certified commit behind a drain cap.
    pub fn install_recovered_preimages(&mut self, transactions: Vec<arc_types::Transaction>) {
        self.recovered_preimages = transactions;
    }

    fn has_durable_dag_writer(&self) -> bool {
        self.recovery_dag_writer
            .as_ref()
            .is_some_and(|slot| slot.lock().is_some())
            || self.dag_wal.is_some()
    }

    fn persist_dag_block(
        &self,
        state: &StateDB,
        block: &arc_consensus::DagBlock,
        transactions: &[arc_types::Transaction],
    ) -> Result<(), String> {
        let block_bytes = bincode::serialize(block)
            .map_err(|error| format!("failed to serialize DAG block {}: {error}", block.hash))?;
        if let Some(writer_slot) = &self.recovery_dag_writer {
            let mut records = Vec::with_capacity(transactions.len().saturating_add(1));
            for transaction in transactions {
                let bytes = bincode::serialize(transaction).map_err(|error| {
                    format!(
                        "failed to serialize DAG transaction {}: {error}",
                        transaction.hash
                    )
                })?;
                records.push(RetainedDagRecord::transaction(
                    block.round,
                    transaction.hash,
                    bytes,
                ));
            }
            records.push(RetainedDagRecord::dag_block(
                block.round,
                block.hash,
                block_bytes,
            ));
            let mut slot = writer_slot.lock();
            let mut writer = slot
                .take()
                .ok_or_else(|| "recovery DAG writer slot is empty".to_string())?;
            if let Some(rollover) = &self.recovery_dag_rollover {
                writer = rollover.prepare_append(state, &self.engine, writer, &records)?;
            }
            writer
                .append_batch(&records, ActiveDurability::Fsync)
                .map_err(|error| error.to_string())?;
            *slot = Some(writer);
            return Ok(());
        }
        if let Some(wal) = &self.dag_wal {
            for transaction in transactions {
                wal.append(
                    arc_state::WalOp::SetFullTransaction(
                        transaction.hash,
                        Box::new(transaction.clone()),
                    ),
                    block.round,
                );
            }
            wal.append(
                arc_state::WalOp::SetDagBlock(block.hash, block_bytes),
                block.round,
            );
            wal.sync().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn persist_dag_commit(
        &self,
        state: &StateDB,
        block: &arc_consensus::DagBlock,
    ) -> Result<(), String> {
        if let Some(writer_slot) = &self.recovery_dag_writer {
            let mut slot = writer_slot.lock();
            let mut writer = slot
                .take()
                .ok_or_else(|| "recovery DAG writer slot is empty".to_string())?;
            writer
                .append_batch(
                    &[RetainedDagRecord::commit(block.round, block.hash)],
                    ActiveDurability::Fsync,
                )
                .map_err(|error| error.to_string())?;
            // This call is deliberately after both canonical state fsync and
            // commit-record fsync. It may drop the old writer, compact through
            // this exact boundary, publish/pin a successor, and return its new
            // active writer before the loop performs any further work.
            if let Some(rollover) = &self.recovery_dag_rollover {
                writer = rollover.prepare_append(state, &self.engine, writer, &[])?;
            }
            *slot = Some(writer);
            return Ok(());
        }
        if let Some(wal) = &self.dag_wal {
            wal.append(arc_state::WalOp::CommitDagBlock(block.hash), block.round);
            wal.sync().map_err(|error| error.to_string())?;
        }
        Ok(())
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
        if state.active_protocol_version().major == 3 {
            if !self.is_multi_validator() {
                tracing::error!(
                    "Protocol-v3 recovery consensus requires a multi-validator checkpoint set"
                );
                return;
            }
            if self.recovery_dag_writer.is_none() || self.recovery_dag_rollover.is_none() {
                tracing::error!(
                    "Protocol-v3 recovery consensus requires bounded generation persistence with live rollover"
                );
                return;
            }
        }

        // Pending transaction index: tx_hash → Transaction
        // Transactions live here between drain from mempool and execution.
        let pending_txs: DashMap<[u8; 32], Transaction> = DashMap::new();
        // Highest DAG round that still references each retained preimage.
        // A body is removable only once this round is behind the contiguous
        // canonical commit cursor.
        let pending_tx_latest_round: DashMap<[u8; 32], u64> = DashMap::new();
        let recovered_round = self.engine.current_round();
        if let Err(error) = retain_dag_preimages(
            &pending_txs,
            &pending_tx_latest_round,
            recovered_round,
            &self.recovered_preimages,
        ) {
            tracing::error!(
                ?error,
                recovered = self.recovered_preimages.len(),
                cap = MAX_PENDING_DAG_PREIMAGES,
                "Bounded recovery DAG preimages violate the live retention cap"
            );
            return;
        }

        // Track last proposed round to avoid double-proposing. Strict replay
        // may already have restored our durable proposal for the current
        // round; in that case it must be re-broadcast, never re-signed.
        let recovered_local_round = match local_proposal_with_preimages(
            &self.engine,
            self.validator_address,
            recovered_round,
            &pending_txs,
        ) {
            Ok(Some((block, _))) => Some(block.round),
            Ok(None) => None,
            Err(error) => {
                tracing::error!(
                    round = recovered_round,
                    ?error,
                    "Fatal recovered local DAG proposal/preimage invariant failure"
                );
                return;
            }
        };
        let mut last_proposed_round = recovered_local_round;
        let mut recovery_rebroadcast_pending = recovered_local_round.is_some();
        let mut next_recovery_rebroadcast = Instant::now();
        let mut recovery_reconnect_replay = std::collections::BTreeSet::<u64>::new();

        // Genesis membership and live transport connectivity are different
        // facts. The validator set must be known/frozen before networking,
        // but proposing before enough of that set is connected strands this
        // node's round-0 block before peers can receive it. Track authenticated
        // connections and wait for live quorum before proposing.
        let mut connected_validators =
            std::collections::HashMap::<Hash256, PeerConnectionGeneration>::new();

        'consensus_loop: loop {
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

            // A previous reconnect attempt may have found the outbound queue
            // full. Retry its exact parent-first window before accepting a
            // dependent peer block or advancing local consensus state.
            if self.engine.requires_full_round_participation()
                && !recovery_reconnect_replay.is_empty()
            {
                match try_drain_recovery_reconnect_replay(
                    &self.engine,
                    self.validator_address,
                    &mut recovery_reconnect_replay,
                    &pending_txs,
                    outbound_tx.as_ref(),
                ) {
                    Ok(RecoveryReplayDrain::Complete) => {
                        next_recovery_rebroadcast =
                            Instant::now() + RECOVERY_DAG_REBROADCAST_INTERVAL;
                    }
                    Ok(RecoveryReplayDrain::Backpressured { round }) => {
                        debug!(round, "Recovery DAG reconnect replay remains backpressured");
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            %error,
                            "Fatal recovery DAG reconnect replay failure"
                        );
                        return;
                    }
                }
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
                            if self.engine.requires_full_round_participation() {
                                // A validator may reconnect one round behind
                                // after its proposal reached us but our same-
                                // round proposal did not reach its WAL. Replay
                                // that missing parent before the current block.
                                queue_recovery_reconnect_replay(
                                    &self.engine,
                                    &mut recovery_reconnect_replay,
                                );
                                match try_drain_recovery_reconnect_replay(
                                    &self.engine,
                                    self.validator_address,
                                    &mut recovery_reconnect_replay,
                                    &pending_txs,
                                    outbound_tx.as_ref(),
                                ) {
                                    Ok(RecoveryReplayDrain::Complete) => {
                                        next_recovery_rebroadcast =
                                            Instant::now() + RECOVERY_DAG_REBROADCAST_INTERVAL;
                                    }
                                    Ok(RecoveryReplayDrain::Backpressured { round }) => {
                                        // Do not drain a newly connected peer's
                                        // dependent block or advance locally
                                        // before the exact parent-first window
                                        // has entered the transport queue.
                                        debug!(
                                            round,
                                            "Recovery DAG reconnect replay is backpressured"
                                        );
                                        continue 'consensus_loop;
                                    }
                                    Err(error) => {
                                        tracing::error!(
                                            %error,
                                            "Fatal recovery DAG reconnect replay failure"
                                        );
                                        return;
                                    }
                                }
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
                            if state.active_protocol_version().major == 3 {
                                let fresh: Vec<_> = verified
                                    .iter()
                                    .filter(|transaction| {
                                        !state.receipts.contains_key(&transaction.hash.0)
                                    })
                                    .cloned()
                                    .collect();
                                if let Err(error) = state.validate_v3_block_admission(&fresh) {
                                    warn!(
                                        author = %block.author,
                                        round = block.round,
                                        error = %error,
                                        "Rejected DAG block that fails protocol-v3 state admission"
                                    );
                                    continue;
                                }
                            }
                            if !pending_preimage_capacity_allows(&pending_txs, &block.transactions)
                            {
                                warn!(
                                    author = %block.author,
                                    round = block.round,
                                    retained = pending_txs.len(),
                                    cap = MAX_PENDING_DAG_PREIMAGES,
                                    "Backpressured DAG block before transaction-preimage capacity exhaustion"
                                );
                                continue;
                            }
                            // Feed block into consensus engine
                            match self.engine.receive_block(&block) {
                                Ok(()) => {
                                    // Availability precedes visibility to the
                                    // commit loop: persist exact bodies + block
                                    // and fsync before retaining them in memory.
                                    if let Err(error) =
                                        self.persist_dag_block(&state, &block, &verified)
                                    {
                                        tracing::error!(
                                            block = %block.hash,
                                            error = %error,
                                            "Fatal DAG persistence failure before commit eligibility"
                                        );
                                        return;
                                    }
                                    if let Err(error) = retain_dag_preimages(
                                        &pending_txs,
                                        &pending_tx_latest_round,
                                        block.round,
                                        &verified,
                                    ) {
                                        tracing::error!(
                                            block = %block.hash,
                                            ?error,
                                            "Fatal transaction-preimage retention invariant failure"
                                        );
                                        return;
                                    }
                                    debug!(
                                        author = %block.author,
                                        round = block.round,
                                        txs = block.transactions.len(),
                                        "Received DAG block from peer"
                                    );
                                    if self.engine.requires_full_round_participation() {
                                        let round = self.engine.current_round();
                                        match try_rebroadcast_local_proposal(
                                            &self.engine,
                                            self.validator_address,
                                            round,
                                            &pending_txs,
                                            outbound_tx.as_ref(),
                                        ) {
                                            Ok(LocalProposalBroadcast::Enqueued) => {
                                                recovery_rebroadcast_pending = false;
                                                next_recovery_rebroadcast = Instant::now()
                                                    + RECOVERY_DAG_REBROADCAST_INTERVAL;
                                            }
                                            Ok(LocalProposalBroadcast::NotPresent) => {}
                                            Ok(LocalProposalBroadcast::Backpressured) => {
                                                recovery_rebroadcast_pending = true;
                                                debug!(
                                                    round,
                                                    "Deferred recovery round advance until exact local proposal can be re-broadcast"
                                                );
                                                continue;
                                            }
                                            Err(error) => {
                                                tracing::error!(
                                                    round,
                                                    %error,
                                                    "Fatal recovery DAG pre-advance re-broadcast failure"
                                                );
                                                return;
                                            }
                                        }
                                    }
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
                                    if state.active_protocol_version().major == 3
                                        && state.validate_v3_transaction_admission(&tx).is_err()
                                    {
                                        continue;
                                    }
                                    // A body retained for one non-leader DAG
                                    // block must remain proposal-eligible until
                                    // it has a canonical receipt. Inbound
                                    // transaction gossip is not immediately
                                    // re-broadcast, and Mempool deduplicates its
                                    // resident set, so accepting this retry does
                                    // not create a wire echo loop.
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
                            // This pre-v0.8 P2P message never had a complete
                            // execution/settlement protocol. Keep it dark: the
                            // supported community path is the authenticated,
                            // job-bound RPC queue and verified result endpoint.
                            warn!(
                                request_id = %request_id,
                                tokens = max_tokens,
                                "Ignored dormant legacy P2P inference request"
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
                            // A response on the old P2P surface is not bound to
                            // an authenticated assignment and must never affect
                            // work counters or reward settlement.
                            warn!(
                                request_id = %request_id,
                                responder = %responder,
                                ms_per_token = ms_per_token,
                                "Ignored dormant legacy P2P inference response"
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

            // A PeerConnected event can discover backpressure while enqueueing
            // the parent-first window. Do not process a dependent block or
            // advance this tick; the retry at the top of the next tick drains
            // the retained oldest round first.
            if self.engine.requires_full_round_participation()
                && !recovery_reconnect_replay.is_empty()
            {
                continue;
            }

            // A recovery-domain validator that restarts after fsync has already
            // signed its one legal block for this round. Re-broadcast that
            // exact block and its exact durable preimages until the transport
            // accepts it. A closed transport is fatal; a full channel is
            // transient and retried on the next tick.
            if self.engine.requires_full_round_participation()
                && Instant::now() >= next_recovery_rebroadcast
            {
                recovery_rebroadcast_pending = true;
            }
            if recovery_rebroadcast_pending && self.engine.requires_full_round_participation() {
                let round = self.engine.current_round();
                match try_rebroadcast_local_proposal(
                    &self.engine,
                    self.validator_address,
                    round,
                    &pending_txs,
                    outbound_tx.as_ref(),
                ) {
                    Ok(LocalProposalBroadcast::Enqueued | LocalProposalBroadcast::NotPresent) => {
                        recovery_rebroadcast_pending = false;
                        next_recovery_rebroadcast =
                            Instant::now() + RECOVERY_DAG_REBROADCAST_INTERVAL;
                    }
                    Ok(LocalProposalBroadcast::Backpressured) => {
                        debug!(round, "Recovery DAG re-broadcast channel is full; retrying")
                    }
                    Err(error) => {
                        tracing::error!(
                            round,
                            %error,
                            "Fatal local DAG re-broadcast/preimage invariant failure"
                        );
                        return;
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
                if self.engine.requires_full_round_participation() {
                    vs.validators
                        .iter()
                        .filter(|validator| validator.stake > 0)
                        .all(|validator| seen_validators.contains(&validator.address))
                } else {
                    connected_stake >= vs.quorum
                }
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
                    if self.engine.requires_full_round_participation() {
                        vs.validators
                            .iter()
                            .filter(|validator| validator.stake > 0)
                            .all(|validator| seen_authors.contains(&validator.address))
                    } else {
                        parent_stake >= vs.quorum
                    }
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
                    let mut transactions = mempool.drain(drain_limit);
                    // Recovery protocol v3 never proposes a transaction that
                    // would become a failed canonical history entry. Validate
                    // the complete candidate first; on a conflicting batch,
                    // deterministically retain the first FIFO-valid disjoint
                    // subset, defer candidate-local conflicts, and discard
                    // envelopes already stale against canonical state.
                    if state.active_protocol_version().major == 3 {
                        transactions.retain(|transaction| {
                            !state.receipts.contains_key(&transaction.hash.0)
                                && state.validate_v3_transaction_admission(transaction).is_ok()
                        });
                        // DagBlock commits lexicographically sorted hashes.
                        // Validate exactly that order, never the local FIFO
                        // attachment order which peers are free to permute.
                        transactions.sort_by_key(|transaction| transaction.hash.0);
                        if state.validate_v3_block_admission(&transactions).is_err() {
                            let mut admitted = Vec::with_capacity(transactions.len());
                            let mut deferred = Vec::new();
                            for transaction in transactions {
                                let mut candidate = admitted.clone();
                                candidate.push(transaction.clone());
                                if state.validate_v3_block_admission(&candidate).is_ok() {
                                    admitted.push(transaction);
                                } else {
                                    deferred.push(transaction);
                                }
                            }
                            transactions = admitted;
                            // Individually valid envelopes can conflict only
                            // within this candidate (for example two spends of
                            // one nonce). Keep the loser available until the
                            // winning canonical state transition decides which
                            // envelope became stale.
                            for transaction in deferred {
                                let _ = mempool.insert(transaction);
                            }
                        }
                    }
                    let transaction_hashes: Vec<_> = transactions
                        .iter()
                        .map(|transaction| transaction.hash)
                        .collect();
                    if !pending_preimage_capacity_allows(&pending_txs, &transaction_hashes) {
                        warn!(
                            retained = pending_txs.len(),
                            candidate = transactions.len(),
                            cap = MAX_PENDING_DAG_PREIMAGES,
                            "Backpressuring local transaction proposal at preimage capacity"
                        );
                        for transaction in transactions.drain(..) {
                            let _ = mempool.insert(transaction);
                        }
                    }
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

                    // The former encrypted-mempool branch was never a network
                    // protocol: ciphertext was proposer-local, absent from the
                    // signed DAG block/WAL, and every process held every test
                    // committee secret. Keep it completely dark until ARC has
                    // a replicated ciphertext commitment and validator-specific
                    // threshold reveal protocol.
                    let has_txs = !transactions.is_empty();

                    if has_txs || multi_validator {
                        let tx_hashes: Vec<Hash256> =
                            transactions.iter().map(|tx| tx.hash).collect();

                        // Gossip admissible envelopes so every leader has an
                        // opportunity to include them. They become commit
                        // eligible locally only after the exact proposed block
                        // and bodies cross the DAG WAL fsync barrier below.
                        if has_txs {
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
                                // Persist our own exact proposal and fsync it
                                // before advancing/broadcasting/committing.
                                if let Err(error) =
                                    self.persist_dag_block(&state, &block, &transactions)
                                {
                                    tracing::error!(
                                        block = %block.hash,
                                        error = %error,
                                        "Fatal local DAG persistence failure"
                                    );
                                    return;
                                }
                                if let Err(error) = retain_dag_preimages(
                                    &pending_txs,
                                    &pending_tx_latest_round,
                                    block.round,
                                    &transactions,
                                ) {
                                    tracing::error!(
                                        block = %block.hash,
                                        ?error,
                                        "Fatal local transaction-preimage retention failure"
                                    );
                                    return;
                                }
                                info!(
                                    round = block.round,
                                    txs = block.transactions.len(),
                                    hash = %block.hash,
                                    "Proposed DAG block"
                                );
                                last_proposed_round = Some(block.round);

                                // Broadcast to peers
                                if let Some(ref tx_chan) = outbound_tx {
                                    match tx_chan.try_send(OutboundMessage::BroadcastDagBlock {
                                        block: block.clone(),
                                        transactions: transactions.clone(),
                                    }) {
                                        Ok(()) => {
                                            recovery_rebroadcast_pending = false;
                                            next_recovery_rebroadcast =
                                                Instant::now() + RECOVERY_DAG_REBROADCAST_INTERVAL;
                                        }
                                        Err(tokio::sync::mpsc::error::TrySendError::Full(_))
                                            if self.engine.requires_full_round_participation() =>
                                        {
                                            recovery_rebroadcast_pending = true;
                                            warn!(
                                                round = block.round,
                                                "Recovery DAG broadcast channel is full; retrying exact proposal"
                                            );
                                        }
                                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
                                            if self.engine.requires_full_round_participation() =>
                                        {
                                            tracing::error!(
                                                round = block.round,
                                                "Recovery DAG broadcast transport channel is closed"
                                            );
                                            return;
                                        }
                                        Err(error) => warn!(
                                            "Failed to broadcast DAG block: {} (channel full or closed)",
                                            error
                                        ),
                                    }
                                } else {
                                    if self.engine.requires_full_round_participation() {
                                        tracing::error!(
                                            round = block.round,
                                            "Recovery DAG proposal has no outbound transport"
                                        );
                                        return;
                                    }
                                    warn!("No outbound channel - cannot broadcast DAG block");
                                }
                                // Only the deterministic leader block for a
                                // round becomes canonical. Keep every
                                // unreceipted envelope proposal-eligible across
                                // rounds; otherwise the first non-leader to
                                // include it would strand it forever in the DAG
                                // preimage cache.
                                for transaction in transactions.iter().cloned() {
                                    let _ = mempool.insert(transaction);
                                }
                            }
                            Err(e) => {
                                warn!("Failed to propose block: {}", e);
                                for transaction in transactions.iter().cloned() {
                                    let _ = mempool.insert(transaction);
                                }
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

                        if multi_validator {
                            // ── Multi-validator: DAG commit path ─────────────
                            if has_txs {
                                debug!(
                                    pending = pending_txs.len(),
                                    "Multi-validator mode: waiting for DAG commit"
                                );
                            }
                        } else if has_txs {
                            // ── Canonical synchronous path: single-validator mode ──
                            // Filter out transactions already applied via RPC
                            // (faucet/submit direct-apply). Without this filter,
                            // consensus re-executes them → double nonce
                            // increment and double balance deduction.
                            let fresh_txs: Vec<Transaction> = transactions
                                .iter()
                                .filter(|tx| !state.receipts.contains_key(&tx.hash.0))
                                .cloned()
                                .collect();

                            if !fresh_txs.is_empty() {
                                let started = std::time::Instant::now();
                                match state.execute_block_adaptive_at(
                                    &fresh_txs,
                                    self.validator_address,
                                    timestamp,
                                ) {
                                    Ok((block, receipts)) => {
                                        info!(
                                            height = block.header.height,
                                            txs = fresh_txs.len(),
                                            success = receipts
                                                .iter()
                                                .filter(|receipt| receipt.success)
                                                .count(),
                                            elapsed_ms = started.elapsed().as_millis(),
                                            "Block produced (canonical synchronous executor)"
                                        );
                                    }
                                    Err(error) => {
                                        warn!(
                                            error = %error,
                                            "Canonical single-validator block execution failed"
                                        );
                                    }
                                }
                            }

                            // Clean up pending index after the synchronous
                            // executor has either durably committed or failed.
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
                    info!(
                        round = dag_block.round,
                        hash = %dag_block.hash,
                        txs = dag_block.transactions.len(),
                        "DAG block committed"
                    );

                    // A committed hash list without every exact transaction
                    // body is not executable consensus. Never silently skip a
                    // missing preimage: stop this loop before state or the
                    // durable commit cursor can move.
                    let all_preimages =
                        match exact_dag_preimages(&pending_txs, &dag_block.transactions) {
                            Ok(transactions) => transactions,
                            Err(error) => {
                                tracing::error!(
                                    block = %dag_block.hash,
                                    round = dag_block.round,
                                    ?error,
                                    "Fatal committed DAG transaction-preimage failure"
                                );
                                return;
                            }
                        };
                    if state.active_protocol_version().major == 3
                        && self.recovery_dag_writer.is_none()
                    {
                        tracing::error!(
                            block = %dag_block.hash,
                            round = dag_block.round,
                            "Fatal protocol-v3 DAG commit has no bounded recovery-generation writer"
                        );
                        return;
                    }
                    let mut committed_txs: Vec<Transaction> = all_preimages
                        .into_iter()
                        .filter(|transaction| !state.receipts.contains_key(&transaction.hash.0))
                        .collect();

                    // State can advance between DAG availability and commit.
                    // Conflicting envelopes that were valid when admitted are
                    // deterministically omitted instead of becoming free failed
                    // receipts. The resulting subset is revalidated as one
                    // exact v3 state block before any mutation.
                    if state.active_protocol_version().major == 3
                        && state.validate_v3_block_admission(&committed_txs).is_err()
                    {
                        let original = committed_txs.len();
                        let mut admitted = Vec::with_capacity(original);
                        for transaction in committed_txs {
                            let mut candidate = admitted.clone();
                            candidate.push(transaction.clone());
                            if state.validate_v3_block_admission(&candidate).is_ok() {
                                admitted.push(transaction);
                            }
                        }
                        committed_txs = admitted;
                        warn!(
                            block = %dag_block.hash,
                            omitted = original.saturating_sub(committed_txs.len()),
                            "Omitted state-stale v3 DAG envelopes without creating failed history"
                        );
                    }

                    let cross_shard_hashes: Vec<Hash256> = committed_txs
                        .iter()
                        .filter_map(|transaction| {
                            let arc_types::TxBody::Transfer(body) = &transaction.body else {
                                return None;
                            };
                            arc_consensus::is_cross_shard(
                                &transaction.from,
                                &body.to,
                                self.num_shards,
                            )
                            .then_some(transaction.hash)
                        })
                        .collect();
                    for transaction in &committed_txs {
                        if let arc_types::TxBody::Transfer(body) = &transaction.body {
                            let source =
                                arc_consensus::assign_shard(&transaction.from, self.num_shards);
                            let target = arc_consensus::assign_shard(&body.to, self.num_shards);
                            if source != target {
                                let _ = self.engine.lock_cross_shard(
                                    transaction.hash,
                                    source,
                                    target,
                                    dag_block.hash,
                                    dag_block.round,
                                );
                            }
                        }
                    }

                    let started = std::time::Instant::now();
                    let received_diff = self.pending_diffs.remove(&dag_block.hash.0);
                    let decision_proof = match self.engine.consensus_domain() {
                        Some(domain) => dag_block.state_decision_commitment(&domain),
                        None if state.active_protocol_version().major == 3 => {
                            tracing::error!(
                                block = %dag_block.hash,
                                round = dag_block.round,
                                "Fatal protocol-v3 DAG decision has no installed consensus domain"
                            );
                            return;
                        }
                        None => Hash256::ZERO,
                    };
                    // Every committed DAG leader maps to exactly one canonical
                    // state block, including an empty block when all envelopes
                    // were previously receipted or became state-stale.
                    let (block, receipts) = match state.execute_block_adaptive_at_with_proof(
                        &committed_txs,
                        dag_block.author,
                        dag_block.timestamp,
                        decision_proof,
                    ) {
                        Ok(result) => result,
                        Err(error) => {
                            tracing::error!(
                                block = %dag_block.hash,
                                round = dag_block.round,
                                error = %error,
                                "Fatal canonical execution failure; DAG commit cursor not persisted"
                            );
                            return;
                        }
                    };
                    for hash in &cross_shard_hashes {
                        let _ = self.engine.commit_cross_shard(*hash);
                    }

                    if let Some((_, (source, diff, reported_height))) = received_diff.as_ref() {
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
                    if self.proposer_mode && dag_block.author == self.validator_address {
                        let dirty = state.drain_dirty_addresses();
                        let diff = state.export_state_diff(&dirty);
                        if let Some(ref channel) = outbound_tx {
                            let _ = channel.try_send(OutboundMessage::BroadcastStateDiff {
                                block_hash: dag_block.hash,
                                diff,
                                block_height: block.header.height,
                            });
                        }
                    }

                    // StateDB's block boundary has already fsynced here. Only
                    // now may the separate DAG WAL record and fsync the commit
                    // cursor. A crash before this point replays the certified
                    // block without ever claiming a durable DAG commit.
                    if let Err(error) = self.persist_dag_commit(&state, dag_block) {
                        tracing::error!(
                            block = %dag_block.hash,
                            error = %error,
                            "Fatal DAG commit-cursor persistence failure"
                        );
                        return;
                    } else if !self.has_durable_dag_writer() {
                        warn!(
                            block = %dag_block.hash,
                            "Legacy/dev DAG commit is running without a durability WAL"
                        );
                    }

                    let elapsed = started.elapsed();
                    let success = receipts.iter().filter(|receipt| receipt.success).count();
                    info!(
                        height = block.header.height,
                        txs = committed_txs.len(),
                        success,
                        elapsed_ms = elapsed.as_millis(),
                        mode = if self.proposer_mode {
                            "proposer"
                        } else {
                            "full"
                        },
                        "Block produced and durably bound to DAG commit"
                    );
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

                // Transaction preimages are pruned only after every DAG round
                // known to reference them is irreversibly behind the contiguous
                // commit cursor. Never use arbitrary DashMap iteration here.
                let pruned = prune_irreversible_preimages(
                    &pending_txs,
                    &pending_tx_latest_round,
                    self.engine.last_committed_round(),
                );
                if pruned > 0 {
                    debug!(
                        pruned,
                        retained = pending_txs.len(),
                        "Pruned irreversibly obsolete DAG transaction preimages"
                    );
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
        assert_eq!(
            verified
                .iter()
                .map(|transaction| transaction.hash)
                .collect::<Vec<_>>(),
            committed,
            "attachment permutations must canonicalize to the DAG hash order"
        );
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
    fn committed_preimages_are_exact_and_never_silently_skipped() {
        let key = KeyPair::generate_ed25519();
        let first = signed_transfer(&key, 11, 0);
        let second = signed_transfer(&key, 12, 1);
        let pending = dashmap::DashMap::new();
        let latest = dashmap::DashMap::new();
        retain_dag_preimages(&pending, &latest, 9, &[first.clone(), second.clone()]).unwrap();

        let exact = exact_dag_preimages(&pending, &[second.hash, first.hash]).unwrap();
        assert_eq!(
            exact
                .iter()
                .map(|transaction| transaction.hash)
                .collect::<Vec<_>>(),
            vec![second.hash, first.hash]
        );

        pending.remove(&second.hash.0);
        assert!(matches!(
            exact_dag_preimages(&pending, &[first.hash, second.hash]),
            Err(DagPreimageError::Missing(hash)) if hash == second.hash
        ));
    }

    #[test]
    fn preimage_pruning_uses_latest_referencing_round_not_map_order() {
        let key = KeyPair::generate_ed25519();
        let obsolete = signed_transfer(&key, 21, 0);
        let future_commit = signed_transfer(&key, 22, 1);
        let pending = dashmap::DashMap::new();
        let latest = dashmap::DashMap::new();
        retain_dag_preimages(&pending, &latest, 4, std::slice::from_ref(&obsolete)).unwrap();
        retain_dag_preimages(
            &pending,
            &latest,
            50_001,
            std::slice::from_ref(&future_commit),
        )
        .unwrap();

        assert_eq!(prune_irreversible_preimages(&pending, &latest, 5), 1);
        assert!(!pending.contains_key(&obsolete.hash.0));
        assert_eq!(
            exact_dag_preimages(&pending, &[future_commit.hash])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn rolling_restart_replays_missing_parent_before_current_and_all_six_advance() {
        let validators: Vec<_> = (0..6)
            .map(|index| hash_bytes(format!("restart-validator-{index}").as_bytes()))
            .collect();
        let validator_set = ValidatorSet::new(
            validators
                .iter()
                .enumerate()
                .map(|(index, address)| {
                    Validator::new(*address, arc_consensus::STAKE_ARC, index as u16).unwrap()
                })
                .collect(),
            1,
        );
        let domain = arc_consensus::ConsensusDomain::new(
            hash_bytes(b"rolling-restart-rebroadcast-domain"),
            7,
            11,
        );
        let engines: Vec<_> = validators
            .iter()
            .map(|address| {
                let engine = ConsensusEngine::new(validator_set.clone(), *address);
                engine.install_consensus_domain(domain.clone()).unwrap();
                engine.install_recovery_cursor(100).unwrap();
                engine
            })
            .collect();
        let bootstrap_proposals: Vec<_> = engines
            .iter()
            .enumerate()
            .map(|(index, engine)| {
                engine
                    .propose_block(Vec::new(), 1_000 + index as u64)
                    .unwrap()
            })
            .collect();

        // All live peers received the soon-to-stop validator's bootstrap
        // proposal, so all six original engines can advance one round.
        for (target, engine) in engines.iter().enumerate() {
            for (source, proposal) in bootstrap_proposals.iter().enumerate() {
                if source != target {
                    engine.receive_block(proposal).unwrap();
                }
            }
            assert!(engine.advance_round());
            assert_eq!(engine.current_round(), 102);
        }

        // Five continuously-running validators produce round 102. They cannot
        // advance without validator 5, but their round-102 blocks all depend on
        // the complete six-author bootstrap parent set.
        let current_proposals: Vec<_> = engines
            .iter()
            .take(5)
            .enumerate()
            .map(|(index, engine)| {
                engine
                    .propose_block(Vec::new(), 2_000 + index as u64)
                    .unwrap()
            })
            .collect();
        for (target, engine) in engines.iter().take(5).enumerate() {
            for (source, proposal) in current_proposals.iter().enumerate() {
                if source != target {
                    engine.receive_block(proposal).unwrap();
                }
            }
            assert!(!engine.advance_round(), "5/6 must remain below unanimity");
        }

        // The restarted validator restores only its own durable round-101
        // proposal. This is the exact live failure: valid peer round-102 blocks
        // remain inadmissible because their other five parent bodies are not in
        // the restarted process.
        let restarted = ConsensusEngine::new(validator_set, validators[5]);
        restarted.install_consensus_domain(domain).unwrap();
        restarted.install_recovery_cursor(100).unwrap();
        restarted.receive_block(&bootstrap_proposals[5]).unwrap();
        for proposal in &current_proposals {
            assert!(
                restarted.receive_block(proposal).is_err(),
                "a signature must not bypass missing parent validation"
            );
        }
        assert_eq!(restarted.current_round(), 101);

        let pending = dashmap::DashMap::new();
        for source in 0..5 {
            let mut queued = std::collections::BTreeSet::new();
            queue_recovery_reconnect_replay(&engines[source], &mut queued);
            assert_eq!(queued.iter().copied().collect::<Vec<_>>(), vec![101, 102]);

            // A one-slot transport proves ordering and lossless retry for the
            // first peer. The others use two slots to prove the complete
            // parent-first window is emitted in one drain.
            let (sender, mut receiver) = mpsc::channel(if source == 0 { 1 } else { 2 });
            if source == 0 {
                assert_eq!(
                    try_drain_recovery_reconnect_replay(
                        &engines[source],
                        validators[source],
                        &mut queued,
                        &pending,
                        Some(&sender),
                    )
                    .unwrap(),
                    RecoveryReplayDrain::Backpressured { round: 102 }
                );
                assert_eq!(queued.iter().copied().collect::<Vec<_>>(), vec![102]);
            } else {
                assert_eq!(
                    try_drain_recovery_reconnect_replay(
                        &engines[source],
                        validators[source],
                        &mut queued,
                        &pending,
                        Some(&sender),
                    )
                    .unwrap(),
                    RecoveryReplayDrain::Complete
                );
                assert!(queued.is_empty());
            }
            let OutboundMessage::BroadcastDagBlock {
                block,
                transactions,
            } = receiver.try_recv().unwrap()
            else {
                panic!("reconnect must enqueue an exact parent proposal")
            };
            assert_eq!(block.hash, bootstrap_proposals[source].hash);
            assert!(transactions.is_empty());
            restarted.receive_block(&block).unwrap();

            if source == 0 {
                assert_eq!(
                    try_drain_recovery_reconnect_replay(
                        &engines[source],
                        validators[source],
                        &mut queued,
                        &pending,
                        Some(&sender),
                    )
                    .unwrap(),
                    RecoveryReplayDrain::Complete,
                    "draining one transport slot must retry the retained current round"
                );
                assert!(queued.is_empty());
            }
            let OutboundMessage::BroadcastDagBlock {
                block: current,
                transactions: current_transactions,
            } = receiver.try_recv().unwrap()
            else {
                panic!("reconnect must enqueue the exact current proposal after its parent")
            };
            assert_eq!(current.hash, current_proposals[source].hash);
            assert!(current_transactions.is_empty());
        }

        assert!(restarted.advance_round());
        assert_eq!(restarted.current_round(), 102);

        // Once the missing parent window is present, the exact same peer
        // current blocks become admissible. The restarted validator then makes
        // its one legal round-102 proposal and restores six-of-six progress.
        for proposal in &current_proposals {
            restarted.receive_block(proposal).unwrap();
        }
        let restarted_current = restarted.propose_block(Vec::new(), 3_000).unwrap();
        for engine in engines.iter().take(5) {
            engine.receive_block(&restarted_current).unwrap();
            assert!(engine.advance_round());
            assert_eq!(engine.current_round(), 103);
        }
        assert!(restarted.advance_round());
        assert_eq!(restarted.current_round(), 103);
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
