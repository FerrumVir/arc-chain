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
use tracing::{debug, error, info, warn};

const MAX_PENDING_DAG_PREIMAGES: usize = 100_000;
const RECOVERY_DAG_REBROADCAST_INTERVAL: Duration = Duration::from_secs(1);

enum DurableAbsenceVoteCollection {
    Withheld,
    Added(Option<arc_consensus::view_change::SkipCertificate>),
}

fn connected_validator_quorum(
    validator_set: &ValidatorSet,
    connected: &std::collections::HashSet<Hash256>,
    require_full_committee: bool,
) -> bool {
    if require_full_committee
        && !validator_set
            .validators
            .iter()
            .filter(|validator| validator.stake > 0)
            .all(|validator| connected.contains(&validator.address))
    {
        return false;
    }
    let connected: Vec<_> = connected.iter().copied().collect();
    validator_set.stake_for_addresses(&connected) >= validator_set.quorum
}

fn proposal_parent_quorum(engine: &ConsensusEngine, current_round: u64) -> bool {
    current_round == 0
        || engine.is_recovery_bootstrap_round(current_round)
        || engine.has_quorum_parents_for_round(current_round.saturating_sub(1))
}

fn history_absence_certificates(
    engine: &ConsensusEngine,
    blocks: &[arc_consensus::DagBlock],
) -> Vec<arc_consensus::view_change::SkipCertificate> {
    let mut rounds = std::collections::BTreeSet::new();
    for block in blocks.iter().take(HISTORY_MAX_BLOCKS) {
        rounds.insert(block.round);
        if let Some(parent_round) = block.round.checked_sub(1) {
            rounds.insert(parent_round);
        }
    }
    let validators = engine.frozen_validator_set();
    let mut certificates = Vec::new();
    for round in rounds {
        for validator in &validators.validators {
            if let Some(certificate) = engine.skip_certificate(round, &validator.address) {
                certificates.push(certificate);
                if certificates.len() == HISTORY_MAX_BLOCKS {
                    return certificates;
                }
            }
        }
    }
    certificates
}

fn persist_then_collect_absence_vote(
    vote: arc_consensus::view_change::SkipVote,
    decision_already_durable: bool,
    record: &arc_consensus::view_change::ConsensusSigningRecord,
    persist: impl FnOnce(&arc_consensus::view_change::ConsensusSigningRecord) -> bool,
    collector: &mut arc_consensus::view_change::SkipVoteCollector,
    domain: &arc_consensus::ConsensusDomain,
    set: &ValidatorSet,
) -> Result<DurableAbsenceVoteCollection, arc_consensus::view_change::CertificateError> {
    let decision_recorded = record
        .skipped_rounds
        .get(&vote.round)
        .and_then(|members| members.get(&vote.absentee))
        .is_some_and(|reason| *reason == vote.reason);
    if !decision_recorded {
        return Ok(DurableAbsenceVoteCollection::Withheld);
    }
    if !decision_already_durable && !persist(record) {
        return Ok(DurableAbsenceVoteCollection::Withheld);
    }
    collector
        .add(vote, domain, set)
        .map(DurableAbsenceVoteCollection::Added)
}

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

#[cfg(test)]
fn exact_dag_preimages(
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    hashes: &[Hash256],
) -> Result<Vec<arc_types::Transaction>, DagPreimageError> {
    exact_dag_preimages_from(pending, None, hashes)
}

fn history_bodies(
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    state: &StateDB,
    wanted: &std::collections::HashSet<[u8; 32]>,
) -> Vec<arc_types::Transaction> {
    wanted
        .iter()
        .filter_map(|hash| {
            pending
                .get(hash)
                .map(|entry| entry.value().clone())
                .or_else(|| state.full_transactions.get(hash).map(|t| t.clone()))
        })
        .collect()
}

fn exact_dag_preimages_from(
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    state: Option<&StateDB>,
    hashes: &[Hash256],
) -> Result<Vec<arc_types::Transaction>, DagPreimageError> {
    hashes
        .iter()
        .map(|expected| {
            let transaction = match pending.get(&expected.0) {
                Some(found) => found.clone(),
                None => state
                    .and_then(|state| state.full_transactions.get(&expected.0).map(|t| t.clone()))
                    .ok_or(DagPreimageError::Missing(*expected))?,
            };
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

fn local_proposal_with_preimages(
    engine: &ConsensusEngine,
    local_address: Hash256,
    round: u64,
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    state: Option<&StateDB>,
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
    let transactions = exact_dag_preimages_from(pending, state, &block.transactions)?;
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
    state: Option<&StateDB>,
) -> Result<LocalProposalBroadcast, String> {
    let Some((block, transactions)) =
        local_proposal_with_preimages(engine, local_address, round, pending, state)
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

const MAX_RECOVERY_REPLAY_ROUNDS: u64 = 16;

fn queue_recovery_reconnect_replay(
    engine: &ConsensusEngine,
    local_address: Hash256,
    pending_rounds: &mut std::collections::BTreeSet<u64>,
) {
    let current = engine.current_round();
    let start = engine
        .last_committed_round()
        .saturating_sub(1)
        .min(current.saturating_sub(1))
        .max(current.saturating_sub(MAX_RECOVERY_REPLAY_ROUNDS - 1));
    for round in start..=current {
        if round == current
            || engine.blocks_in_round(round).iter().any(|hash| {
                engine
                    .get_block(hash)
                    .is_some_and(|block| block.author == local_address)
            })
        {
            pending_rounds.insert(round);
        }
    }
}

fn complete_recovery_rebroadcast(pending: &mut bool, next: &mut Instant) {
    *pending = false;
    *next = Instant::now() + RECOVERY_DAG_REBROADCAST_INTERVAL;
}

fn queue_due_recovery_replay(
    engine: &ConsensusEngine,
    local_address: Hash256,
    rounds: &mut std::collections::BTreeSet<u64>,
    pending: &mut bool,
    next: Instant,
) -> bool {
    *pending |= Instant::now() >= next;
    if !*pending {
        return false;
    }
    queue_recovery_reconnect_replay(engine, local_address, rounds);
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryReplayDrain {
    Complete,
    Backpressured { round: u64 },
}

fn try_drain_recovery_reconnect_replay(
    engine: &ConsensusEngine,
    local_address: Hash256,
    pending_rounds: &mut std::collections::BTreeSet<u64>,
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    outbound: Option<&mpsc::Sender<OutboundMessage>>,
    state: Option<&StateDB>,
) -> Result<RecoveryReplayDrain, String> {
    while let Some(round) = pending_rounds.first().copied() {
        match try_rebroadcast_local_proposal(
            engine,
            local_address,
            round,
            pending,
            outbound,
            state,
        )? {
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
                pending_rounds.remove(&round);
            }
            LocalProposalBroadcast::Backpressured => {
                return Ok(RecoveryReplayDrain::Backpressured { round });
            }
        }
    }
    Ok(RecoveryReplayDrain::Complete)
}

const OUTBOUND_BACKPRESSURE_WARN_AFTER: Duration = Duration::from_secs(30);
const OUTBOUND_BACKPRESSURE_WARN_EVERY: Duration = Duration::from_secs(60);
const OUTBOUND_BACKPRESSURE_EXIT_AFTER: Duration = Duration::from_secs(120);
const MAX_PARKED_INBOUND: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutboundBackpressureVerdict {
    Continue,
    Warn { stalled: Duration },
    GiveUp { stalled: Duration },
}

#[derive(Debug, Default)]
struct OutboundBackpressure {
    full_since: Option<Instant>,
    last_warned: Option<Instant>,
}

impl OutboundBackpressure {
    fn observe(&mut self, queue_full: bool, now: Instant) -> OutboundBackpressureVerdict {
        if !queue_full {
            self.full_since = None;
            self.last_warned = None;
            return OutboundBackpressureVerdict::Continue;
        }
        let since = *self.full_since.get_or_insert(now);
        let stalled = now.saturating_duration_since(since);
        if stalled >= OUTBOUND_BACKPRESSURE_EXIT_AFTER {
            return OutboundBackpressureVerdict::GiveUp { stalled };
        }
        if stalled >= OUTBOUND_BACKPRESSURE_WARN_AFTER
            && self.last_warned.is_none_or(|at| {
                now.saturating_duration_since(at) >= OUTBOUND_BACKPRESSURE_WARN_EVERY
            })
        {
            self.last_warned = Some(now);
            return OutboundBackpressureVerdict::Warn { stalled };
        }
        OutboundBackpressureVerdict::Continue
    }
}

fn park_inbound<T>(
    rx: Option<&mut mpsc::Receiver<T>>,
    parked: &mut std::collections::VecDeque<T>,
    cap: usize,
) -> usize {
    let Some(rx) = rx else {
        return 0;
    };
    let mut moved = 0;
    while parked.len() < cap {
        match rx.try_recv() {
            Ok(message) => {
                parked.push_back(message);
                moved += 1;
            }
            Err(_) => break,
        }
    }
    moved
}

fn next_inbound<T>(
    released: &mut std::collections::VecDeque<T>,
    parked: &mut std::collections::VecDeque<T>,
    rx: &mut mpsc::Receiver<T>,
) -> Option<T> {
    released
        .pop_front()
        .or_else(|| parked.pop_front())
        .or_else(|| rx.try_recv().ok())
}

#[cfg(test)]
mod outbound_backpressure_tests {
    use super::*;

    #[test]
    fn a_queue_with_room_never_warns_or_exits() {
        let mut pressure = OutboundBackpressure::default();
        let start = Instant::now();
        for second in 0..600 {
            let now = start + Duration::from_secs(second);
            let verdict = pressure.observe(second % 20 != 0, now);
            assert_eq!(
                verdict,
                OutboundBackpressureVerdict::Continue,
                "second {second}"
            );
        }
    }

    #[test]
    fn a_queue_that_stays_full_warns_then_gives_up() {
        let mut pressure = OutboundBackpressure::default();
        let start = Instant::now();
        let mut warnings = Vec::new();
        let mut gave_up = None;
        for tick in 0..=(130 * 20) {
            let now = start + Duration::from_millis(tick * 50);
            match pressure.observe(true, now) {
                OutboundBackpressureVerdict::Continue => {}
                OutboundBackpressureVerdict::Warn { stalled } => warnings.push(stalled),
                OutboundBackpressureVerdict::GiveUp { stalled } => {
                    gave_up = Some(stalled);
                    break;
                }
            }
        }
        assert_eq!(
            warnings,
            vec![Duration::from_secs(30), Duration::from_secs(90)],
            "first warning at 30 s, then once a minute"
        );
        assert_eq!(gave_up, Some(OUTBOUND_BACKPRESSURE_EXIT_AFTER));
    }

    #[test]
    fn one_sample_with_room_restarts_the_clock() {
        let mut pressure = OutboundBackpressure::default();
        let start = Instant::now();
        assert_eq!(
            pressure.observe(true, start),
            OutboundBackpressureVerdict::Continue
        );
        assert!(matches!(
            pressure.observe(true, start + Duration::from_secs(119)),
            OutboundBackpressureVerdict::Warn { .. }
        ));
        assert_eq!(
            pressure.observe(false, start + Duration::from_secs(119)),
            OutboundBackpressureVerdict::Continue
        );
        assert_eq!(
            pressure.observe(true, start + Duration::from_secs(120)),
            OutboundBackpressureVerdict::Continue
        );
        assert!(matches!(
            pressure.observe(true, start + Duration::from_secs(240)),
            OutboundBackpressureVerdict::GiveUp { .. }
        ));
    }

    #[test]
    fn a_real_full_channel_is_seen_as_full() {
        let (tx, mut rx) = mpsc::channel::<u8>(2);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert_eq!(tx.capacity(), 0);
        let mut pressure = OutboundBackpressure::default();
        let start = Instant::now();
        pressure.observe(tx.capacity() == 0, start);
        assert!(matches!(
            pressure.observe(tx.capacity() == 0, start + OUTBOUND_BACKPRESSURE_EXIT_AFTER),
            OutboundBackpressureVerdict::GiveUp { .. }
        ));
        rx.try_recv().unwrap();
        assert_eq!(
            pressure.observe(tx.capacity() == 0, start + OUTBOUND_BACKPRESSURE_EXIT_AFTER),
            OutboundBackpressureVerdict::Continue
        );
    }

    #[test]
    fn parked_inbound_keeps_channel_order_and_its_cap() {
        let (tx, mut rx) = mpsc::channel::<u32>(16);
        for message in 0..10 {
            tx.try_send(message).unwrap();
        }
        let mut parked = std::collections::VecDeque::new();
        assert_eq!(park_inbound(Some(&mut rx), &mut parked, 4), 4);
        assert_eq!(park_inbound(Some(&mut rx), &mut parked, 4), 0, "cap holds");
        assert_eq!(
            park_inbound(None::<&mut mpsc::Receiver<u32>>, &mut parked, 99),
            0
        );
        tx.try_send(10).unwrap();

        let mut released = std::collections::VecDeque::from([100, 101]);
        let mut order = Vec::new();
        while let Some(message) = next_inbound(&mut released, &mut parked, &mut rx) {
            order.push(message);
            if message == 1 {
                released.push_back(102);
            }
        }
        assert_eq!(order, vec![100, 101, 0, 1, 102, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    }
}

const NATIVE_REQUEUE_LIFETIME: Duration = Duration::from_secs(120);

fn requeue_native(
    requeued_since: &mut std::collections::HashMap<[u8; 32], Instant>,
    transaction: &arc_types::Transaction,
) -> bool {
    let first = *requeued_since
        .entry(transaction.hash.0)
        .or_insert_with(Instant::now);
    first.elapsed() < NATIVE_REQUEUE_LIFETIME
}

fn select_native_transactions_and_requeue(
    state: &StateDB,
    candidates: &[arc_types::Transaction],
    mempool: &Mempool,
    admission: &crate::native_inference::NativeRequestAdmission,
    requeued_since: &mut std::collections::HashMap<[u8; 32], Instant>,
) -> Vec<arc_types::Transaction> {
    let selected = state.select_native_block_transactions(candidates);
    for transaction in candidates {
        if selected.iter().any(|kept| kept.hash == transaction.hash)
            || state.receipts.contains_key(&transaction.hash.0)
        {
            continue;
        }
        let eligible = if StateDB::is_native_inference_transaction(transaction) {
            state.native_transaction_still_admissible(transaction)
                && requeue_native(requeued_since, transaction)
        } else {
            state.active_protocol_version().major == 3
                && state.native_migration().is_some()
                && state.validate_v3_transaction_admission(transaction).is_ok()
        };
        if eligible {
            admission.insert(mempool, transaction.clone());
        }
    }
    selected
}

fn prune_irreversible_preimages(
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    latest_round: &dashmap::DashMap<[u8; 32], u64>,
    committed_round_exclusive: u64,
    unexecuted_floor: u64,
    executed: impl Fn(&[u8; 32]) -> bool,
) -> usize {
    let obsolete: Vec<_> = latest_round
        .iter()
        .filter(|entry| {
            *entry.value() < committed_round_exclusive
                && (*entry.value() < unexecuted_floor || executed(entry.key()))
        })
        .map(|entry| *entry.key())
        .collect();
    for hash in &obsolete {
        latest_round.remove(hash);
        pending.remove(hash);
    }
    obsolete.len()
}

fn prune_dag_preimages(
    pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
    latest_round: &dashmap::DashMap<[u8; 32], u64>,
    committed_round_exclusive: u64,
    unexecuted_floor: u64,
    executed: impl Fn(&[u8; 32]) -> bool,
) -> usize {
    prune_irreversible_preimages(
        pending,
        latest_round,
        committed_round_exclusive.saturating_sub(1),
        unexecuted_floor,
        executed,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerStateDiffError {
    UnexpectedSource,
    HeightMismatch,
    StateRootMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerConnectionGeneration {
    connection_id: u64,
    connected: bool,
}

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

        assert!(!record_peer_disconnected(&mut connected, peer, 7));
        assert!(!record_peer_connected(&mut connected, peer, 7));
        assert!(!connected.get(&peer).unwrap().connected);

        assert!(record_peer_connected(&mut connected, peer, 8));
        assert!(!record_peer_connected(&mut connected, peer, 7));
        assert_eq!(connected.get(&peer).unwrap().connection_id, 8);
        assert!(connected.get(&peer).unwrap().connected);
    }
}

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

#[cfg(test)]
fn verify_peer_dag_transactions(
    committed_hashes: &[Hash256],
    transactions: &[arc_types::Transaction],
) -> Result<Vec<arc_types::Transaction>, PeerDagTransactionError> {
    verify_peer_dag_transactions_in_domain(committed_hashes, transactions, None)
}

fn divergence(state: &StateDB, height: u64, certified: Hash256) -> Option<Hash256> {
    state
        .get_block(height)
        .map(|block| block.hash)
        .filter(|local| *local != certified)
}

fn lower_divergence(recorded_plus_one: u64, height: u64) -> Option<u64> {
    let candidate = height.saturating_add(1);
    (recorded_plus_one == 0 || candidate < recorded_plus_one).then_some(candidate)
}

static REPORTED_DIVERGENCES: std::sync::Mutex<std::collections::BTreeSet<u64>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());
const MAX_REPORTED_DIVERGENCES: usize = 256;

fn report_divergence(height: u64, local: Hash256, certified: Hash256) {
    let _ = crate::consensus_diagnostics::DIAG
        .diverged_at_height_plus_one
        .fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |recorded| lower_divergence(recorded, height),
        );
    {
        let mut reported = REPORTED_DIVERGENCES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !reported.insert(height) {
            return;
        }
        if reported.len() > MAX_REPORTED_DIVERGENCES {
            reported.pop_first();
        }
    }
    error!(
        height,
        local = %local,
        certified = %certified,
        "SAFETY: a verified quorum finality certificate names a different block at a height \
         this node executed. Either this node's chain diverged from the committee's, or the \
         certificate comes from another run of a chain with the same genesis and committee \
         (certificates are not yet bound to one chain run). This node keeps running: compare \
         its block at this height with its peers' and, if it diverged, restore it from a \
         verified backup."
    );
}

pub(crate) fn admit_gossiped_transaction(
    state: &StateDB,
    mempool: &Mempool,
    admission: &crate::native_inference::NativeRequestAdmission,
    bytes: &[u8],
) -> bool {
    let Ok(mut tx) = bincode::deserialize::<arc_types::Transaction>(bytes) else {
        return false;
    };
    tx.sig_verified = false;
    if state.verify_transaction_signature(&tx).is_err() {
        return false;
    }
    tx.sig_verified = true;
    if state.active_protocol_version().major == 3
        && state.validate_v3_transaction_admission(&tx).is_err()
    {
        return false;
    }
    let native_only =
        state.native_inference_context().is_some() && state.native_migration().is_none();
    let is_native = arc_state::StateDB::is_native_inference_transaction(&tx);
    if (native_only && !is_native)
        || (is_native
            && state.native_inference_context().is_some()
            && !state.native_transaction_still_admissible(&tx))
    {
        return false;
    }
    if state.receipts.contains_key(&tx.hash.0) {
        crate::consensus_diagnostics::bump(
            &crate::consensus_diagnostics::DIAG.stale_transactions_dropped,
        );
        return false;
    }
    admission.insert(mempool, tx)
}

fn retain_unreceipted(state: &StateDB, transactions: &mut Vec<arc_types::Transaction>) -> usize {
    let before = transactions.len();
    transactions.retain(|transaction| !state.receipts.contains_key(&transaction.hash.0));
    before - transactions.len()
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
    verified.sort_by_key(|transaction| transaction.hash.0);
    Ok(verified)
}

fn verify_peer_dag_availability(
    state: &StateDB,
    committed_hashes: &[Hash256],
    transactions: &[arc_types::Transaction],
) -> Result<Vec<arc_types::Transaction>, String> {
    let verified = verify_peer_dag_transactions_in_domain(
        committed_hashes,
        transactions,
        state.transaction_domain_hash(),
    )
    .map_err(|error| format!("invalid DAG transaction attachment: {error:?}"))?;
    if state.active_protocol_version().major == 3 {
        state
            .validate_v3_dag_availability(&verified)
            .map_err(|error| error.to_string())?;
    }
    Ok(verified)
}

fn should_execute_local_benchmark(
    benchmark: bool,
    can_produce: bool,
    validator_count: usize,
) -> bool {
    benchmark && can_produce && validator_count == 1
}

const DECISION_INDEX_MARGIN_BLOCKS: u64 = 512;

pub const DEFAULT_SNAPSHOT_EVERY_BLOCKS: u64 = 1024;

const UNPRODUCTIVE_REQUESTS_BEFORE_CHECKPOINT: u32 = 8;

const ABSENCE_REGOSSIP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

const RECORD_PRUNE_EVERY: u64 = 128;

const FINALITY_RECORD_MARGIN: u64 = 256;

const ABSENCE_RECORD_MARGIN: u64 = 2 * ABSENCE_SCAN_ROUNDS;

const HISTORY_MAX_ROUNDS: u64 = 256;
const HISTORY_MAX_BLOCKS: usize = 4_096;
const HISTORY_PARENT_CONTEXT: u64 = 2;
const HISTORY_REQUEST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

const TARGETED_FETCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const CHECKPOINT_SERVE_INTERVAL: Duration = Duration::from_secs(30);
const HISTORY_STUCK_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

const ABSENCE_SCAN_ROUNDS: u64 = 64;

pub struct ConsensusManager {
    pub engine: Arc<ConsensusEngine>,
    pub validator_address: Hash256,
    pub stake: u64,
    pub tier: StakeTier,
    pub num_shards: u16,
    pub benchmark: bool,
    pub proposer_mode: bool,
    pending_diffs: dashmap::DashMap<[u8; 32], (Hash256, arc_types::StateDiff, u64)>,
    vrf_selector: Option<ProposerSelector>,
    pub dag_validators: Option<SharedValidators>,
    pub dag_round: Option<Arc<std::sync::atomic::AtomicU64>>,
    pub dag_committed: Option<Arc<std::sync::atomic::AtomicU64>>,
    pub dag_wal: Option<Arc<arc_state::WalWriter>>,
    pub recovery_dag_writer: Option<Arc<parking_lot::Mutex<Option<ActiveLogWriter>>>>,
    pub recovery_dag_rollover: Option<Arc<dyn RecoveryDagRollover>>,
    pub checkpoint_registry: std::sync::Mutex<arc_consensus::security::CheckpointRegistry>,
    pub stake_tracker: std::sync::Mutex<arc_consensus::security::StakeTracker>,
    recovered_preimages: Vec<arc_types::Transaction>,
    pub signing_record_path: Option<std::path::PathBuf>,
    pub snapshot_every_blocks: u64,
    pub native_vote_relay: Option<Arc<crate::native_inference::NativeFinalizeSink>>,
    pub native_request_admission: Arc<crate::native_inference::NativeRequestAdmission>,
    applied_decisions: dashmap::DashMap<Hash256, u64>,
    decisions_indexed_through: std::sync::atomic::AtomicU64,
    pub require_full_committee_at_genesis: bool,
}

impl ConsensusManager {
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
            signing_record_path: None,
            snapshot_every_blocks: DEFAULT_SNAPSHOT_EVERY_BLOCKS,
            native_vote_relay: None,
            native_request_admission: Arc::new(
                crate::native_inference::NativeRequestAdmission::default(),
            ),
            applied_decisions: dashmap::DashMap::new(),
            decisions_indexed_through: std::sync::atomic::AtomicU64::new(0),
            require_full_committee_at_genesis: false,
        }
    }

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
            signing_record_path: None,
            snapshot_every_blocks: DEFAULT_SNAPSHOT_EVERY_BLOCKS,
            native_vote_relay: None,
            native_request_admission: Arc::new(
                crate::native_inference::NativeRequestAdmission::default(),
            ),
            applied_decisions: dashmap::DashMap::new(),
            decisions_indexed_through: std::sync::atomic::AtomicU64::new(0),
            require_full_committee_at_genesis: false,
        }
    }

    pub fn install_recovered_preimages(&mut self, transactions: Vec<arc_types::Transaction>) {
        self.recovered_preimages = transactions;
    }

    fn has_durable_dag_writer(&self) -> bool {
        self.recovery_dag_writer
            .as_ref()
            .is_some_and(|slot| slot.lock().is_some())
            || self.dag_wal.is_some()
    }

    fn import_history_durably(
        &self,
        state: &StateDB,
        blocks: &[arc_consensus::DagBlock],
        attached: &[arc_types::Transaction],
        pending: &dashmap::DashMap<[u8; 32], arc_types::Transaction>,
        latest_round: &dashmap::DashMap<[u8; 32], u64>,
    ) -> Result<u64, arc_consensus::ConsensusError> {
        use arc_consensus::ConsensusError;
        let mut attachments = std::collections::HashMap::new();
        for transaction in attached {
            if attachments
                .insert(transaction.hash.0, transaction)
                .is_some()
            {
                return Err(ConsensusError::InvalidBlock(
                    "history contains duplicate transaction attachments".into(),
                ));
            }
        }
        let mut verified = std::collections::HashMap::new();
        self.engine.import_history_with_persistence(
            blocks,
            HISTORY_MAX_ROUNDS,
            |block| {
                if block.transactions.windows(2).any(|pair| pair[0] == pair[1]) {
                    return Err(ConsensusError::InvalidBlock(
                        "history block repeats a transaction hash".into(),
                    ));
                }
                if !pending_preimage_capacity_allows(pending, &block.transactions) {
                    return Err(ConsensusError::InvalidBlock(
                        "history exceeds pending transaction-preimage capacity".into(),
                    ));
                }
                let mut bodies = Vec::with_capacity(block.transactions.len());
                for hash in &block.transactions {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        verified.entry(hash.0)
                    {
                        let body = match attachments.get(&hash.0) {
                            Some(body) => (**body).clone(),
                            None => exact_dag_preimages_from(pending, Some(state), &[*hash])
                                .map_err(|error| {
                                    ConsensusError::InvalidBlock(format!(
                                        "history is missing an exact transaction preimage: {error:?}"
                                    ))
                                })?
                                .remove(0),
                        };
                        let mut checked = verify_peer_dag_transactions_in_domain(
                            &[*hash], &[body], state.transaction_domain_hash(),
                        ).map_err(|error| {
                            ConsensusError::InvalidBlock(format!(
                                "history transaction failed signature/domain validation: {error:?}"
                            ))
                        })?;
                        entry.insert(checked.remove(0));
                    }
                    bodies.push(verified[&hash.0].clone());
                }
                self.persist_dag_block(state, block, &bodies)
                    .map_err(ConsensusError::HistoryPersistence)?;
                retain_dag_preimages(pending, latest_round, block.round, &bodies)
                    .map_err(|error| ConsensusError::HistoryPersistence(format!(
                        "history preimage retention failed: {error:?}"
                    )))
            },
        )
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
            if slot
                .as_ref()
                .is_some_and(|writer| block.round < writer.retention_floor_round())
            {
                return Ok(());
            }
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

    pub fn set_proposer_mode(&mut self, enabled: bool) {
        self.proposer_mode = enabled;
        info!(proposer_mode = enabled, "Propose-Verify mode updated");
    }

    fn build_validator_set(
        validator_address: Hash256,
        stake: u64,
        peer_validators: &[(Hash256, u64)],
    ) -> (ValidatorSet, StakeTier) {
        let tier = StakeTier::from_stake(stake).unwrap_or(StakeTier::Spark);

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
        let validator_set = ValidatorSet::new(validators, 1);
        (validator_set, tier)
    }

    fn build_vrf_selector(
        validator_address: Hash256,
        stake: u64,
        peer_validators: &[(Hash256, u64)],
    ) -> Option<ProposerSelector> {
        use crate::vrf::ValidatorInfo;

        let mut vrf_validators = vec![ValidatorInfo {
            public_key: validator_address.0,
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

    fn adopt_absence_certificate(
        &self,
        state: &StateDB,
        certificate: &arc_consensus::view_change::SkipCertificate,
        tracker: &mut Option<arc_consensus::view_change::SkipTracker>,
        outbound: Option<&tokio::sync::mpsc::Sender<OutboundMessage>>,
    ) -> bool {
        let Some(domain) = self.engine.certificate_domain() else {
            return false;
        };
        let frozen = self.engine.frozen_validator_set();
        if certificate.verify(&domain, &frozen).is_err() {
            return false;
        }
        drop(frozen);
        if self
            .engine
            .has_skip_certificate(certificate.round, &certificate.absentee)
        {
            return true;
        }

        if self.engine.requires_full_round_participation() {
            let Some(writer_slot) = &self.recovery_dag_writer else {
                return false;
            };
            let (floor, ceiling) = {
                let slot = writer_slot.lock();
                let Some(writer) = slot.as_ref() else {
                    return false;
                };
                (
                    writer.retention_floor_round(),
                    writer.retention_ceiling_round(),
                )
            };
            if certificate.round < floor || certificate.round > ceiling {
                return false;
            }
            let Ok(payload) = bincode::serialize(certificate) else {
                return false;
            };
            let record = RetainedDagRecord::absence_certificate(
                certificate.round,
                arc_crypto::hash_bytes(&payload),
                payload,
            );
            let mut slot = writer_slot.lock();
            let Some(mut writer) = slot.take() else {
                return false;
            };
            if let Some(rollover) = &self.recovery_dag_rollover {
                writer = match rollover.prepare_append(
                    state,
                    &self.engine,
                    writer,
                    std::slice::from_ref(&record),
                ) {
                    Ok(writer) => writer,
                    Err(error) => {
                        error!(%error, round = certificate.round, "Could not prepare durable absence-certificate append");
                        return false;
                    }
                };
            }
            if certificate.round < writer.retention_floor_round()
                || certificate.round > writer.retention_ceiling_round()
            {
                *slot = Some(writer);
                return false;
            }
            if let Err(error) = writer.append_batch(&[record], ActiveDurability::Fsync) {
                error!(%error, round = certificate.round, "Could not fsync durable absence certificate");
                *slot = Some(writer);
                return false;
            }
            *slot = Some(writer);
        }

        if let Some(tracker) = tracker.as_mut() {
            tracker.adopt_certificate(certificate);
            if !self.persist_signing_record(tracker.record()) && self.signing_record_path.is_some()
            {
                return false;
            }
        }
        if self
            .engine
            .register_skip_certificate(certificate.clone())
            .is_err()
        {
            return false;
        }
        if let Some(tx_chan) = outbound {
            let _ = tx_chan.try_send(OutboundMessage::BroadcastAbsenceCertificate(
                certificate.clone(),
            ));
        }
        true
    }

    pub fn canonical_height_for_decision(&self, state: &StateDB, decision: Hash256) -> Option<u64> {
        self.ensure_decision_index(state);
        self.applied_decisions
            .get(&decision)
            .map(|entry| *entry.value())
    }

    fn decision_index_span(&self) -> u64 {
        self.engine
            .retained_rounds()
            .saturating_add(DECISION_INDEX_MARGIN_BLOCKS)
    }

    fn ensure_decision_index(&self, state: &StateDB) {
        let top = state.height();
        let span = self.decision_index_span();
        let floor = top.saturating_sub(span);
        let indexed_through = self
            .decisions_indexed_through
            .load(std::sync::atomic::Ordering::Acquire);
        if indexed_through >= top && indexed_through > 0 {
            return;
        }
        let start = if indexed_through == 0 {
            floor.saturating_add(1)
        } else {
            indexed_through.saturating_add(1)
        };
        let mut added = 0usize;
        for height in start..=top {
            if let Some(block) = state.get_block(height) {
                self.applied_decisions
                    .insert(block.header.proof_hash, height);
                added += 1;
            }
        }
        if floor > 0 {
            self.applied_decisions.retain(|_, height| *height > floor);
        }
        self.decisions_indexed_through
            .store(top, std::sync::atomic::Ordering::Release);
        if added > 0 {
            debug!(
                added,
                top,
                span,
                size = self.applied_decisions.len(),
                "Indexed canonical blocks by the DAG decision that produced them"
            );
        }
    }

    pub fn load_signing_record(&self) -> arc_consensus::view_change::ConsensusSigningRecord {
        let Some(path) = self.signing_record_path.as_ref() else {
            return Default::default();
        };
        match std::fs::read(path) {
            Ok(bytes) => match arc_consensus::view_change::ConsensusSigningRecord::decode(&bytes) {
                Ok(record) => {
                    info!(
                        path = %path.display(),
                        absences = record.skipped_rounds.len(),
                        finality = record.finality_votes.len(),
                        "Loaded consensus anti-equivocation record"
                    );
                    record
                }
                Err(error) => panic!(
                    "consensus signing record at {} is unreadable ({error}); refusing to \
                         sign without knowing what this key already promised",
                    path.display()
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(error) => panic!(
                "consensus signing record at {} could not be read ({error})",
                path.display()
            ),
        }
    }

    pub fn persist_signing_record(
        &self,
        record: &arc_consensus::view_change::ConsensusSigningRecord,
    ) -> bool {
        let Some(path) = self.signing_record_path.as_ref() else {
            return false;
        };
        let persist_started = std::time::Instant::now();
        crate::consensus_diagnostics::bump(
            &crate::consensus_diagnostics::DIAG.signing_record_persists,
        );
        let bytes = record.encode();
        let temporary = path.with_extension("tmp");
        let write = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            if let Some(parent) = path.parent() {
                let dir = std::fs::File::open(parent)?;
                dir.sync_all()?;
            }
            Ok(())
        })();
        crate::consensus_diagnostics::add_elapsed(
            &crate::consensus_diagnostics::DIAG.signing_record_persist_us,
            persist_started,
        );
        match write {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(
                    path = %path.display(),
                    %error,
                    "Could not persist the consensus signing record; withholding the signature"
                );
                false
            }
        }
    }

    pub fn is_multi_validator(&self) -> bool {
        self.engine.validator_set().len() > 1
    }

    pub async fn run_consensus_loop(
        &self,
        state: Arc<StateDB>,
        mempool: Arc<Mempool>,
        inbound_rx: Option<mpsc::Receiver<InboundMessage>>,
        outbound_tx: Option<mpsc::Sender<OutboundMessage>>,
        benchmark_pool: Option<Arc<crate::benchmark::BenchmarkPool>>,
    ) {
        self.run_consensus_loop_inner(
            state,
            mempool,
            inbound_rx,
            outbound_tx,
            benchmark_pool,
            None,
        )
        .await;
    }

    pub async fn run_consensus_loop_with_shutdown(
        &self,
        state: Arc<StateDB>,
        mempool: Arc<Mempool>,
        inbound_rx: Option<mpsc::Receiver<InboundMessage>>,
        outbound_tx: Option<mpsc::Sender<OutboundMessage>>,
        benchmark_pool: Option<Arc<crate::benchmark::BenchmarkPool>>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        self.run_consensus_loop_inner(
            state,
            mempool,
            inbound_rx,
            outbound_tx,
            benchmark_pool,
            Some(shutdown),
        )
        .await;
    }

    async fn run_consensus_loop_inner(
        &self,
        state: Arc<StateDB>,
        mempool: Arc<Mempool>,
        mut inbound_rx: Option<mpsc::Receiver<InboundMessage>>,
        outbound_tx: Option<mpsc::Sender<OutboundMessage>>,
        benchmark_pool: Option<Arc<crate::benchmark::BenchmarkPool>>,
        mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
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

        let mut skip_tracker: Option<arc_consensus::view_change::SkipTracker> = None;
        let mut skip_vote_collector = arc_consensus::view_change::SkipVoteCollector::new();
        let mut durable_skip_decisions = std::collections::HashSet::<(u64, Hash256)>::new();
        let mut finality_vote_collector = arc_consensus::view_change::FinalityVoteCollector::new();
        let mut finality_signed_heights: std::collections::HashSet<u64> =
            std::collections::HashSet::new();

        let absence_clock = std::time::Instant::now();
        let mut bootstrap_watermark: Option<u64> = None;
        let mut bootstrap_checked_at: Option<Instant> = None;
        let mut pending_blocks = arc_consensus::pending::PendingBlocks::new();
        let mut replay: std::collections::VecDeque<InboundMessage> =
            std::collections::VecDeque::new();
        let mut parked_inbound: std::collections::VecDeque<InboundMessage> =
            std::collections::VecDeque::new();
        let mut outbound_backpressure = OutboundBackpressure::default();
        let mut targeted_fetch_at: std::collections::HashMap<Hash256, Instant> =
            std::collections::HashMap::new();
        let mut unproductive_bootstrap_requests: u32 = 0;
        let mut last_bootstrap_request: Option<u64> = None;
        let mut iteration_started: Option<Instant> = None;
        let mut gauges_published_at: Option<Instant> = None;
        let mut requeued_since: std::collections::HashMap<[u8; 32], Instant> =
            std::collections::HashMap::new();
        let mut commit_backlog: Vec<arc_consensus::DagBlock> = Vec::new();
        let mut commit_stall_fetch_at: Option<Instant> = None;
        let mut vote_refusal_logged_at: Option<Instant> = None;
        let mut dag_wal_checkpoints: std::collections::VecDeque<(u64, u64)> =
            std::collections::VecDeque::new();
        let mut checkpoint_requested_at: Option<Instant> = None;
        let mut checkpoint_requested_from: std::collections::HashSet<Hash256> =
            std::collections::HashSet::new();
        let mut checkpoint_served_at: std::collections::HashMap<Hash256, Instant> =
            std::collections::HashMap::new();
        let mut absence_gossiped_at: std::collections::HashMap<(u64, Hash256), std::time::Instant> =
            std::collections::HashMap::new();
        let mut last_round_seen = self.engine.current_round();
        let mut last_round_change =
            if self.engine.dag_is_empty() && self.engine.last_committed_round() > 0 {
                std::time::Instant::now()
                    .checked_sub(HISTORY_STUCK_AFTER)
                    .unwrap_or_else(std::time::Instant::now)
            } else {
                std::time::Instant::now()
            };
        let mut history_clock = std::time::Instant::now()
            .checked_sub(HISTORY_REQUEST_INTERVAL)
            .unwrap_or_else(std::time::Instant::now);

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

        let pending_txs: DashMap<[u8; 32], Transaction> = DashMap::new();
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

        let recovered_local_round = match local_proposal_with_preimages(
            &self.engine,
            self.validator_address,
            recovered_round,
            &pending_txs,
            Some(&state),
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

        let mut connected_validators =
            std::collections::HashMap::<Hash256, PeerConnectionGeneration>::new();

        'consensus_loop: loop {
            let tick = if self.is_multi_validator() { 50 } else { 1 };
            let tick_delay = tokio::time::Duration::from_millis(tick);
            if let Some(started) = iteration_started.take() {
                let busy = started.elapsed().saturating_sub(tick_delay);
                let busy_us = busy.as_micros() as u64;
                crate::consensus_diagnostics::DIAG
                    .loop_busy_us
                    .fetch_add(busy_us, std::sync::atomic::Ordering::Relaxed);
                crate::consensus_diagnostics::DIAG
                    .loop_max_iteration_us
                    .fetch_max(busy_us, std::sync::atomic::Ordering::Relaxed);
                if busy >= std::time::Duration::from_secs(1) {
                    crate::consensus_diagnostics::bump(
                        &crate::consensus_diagnostics::DIAG.loop_slow_iterations,
                    );
                }
            }
            if gauges_published_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                gauges_published_at = Some(Instant::now());
                use crate::consensus_diagnostics::{DIAG, set};
                set(&DIAG.gauge_pending_txs, pending_txs.len());
                set(
                    &DIAG.gauge_pending_tx_latest_round,
                    pending_tx_latest_round.len(),
                );
                set(&DIAG.gauge_replay_queue, replay.len());
                set(&DIAG.gauge_targeted_fetch, targeted_fetch_at.len());
                set(&DIAG.gauge_commit_backlog, commit_backlog.len());
                set(&DIAG.gauge_dag_wal_checkpoints, dag_wal_checkpoints.len());
                set(&DIAG.gauge_absence_gossiped, absence_gossiped_at.len());
                set(
                    &DIAG.gauge_finality_signed_heights,
                    finality_signed_heights.len(),
                );
                set(&DIAG.gauge_skip_vote_slots, skip_vote_collector.len());
                set(
                    &DIAG.gauge_finality_vote_slots,
                    finality_vote_collector.len(),
                );
                if let Some(tracker) = skip_tracker.as_ref() {
                    let (observations, refused, skipped, finality) = tracker.sizes();
                    set(&DIAG.gauge_skip_observations, observations);
                    set(&DIAG.gauge_skip_refused, refused);
                    set(&DIAG.gauge_record_skipped_rounds, skipped);
                    set(&DIAG.gauge_record_finality_votes, finality);
                }
            }
            let shutdown_before_tick = if let Some(receiver) = shutdown.as_mut() {
                if *receiver.borrow() {
                    true
                } else {
                    tokio::select! {
                        biased;
                        changed = receiver.changed() => {
                            changed.is_err() || *receiver.borrow_and_update()
                        }
                        _ = tokio::time::sleep(tick_delay) => false,
                    }
                }
            } else {
                tokio::time::sleep(tick_delay).await;
                false
            };
            iteration_started = Some(
                Instant::now()
                    .checked_sub(tick_delay)
                    .unwrap_or_else(Instant::now),
            );
            if shutdown_before_tick {
                info!(
                    round = self.engine.current_round(),
                    committed_round = self.engine.last_committed_round(),
                    "Consensus loop stopped at the lifecycle barrier"
                );
                break 'consensus_loop;
            }

            if let Some(tx) = outbound_tx.as_ref() {
                match outbound_backpressure.observe(tx.capacity() == 0, Instant::now()) {
                    OutboundBackpressureVerdict::Continue => {}
                    OutboundBackpressureVerdict::Warn { stalled } => warn!(
                        stalled_secs = stalled.as_secs(),
                        queue = tx.max_capacity(),
                        parked_inbound = parked_inbound.len(),
                        round = self.engine.current_round(),
                        "Outbound P2P queue has stayed full; the transport is not taking messages"
                    ),
                    OutboundBackpressureVerdict::GiveUp { stalled } => {
                        error!(
                            stalled_secs = stalled.as_secs(),
                            queue = tx.max_capacity(),
                            round = self.engine.current_round(),
                            committed_round = self.engine.last_committed_round(),
                            "Outbound P2P queue full for too long; stopping consensus so the node exits and restarts"
                        );
                        return;
                    }
                }
            }

            if self.engine.requires_full_round_participation()
                && !recovery_reconnect_replay.is_empty()
            {
                match try_drain_recovery_reconnect_replay(
                    &self.engine,
                    self.validator_address,
                    &mut recovery_reconnect_replay,
                    &pending_txs,
                    outbound_tx.as_ref(),
                    Some(&state),
                ) {
                    Ok(RecoveryReplayDrain::Complete) => {
                        complete_recovery_rebroadcast(
                            &mut recovery_rebroadcast_pending,
                            &mut next_recovery_rebroadcast,
                        );
                    }
                    Ok(RecoveryReplayDrain::Backpressured { round }) => {
                        let parked = park_inbound(
                            inbound_rx.as_mut(),
                            &mut parked_inbound,
                            MAX_PARKED_INBOUND,
                        );
                        debug!(
                            round,
                            parked, "Recovery DAG reconnect replay remains backpressured"
                        );
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

            let mut phase_mark = Instant::now();
            let mut inbound_absence_votes: Vec<(Hash256, arc_consensus::view_change::SkipVote)> =
                Vec::new();
            let mut inbound_absence_certificates: Vec<(
                Hash256,
                arc_consensus::view_change::SkipCertificate,
            )> = Vec::new();
            let mut inbound_finality_votes: Vec<(
                Hash256,
                arc_consensus::view_change::FinalityVote,
            )> = Vec::new();
            let mut inbound_finality_certificates: Vec<(
                Hash256,
                arc_consensus::view_change::FinalityCertificate,
            )> = Vec::new();
            let mut inbound_history_requests: Vec<(Hash256, u64, u64)> = Vec::new();
            let mut inbound_checkpoint_requests: Vec<(Hash256, u64)> = Vec::new();
            let mut inbound_checkpoints: Vec<(
                Hash256,
                Box<arc_consensus::view_change::CheckpointEnvelope>,
                Vec<u8>,
            )> = Vec::new();
            let mut history_requests: Vec<(Hash256, u64)> = Vec::new();
            let mut history_requests_broadcast: Option<u64> = None;
            let mut inbound_history: Vec<(
                Hash256,
                Vec<arc_consensus::DagBlock>,
                Vec<arc_types::Transaction>,
            )> = Vec::new();
            {
                let expired = pending_blocks.expire(Instant::now());
                if expired > 0 {
                    crate::consensus_diagnostics::DIAG
                        .pending_blocks_expired
                        .fetch_add(expired as u64, std::sync::atomic::Ordering::Relaxed);
                }
                crate::consensus_diagnostics::DIAG.pending_blocks_now.store(
                    pending_blocks.len() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            if let Some(ref mut rx) = inbound_rx {
                while let Some(msg) = next_inbound(&mut replay, &mut parked_inbound, rx) {
                    let _inbound_timer =
                        crate::consensus_diagnostics::InboundTimer::start(match &msg {
                            InboundMessage::DagBlockWithTxs { .. } => 0,
                            InboundMessage::Transactions { .. } => 1,
                            InboundMessage::ConsensusFinalityVote { .. } => 2,
                            InboundMessage::ConsensusFinalityCertificate { .. } => 3,
                            InboundMessage::ConsensusAbsenceVote { .. } => 4,
                            InboundMessage::ConsensusAbsenceCertificate { .. } => 5,
                            InboundMessage::DagHistoryRequest { .. } => 6,
                            InboundMessage::DagHistoryResponse { .. } => 7,
                            InboundMessage::HeartbeatWithRound { .. } => 8,
                            InboundMessage::PeerConnected { .. } => 9,
                            InboundMessage::PeerDisconnected { .. } => 10,
                            _ => 11,
                        });
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
                                queue_recovery_reconnect_replay(
                                    &self.engine,
                                    self.validator_address,
                                    &mut recovery_reconnect_replay,
                                );
                                match try_drain_recovery_reconnect_replay(
                                    &self.engine,
                                    self.validator_address,
                                    &mut recovery_reconnect_replay,
                                    &pending_txs,
                                    outbound_tx.as_ref(),
                                    Some(&state),
                                ) {
                                    Ok(RecoveryReplayDrain::Complete) => {
                                        complete_recovery_rebroadcast(
                                            &mut recovery_rebroadcast_pending,
                                            &mut next_recovery_rebroadcast,
                                        );
                                    }
                                    Ok(RecoveryReplayDrain::Backpressured { round }) => {
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
                            let verified = match verify_peer_dag_availability(
                                &state,
                                &block.transactions,
                                &transactions,
                            ) {
                                Ok(verified) => verified,
                                Err(error) => {
                                    warn!(
                                        author = %block.author,
                                        round = block.round,
                                        error = %error,
                                        "Rejected DAG block with invalid transaction availability"
                                    );
                                    continue;
                                }
                            };
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
                            crate::consensus_diagnostics::bump(
                                &crate::consensus_diagnostics::DIAG.live_blocks_received,
                            );
                            let validate_started = std::time::Instant::now();
                            let received = self.engine.receive_block(&block);
                            crate::consensus_diagnostics::add_elapsed(
                                &crate::consensus_diagnostics::DIAG.live_block_validate_us,
                                validate_started,
                            );
                            let received = match received {
                                Err(
                                    error @ (arc_consensus::ConsensusError::MissingParents {
                                        ..
                                    }
                                    | arc_consensus::ConsensusError::RoundTooFarAhead {
                                        ..
                                    }),
                                ) => {
                                    crate::consensus_diagnostics::classify_live_rejection(
                                        &error.to_string(),
                                    );
                                    match arc_consensus::pending::hold_if_early(
                                        &self.engine,
                                        &mut pending_blocks,
                                        &block,
                                        &verified,
                                        error,
                                        Instant::now(),
                                    ) {
                                        arc_consensus::pending::Offered::Held => {
                                            crate::consensus_diagnostics::bump(
                                                &crate::consensus_diagnostics::DIAG
                                                    .pending_blocks_held,
                                            );
                                            let due =
                                                targeted_fetch_at.get(&block.author).is_none_or(
                                                    |at| at.elapsed() >= TARGETED_FETCH_INTERVAL,
                                                );
                                            if due && let Some(ref tx_chan) = outbound_tx {
                                                targeted_fetch_at
                                                    .insert(block.author, Instant::now());
                                                crate::consensus_diagnostics::bump(
                                                    &crate::consensus_diagnostics::DIAG
                                                        .targeted_history_requests,
                                                );
                                                let sent = tx_chan.try_send(
                                                    OutboundMessage::SendDagHistoryRequest {
                                                        target: block.author,
                                                        from_round: self.engine.current_round(),
                                                        max_rounds: HISTORY_MAX_ROUNDS,
                                                    },
                                                );
                                                crate::consensus_diagnostics::note_send(&sent);
                                            }
                                            continue;
                                        }
                                        arc_consensus::pending::Offered::NotHeld(error)
                                        | arc_consensus::pending::Offered::Rejected(error) => {
                                            Err(error)
                                        }
                                        arc_consensus::pending::Offered::Accepted(..) => Ok(()),
                                    }
                                }
                                other => other,
                            };
                            match received {
                                Ok(()) => {
                                    crate::consensus_diagnostics::bump(
                                        &crate::consensus_diagnostics::DIAG.live_blocks_accepted,
                                    );
                                    let persist_started = std::time::Instant::now();
                                    let persisted =
                                        self.persist_dag_block(&state, &block, &verified);
                                    crate::consensus_diagnostics::add_elapsed(
                                        &crate::consensus_diagnostics::DIAG.dag_block_persist_us,
                                        persist_started,
                                    );
                                    if let Err(error) = persisted {
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
                                            Some(&state),
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
                                    if advanced {
                                        crate::consensus_diagnostics::bump(
                                            &crate::consensus_diagnostics::DIAG.rounds_advanced,
                                        );
                                    }
                                    let mut released = pending_blocks.release_on(&block.hash);
                                    released.extend(
                                        pending_blocks
                                            .release_up_to_round(self.engine.current_round()),
                                    );
                                    for (early, transactions) in released {
                                        crate::consensus_diagnostics::bump(
                                            &crate::consensus_diagnostics::DIAG
                                                .pending_blocks_released,
                                        );
                                        replay.push_back(InboundMessage::DagBlockWithTxs {
                                            block: early,
                                            transactions,
                                        });
                                    }
                                    if advanced || block.round == round_before {
                                        self.engine.reset_round_timer();
                                    }
                                    let peer_round = self.engine.current_round();
                                    if last_proposed_round != Some(peer_round) {}
                                }
                                Err(e) => {
                                    let message = e.to_string();
                                    if !message.contains("missing or wrong-round parents")
                                        && !message.contains("too far ahead")
                                    {
                                        crate::consensus_diagnostics::classify_live_rejection(
                                            &message,
                                        );
                                    }
                                    if message.contains("missing or wrong-round parents")
                                        || message
                                            .contains("duplicate block: already exists in DAG")
                                    {
                                        debug!(
                                            author = %block.author,
                                            round = block.round,
                                            "Rejected DAG block: {}",
                                            message
                                        );
                                    } else {
                                        warn!(
                                            author = %block.author,
                                            round = block.round,
                                            "Rejected DAG block: {}",
                                            message
                                        );
                                    }
                                }
                            }
                        }
                        InboundMessage::StateDiff {
                            source,
                            block_hash,
                            diff,
                            block_height,
                        } => {
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
                                if admit_gossiped_transaction(
                                    &state,
                                    &mempool,
                                    &self.native_request_admission,
                                    &tx_bytes,
                                ) {
                                    inserted += 1;
                                }
                            }
                            if inserted > 0 {
                                debug!(count = inserted, "Inserted gossiped txs into mempool");
                            }
                        }
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
                            warn!(
                                request_id = %request_id,
                                responder = %responder,
                                ms_per_token = ms_per_token,
                                "Ignored dormant legacy P2P inference response"
                            );
                        }
                        InboundMessage::HeartbeatWithRound {
                            peer,
                            dag_round,
                            committed_round,
                        } => {
                            let my_round = self.engine.current_round();
                            if dag_round > my_round.saturating_add(1) {
                                if history_clock.elapsed() < HISTORY_REQUEST_INTERVAL {
                                    crate::consensus_diagnostics::bump(
                                        &crate::consensus_diagnostics::DIAG
                                            .history_requests_throttled,
                                    );
                                }
                                history_requests.push((peer, my_round));
                            }
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
                        InboundMessage::RoundSyncRequest {
                            peer,
                            their_round,
                            their_committed,
                        } => {
                            let my_round = self.engine.current_round();
                            let my_committed = self.engine.last_committed_round();
                            let vs = self.engine.validator_set();
                            if let Some(ref tx) = outbound_tx {
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
                        InboundMessage::RoundSyncResponse {
                            current_round,
                            last_committed_round,
                        } => {
                            let my_round = self.engine.current_round();
                            if current_round > my_round.saturating_add(1) {
                                history_requests_broadcast = Some(my_round);
                            }
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
                        InboundMessage::ConsensusAbsenceVote { source, vote } => {
                            inbound_absence_votes.push((source, vote));
                        }
                        InboundMessage::ConsensusAbsenceCertificate {
                            source,
                            certificate,
                        } => {
                            inbound_absence_certificates.push((source, certificate));
                        }
                        InboundMessage::ConsensusFinalityVote { source, vote } => {
                            inbound_finality_votes.push((source, vote));
                        }
                        InboundMessage::ConsensusFinalityCertificate {
                            source,
                            certificate,
                        } => {
                            inbound_finality_certificates.push((source, certificate));
                        }
                        InboundMessage::DagHistoryRequest {
                            source,
                            from_round,
                            max_rounds,
                        } => {
                            inbound_history_requests.push((source, from_round, max_rounds));
                        }
                        InboundMessage::DagHistoryResponse {
                            source,
                            blocks,
                            transactions,
                        } => {
                            inbound_history.push((source, blocks, transactions));
                        }
                        InboundMessage::NativeInferenceVote {
                            source,
                            request_id,
                            tokens,
                            vote,
                        } => {
                            if let Some(relay) = self.native_vote_relay.as_ref() {
                                let output_hash = vote.output_hash;
                                let decision = crate::native_inference::StoredVote {
                                    request_id,
                                    output_hash,
                                    tokens,
                                    vote,
                                };
                                match relay.accept_peer_vote(&decision) {
                                    Ok(()) => crate::consensus_diagnostics::bump(
                                        &crate::consensus_diagnostics::DIAG.native_votes_accepted,
                                    ),
                                    Err(error) => {
                                        crate::consensus_diagnostics::bump(
                                            &crate::consensus_diagnostics::DIAG
                                                .native_votes_refused,
                                        );
                                        if vote_refusal_logged_at.is_none_or(|at: Instant| {
                                            at.elapsed() >= std::time::Duration::from_secs(2)
                                        }) {
                                            vote_refusal_logged_at = Some(Instant::now());
                                            warn!(%source, %request_id, ?error, "Native vote not counted");
                                        }
                                    }
                                }
                            }
                        }
                        InboundMessage::CheckpointRequest {
                            source,
                            needed_below_height,
                        } => {
                            inbound_checkpoint_requests.push((source, needed_below_height));
                        }
                        InboundMessage::CheckpointResponse {
                            source,
                            envelope,
                            payload,
                        } => {
                            inbound_checkpoints.push((source, envelope, payload));
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

            if let (Some(relay), Some(tx_chan)) =
                (self.native_vote_relay.as_ref(), outbound_tx.as_ref())
            {
                for decision in relay.take_outbound_votes(64) {
                    crate::consensus_diagnostics::bump(
                        &crate::consensus_diagnostics::DIAG.native_votes_gossiped,
                    );
                    let sent = tx_chan.try_send(OutboundMessage::BroadcastNativeInferenceVote {
                        request_id: decision.request_id,
                        tokens: decision.tokens,
                        vote: decision.vote,
                    });
                    crate::consensus_diagnostics::note_send(&sent);
                }
            }
            phase_mark = crate::consensus_diagnostics::phase(
                &crate::consensus_diagnostics::DIAG.phase_inbound_us,
                phase_mark,
            );
            for (source, vote) in inbound_absence_votes {
                let Some(domain) = self.engine.certificate_domain() else {
                    continue;
                };
                let set = self.engine.validator_set();
                match skip_vote_collector.add(vote, &domain, &set) {
                    Ok(Some(certificate)) => {
                        if self.adopt_absence_certificate(
                            &state,
                            &certificate,
                            &mut skip_tracker,
                            outbound_tx.as_ref(),
                        ) {
                            info!(
                                round = certificate.round,
                                absentee = %certificate.absentee,
                                "Assembled an absence certificate from peer votes"
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        debug!(%source, ?error, "Rejected an absence vote");
                    }
                }
            }
            for (source, certificate) in inbound_absence_certificates {
                if self
                    .engine
                    .has_skip_certificate(certificate.round, &certificate.absentee)
                {
                    continue;
                }
                if !self.adopt_absence_certificate(
                    &state,
                    &certificate,
                    &mut skip_tracker,
                    outbound_tx.as_ref(),
                ) {
                    debug!(%source, "Rejected a peer absence certificate");
                }
            }
            for (source, vote) in inbound_finality_votes {
                let Some(domain) = self.engine.certificate_domain() else {
                    continue;
                };
                let set = self.engine.validator_set();
                match finality_vote_collector.add(vote, &domain, &set) {
                    Ok(Some(certificate)) => {
                        let height = certificate.height;
                        match self.engine.register_finality_certificate(certificate.clone()) {
                            Ok(signing) => {
                                debug!(
                                    height,
                                    block = %certificate.block_hash,
                                    state_root = %certificate.state_root,
                                    signing_stake = signing,
                                    "Committed-block finality certificate assembled"
                                );
                                if let Some(local) =
                                    divergence(&state, height, certificate.block_hash)
                                {
                                    report_divergence(height, local, certificate.block_hash);
                                }
                                if let Some(ref tx_chan) = outbound_tx {
                                    let _ = tx_chan.try_send(
                                        OutboundMessage::BroadcastFinalityCertificate(certificate),
                                    );
                                }
                            }
                            Err(
                                conflict @ arc_consensus::view_change::CertificateError::ConflictingFinality { .. },
                            ) => {
                                error!(
                                    %conflict,
                                    "SAFETY VIOLATION: two quorum finality certificates at one \
                                     height name different blocks"
                                );
                            }
                            Err(error) => debug!(?error, "Rejected a finality certificate"),
                        }
                    }
                    Ok(None) => {}
                    Err(error) => debug!(%source, ?error, "Rejected a finality vote"),
                }
            }
            for (source, certificate) in inbound_finality_certificates {
                let (height, certified) = (certificate.height, certificate.block_hash);
                match self.engine.register_finality_certificate(certificate) {
                    Ok(_) => {
                        if let Some(local) = divergence(&state, height, certified) {
                            report_divergence(height, local, certified);
                        }
                    }
                    Err(
                        conflict @ arc_consensus::view_change::CertificateError::ConflictingFinality { .. },
                    ) => {
                        error!(
                            %source,
                            %conflict,
                            "SAFETY VIOLATION: a peer holds a quorum finality certificate for a \
                             different block at a height this node has already finalised"
                        );
                    }
                    Err(error) => debug!(%source, ?error, "Rejected a peer finality certificate"),
                }
            }

            if let Some(mark) = bootstrap_watermark
                && bootstrap_checked_at.is_none_or(|at| at.elapsed() >= Duration::from_millis(250))
            {
                bootstrap_checked_at = Some(Instant::now());
                let now_round = self.engine.current_round();
                let from = mark.max(self.engine.last_committed_round());
                let next = self
                    .engine
                    .first_missing_round(from, HISTORY_MAX_ROUNDS)
                    .min(now_round);
                if next >= now_round {
                    bootstrap_watermark = None;
                    self.engine.close_restart_base_round();
                    self.engine.set_dag_bootstrapping(false);
                    info!(round = next, "DAG bootstrap complete");
                    checkpoint_requested_at = None;
                    checkpoint_requested_from.clear();
                } else if next > mark {
                    bootstrap_watermark = Some(next);
                    checkpoint_requested_at = None;
                    checkpoint_requested_from.clear();
                }
            }

            {
                let now_round = self.engine.current_round();
                if now_round != last_round_seen {
                    last_round_seen = now_round;
                    last_round_change = std::time::Instant::now();
                } else if self.is_multi_validator()
                    && last_round_change.elapsed() >= HISTORY_STUCK_AFTER
                    && connected_validators.iter().any(|(_, g)| g.connected)
                {
                    let from_round = match bootstrap_watermark {
                        Some(mark) => {
                            let from = mark.max(self.engine.last_committed_round());
                            let next = self
                                .engine
                                .first_missing_round(from, HISTORY_MAX_ROUNDS)
                                .min(now_round);
                            bootstrap_watermark = Some(next);
                            if next > mark {
                                checkpoint_requested_at = None;
                                checkpoint_requested_from.clear();
                            }
                            if next >= now_round {
                                bootstrap_watermark = None;
                                self.engine.close_restart_base_round();
                                self.engine.set_dag_bootstrapping(false);
                                info!(round = next, "DAG bootstrap complete");
                                checkpoint_requested_at = None;
                                checkpoint_requested_from.clear();
                            }
                            next
                        }
                        None => {
                            let cursor = self.engine.last_committed_round();
                            if self.engine.dag_is_empty()
                                && cursor > 0
                                && self.engine.set_restart_base_round(cursor).is_ok()
                            {
                                bootstrap_watermark = Some(cursor);
                                self.engine.set_dag_bootstrapping(true);
                                info!(
                                    round = cursor,
                                    "DAG is empty after a restart; bootstrapping history from \
                                     this node's durable commit cursor"
                                );
                                cursor.min(now_round)
                            } else if self.engine.dag_is_empty() {
                                bootstrap_watermark = Some(0);
                                self.engine.set_dag_bootstrapping(true);
                                info!("DAG is empty; bootstrapping history from round 0");
                                0
                            } else {
                                self.engine.last_committed_round().min(now_round)
                            }
                        }
                    };
                    history_requests_broadcast = Some(from_round);
                }
            }

            if (!history_requests.is_empty() || history_requests_broadcast.is_some())
                && history_clock.elapsed() >= HISTORY_REQUEST_INTERVAL
            {
                history_clock = std::time::Instant::now();
                let mut asked: std::collections::HashSet<Hash256> =
                    std::collections::HashSet::new();
                for (peer, from_round) in history_requests.drain(..) {
                    if !asked.insert(peer) {
                        continue;
                    }
                    info!(%peer, from_round, "Requesting bounded DAG history");
                    if let Some(ref tx_chan) = outbound_tx {
                        crate::consensus_diagnostics::bump(
                            &crate::consensus_diagnostics::DIAG.history_requests_sent,
                        );
                        let sent = tx_chan.try_send(OutboundMessage::SendDagHistoryRequest {
                            target: peer,
                            from_round,
                            max_rounds: HISTORY_MAX_ROUNDS,
                        });
                        crate::consensus_diagnostics::note_send(&sent);
                    }
                }
                if let Some(from_round) = history_requests_broadcast {
                    if bootstrap_watermark.is_some() {
                        if last_bootstrap_request == Some(from_round) {
                            unproductive_bootstrap_requests =
                                unproductive_bootstrap_requests.saturating_add(1);
                        } else {
                            unproductive_bootstrap_requests = 0;
                        }
                        last_bootstrap_request = Some(from_round);
                    }
                    for (peer, generation) in connected_validators.iter() {
                        if !generation.connected || !asked.insert(*peer) {
                            continue;
                        }
                        info!(%peer, from_round, "Requesting bounded DAG history");
                        if let Some(ref tx_chan) = outbound_tx {
                            crate::consensus_diagnostics::bump(
                                &crate::consensus_diagnostics::DIAG.history_requests_sent,
                            );
                            let sent = tx_chan.try_send(OutboundMessage::SendDagHistoryRequest {
                                target: *peer,
                                from_round,
                                max_rounds: HISTORY_MAX_ROUNDS,
                            });
                            crate::consensus_diagnostics::note_send(&sent);
                        }
                    }
                }

                if unproductive_bootstrap_requests >= UNPRODUCTIVE_REQUESTS_BEFORE_CHECKPOINT
                    && checkpoint_requested_at
                        .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
                {
                    checkpoint_requested_at = Some(Instant::now());
                    checkpoint_requested_from.clear();
                    let needed_below_height = state.height();
                    warn!(
                        attempts = unproductive_bootstrap_requests,
                        needed_below_height,
                        "DAG history is not rescuing this node; requesting an authenticated \
                         checkpoint, which is a different and stronger trust boundary"
                    );
                    for (peer, generation) in connected_validators.iter() {
                        if !generation.connected {
                            continue;
                        }
                        if let Some(ref tx_chan) = outbound_tx
                            && tx_chan
                                .try_send(OutboundMessage::SendCheckpointRequest {
                                    target: *peer,
                                    needed_below_height,
                                })
                                .is_ok()
                        {
                            checkpoint_requested_from.insert(*peer);
                        }
                    }
                }
            }

            phase_mark = crate::consensus_diagnostics::phase(
                &crate::consensus_diagnostics::DIAG.phase_certificates_us,
                phase_mark,
            );
            for (source, from_round, max_rounds) in inbound_history_requests {
                let span = max_rounds.min(HISTORY_MAX_ROUNDS);
                let first = from_round.saturating_sub(HISTORY_PARENT_CONTEXT);
                let mut blocks = Vec::new();
                for round in first..first.saturating_add(span) {
                    for hash in self.engine.blocks_in_round(round) {
                        if let Some(block) = self.engine.get_block(&hash) {
                            blocks.push(block);
                        }
                    }
                    if blocks.len() >= HISTORY_MAX_BLOCKS {
                        break;
                    }
                }
                if blocks.is_empty() {
                    let oldest_retained = self
                        .engine
                        .last_committed_round()
                        .saturating_sub(self.engine.retained_rounds());
                    if from_round < oldest_retained {
                        crate::consensus_diagnostics::bump(
                            &crate::consensus_diagnostics::DIAG.history_requests_empty_pruned,
                        );
                        warn!(
                            %source,
                            from_round,
                            oldest_retained,
                            retained_rounds = self.engine.retained_rounds(),
                            "Cannot serve DAG history: the requested round was PRUNED here. \
                             The peer needs an authenticated checkpoint."
                        );
                    } else {
                        crate::consensus_diagnostics::bump(
                            &crate::consensus_diagnostics::DIAG.history_requests_empty_not_held,
                        );
                        debug!(
                            %source,
                            from_round,
                            local_round = self.engine.current_round(),
                            "Cannot serve DAG history: this node does not hold that round \
                             (not pruned - never received, or not yet reached)"
                        );
                    }
                    continue;
                }
                let wanted: std::collections::HashSet<[u8; 32]> = blocks
                    .iter()
                    .flat_map(|block| block.transactions.iter().map(|hash| hash.0))
                    .collect();
                let transactions = history_bodies(&pending_txs, &state, &wanted);
                crate::consensus_diagnostics::bump(
                    &crate::consensus_diagnostics::DIAG.history_requests_served,
                );
                crate::consensus_diagnostics::DIAG
                    .history_blocks_served
                    .fetch_add(blocks.len() as u64, std::sync::atomic::Ordering::Relaxed);
                info!(
                    %source,
                    from_round,
                    blocks = blocks.len(),
                    bodies = transactions.len(),
                    "Serving bounded DAG history"
                );
                if let Some(ref tx_chan) = outbound_tx {
                    let absence_certificates = history_absence_certificates(&self.engine, &blocks);
                    let sent = tx_chan.try_send(OutboundMessage::SendDagHistoryResponse {
                        target: source,
                        blocks,
                        transactions,
                        absence_certificates,
                    });
                    crate::consensus_diagnostics::note_send(&sent);
                }
            }
            let mut bootstrap_reject_logged = false;
            for (source, blocks, transactions) in inbound_history {
                let block_count = blocks.len();
                crate::consensus_diagnostics::bump(
                    &crate::consensus_diagnostics::DIAG.history_responses_received,
                );
                let round_before_import = self.engine.current_round();
                let blocks_before_import = self.engine.dag_block_count();
                let import_started = std::time::Instant::now();
                let imported = self.import_history_durably(
                    &state,
                    &blocks,
                    &transactions,
                    &pending_txs,
                    &pending_tx_latest_round,
                );
                crate::consensus_diagnostics::add_elapsed(
                    &crate::consensus_diagnostics::DIAG.history_import_us,
                    import_started,
                );
                if let Err(arc_consensus::ConsensusError::HistoryPersistence(error)) = &imported {
                    error!(%source, %error, "Fatal DAG history persistence failure before commit eligibility");
                    return;
                }
                if let Err(error) = &imported {
                    crate::consensus_diagnostics::classify_import_rejection(&error.to_string());
                }
                match imported {
                    Ok(reached) => {
                        crate::consensus_diagnostics::bump(
                            &crate::consensus_diagnostics::DIAG.history_imports_ok,
                        );
                        for (early, transactions) in pending_blocks.drain_all() {
                            crate::consensus_diagnostics::bump(
                                &crate::consensus_diagnostics::DIAG.pending_blocks_released,
                            );
                            replay.push_back(InboundMessage::DagBlockWithTxs {
                                block: early,
                                transactions,
                            });
                        }
                        crate::consensus_diagnostics::DIAG
                            .history_import_rounds_advanced
                            .fetch_add(
                                self.engine
                                    .current_round()
                                    .saturating_sub(round_before_import),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        crate::consensus_diagnostics::DIAG
                            .history_import_blocks_inserted
                            .fetch_add(
                                self.engine
                                    .dag_block_count()
                                    .saturating_sub(blocks_before_import)
                                    as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        info!(%source, reached, "Joined a running chain from authenticated history");
                    }
                    Err(error) => {
                        if bootstrap_watermark.is_some() && !bootstrap_reject_logged {
                            bootstrap_reject_logged = true;
                            warn!(
                                %source,
                                %error,
                                blocks = block_count,
                                "Bootstrap history rejected"
                            );
                        } else {
                            debug!(%source, %error, "History response not usable");
                        }
                    }
                }
            }

            phase_mark = crate::consensus_diagnostics::phase(
                &crate::consensus_diagnostics::DIAG.phase_history_us,
                phase_mark,
            );
            for (source, needed_below_height) in inbound_checkpoint_requests {
                if checkpoint_served_at
                    .get(&source)
                    .is_some_and(|at| at.elapsed() < CHECKPOINT_SERVE_INTERVAL)
                {
                    debug!(%source, "Not serving a checkpoint again so soon");
                    continue;
                }
                checkpoint_served_at.retain(|_, at| at.elapsed() < CHECKPOINT_SERVE_INTERVAL);
                checkpoint_served_at.insert(source, Instant::now());
                let Some(dir) = state.persistence_dir() else {
                    warn!(%source, "Cannot serve a checkpoint: this node has no durable state");
                    continue;
                };
                let snapshot = match arc_state::snapshot::load(&dir) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        warn!(%source, %error, "Cannot serve a checkpoint: no usable local snapshot");
                        continue;
                    }
                };
                let height = snapshot.manifest.identity.height;
                let Some(certificate) = self.engine.finality_certificate(height) else {
                    warn!(
                        %source, height,
                        "Cannot serve a checkpoint: no finality certificate at the snapshot's height"
                    );
                    continue;
                };
                if certificate.state_root != snapshot.manifest.identity.state_root {
                    warn!(
                        %source, height,
                        "Refusing to serve a checkpoint: the finality certificate and the local \
                         snapshot disagree about the state root"
                    );
                    continue;
                }
                let payload = match std::fs::read(dir.join(arc_state::snapshot::PAYLOAD_FILE)) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        warn!(%source, %error, "Cannot serve a checkpoint: payload unreadable");
                        continue;
                    }
                };
                let anchor = self
                    .engine
                    .consensus_domain()
                    .or_else(|| self.engine.certificate_domain())
                    .zip(state.get_block(height))
                    .and_then(|(domain, tip)| {
                        self.engine
                            .find_committed_anchor(&tip.header.proof_hash, &domain)
                    });
                let Some((anchor_hash, anchor_round)) = anchor else {
                    warn!(
                        %source, height,
                        "Cannot serve a checkpoint: the anchor behind its height is no longer in \
                         the retained DAG"
                    );
                    continue;
                };
                let envelope = arc_consensus::view_change::CheckpointEnvelope {
                    certificate,
                    snapshot: arc_consensus::view_change::SnapshotIdentity {
                        height,
                        state_root: snapshot.manifest.identity.state_root,
                        digest: snapshot.manifest.identity.digest,
                    },
                    anchor_hash,
                    anchor_round,
                };
                info!(
                    %source, height, needed_below_height, bytes = payload.len(),
                    "Serving an authenticated state checkpoint"
                );
                if let Some(ref tx_chan) = outbound_tx {
                    let _ = tx_chan.try_send(OutboundMessage::SendCheckpointResponse {
                        target: source,
                        envelope: Box::new(envelope),
                        payload,
                    });
                }
            }

            for (source, envelope, payload) in inbound_checkpoints {
                if checkpoint_requested_at.is_none() || !checkpoint_requested_from.remove(&source) {
                    warn!(%source, "Ignoring an unsolicited checkpoint; nothing was installed");
                    continue;
                }
                let Some(domain) = self.engine.certificate_domain() else {
                    warn!(%source, "Refusing a checkpoint: this node has no certificate domain");
                    continue;
                };
                let verified = {
                    let set = self.engine.frozen_validator_set();
                    envelope.verify_payload(&payload, &domain, &set)
                };
                if let Err(error) = verified {
                    warn!(
                        %source, %error,
                        "REFUSED an unauthenticated checkpoint; nothing was installed"
                    );
                    continue;
                }
                if state.height() >= envelope.snapshot.height {
                    debug!(
                        %source,
                        offered = envelope.snapshot.height,
                        local = state.height(),
                        "Ignoring a checkpoint this node is already past"
                    );
                    continue;
                }
                let Ok(decoded) = arc_state::snapshot::SnapshotPayload::decode_from_peer(&payload)
                else {
                    warn!(
                        %source,
                        "A certified checkpoint payload would not decode within bounds; nothing \
                         was installed"
                    );
                    continue;
                };
                let Some((tip_height, tip)) = decoded.blocks.last().cloned() else {
                    warn!(%source, "REFUSED a checkpoint that carries no tip block");
                    continue;
                };
                let Some(resume_domain) = self
                    .engine
                    .consensus_domain()
                    .or_else(|| self.engine.certificate_domain())
                else {
                    warn!(%source, "Cannot adopt a checkpoint without a decision domain");
                    continue;
                };
                let cursor = match envelope.verify_resume_point(&tip, &resume_domain) {
                    Ok(cursor) if tip_height == envelope.snapshot.height => cursor,
                    Ok(_) => {
                        warn!(%source, "REFUSED a checkpoint whose tip is not at its height");
                        continue;
                    }
                    Err(error) => {
                        warn!(%source, %error, "REFUSED a checkpoint's resume point; nothing was installed");
                        continue;
                    }
                };
                let adopted = match state.plan_checkpoint_adoption(&decoded, &tip) {
                    Ok(adopted) => adopted,
                    Err(error) => {
                        warn!(%source, %error, "REFUSED a checkpoint; nothing was installed");
                        continue;
                    }
                };
                if let Err(error) = self.engine.check_rebase(envelope.anchor_round) {
                    warn!(%source, %error, "REFUSED a checkpoint the engine cannot resume from");
                    continue;
                }

                if let Err(error) = state.rebase_onto_checkpoint(
                    &adopted,
                    &tip,
                    envelope.anchor_hash,
                    envelope.anchor_round,
                ) {
                    if state.height() < envelope.snapshot.height {
                        error!(%source, %error, "Could not adopt a verified checkpoint; nothing moved");
                        continue;
                    }
                    panic!(
                        "a checkpoint was written to the WAL but could not be completed ({error}); \
                         restart to recover from the durable record"
                    );
                }
                let persisted = match skip_tracker.as_mut() {
                    Some(tracker) => {
                        tracker.note_durable_commit_round(envelope.anchor_round);
                        self.persist_signing_record(tracker.record())
                    }
                    None => {
                        let mut record = self.load_signing_record();
                        if record
                            .last_applied_round
                            .is_none_or(|applied| envelope.anchor_round > applied)
                        {
                            record.last_applied_round = Some(envelope.anchor_round);
                        }
                        self.persist_signing_record(&record)
                    }
                };
                if !persisted && self.signing_record_path.is_some() {
                    panic!(
                        "an adopted checkpoint's commit record could not be persisted; restart \
                         to recover from the durable state record"
                    );
                }
                if let Err(error) = self.engine.rebase_to(envelope.anchor_round) {
                    panic!(
                        "the engine refused a rebase it had accepted ({error}); restart to recover \
                         from the durable state record"
                    );
                }
                commit_backlog.clear();
                pending_blocks = arc_consensus::pending::PendingBlocks::new();
                bootstrap_watermark = Some(cursor);
                self.engine.set_dag_bootstrapping(true);
                checkpoint_requested_at = None;
                checkpoint_requested_from.clear();
                unproductive_bootstrap_requests = 0;
                last_bootstrap_request = None;
                last_round_change = std::time::Instant::now()
                    .checked_sub(HISTORY_STUCK_AFTER)
                    .unwrap_or_else(std::time::Instant::now);
                if let Err(error) = state.publish_durable_snapshot() {
                    warn!(%error, "Adopted a checkpoint; publishing a fresh snapshot failed");
                }
                info!(
                    %source,
                    height = envelope.snapshot.height,
                    anchor_round = envelope.anchor_round,
                    resume_round = cursor,
                    "Adopted an authenticated checkpoint; rebuilding the DAG from its resume round"
                );
            }

            phase_mark = crate::consensus_diagnostics::phase(
                &crate::consensus_diagnostics::DIAG.phase_checkpoint_us,
                phase_mark,
            );
            if self.is_multi_validator()
                && let Some(domain) = self.engine.certificate_domain()
                && let Some(keypair) = self.engine.local_keypair()
            {
                if skip_tracker.is_none() {
                    let record = self.load_signing_record();
                    durable_skip_decisions.extend(record.skipped_rounds.iter().flat_map(
                        |(round, members)| members.keys().map(|member| (*round, *member)),
                    ));
                    skip_tracker = Some(
                        arc_consensus::view_change::SkipTracker::new(
                            domain,
                            self.engine.frozen_validator_set_hash(),
                            arc_consensus::view_change::DEFAULT_SKIP_GRACE_MS,
                            record,
                        )
                        .with_recent_absence_grace(
                            arc_consensus::view_change::RECENT_ABSENCE_GRACE_MS,
                        ),
                    );
                }
                let tracker = skip_tracker.as_mut().expect("initialised just above");
                let vs = self.engine.validator_set();
                let cursor = self.engine.last_committed_round();
                let now_ms = absence_clock.elapsed().as_millis() as u64;
                let scan_round = self.engine.current_round();
                let mut locally_assembled_absence_certificates = Vec::new();
                let window_start = cursor.max(scan_round.saturating_sub(ABSENCE_SCAN_ROUNDS));
                for round in window_start..=scan_round {
                    let hashes = self.engine.blocks_in_round(round);
                    let mut seen = std::collections::HashSet::new();
                    let mut stake = 0u64;
                    for hash in &hashes {
                        if let Some(block) = self.engine.get_block(hash)
                            && seen.insert(block.author)
                            && let Some(validator) = vs.get_validator(&block.author)
                        {
                            stake = stake.saturating_add(validator.stake);
                        }
                    }
                    if stake < vs.quorum {
                        continue;
                    }
                    for validator in vs.validators.iter().filter(|v| v.stake > 0) {
                        let member = validator.address;
                        if self.engine.has_skip_certificate(round, &member) {
                            continue;
                        }
                        let present = seen.contains(&member);
                        tracker.observe(
                            round,
                            &member,
                            arc_consensus::view_change::AbsenceReason::NoBlock,
                            stake,
                            !present,
                            vs.quorum,
                            now_ms,
                        );
                        let decision_durable = durable_skip_decisions.contains(&(round, member));
                        let gossip_key = (round, member);
                        if decision_durable
                            && absence_gossiped_at.get(&gossip_key).is_some_and(
                                |at: &std::time::Instant| at.elapsed() < ABSENCE_REGOSSIP_INTERVAL,
                            )
                        {
                            continue;
                        }
                        if let Ok(vote) = tracker.sign_if_permitted(
                            round,
                            member,
                            arc_consensus::view_change::AbsenceReason::NoBlock,
                            cursor,
                            vs.quorum,
                            now_ms,
                            keypair,
                        ) {
                            let mut persisted_now = false;
                            let collection = persist_then_collect_absence_vote(
                                vote.clone(),
                                decision_durable,
                                tracker.record(),
                                |record| {
                                    persisted_now = self.persist_signing_record(record);
                                    persisted_now
                                },
                                &mut skip_vote_collector,
                                &domain,
                                &vs,
                            );
                            if persisted_now {
                                durable_skip_decisions.insert((round, member));
                            }
                            match collection {
                                Ok(DurableAbsenceVoteCollection::Withheld) => {
                                    tracing::error!(
                                        round,
                                        %member,
                                        "Withholding an absence vote: its decision is not durable"
                                    );
                                    continue;
                                }
                                Ok(DurableAbsenceVoteCollection::Added(Some(certificate))) => {
                                    locally_assembled_absence_certificates.push(certificate);
                                }
                                Ok(DurableAbsenceVoteCollection::Added(None)) => {}
                                Err(error) => {
                                    tracing::error!(
                                        round,
                                        %member,
                                        %error,
                                        "Could not collect the locally signed absence vote"
                                    );
                                }
                            }
                            if decision_durable {
                                debug!(round, %member, "Re-gossiping an absence attestation");
                            } else {
                                info!(
                                    round,
                                    %member,
                                    observed_round_stake = stake,
                                    "Signed an absence attestation (no block)"
                                );
                            }
                            absence_gossiped_at.insert(gossip_key, std::time::Instant::now());
                            if let Some(ref tx_chan) = outbound_tx {
                                let _ =
                                    tx_chan.try_send(OutboundMessage::BroadcastAbsenceVote(vote));
                            }
                        }
                    }
                }
                for certificate in locally_assembled_absence_certificates {
                    if self.adopt_absence_certificate(
                        &state,
                        &certificate,
                        &mut skip_tracker,
                        outbound_tx.as_ref(),
                    ) {
                        info!(
                            round = certificate.round,
                            absentee = %certificate.absentee,
                            "Assembled an absence certificate from peer votes"
                        );
                    }
                }
                if let Some(tracker) = skip_tracker.as_mut() {
                    tracker.prune_observations_below(cursor.saturating_sub(ABSENCE_SCAN_ROUNDS));
                }
                let absence_floor = cursor.saturating_sub(ABSENCE_SCAN_ROUNDS);
                durable_skip_decisions.retain(|(round, _)| *round >= absence_floor);
                skip_vote_collector.prune_below(absence_floor);
            }

            if self.engine.requires_full_round_participation()
                && !recovery_reconnect_replay.is_empty()
            {
                continue;
            }

            if self.engine.requires_full_round_participation()
                && queue_due_recovery_replay(
                    &self.engine,
                    self.validator_address,
                    &mut recovery_reconnect_replay,
                    &mut recovery_rebroadcast_pending,
                    next_recovery_rebroadcast,
                )
            {
                match try_drain_recovery_reconnect_replay(
                    &self.engine,
                    self.validator_address,
                    &mut recovery_reconnect_replay,
                    &pending_txs,
                    outbound_tx.as_ref(),
                    Some(&state),
                ) {
                    Ok(RecoveryReplayDrain::Complete) => {
                        complete_recovery_rebroadcast(
                            &mut recovery_rebroadcast_pending,
                            &mut next_recovery_rebroadcast,
                        );
                    }
                    Ok(RecoveryReplayDrain::Backpressured { round }) => {
                        debug!(round, "Recovery DAG re-broadcast channel is full; retrying");
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(%error, "Fatal local DAG re-broadcast/preimage invariant failure");
                        return;
                    }
                }
            }

            let multi_validator = self.is_multi_validator();
            let current_round = self.engine.current_round();
            let already_proposed = last_proposed_round == Some(current_round);
            let has_connected_quorum = if multi_validator {
                let vs = self.engine.validator_set();
                let mut connected = std::collections::HashSet::new();
                connected.insert(self.validator_address);
                for (address, generation) in connected_validators.iter() {
                    if generation.connected && vs.is_validator(address) {
                        connected.insert(*address);
                    }
                }
                let genesis_barrier = self.require_full_committee_at_genesis && current_round == 0;
                connected_validator_quorum(&vs, &connected, genesis_barrier)
            } else {
                true
            };

            phase_mark = crate::consensus_diagnostics::phase(
                &crate::consensus_diagnostics::DIAG.phase_absence_us,
                phase_mark,
            );
            if self.benchmark
                && multi_validator
                && mempool.len() < 5_000
                && let Some(ref pool) = benchmark_pool
            {
                let signed_txs = pool.drain(200);
                let fed = signed_txs.len();
                for tx in signed_txs {
                    self.native_request_admission.insert(&mempool, tx);
                }
                if fed > 0 && mempool.len() % 10_000 < 2_000 {
                    info!(
                        "Benchmark pre-feed: {} txs (mempool: {})",
                        fed,
                        mempool.len()
                    );
                }
            }

            let has_quorum_parents = proposal_parent_quorum(&self.engine, current_round);

            let vrf_approved = if multi_validator {
                true
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
                true
            };

            let allow_propose = if multi_validator {
                has_connected_quorum
            } else {
                vrf_approved
            };
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
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let _ = self.engine.propose_block(vec![], timestamp);
                    let _ = self.engine.advance_round();
                } else {
                    let drain_limit = if self.benchmark && multi_validator {
                        1_000
                    } else if self.benchmark {
                        50_000
                    } else {
                        500
                    };
                    let mempool_len_pre = mempool.len();
                    let mut transactions = mempool.drain(drain_limit);
                    self.native_request_admission
                        .retain_for_proposal(&mut transactions);
                    let stale = retain_unreceipted(&state, &mut transactions);
                    if state.native_inference_context().is_some() && !transactions.is_empty() {
                        transactions = select_native_transactions_and_requeue(
                            &state,
                            &transactions,
                            &mempool,
                            &self.native_request_admission,
                            &mut requeued_since,
                        );
                    }
                    if stale > 0 {
                        crate::consensus_diagnostics::DIAG
                            .stale_transactions_dropped
                            .fetch_add(stale as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    if state.active_protocol_version().major == 3 {
                        transactions.retain(|transaction| {
                            !state.receipts.contains_key(&transaction.hash.0)
                                && state.validate_v3_transaction_admission(transaction).is_ok()
                        });
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
                            for transaction in deferred {
                                self.native_request_admission.insert(&mempool, transaction);
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
                            self.native_request_admission.insert(&mempool, transaction);
                        }
                    }
                    if !transactions.is_empty() {
                        debug!(
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
                        debug!(
                            mempool_len = mempool_len_pre,
                            stale, "Nothing proposable after filtering the drained mempool"
                        );
                    }

                    self.native_request_admission
                        .retain_for_proposal(&mut transactions);
                    let has_txs = !transactions.is_empty();

                    if has_txs || multi_validator {
                        let tx_hashes: Vec<Hash256> =
                            transactions.iter().map(|tx| tx.hash).collect();

                        if has_txs {
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
                                debug!(
                                    round = block.round,
                                    txs = block.transactions.len(),
                                    hash = %block.hash,
                                    "Proposed DAG block"
                                );
                                last_proposed_round = Some(block.round);

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
                                for transaction in transactions.iter().cloned() {
                                    if !state.receipts.contains_key(&transaction.hash.0) {
                                        self.native_request_admission.insert(&mempool, transaction);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to propose block: {}", e);
                                for transaction in transactions.iter().cloned() {
                                    self.native_request_admission.insert(&mempool, transaction);
                                }
                            }
                        }

                        if !multi_validator {
                            let _ = self.engine.advance_round();
                        }

                        if multi_validator {
                            if has_txs {
                                debug!(
                                    pending = pending_txs.len(),
                                    "Multi-validator mode: waiting for DAG commit"
                                );
                            }
                        } else if has_txs {
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
                        }
                    }
                }
            }

            if already_proposed {
                let _ = self.engine.advance_round();
            }

            phase_mark = crate::consensus_diagnostics::phase(
                &crate::consensus_diagnostics::DIAG.phase_propose_us,
                phase_mark,
            );
            let mut committed: Vec<arc_consensus::DagBlock> = std::mem::take(&mut commit_backlog);
            committed.extend(self.engine.try_commit());
            committed.sort_by_key(|b| b.round);
            if let Some(first_missing) = committed.iter().position(|block| {
                exact_dag_preimages_from(&pending_txs, Some(&state), &block.transactions).is_err()
            }) {
                let stalled = committed.split_off(first_missing);
                let round = stalled[0].round;
                crate::consensus_diagnostics::DIAG
                    .commit_stalled_blocks
                    .store(stalled.len() as u64, std::sync::atomic::Ordering::Relaxed);
                if commit_stall_fetch_at
                    .is_none_or(|at: Instant| at.elapsed() >= TARGETED_FETCH_INTERVAL * 4)
                {
                    commit_stall_fetch_at = Some(Instant::now());
                    crate::consensus_diagnostics::bump(
                        &crate::consensus_diagnostics::DIAG.commit_stalls,
                    );
                    warn!(
                        round,
                        block = %stalled[0].hash,
                        waiting = stalled.len(),
                        "Commit waiting for transaction bodies; requesting the history behind it"
                    );
                    if let Some(ref tx_chan) = outbound_tx {
                        for (peer, generation) in connected_validators.iter() {
                            if generation.connected {
                                let sent =
                                    tx_chan.try_send(OutboundMessage::SendDagHistoryRequest {
                                        target: *peer,
                                        from_round: round,
                                        max_rounds: 4,
                                    });
                                crate::consensus_diagnostics::note_send(&sent);
                            }
                        }
                    }
                }
                commit_backlog = stalled;
            } else {
                crate::consensus_diagnostics::DIAG
                    .commit_stalled_blocks
                    .store(0, std::sync::atomic::Ordering::Relaxed);
            }
            if !committed.is_empty() {
                for dag_block in &committed {
                    debug!(
                        round = dag_block.round,
                        hash = %dag_block.hash,
                        txs = dag_block.transactions.len(),
                        "DAG block committed"
                    );

                    let all_preimages = match exact_dag_preimages_from(
                        &pending_txs,
                        Some(&state),
                        &dag_block.transactions,
                    ) {
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

                    if state.native_inference_context().is_some() {
                        let selected = select_native_transactions_and_requeue(
                            &state,
                            &committed_txs,
                            &mempool,
                            &self.native_request_admission,
                            &mut requeued_since,
                        );
                        let omitted = committed_txs.len().saturating_sub(selected.len());
                        if omitted > 0 {
                            debug!(
                                block = %dag_block.hash,
                                omitted,
                                "Native block carries at most one transaction; the rest stay \
                                 eligible or are dropped"
                            );
                            crate::consensus_diagnostics::DIAG
                                .protocol4_omitted_transactions
                                .fetch_add(omitted as u64, std::sync::atomic::Ordering::Relaxed);
                        }
                        committed_txs = selected;
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
                    let decision_proof = match self
                        .engine
                        .consensus_domain()
                        .or_else(|| self.engine.certificate_domain())
                    {
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

                    if decision_proof != Hash256::ZERO
                        && let Some(existing) =
                            self.canonical_height_for_decision(&state, decision_proof)
                    {
                        warn!(
                            round = dag_block.round,
                            anchor = %dag_block.hash,
                            existing_height = existing,
                            "Anchor already applied; not appending a second canonical block"
                        );
                        if let Some(tracker) = skip_tracker.as_mut() {
                            tracker.note_durable_commit_round(dag_block.round);
                            let _ = self.persist_signing_record(tracker.record());
                        }
                        continue;
                    }
                    let execute_started = Instant::now();
                    let executed = state.execute_block_adaptive_at_with_proof(
                        &committed_txs,
                        dag_block.author,
                        dag_block.timestamp,
                        decision_proof,
                    );
                    crate::consensus_diagnostics::add_elapsed(
                        &crate::consensus_diagnostics::DIAG.commit_execute_us,
                        execute_started,
                    );
                    let (block, receipts) = match executed {
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
                    let height_now = block.header.height;
                    if committed_txs.is_empty() {
                        debug!(
                            height = height_now,
                            elapsed_ms = elapsed.as_millis(),
                            "Block produced and durably bound to DAG commit"
                        );
                    } else {
                        info!(
                            height = height_now,
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
                    if height_now.is_multiple_of(1_000) {
                        info!(
                            height = height_now,
                            dag_round = self.engine.current_round(),
                            committed_round = self.engine.last_committed_round(),
                            "Canonical progress"
                        );
                    }
                    crate::consensus_diagnostics::bump(
                        &crate::consensus_diagnostics::DIAG.canonical_blocks_produced,
                    );

                    if let Some(tracker) = skip_tracker.as_mut() {
                        tracker.note_durable_commit_round(dag_block.round);
                        if !self.persist_signing_record(tracker.record()) {
                            tracing::error!(
                                round = dag_block.round,
                                "Could not persist the local commit record; a restart will \
                                 resume from an earlier round"
                            );
                        }
                    }

                    if self.snapshot_every_blocks > 0 {
                        let height = state.height();
                        if height > 0 && height.is_multiple_of(self.snapshot_every_blocks) {
                            let snapshot_started = Instant::now();
                            let published = state.publish_durable_snapshot();
                            crate::consensus_diagnostics::add_elapsed(
                                &crate::consensus_diagnostics::DIAG.state_snapshot_publish_us,
                                snapshot_started,
                            );
                            match published {
                                Ok(manifest) => info!(
                                    height = manifest.identity.height,
                                    resume_from = manifest.resume_from_sequence,
                                    "Published a durable state snapshot"
                                ),
                                Err(error) => warn!(
                                    %error,
                                    height,
                                    "Could not publish a state snapshot; recovery will replay \
                                     more of the WAL"
                                ),
                            }
                        }
                    }

                    {
                        let height = state.height();
                        if height > 0 && height.is_multiple_of(RECORD_PRUNE_EVERY) {
                            let finality_floor = height.saturating_sub(FINALITY_RECORD_MARGIN);
                            let absence_floor = self
                                .engine
                                .last_committed_round()
                                .saturating_sub(ABSENCE_RECORD_MARGIN);
                            if let Some(tracker) = skip_tracker.as_mut() {
                                tracker.prune_record(finality_floor, absence_floor);
                            }
                            finality_vote_collector.prune_below(finality_floor);
                            finality_signed_heights.retain(|h| *h >= finality_floor);
                            absence_gossiped_at.retain(|(round, _), _| *round >= absence_floor);

                            if let Some(wal) = &self.dag_wal {
                                let committed_round = self.engine.last_committed_round();
                                dag_wal_checkpoints.push_back((committed_round, wal.sequence()));
                                let horizon =
                                    committed_round.saturating_sub(self.engine.retained_rounds());
                                let mut prune_before = None;
                                while dag_wal_checkpoints
                                    .front()
                                    .is_some_and(|(round, _)| *round <= horizon)
                                {
                                    prune_before =
                                        dag_wal_checkpoints.pop_front().map(|(_, seq)| seq);
                                }
                                if let Some(sequence) = prune_before {
                                    let pruned_started = Instant::now();
                                    match wal.delete_segments_before(sequence) {
                                        Ok(deleted) if deleted > 0 => {
                                            crate::consensus_diagnostics::DIAG
                                                .dag_wal_segments_deleted
                                                .fetch_add(
                                                    deleted as u64,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );
                                            debug!(
                                                deleted,
                                                horizon, "Pruned legacy DAG WAL segments"
                                            );
                                        }
                                        Ok(_) => {}
                                        Err(error) => {
                                            warn!(%error, "Could not prune legacy DAG WAL segments")
                                        }
                                    }
                                    crate::consensus_diagnostics::add_elapsed(
                                        &crate::consensus_diagnostics::DIAG.dag_wal_prune_us,
                                        pruned_started,
                                    );
                                }
                            }
                        }
                    }

                    if let Some(certificate) = self.engine.finality_certificate(block.header.height)
                        && certificate.block_hash != block.hash
                    {
                        report_divergence(block.header.height, block.hash, certificate.block_hash);
                    }

                    if self.is_multi_validator()
                        && let Some(domain) = self.engine.certificate_domain()
                        && let Some(keypair) = self.engine.local_keypair()
                        && let Some(tracker) = skip_tracker.as_mut()
                        && finality_signed_heights.insert(block.header.height)
                    {
                        let height = block.header.height;
                        let identity = (block.hash, block.header.state_root, block.header.tx_root);
                        let previous = tracker.record().finality_votes.get(&height).copied();
                        match previous {
                            Some(existing) if existing != identity => {
                                tracing::error!(
                                    height,
                                    "Refusing to sign a second, different finality transcript \
                                     for one height"
                                );
                            }
                            _ => {
                                tracker.note_finality_vote(height, identity);
                                if self.persist_signing_record(tracker.record()) {
                                    match arc_consensus::view_change::FinalityVote::sign(
                                        domain,
                                        self.engine.frozen_validator_set_hash(),
                                        height,
                                        identity.0,
                                        identity.1,
                                        identity.2,
                                        keypair,
                                    ) {
                                        Ok(vote) => {
                                            if let Some(ref tx_chan) = outbound_tx {
                                                let _ = tx_chan.try_send(
                                                    OutboundMessage::BroadcastFinalityVote(
                                                        vote.clone(),
                                                    ),
                                                );
                                            }
                                            let set = self.engine.validator_set();
                                            if let Ok(Some(certificate)) =
                                                finality_vote_collector.add(vote, &domain, &set)
                                            {
                                                let _ = self
                                                    .engine
                                                    .register_finality_certificate(certificate);
                                            }
                                        }
                                        Err(error) => tracing::error!(
                                            height,
                                            %error,
                                            "Could not sign the finality transcript"
                                        ),
                                    }
                                } else {
                                    finality_signed_heights.remove(&height);
                                    tracing::error!(
                                        height,
                                        "Withholding a finality attestation: its decision is \
                                         not durable; will retry"
                                    );
                                }
                            }
                        }
                    }
                }
            }

            phase_mark = crate::consensus_diagnostics::phase(
                &crate::consensus_diagnostics::DIAG.phase_commit_us,
                phase_mark,
            );
            if let Some(ref r) = self.dag_round {
                r.store(current_round, std::sync::atomic::Ordering::Relaxed);
            }
            if !committed.is_empty() {
                if let Some(ref c) = self.dag_committed {
                    c.fetch_add(committed.len() as u64, std::sync::atomic::Ordering::Relaxed);
                }

                if let Ok(mut tracker) = self.stake_tracker.lock() {
                    for dag_block in &committed {
                        tracker.report_vote(dag_block.author, dag_block.round, dag_block.hash.0);

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

                    if current_round.is_multiple_of(10_000) && current_round > 10_000 {
                        tracker.prune_votes(current_round - 10_000);
                    }
                }

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

            if multi_validator && self.engine.needs_view_change() {
                warn!(
                    round = current_round,
                    "Round stalled, but no authenticated quorum view-change certificate is available; consensus view unchanged"
                );
                self.engine.reset_round_timer();
            }

            static HEARTBEAT_COUNTER: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let hb_count = HEARTBEAT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let hb_interval = if self.is_multi_validator() { 600 } else { 6000 };
            if hb_count.is_multiple_of(hb_interval)
                && let Some(ref tx) = outbound_tx
            {
                let _ = tx.try_send(
                    arc_net::transport::OutboundMessage::BroadcastHeartbeatWithRound {
                        dag_round: current_round,
                        committed_round: self.engine.last_committed_round(),
                    },
                );
            }

            static EVICTION_COUNTER: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let count = EVICTION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count.is_multiple_of(100) {
                state.evict_transactions(1_000_000);

                let committed_round = self.engine.last_committed_round();
                let pruned = prune_dag_preimages(
                    &pending_txs,
                    &pending_tx_latest_round,
                    committed_round,
                    committed_round.saturating_sub(self.engine.retained_rounds()),
                    |hash| state.receipts.contains_key(hash),
                );
                if pruned > 0 {
                    debug!(
                        pruned,
                        retained = pending_txs.len(),
                        "Pruned irreversibly obsolete DAG transaction preimages"
                    );
                }
                requeued_since.retain(|_, since| since.elapsed() < NATIVE_REQUEUE_LIFETIME * 2);
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
    #[test]
    fn a_certified_block_other_than_this_nodes_own_is_a_divergence() {
        let dir = tempfile::tempdir().unwrap();
        let sender = arc_crypto::hash_bytes(b"divergence-sender");
        let state = StateDB::with_genesis_persistent(&[(sender, 1_000)], dir.path(), Hash256::ZERO)
            .unwrap();
        let mut tx = arc_types::Transaction::new_transfer(
            sender,
            arc_crypto::hash_bytes(b"divergence-recipient"),
            1,
            0,
        );
        tx.sig_verified = true;
        state
            .execute_block_adaptive_at_with_proof(
                &[tx],
                sender,
                1_700_000_000_000,
                arc_crypto::hash_bytes(b"divergence-proof"),
            )
            .unwrap();
        let local = state.get_block(1).unwrap().hash;
        assert_eq!(divergence(&state, 1, local), None);
        let other = arc_crypto::hash_bytes(b"another block");
        assert_eq!(divergence(&state, 1, other), Some(local));
        assert_eq!(divergence(&state, 2, other), None);
    }

    #[test]
    fn the_lowest_diverged_height_is_the_one_recorded() {
        assert_eq!(lower_divergence(0, 7), Some(8));
        assert_eq!(lower_divergence(8, 9), None);
        assert_eq!(lower_divergence(8, 7), None);
        assert_eq!(lower_divergence(8, 3), Some(4));
        assert_eq!(lower_divergence(0, u64::MAX), Some(u64::MAX));
    }

    #[test]
    fn a_committed_block_s_executed_body_is_found_in_state_when_the_cache_dropped_it() {
        let sender = arc_crypto::signature::KeyPair::from_ed25519_secret_bytes(
            &arc_crypto::hash_bytes(b"preimage sender").0,
        );
        let state = StateDB::with_genesis(&[(sender.address(), 1_000_000)]);
        let mut executed = arc_types::Transaction::new_transfer(
            sender.address(),
            arc_crypto::hash_bytes(b"to"),
            5,
            0,
        );
        executed.sign(&sender).unwrap();
        state
            .execute_block_verified_at(std::slice::from_ref(&executed), sender.address(), 1)
            .expect("executes");
        let pending: dashmap::DashMap<[u8; 32], arc_types::Transaction> = dashmap::DashMap::new();

        assert!(matches!(
            exact_dag_preimages(&pending, &[executed.hash]),
            Err(DagPreimageError::Missing(_))
        ));
        let found = exact_dag_preimages_from(&pending, Some(&state), &[executed.hash])
            .expect("the canonical state holds the executed body");
        assert_eq!(found[0].hash, executed.hash);
        let set = ValidatorSet::new(
            vec![Validator::new(sender.address(), arc_consensus::STAKE_ARC, 0).unwrap()],
            1,
        );
        let engine = ConsensusEngine::new_with_keypair(set, sender.address(), sender.clone());
        let original = engine.propose_block(vec![executed.hash], 1_000).unwrap();
        let (replayed, bodies) =
            local_proposal_with_preimages(&engine, sender.address(), 0, &pending, Some(&state))
                .unwrap()
                .unwrap();
        assert_eq!(
            bincode::serialize(&replayed).unwrap(),
            bincode::serialize(&original).unwrap()
        );
        assert_eq!(bodies[0].hash, executed.hash);
        assert!(
            local_proposal_with_preimages(&engine, sender.address(), 0, &pending, None).is_err()
        );

        let wanted: std::collections::HashSet<[u8; 32]> = [executed.hash.0].into_iter().collect();
        let served = history_bodies(&pending, &state, &wanted);
        assert_eq!(
            served.len(),
            1,
            "a peer must be able to fetch an executed body"
        );
        assert_eq!(served[0].hash, executed.hash);
    }

    #[test]
    fn a_body_nobody_holds_is_still_reported_missing() {
        let state = StateDB::with_genesis(&[]);
        let pending: dashmap::DashMap<[u8; 32], arc_types::Transaction> = dashmap::DashMap::new();
        let unknown = arc_crypto::hash_bytes(b"nobody has this");
        assert!(matches!(
            exact_dag_preimages_from(&pending, Some(&state), &[unknown]),
            Err(DagPreimageError::Missing(h)) if h == unknown
        ));
    }

    #[test]
    fn a_receipted_transaction_is_never_offered_again() {
        let sender = arc_crypto::signature::KeyPair::from_ed25519_secret_bytes(
            &arc_crypto::hash_bytes(b"circulating sender").0,
        );
        let state = StateDB::with_genesis(&[(sender.address(), 1_000_000)]);
        let make = |nonce: u64| {
            let mut tx = arc_types::Transaction::new_transfer(
                sender.address(),
                arc_crypto::hash_bytes(b"recipient"),
                10,
                nonce,
            );
            tx.sign(&sender).unwrap();
            tx
        };
        let executed = make(0);
        let pending = make(1);
        state
            .execute_block_verified_at(std::slice::from_ref(&executed), sender.address(), 1)
            .expect("executes");
        assert!(
            state.receipts.contains_key(&executed.hash.0),
            "it now has a receipt"
        );

        let mut offered = vec![executed.clone(), pending.clone()];
        let dropped = retain_unreceipted(&state, &mut offered);
        assert_eq!(dropped, 1);
        assert_eq!(offered.len(), 1);
        assert_eq!(
            offered[0].hash, pending.hash,
            "only the unexecuted one remains eligible"
        );
    }

    use super::*;
    use arc_crypto::{KeyPair, Signature, hash_bytes};
    use arc_types::{Account, AccountChange, StateDiff, Transaction, TxType};

    #[test]
    fn recovery_parent_gate_uses_quorum_and_exact_verified_absences() {
        use arc_consensus::view_change::{
            AbsenceReason, SkipCertificate, SkipVote, validator_set_hash,
        };

        let keys: Vec<_> = (0..6)
            .map(|index| {
                KeyPair::from_ed25519_secret_bytes(
                    &hash_bytes(format!("proposal-parent-gate-{index}").as_bytes()).0,
                )
            })
            .collect();
        let set = ValidatorSet::new(
            keys.iter()
                .enumerate()
                .map(|(index, key)| {
                    let stake = if index < 4 { 6_666_667 } else { 6_666_666 };
                    Validator::new(key.address(), stake, index as u16).unwrap()
                })
                .collect(),
            1,
        );
        assert_eq!(set.quorum, 26_666_667);
        let domain = arc_consensus::ConsensusDomain::new(hash_bytes(b"proposal-parent-gate"), 2, 7);
        let engines: Vec<_> = keys
            .iter()
            .map(|key| {
                let engine =
                    ConsensusEngine::new_with_keypair(set.clone(), key.address(), key.clone());
                for registered in &keys {
                    engine.register_validator_key(
                        registered.address(),
                        registered.public_key_bytes().try_into().unwrap(),
                    );
                }
                engine.install_consensus_domain(domain).unwrap();
                engine.install_recovery_cursor(100).unwrap();
                engine
            })
            .collect();
        let bootstrap: Vec<_> = engines
            .iter()
            .enumerate()
            .map(|(index, engine)| engine.propose_block(vec![], 1_000 + index as u64).unwrap())
            .collect();
        for block in bootstrap.iter().skip(1).take(3) {
            engines[0].receive_block(block).unwrap();
        }
        assert!(
            !proposal_parent_quorum(&engines[0], 102),
            "a stake quorum still needs exact certificates for both absent members"
        );

        let set_hash = validator_set_hash(&set);
        for absentee_index in [4usize, 5] {
            let votes = keys[..4]
                .iter()
                .map(|key| {
                    SkipVote::sign(
                        domain,
                        set_hash,
                        101,
                        keys[absentee_index].address(),
                        AbsenceReason::NoBlock,
                        set.quorum,
                        key,
                    )
                    .unwrap()
                })
                .collect();
            engines[0]
                .register_skip_certificate(SkipCertificate::new(
                    domain,
                    set_hash,
                    101,
                    keys[absentee_index].address(),
                    AbsenceReason::NoBlock,
                    votes,
                ))
                .unwrap();
        }
        assert!(proposal_parent_quorum(&engines[0], 102));
        assert!(engines[0].advance_round());
        assert_eq!(
            engines[0]
                .propose_block(vec![], 2_000)
                .unwrap()
                .parents
                .len(),
            4
        );

        let under_quorum = &engines[2];
        for block in bootstrap.iter().take(2) {
            under_quorum.receive_block(block).unwrap();
        }
        for absentee_index in [3usize, 4, 5] {
            let votes = keys[..4]
                .iter()
                .map(|key| {
                    SkipVote::sign(
                        domain,
                        set_hash,
                        101,
                        keys[absentee_index].address(),
                        AbsenceReason::NoBlock,
                        set.quorum,
                        key,
                    )
                    .unwrap()
                })
                .collect();
            under_quorum
                .register_skip_certificate(SkipCertificate::new(
                    domain,
                    set_hash,
                    101,
                    keys[absentee_index].address(),
                    AbsenceReason::NoBlock,
                    votes,
                ))
                .unwrap();
        }
        assert!(
            !proposal_parent_quorum(under_quorum, 102),
            "certificates replace missing membership only; they do not count stake"
        );

        let wrong_round = &engines[1];
        for block in bootstrap.iter().take(4) {
            if block.author != keys[1].address() {
                wrong_round.receive_block(block).unwrap();
            }
        }
        for absentee_index in [4usize, 5] {
            let votes = keys[..4]
                .iter()
                .map(|key| {
                    SkipVote::sign(
                        domain,
                        set_hash,
                        100,
                        keys[absentee_index].address(),
                        AbsenceReason::NoBlock,
                        set.quorum,
                        key,
                    )
                    .unwrap()
                })
                .collect();
            wrong_round
                .register_skip_certificate(SkipCertificate::new(
                    domain,
                    set_hash,
                    100,
                    keys[absentee_index].address(),
                    AbsenceReason::NoBlock,
                    votes,
                ))
                .unwrap();
        }
        assert!(
            !proposal_parent_quorum(wrong_round, 102),
            "certificates for round 100 cannot excuse missing parents in round 101"
        );
        assert!(!wrong_round.advance_round());
    }

    #[test]
    fn connected_quorum_is_not_a_full_committee_gate_except_at_genesis() {
        let keys: Vec<_> = (0..6)
            .map(|index| {
                KeyPair::from_ed25519_secret_bytes(
                    &hash_bytes(format!("connected-quorum-{index}").as_bytes()).0,
                )
            })
            .collect();
        let set = ValidatorSet::new(
            keys.iter()
                .enumerate()
                .map(|(index, key)| {
                    let stake = if index < 4 { 6_666_667 } else { 6_666_666 };
                    Validator::new(key.address(), stake, index as u16).unwrap()
                })
                .collect(),
            1,
        );
        let four_connected: std::collections::HashSet<_> =
            keys[..4].iter().map(KeyPair::address).collect();
        assert!(connected_validator_quorum(&set, &four_connected, false));
        assert!(!connected_validator_quorum(&set, &four_connected, true));

        let three_connected: std::collections::HashSet<_> =
            keys[..3].iter().map(KeyPair::address).collect();
        assert!(!connected_validator_quorum(&set, &three_connected, false));

        let all_connected: std::collections::HashSet<_> =
            keys.iter().map(KeyPair::address).collect();
        assert!(connected_validator_quorum(&set, &all_connected, true));
    }

    #[test]
    fn durable_local_absence_vote_completes_exact_four_of_six_quorum() {
        use arc_consensus::view_change::{
            AbsenceReason, ConsensusSigningRecord, SkipVote, validator_set_hash,
        };

        let keys: Vec<_> = (0..6)
            .map(|index| {
                KeyPair::from_ed25519_secret_bytes(
                    &hash_bytes(format!("absence-local-quorum-{index}").as_bytes()).0,
                )
            })
            .collect();
        let set = ValidatorSet::new(
            keys.iter()
                .enumerate()
                .map(|(index, key)| {
                    let stake = if index < 4 { 6_666_667 } else { 6_666_666 };
                    Validator::new(key.address(), stake, 0).unwrap()
                })
                .collect(),
            1,
        );
        assert_eq!(set.total_stake, 40_000_000);
        assert_eq!(set.quorum, 26_666_667);
        assert_eq!(
            set.stake_for_addresses(&keys[..4].iter().map(KeyPair::address).collect::<Vec<_>>()),
            26_666_668
        );

        let domain = arc_consensus::ConsensusDomain::new(hash_bytes(b"local-absence-quorum"), 1, 1);
        let set_hash = validator_set_hash(&set);
        let absentee = keys[5].address();
        let mut record = ConsensusSigningRecord::default();
        record
            .skipped_rounds
            .entry(123)
            .or_default()
            .insert(absentee, AbsenceReason::NoBlock);

        for local in 0..4 {
            let mut collector = arc_consensus::view_change::SkipVoteCollector::new();
            for (peer, key) in keys.iter().take(4).enumerate() {
                if peer == local {
                    continue;
                }
                let vote = SkipVote::sign(
                    domain,
                    set_hash,
                    123,
                    absentee,
                    AbsenceReason::NoBlock,
                    set.quorum,
                    key,
                )
                .unwrap();
                assert!(collector.add(vote, &domain, &set).unwrap().is_none());
            }

            let repeated = SkipVote::sign(
                domain,
                set_hash,
                123,
                absentee,
                AbsenceReason::NoBlock,
                set.quorum,
                &keys[(local + 1) % 4],
            )
            .unwrap();
            assert!(collector.add(repeated, &domain, &set).unwrap().is_none());

            let own_vote = SkipVote::sign(
                domain,
                set_hash,
                123,
                absentee,
                AbsenceReason::NoBlock,
                set.quorum,
                &keys[local],
            )
            .unwrap();
            let mut writes = 0;
            let result = persist_then_collect_absence_vote(
                own_vote,
                false,
                &record,
                |_| {
                    writes += 1;
                    true
                },
                &mut collector,
                &domain,
                &set,
            )
            .unwrap();
            assert_eq!(
                writes, 1,
                "the signed choice must persist before collection"
            );
            let DurableAbsenceVoteCollection::Added(Some(certificate)) = result else {
                panic!("four enabled votes should form quorum after the durable local vote");
            };
            assert_eq!(certificate.verify(&domain, &set).unwrap(), 26_666_668);
        }

        let mut collector = arc_consensus::view_change::SkipVoteCollector::new();
        for key in keys.iter().take(4).skip(1) {
            let vote = SkipVote::sign(
                domain,
                set_hash,
                124,
                absentee,
                AbsenceReason::NoBlock,
                set.quorum,
                key,
            )
            .unwrap();
            assert!(collector.add(vote, &domain, &set).unwrap().is_none());
        }
        let mut tracker = arc_consensus::view_change::SkipTracker::new(
            domain,
            set_hash,
            arc_consensus::view_change::DEFAULT_SKIP_GRACE_MS,
            ConsensusSigningRecord::default(),
        );
        tracker.observe(
            124,
            &absentee,
            AbsenceReason::NoBlock,
            set.quorum,
            true,
            set.quorum,
            0,
        );
        let own_vote = tracker
            .sign_if_permitted(
                124,
                absentee,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                arc_consensus::view_change::DEFAULT_SKIP_GRACE_MS,
                &keys[0],
            )
            .unwrap();
        assert!(matches!(
            persist_then_collect_absence_vote(
                own_vote.clone(),
                false,
                tracker.record(),
                |_| false,
                &mut collector,
                &domain,
                &set,
            )
            .unwrap(),
            DurableAbsenceVoteCollection::Withheld
        ));
        assert!(
            collector
                .add(
                    SkipVote::sign(
                        domain,
                        set_hash,
                        124,
                        absentee,
                        AbsenceReason::NoBlock,
                        set.quorum,
                        &keys[3],
                    )
                    .unwrap(),
                    &domain,
                    &set,
                )
                .unwrap()
                .is_none()
        );
        let retry_vote = tracker
            .sign_if_permitted(
                124,
                absentee,
                AbsenceReason::NoBlock,
                0,
                set.quorum,
                arc_consensus::view_change::DEFAULT_SKIP_GRACE_MS + 1,
                &keys[0],
            )
            .unwrap();
        assert_eq!(retry_vote, own_vote);
        let DurableAbsenceVoteCollection::Added(Some(certificate)) =
            persist_then_collect_absence_vote(
                retry_vote,
                false,
                tracker.record(),
                |_| true,
                &mut collector,
                &domain,
                &set,
            )
            .unwrap()
        else {
            panic!("retry with a durable decision should complete the quorum");
        };
        assert_eq!(certificate.verify(&domain, &set).unwrap(), 26_666_668);
    }

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

        assert_eq!(
            prune_irreversible_preimages(&pending, &latest, 5, 0, |_| true),
            1
        );
        assert!(!pending.contains_key(&obsolete.hash.0));
        assert_eq!(
            exact_dag_preimages(&pending, &[future_commit.hash])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_native_transaction_is_put_back_only_until_its_lifetime_ends() {
        let key = KeyPair::generate_ed25519();
        let tx = signed_transfer(&key, 41, 0);
        let mut since = std::collections::HashMap::new();
        assert!(requeue_native(&mut since, &tx));
        assert!(requeue_native(&mut since, &tx), "still inside its lifetime");
        since.insert(
            tx.hash.0,
            Instant::now()
                .checked_sub(NATIVE_REQUEUE_LIFETIME + Duration::from_secs(1))
                .unwrap(),
        );
        assert!(
            !requeue_native(&mut since, &tx),
            "past its lifetime it is dropped"
        );
    }

    #[test]
    fn migrated_v3_native_selection_retains_ordinary_work_and_drops_stale_finalizers() {
        use crate::native_inference::NativeRequestAdmission;
        use arc_state::recovery::{
            ArcCheckpoint, RecoveryExportSpec, RecoveryImport, RecoveryNetworkPolicy,
            RecoveryValidator,
        };
        use arc_types::TxBody;
        use arc_types::inference_contract::{
            INFERENCE_CONTRACT_VERSION, InferenceCertificate, InferenceDomain, InferenceJob,
            InferenceRequest, ValidatorMember, sign_vote, validator_set_commitment,
        };
        use arc_types::transaction::{
            JoinValidatorBody, NativeInferenceFinalizeBody, NativeInferenceRequestBody, gas_costs,
        };

        let directory = tempfile::tempdir().unwrap();
        let requester = KeyPair::from_ed25519_secret_bytes(&hash_bytes(b"requeue-requester").0);
        let keys: Vec<_> = (0..6)
            .map(|i| {
                KeyPair::from_ed25519_secret_bytes(&hash_bytes(format!("requeue-{i}").as_bytes()).0)
            })
            .collect();
        let genesis = hash_bytes(b"requeue-recovered-v3");
        let validators: Vec<_> = keys
            .iter()
            .map(|key| RecoveryValidator {
                address: key.address(),
                public_key: key.public_key_bytes().try_into().unwrap(),
                stake: 5_000_000,
            })
            .collect();
        let mut members: Vec<_> = validators
            .iter()
            .map(|v| ValidatorMember::new(v.address, v.stake))
            .collect();
        members.sort_by_key(|v| v.address.0);
        let mut funds = vec![
            (requester.address(), 1_000),
            (
                arc_state::recovery::recovery_stake_reserve_address(),
                60_000_000,
            ),
        ];
        funds.extend(keys.iter().map(|key| (key.address(), 5_000_000)));
        let source = StateDB::with_genesis(&funds);
        let joins: Vec<_> = keys
            .iter()
            .map(|key| {
                let mut tx = Transaction::new_transfer(key.address(), key.address(), 0, 0);
                tx.tx_type = TxType::JoinValidator;
                tx.body = TxBody::JoinValidator(JoinValidatorBody {
                    pubkey: key.public_key_bytes().try_into().unwrap(),
                    initial_stake: 5_000_000,
                });
                tx.sign(key).unwrap();
                tx
            })
            .collect();
        let (_, receipts) = source.execute_block(&joins, keys[0].address()).unwrap();
        assert!(receipts.iter().all(|receipt| receipt.success));
        for key in &keys {
            let mut account = source.get_account(&key.address()).unwrap();
            account.staked_balance = 0;
            source.update_account(&key.address(), account);
        }
        source.execute_block(&[], keys[0].address()).unwrap();
        let mut checkpoint = ArcCheckpoint::export_unsigned(
            &source,
            RecoveryExportSpec {
                chain_id: "requeue-recovered-v3".into(),
                genesis_hash: genesis,
                source_consensus_round: 0,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators,
                community_rewards_v1_activation_height: None,
                created_at_unix_ms: 1,
            },
        )
        .unwrap();
        for key in keys.iter().take(5) {
            checkpoint.add_signature(key).unwrap();
        }
        let path = directory.path().join("approved.arcchkpt");
        checkpoint.write_to(&path).unwrap();
        let state = StateDB::with_genesis_persistent_recovery(
            &[],
            directory.path().join("state"),
            RecoveryNetworkPolicy {
                chain_id: "requeue-recovered-v3".into(),
                genesis_hash: genesis,
                recovery_epoch: 1,
                validator_set_id: 1,
                validators: members.iter().map(|v| (v.address, v.stake)).collect(),
                community_rewards_v1_activation_height: None,
            },
            Some(RecoveryImport {
                checkpoint_path: path,
                approved_manifest_hash: checkpoint.manifest_hash(),
            }),
        )
        .unwrap();
        let tuple = hash_bytes(b"synthetic-requeue-execution");
        let context = arc_state::InferenceAdmissionContext {
            domain: InferenceDomain {
                chain_genesis: genesis,
                recovery_epoch: 1,
                validator_set_hash: validator_set_commitment(&members).unwrap(),
            },
            members,
            allowed_executions: vec![arc_state::AllowedExecution {
                model_hash: tuple,
                profile_hash: tuple,
                generation_hash: tuple,
                assignment_hash: tuple,
            }],
            selection_rule: arc_state::NativeSelectionRule::SkipUsedNoncesV2,
        };
        let mut future_native =
            Transaction::new_transfer(requester.address(), requester.address(), 0, 0);
        future_native.body =
            TxBody::NativeInferenceRefund(arc_types::transaction::NativeInferenceRefundBody {
                request_id: [71; 32],
            });
        future_native.tx_type = future_native.body.tx_type();
        state
            .sign_transaction(&mut future_native, &requester)
            .unwrap();
        let availability = |tx: &Transaction| {
            verify_peer_dag_availability(&state, &[tx.hash], std::slice::from_ref(tx))
        };
        assert!(
            availability(&future_native).is_err(),
            "unconfigured recovery never admits the native family"
        );
        let migration = arc_state::NativeMigrationRecord {
            chain_genesis: genesis,
            recovery_epoch: 1,
            validator_set_id: 1,
            activation_height: state.height() + 1,
            context_commitment: context.commitment().unwrap(),
        };
        let mut wrong_context = context.clone();
        wrong_context.allowed_executions[0].assignment_hash =
            hash_bytes(b"unapproved pending binding");
        state
            .authorize_native_migration(migration.clone(), wrong_context)
            .unwrap();
        assert!(
            availability(&future_native).is_err(),
            "a pending context must match the operator's commitment"
        );
        state
            .authorize_native_migration(migration, context.clone())
            .unwrap();
        assert!(state.native_inference_context().is_none());
        assert!(
            availability(&future_native).is_ok(),
            "authorized lagging nodes can retain future DAG envelopes"
        );
        assert!(
            state.validate_v3_block_admission(&[future_native]).is_err(),
            "retention must not activate execution"
        );
        let commit = |transactions: &[Transaction]| {
            state.validate_v3_block_admission(transactions).unwrap();
            let (block, receipts) = state
                .execute_block_adaptive_at_with_proof(
                    transactions,
                    keys[0].address(),
                    state.get_block(state.height()).unwrap().header.timestamp + 1,
                    hash_bytes(&(state.height() + 1).to_le_bytes()),
                )
                .unwrap();
            assert_eq!(block.header.protocol_version.major, 3);
            assert!(receipts.iter().all(|receipt| receipt.success));
            block
        };
        commit(&[]);
        assert!(state.native_inference_context().is_some());
        let signed = |key: &KeyPair, body: TxBody, gas| {
            let nonce = state.get_account(&key.address()).unwrap().nonce;
            let mut tx = Transaction::new_transfer(key.address(), key.address(), 0, nonce);
            tx.tx_type = body.tx_type();
            tx.body = body;
            tx.fee = 0;
            tx.gas_limit = gas;
            state.sign_transaction(&mut tx, key).unwrap();
            tx
        };
        let input = vec![1, 2, 3];
        let request = InferenceRequest::sign(
            InferenceJob {
                version: INFERENCE_CONTRACT_VERSION,
                domain: context.domain,
                requester: requester.address(),
                nonce: state.get_account(&requester.address()).unwrap().nonce,
                model_hash: tuple,
                profile_hash: tuple,
                input_hash: hash_bytes(&input),
                generation_hash: tuple,
                assignment_hash: tuple,
                max_tokens: 2,
                max_output_bytes: 8,
                execution_price: 10,
                reserved_max_payment: 100,
                expires_at: 100,
            },
            &requester,
        )
        .unwrap();
        let id = request.job.request_id();
        let request_tx = signed(
            &requester,
            TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
                request,
                input_blob: input,
            }),
            gas_costs::NATIVE_INFERENCE_REQUEST,
        );
        commit(std::slice::from_ref(&request_tx));
        let finalize = |submitter: usize, voters: &[KeyPair]| {
            let output = 7u32.to_le_bytes().to_vec();
            let mut votes: Vec<_> = voters
                .iter()
                .map(|key| sign_vote(id, &output, key).unwrap())
                .collect();
            votes.sort_by_key(|vote| vote.validator.0);
            signed(
                &keys[submitter],
                TxBody::NativeInferenceFinalize(NativeInferenceFinalizeBody {
                    request_id: id.0,
                    certificate: InferenceCertificate { output, votes },
                }),
                gas_costs::NATIVE_INFERENCE_FINALIZE,
            )
        };
        let winner = finalize(0, &keys[..5]);
        let alternative = finalize(1, &keys[1..]);
        state.validate_v3_transaction_admission(&winner).unwrap();
        state
            .validate_v3_transaction_admission(&alternative)
            .unwrap();
        let mut ordinary = Transaction::new_transfer(
            requester.address(),
            keys[0].address(),
            1,
            state.get_account(&requester.address()).unwrap().nonce,
        );
        ordinary.fee = 1;
        state.sign_transaction(&mut ordinary, &requester).unwrap();
        state.validate_v3_transaction_admission(&ordinary).unwrap();
        assert!(
            state
                .validate_v3_block_admission(&[winner.clone(), ordinary.clone()])
                .is_err(),
            "the native transition must still execute alone"
        );
        let domain =
            arc_consensus::ConsensusDomain::new(state.transaction_domain_hash().unwrap(), 1, 1);
        let make_engine = || {
            let set = ValidatorSet::new(
                keys.iter()
                    .map(|key| Validator::new(key.address(), 5_000_000, 0).unwrap())
                    .collect(),
                0,
            );
            let engine = ConsensusEngine::new_with_keypair(set, keys[0].address(), keys[0].clone());
            engine.install_consensus_domain(domain).unwrap();
            engine.install_recovery_cursor(0).unwrap();
            engine
        };
        let early = make_engine();
        let late = make_engine();
        let dag_block = |index: usize, round, parents: Vec<Hash256>, txs: Vec<Hash256>| {
            let mut block = arc_consensus::DagBlock {
                author: keys[index].address(),
                round,
                parents,
                ordering_commitment: arc_consensus::DagBlock::compute_ordering_commitment(&txs),
                transactions: txs,
                timestamp: round,
                hash: Hash256::ZERO,
                signature: vec![],
            };
            block.hash = block.compute_hash_in_domain(&domain);
            block.signature = bincode::serialize(&keys[index].sign(&block.hash).unwrap()).unwrap();
            block
        };
        let parents: Vec<_> = (0..6)
            .map(|i| dag_block(i, 1, vec![], vec![alternative.hash]))
            .collect();
        let mut unsigned_dag = parents[0].clone();
        unsigned_dag.signature.clear();
        assert!(early.receive_block(&unsigned_dag).is_err());
        for block in &parents {
            verify_peer_dag_availability(
                &state,
                &block.transactions,
                std::slice::from_ref(&alternative),
            )
            .unwrap();
            early.receive_block(block).unwrap();
        }
        for block in parents.iter().take(3) {
            late.receive_block(block).unwrap();
        }
        assert!(early.advance_round());
        assert!(
            !late.advance_round(),
            "three of six cannot advance recovery consensus"
        );
        assert!(
            late.import_history(&parents[..3], HISTORY_MAX_ROUNDS)
                .is_err(),
            "history cannot repair a tip below quorum by relaxing its cursor"
        );

        assert!(verify_peer_dag_availability(&state, &[alternative.hash], &[]).is_err());
        assert!(
            verify_peer_dag_availability(
                &state,
                &[alternative.hash],
                std::slice::from_ref(&winner),
            )
            .is_err()
        );
        let mut wrong_domain = alternative.clone();
        wrong_domain
            .sign_in_domain(&keys[1], &hash_bytes(b"foreign-recovery"))
            .unwrap();
        assert!(availability(&wrong_domain).is_err());
        let mut wrong_type = alternative.clone();
        wrong_type.tx_type = TxType::Transfer;
        state.sign_transaction(&mut wrong_type, &keys[1]).unwrap();
        assert!(availability(&wrong_type).is_err());
        let unsupported = signed(&keys[1], TxBody::LeaveValidator, 0);
        assert!(availability(&unsupported).is_err());
        let mut under_quorum_body = alternative.body.clone();
        let TxBody::NativeInferenceFinalize(body) = &mut under_quorum_body else {
            unreachable!()
        };
        body.certificate.votes.truncate(4);
        let under_quorum = signed(
            &keys[1],
            under_quorum_body,
            gas_costs::NATIVE_INFERENCE_FINALIZE,
        );
        assert!(
            availability(&under_quorum).is_ok(),
            "a signed body is available, not execution-authorized"
        );
        assert!(
            state
                .validate_v3_block_admission(&[under_quorum])
                .unwrap_err()
                .to_string()
                .contains("two-thirds-plus stake"),
            "execution still requires the full native certificate quorum"
        );
        let bad_gas = signed(
            &keys[1],
            alternative.body.clone(),
            gas_costs::NATIVE_INFERENCE_FINALIZE - 1,
        );
        assert!(availability(&bad_gas).is_err());
        let mut oversized_body = alternative.body.clone();
        let TxBody::NativeInferenceFinalize(body) = &mut oversized_body else {
            unreachable!()
        };
        body.certificate.output = vec![0; arc_state::V3_MAX_TRANSACTION_BYTES as usize + 1];
        let oversized = signed(
            &keys[1],
            oversized_body,
            gas_costs::NATIVE_INFERENCE_FINALIZE,
        );
        assert!(availability(&oversized).is_err());
        assert!(
            state
                .validate_v3_dag_availability(&[winner.clone(), ordinary.clone()])
                .is_err()
        );
        assert!(
            state
                .validate_v3_dag_availability(&[winner.clone(), alternative.clone()])
                .is_err()
        );
        assert!(
            state
                .validate_v3_dag_availability(&vec![
                    ordinary.clone();
                    arc_state::V3_MAX_TRANSACTIONS_PER_BLOCK + 1
                ])
                .is_err()
        );
        assert!(
            state
                .validate_v3_dag_availability(&[ordinary.clone(), ordinary.clone()])
                .is_err()
        );
        let mut same_sender = ordinary.clone();
        same_sender.nonce += 1;
        state
            .sign_transaction(&mut same_sender, &requester)
            .unwrap();
        assert!(
            state
                .validate_v3_dag_availability(&[ordinary.clone(), same_sender])
                .is_err()
        );
        let mut overlap = Transaction::new_transfer(keys[1].address(), keys[0].address(), 1, 0);
        overlap.fee = 1;
        state.sign_transaction(&mut overlap, &keys[1]).unwrap();
        assert!(
            state
                .validate_v3_dag_availability(&[ordinary.clone(), overlap])
                .is_err()
        );
        let mempool = Mempool::new(16);
        let admission = NativeRequestAdmission::default();
        let mut since = std::collections::HashMap::new();
        since.insert(
            ordinary.hash.0,
            Instant::now()
                .checked_sub(NATIVE_REQUEUE_LIFETIME + Duration::from_secs(1))
                .unwrap(),
        );
        mempool.insert(ordinary.clone()).unwrap();
        mempool.insert(winner.clone()).unwrap();
        let selected = select_native_transactions_and_requeue(
            &state,
            &mempool.drain(16),
            &mempool,
            &admission,
            &mut since,
        );
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].hash, winner.hash);
        assert!(mempool.contains(&ordinary.hash));
        assert_eq!(commit(&selected).header.tx_count, 1);
        assert_eq!(
            state.get_account(&requester.address()).unwrap().balance,
            990
        );

        let stale_error = state
            .validate_v3_transaction_admission(&alternative)
            .unwrap_err();
        assert!(
            stale_error
                .to_string()
                .contains("finalize certificate does not match stored terminal output")
        );
        assert!(!state.native_transaction_still_admissible(&alternative));
        assert!(
            state
                .validate_v3_block_admission(std::slice::from_ref(&alternative))
                .is_err()
        );
        let root_after_finalization = state.get_state_root();
        let history_peers: Vec<_> = keys
            .iter()
            .skip(1)
            .map(|key| (key.address(), 5_000_000))
            .collect();
        let history = ConsensusManager::new_with_keypair(
            keys[0].address(),
            5_000_000,
            1,
            false,
            &history_peers,
            keys[0].clone(),
        );
        history.engine.install_consensus_domain(domain).unwrap();
        history.engine.install_recovery_cursor(0).unwrap();
        let pending = dashmap::DashMap::new();
        assert!(state.receipts.contains_key(&request_tx.hash.0));
        assert!(
            state
                .validate_v3_dag_availability(&[request_tx.clone(), alternative.clone()])
                .is_err()
        );
        let mut old_hashes = vec![request_tx.hash, alternative.hash];
        old_hashes.sort_by_key(|hash| hash.0);
        let old_parents: Vec<_> = (0..6)
            .map(|i| dag_block(i, 1, vec![], old_hashes.clone()))
            .collect();
        history
            .import_history_durably(
                &state,
                &old_parents,
                &[request_tx.clone(), alternative.clone()],
                &pending,
                &dashmap::DashMap::new(),
            )
            .unwrap();
        assert!(pending.contains_key(&alternative.hash.0));
        assert_eq!(history.engine.current_round(), 2);
        for block in parents.iter().skip(3) {
            verify_peer_dag_availability(
                &state,
                &block.transactions,
                std::slice::from_ref(&alternative),
            )
            .unwrap();
            late.receive_block(block).unwrap();
        }
        assert!(
            late.advance_round(),
            "late valid parent delivery must recover the round"
        );
        assert_eq!(late.current_round(), early.current_round());
        let children: Vec<_> = (0..6)
            .map(|i| {
                dag_block(
                    i,
                    2,
                    parents.iter().map(|block| block.hash).collect(),
                    vec![],
                )
            })
            .collect();
        for block in &children {
            early.receive_block(block).unwrap();
            late.receive_block(block).unwrap();
        }
        assert!(early.advance_round());
        assert!(late.advance_round());
        assert_eq!(late.current_round(), 3);
        assert!(availability(&winner).is_ok());
        let mut forged_receipted = winner.clone();
        forged_receipted.gas_limit = u64::MAX;
        forged_receipted.sig_verified = true;
        assert!(availability(&forged_receipted).is_err());
        assert_eq!(state.get_state_root(), root_after_finalization);
        assert!(!state.receipts.contains_key(&alternative.hash.0));
        mempool.insert(alternative.clone()).unwrap();
        let mut invalid_ordinary = ordinary.clone();
        invalid_ordinary.fee = 0;
        state
            .sign_transaction(&mut invalid_ordinary, &requester)
            .unwrap();
        assert!(
            state
                .validate_v3_transaction_admission(&invalid_ordinary)
                .is_err()
        );
        mempool.insert(invalid_ordinary.clone()).unwrap();
        let selected = select_native_transactions_and_requeue(
            &state,
            &mempool.drain(16),
            &mempool,
            &admission,
            &mut since,
        );
        assert!(selected.is_empty(), "the stale native body cannot execute");
        assert_eq!(mempool.len(), 1);
        assert!(mempool.contains(&ordinary.hash));
        assert!(!mempool.contains(&alternative.hash));
        assert!(!mempool.contains(&invalid_ordinary.hash));

        let selected = select_native_transactions_and_requeue(
            &state,
            &mempool.drain(16),
            &mempool,
            &admission,
            &mut since,
        );
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].hash, ordinary.hash);
        let block = commit(&selected);
        assert_eq!(block.tx_hashes, vec![ordinary.hash]);
        assert!(state.receipts.get(&ordinary.hash.0).unwrap().success);
        assert!(availability(&ordinary).is_ok());
        assert!(state.validate_v3_transaction_admission(&ordinary).is_err());
        assert_eq!(
            state.get_account(&requester.address()).unwrap().balance,
            988
        );
        assert!(mempool.is_empty());
        assert!(
            select_native_transactions_and_requeue(
                &state,
                &[ordinary, alternative],
                &mempool,
                &admission,
                &mut since,
            )
            .is_empty()
        );
        assert!(mempool.is_empty());
    }

    #[test]
    fn a_body_that_never_executed_is_kept_for_the_retention_window() {
        let key = KeyPair::generate_ed25519();
        let omitted = signed_transfer(&key, 31, 0);
        let executed = signed_transfer(&key, 32, 1);
        let pending = dashmap::DashMap::new();
        let latest = dashmap::DashMap::new();
        retain_dag_preimages(&pending, &latest, 100, &[omitted.clone(), executed.clone()]).unwrap();
        let ran = |hash: &[u8; 32]| *hash == executed.hash.0;

        assert_eq!(
            prune_irreversible_preimages(&pending, &latest, 150, 50, ran),
            1
        );
        assert!(
            !pending.contains_key(&executed.hash.0),
            "the state serves executed bodies"
        );
        let state = StateDB::new();
        let wanted = std::collections::HashSet::from([omitted.hash.0]);
        assert_eq!(
            history_bodies(&pending, &state, &wanted)
                .iter()
                .map(|t| t.hash)
                .collect::<Vec<_>>(),
            vec![omitted.hash],
            "a peer can still be served the omitted body"
        );
        assert_eq!(
            prune_irreversible_preimages(&pending, &latest, 5_000, 101, ran),
            1
        );
        assert!(pending.is_empty());
    }

    struct HistoryDurabilityFixture {
        _directory: tempfile::TempDir,
        manager: ConsensusManager,
        state: StateDB,
        keys: Vec<KeyPair>,
        domain: arc_consensus::ConsensusDomain,
        store: crate::recovery_dag_wal::GenerationStore,
        generation: crate::recovery_dag_wal::VerifiedGeneration,
        pending: dashmap::DashMap<[u8; 32], Transaction>,
        latest: dashmap::DashMap<[u8; 32], u64>,
    }

    impl HistoryDurabilityFixture {
        fn new() -> Self {
            use crate::recovery_dag_wal::{
                BaselineState, DagCursor, GenerationInput, GenerationStore, RecoveryDagBinding,
                RetentionLimits,
            };
            let directory = tempfile::tempdir().unwrap();
            let keys: Vec<_> = (0..6)
                .map(|index| {
                    KeyPair::from_ed25519_secret_bytes(
                        &hash_bytes(format!("durable-history-{index}").as_bytes()).0,
                    )
                })
                .collect();
            let peers: Vec<_> = keys
                .iter()
                .enumerate()
                .skip(1)
                .map(|(index, key)| (key.address(), if index < 4 { 6_666_667 } else { 6_666_666 }))
                .collect();
            let mut manager = ConsensusManager::new_with_keypair(
                keys[0].address(),
                6_666_667,
                4,
                false,
                &peers,
                keys[0].clone(),
            );
            let domain = arc_consensus::ConsensusDomain::new(hash_bytes(b"history-test"), 1, 1);
            manager.engine.install_consensus_domain(domain).unwrap();
            manager.engine.install_recovery_cursor(64).unwrap();
            let state = StateDB::with_genesis(&[(keys[0].address(), 1)]);
            let (baseline, _) = state
                .execute_block_verified_at(&[], keys[0].address(), 64)
                .unwrap();
            assert_ne!(baseline.hash, Hash256::ZERO);
            assert_ne!(baseline.header.state_root, Hash256::ZERO);
            assert_eq!(baseline.header.state_root, state.get_state_root());
            let store = GenerationStore::new(directory.path().join("dag"));
            let generation = store
                .create_initial(
                    GenerationInput {
                        binding: RecoveryDagBinding {
                            recovery_manifest_hash: hash_bytes(b"history-manifest"),
                            recovery_domain: domain.domain_hash,
                            validator_set_commitment: manager.engine.frozen_validator_set_hash(),
                        },
                        baseline_state: BaselineState {
                            height: baseline.header.height,
                            block_hash: baseline.hash,
                            state_root: baseline.header.state_root,
                            dag_anchor: None,
                        },
                        dag_cursor: DagCursor {
                            committed_block_count: 0,
                            next_dag_round: 65,
                            current_round: 65,
                            retention_floor_round: 65,
                            retention_ceiling_round: 100,
                        },
                        retention_limits: RetentionLimits {
                            max_records: 100,
                            max_payload_bytes: 1_048_576,
                        },
                    },
                    Vec::new(),
                )
                .unwrap();
            let writer = store
                .open_current_active_writer(&generation.manifest.binding, generation.pin)
                .unwrap();
            manager.recovery_dag_writer = Some(Arc::new(parking_lot::Mutex::new(Some(writer))));
            Self {
                _directory: directory,
                manager,
                state,
                keys,
                domain,
                store,
                generation,
                pending: dashmap::DashMap::new(),
                latest: dashmap::DashMap::new(),
            }
        }

        fn block(
            &self,
            author: usize,
            round: u64,
            parents: Vec<Hash256>,
            mut transactions: Vec<Hash256>,
        ) -> arc_consensus::DagBlock {
            transactions.sort_by_key(|hash| hash.0);
            let mut block = arc_consensus::DagBlock {
                author: self.keys[author].address(),
                round,
                parents,
                ordering_commitment: arc_consensus::DagBlock::compute_ordering_commitment(
                    &transactions,
                ),
                transactions,
                timestamp: round,
                hash: Hash256::ZERO,
                signature: Vec::new(),
            };
            block.hash = block.compute_hash_in_domain(&self.domain);
            block.signature =
                bincode::serialize(&self.keys[author].sign(&block.hash).unwrap()).unwrap();
            block
        }

        fn import(
            &self,
            blocks: &[arc_consensus::DagBlock],
            attached: &[Transaction],
        ) -> Result<u64, arc_consensus::ConsensusError> {
            self.manager.import_history_durably(
                &self.state,
                blocks,
                attached,
                &self.pending,
                &self.latest,
            )
        }

        fn records(&self) -> Vec<RetainedDagRecord> {
            drop(
                self.manager
                    .recovery_dag_writer
                    .as_ref()
                    .unwrap()
                    .lock()
                    .take(),
            );
            let mut records = Vec::new();
            self.store
                .stream_current_generation_and_active(
                    &self.generation.manifest.binding,
                    self.generation.pin,
                    |record| {
                        records.push(record);
                        Ok(())
                    },
                )
                .unwrap();
            records
        }
    }

    #[test]
    fn recovery_absence_certificate_is_fsynced_before_engine_authority() {
        use crate::recovery_dag_wal::RetainedRecordKind;
        use arc_consensus::view_change::{
            AbsenceReason, SkipCertificate, SkipVote, validator_set_hash,
        };

        let f = HistoryDurabilityFixture::new();
        let absentee = f.keys[5].address();
        let set = f.manager.engine.frozen_validator_set();
        let set_hash = validator_set_hash(&set);
        let votes = f.keys[..4]
            .iter()
            .map(|key| {
                SkipVote::sign(
                    f.domain,
                    set_hash,
                    65,
                    absentee,
                    AbsenceReason::NoBlock,
                    set.quorum,
                    key,
                )
                .unwrap()
            })
            .collect();
        drop(set);
        let certificate = SkipCertificate::new(
            f.domain,
            set_hash,
            65,
            absentee,
            AbsenceReason::NoBlock,
            votes,
        );

        assert!(
            f.manager
                .adopt_absence_certificate(&f.state, &certificate, &mut None, None,)
        );
        assert!(f.manager.engine.has_skip_certificate(65, &absentee));
        let writer_slot = f.manager.recovery_dag_writer.as_ref().unwrap();
        assert_eq!(
            writer_slot
                .lock()
                .as_ref()
                .unwrap()
                .inspection()
                .record_count,
            1
        );
        assert!(
            f.manager
                .adopt_absence_certificate(&f.state, &certificate, &mut None, None,)
        );
        assert_eq!(
            writer_slot
                .lock()
                .as_ref()
                .unwrap()
                .inspection()
                .record_count,
            1,
            "already-held certificates must not append a duplicate"
        );

        let future_votes = f.keys[..4]
            .iter()
            .map(|key| {
                SkipVote::sign(
                    f.domain,
                    set_hash,
                    66,
                    absentee,
                    AbsenceReason::NoBlock,
                    26_666_667,
                    key,
                )
                .unwrap()
            })
            .collect();
        let future_certificate = SkipCertificate::new(
            f.domain,
            set_hash,
            66,
            absentee,
            AbsenceReason::NoBlock,
            future_votes,
        );
        assert!(f.manager.adopt_absence_certificate(
            &f.state,
            &future_certificate,
            &mut None,
            None,
        ));
        assert!(f.manager.engine.has_skip_certificate(66, &absentee));

        let child = f.block(0, 66, vec![], vec![]);
        let certificates = history_absence_certificates(&f.manager.engine, &[child.clone(), child]);
        assert_eq!(
            certificates.len(),
            2,
            "duplicate blocks must not duplicate certificates"
        );
        assert!(history_absence_certificates(&f.manager.engine, &[]).is_empty());
        let catchup = HistoryDurabilityFixture::new();
        let parents: Vec<_> = (0..5)
            .map(|author| f.block(author, 65, vec![], vec![]))
            .collect();
        catchup.import(&parents, &[]).unwrap();
        assert_eq!(catchup.manager.engine.current_round(), 65);
        let dependent = f.block(
            0,
            66,
            parents.iter().map(|block| block.hash).collect(),
            vec![],
        );
        assert!(catchup.manager.engine.receive_block(&dependent).is_err());
        for certificate in certificates {
            assert!(catchup.manager.adopt_absence_certificate(
                &catchup.state,
                &certificate,
                &mut None,
                None,
            ));
        }
        catchup.import(&parents, &[]).unwrap();
        assert_eq!(catchup.manager.engine.current_round(), 66);
        catchup.manager.engine.receive_block(&dependent).unwrap();

        let records = f.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, RetainedRecordKind::AbsenceCertificate);
        assert_eq!(records[0].round, 65);
        assert_eq!(
            bincode::deserialize::<SkipCertificate>(&records[0].payload).unwrap(),
            certificate
        );
        assert_eq!(records[1].round, 66);
        assert_eq!(
            bincode::deserialize::<SkipCertificate>(&records[1].payload).unwrap(),
            future_certificate
        );

        let above_ceiling = HistoryDurabilityFixture::new();
        let above_votes = above_ceiling.keys[..4]
            .iter()
            .map(|key| {
                SkipVote::sign(
                    above_ceiling.domain,
                    set_hash,
                    101,
                    above_ceiling.keys[5].address(),
                    AbsenceReason::NoBlock,
                    26_666_667,
                    key,
                )
                .unwrap()
            })
            .collect();
        let above_certificate = SkipCertificate::new(
            above_ceiling.domain,
            set_hash,
            101,
            above_ceiling.keys[5].address(),
            AbsenceReason::NoBlock,
            above_votes,
        );
        assert!(!above_ceiling.manager.adopt_absence_certificate(
            &above_ceiling.state,
            &above_certificate,
            &mut None,
            None,
        ));
        assert!(
            !above_ceiling
                .manager
                .engine
                .has_skip_certificate(101, &above_ceiling.keys[5].address())
        );

        let mut missing_writer = HistoryDurabilityFixture::new();
        missing_writer.manager.recovery_dag_writer = None;
        let fresh_certificate = SkipCertificate::new(
            missing_writer.domain,
            set_hash,
            65,
            missing_writer.keys[5].address(),
            AbsenceReason::NoBlock,
            f.keys[..4]
                .iter()
                .map(|key| {
                    SkipVote::sign(
                        missing_writer.domain,
                        set_hash,
                        65,
                        missing_writer.keys[5].address(),
                        AbsenceReason::NoBlock,
                        26_666_667,
                        key,
                    )
                    .unwrap()
                })
                .collect(),
        );
        assert!(!missing_writer.manager.adopt_absence_certificate(
            &missing_writer.state,
            &fresh_certificate,
            &mut None,
            None,
        ));
        assert!(
            !missing_writer
                .manager
                .engine
                .has_skip_certificate(65, &missing_writer.keys[5].address())
        );
    }

    #[test]
    fn history_parents_are_durable_before_subsequent_live_child_and_reopen() {
        use crate::recovery_dag_wal::RetainedRecordKind;
        let f = HistoryDurabilityFixture::new();
        let parents: Vec<_> = (0..6).map(|i| f.block(i, 65, vec![], vec![])).collect();
        for block in parents.iter().take(4) {
            f.manager.engine.receive_block(block).unwrap();
            f.manager.persist_dag_block(&f.state, block, &[]).unwrap();
        }
        assert!(!f.manager.engine.advance_round());
        assert_eq!(f.import(&parents, &[]).unwrap(), 66);
        let child = f.block(0, 66, parents.iter().map(|b| b.hash).collect(), vec![]);
        f.manager.engine.receive_block(&child).unwrap();
        f.manager.persist_dag_block(&f.state, &child, &[]).unwrap();
        assert!(f.import(&parents, &[]).is_err());
        let records = f.records();
        assert_eq!(records.len(), 7);
        let restarted = ConsensusEngine::new_with_keypair(
            f.manager.engine.validator_set(),
            f.keys[0].address(),
            f.keys[0].clone(),
        );
        restarted.install_consensus_domain(f.domain).unwrap();
        restarted.install_recovery_cursor(64).unwrap();
        for record in &records {
            assert_eq!(record.kind, RetainedRecordKind::DagBlock);
            let block = bincode::deserialize(&record.payload).unwrap();
            restarted.receive_block(&block).unwrap();
            restarted.advance_round();
        }
        assert!(restarted.get_block(&child.hash).is_some());
        assert_eq!(restarted.current_round(), 66);
        let old = ConsensusEngine::new_with_keypair(
            f.manager.engine.validator_set(),
            f.keys[0].address(),
            f.keys[0].clone(),
        );
        old.install_consensus_domain(f.domain).unwrap();
        old.install_recovery_cursor(64).unwrap();
        for parent in parents.iter().take(4) {
            old.receive_block(parent).unwrap();
        }
        assert!(matches!(
            old.receive_block(&child),
            Err(arc_consensus::ConsensusError::MissingParents { missing: 2, .. })
        ));
    }

    #[test]
    fn history_below_pinned_floor_is_not_appended_and_floor_children_reopen() {
        use crate::recovery_dag_wal::{
            BaselineState, DagCursor, GenerationInput, RetainedRecordKind,
        };
        let mut f = HistoryDurabilityFixture::new();
        let mut parents = Vec::new();
        let mut old_parents = Vec::new();
        for round in 65..=67 {
            let blocks: Vec<_> = (0..6)
                .map(|i| f.block(i, round, parents.clone(), vec![]))
                .collect();
            for block in &blocks {
                f.manager.engine.receive_block(block).unwrap();
                f.manager.persist_dag_block(&f.state, block, &[]).unwrap();
            }
            assert!(f.manager.engine.advance_round());
            parents = blocks.iter().map(|b| b.hash).collect();
            if round == 65 {
                old_parents = blocks;
            }
        }
        let committed = f.manager.engine.try_commit();
        assert_eq!(committed.len(), 1);
        let leader = &committed[0];
        let (baseline, _) = f
            .state
            .execute_block_adaptive_at_with_proof(
                &[],
                leader.author,
                leader.timestamp,
                leader.state_decision_commitment(&f.domain),
            )
            .unwrap();
        assert_eq!(
            baseline.header.height,
            f.generation.manifest.baseline_state.height + 1
        );
        assert_eq!(
            baseline.header.parent_hash,
            f.generation.manifest.baseline_state.block_hash
        );
        f.manager.persist_dag_commit(&f.state, leader).unwrap();
        let records = f.records();
        let summary = f
            .store
            .stream_current_generation_and_active(
                &f.generation.manifest.binding,
                f.generation.pin,
                |_| Ok(()),
            )
            .unwrap();
        let retained: Vec<_> = records
            .into_iter()
            .filter(|r| r.round >= 66 && r.kind != RetainedRecordKind::Commit)
            .collect();
        f.generation = f
            .store
            .append_compacted(
                f.generation.pin,
                summary.active_pin,
                GenerationInput {
                    binding: f.generation.manifest.binding.clone(),
                    baseline_state: BaselineState {
                        height: baseline.header.height,
                        block_hash: baseline.hash,
                        state_root: baseline.header.state_root,
                        dag_anchor: None,
                    },
                    dag_cursor: DagCursor {
                        committed_block_count: 1,
                        next_dag_round: 66,
                        current_round: 68,
                        retention_floor_round: 66,
                        retention_ceiling_round: 100,
                    },
                    retention_limits: f.generation.manifest.retained_records.limits,
                },
                retained.clone(),
            )
            .unwrap();
        let reopened = {
            let engine = ConsensusEngine::new_with_keypair(
                f.manager.engine.validator_set(),
                f.keys[0].address(),
                f.keys[0].clone(),
            );
            engine.install_consensus_domain(f.domain).unwrap();
            engine.install_recovery_cursor(64).unwrap();
            engine
                .install_recovery_generation_cursor(66, 68, 66)
                .unwrap();
            for record in &retained {
                let block = bincode::deserialize(&record.payload).unwrap();
                engine.receive_block(&block).unwrap();
                engine.advance_round();
            }
            engine.finish_recovery_generation_replay().unwrap();
            engine
        };
        f.manager.engine = Arc::new(reopened);
        assert!(f.manager.engine.get_block(&old_parents[0].hash).is_none());
        f.manager
            .recovery_dag_writer
            .as_ref()
            .unwrap()
            .lock()
            .replace(
                f.store
                    .open_current_active_writer(&f.generation.manifest.binding, f.generation.pin)
                    .unwrap(),
            );
        assert_eq!(f.import(&old_parents, &[]).unwrap(), 68);
        assert!(f.manager.engine.get_block(&old_parents[0].hash).is_some());
        let after = f.records();
        assert_eq!(
            after, retained,
            "late ancestors stay behind the pinned boundary"
        );
        let reopened = ConsensusEngine::new_with_keypair(
            f.manager.engine.validator_set(),
            f.keys[0].address(),
            f.keys[0].clone(),
        );
        reopened.install_consensus_domain(f.domain).unwrap();
        reopened.install_recovery_cursor(64).unwrap();
        reopened
            .install_recovery_generation_cursor(66, 68, 66)
            .unwrap();
        for record in after {
            let block = bincode::deserialize(&record.payload).unwrap();
            reopened.receive_block(&block).unwrap();
        }
        reopened.finish_recovery_generation_replay().unwrap();
        assert_eq!(reopened.blocks_in_round(66).len(), 6);
        assert!(reopened.get_block(&old_parents[0].hash).is_none());
    }

    #[test]
    fn history_persists_exact_bodies_from_attachment_pending_and_canonical_state() {
        use crate::recovery_dag_wal::RetainedRecordKind;
        let f = HistoryDurabilityFixture::new();
        let key = KeyPair::generate_ed25519();
        let attached = signed_transfer(&key, 1, 0);
        let pending = signed_transfer(&key, 2, 1);
        let canonical = signed_transfer(&key, 3, 2);
        f.pending.insert(pending.hash.0, pending.clone());
        f.state
            .full_transactions
            .insert(canonical.hash.0, canonical.clone());
        let bodies = [&attached, &pending, &canonical];
        let blocks: Vec<_> = (0..6)
            .map(|i| {
                f.block(
                    i,
                    65,
                    vec![],
                    bodies
                        .get(i)
                        .map(|body| vec![body.hash])
                        .unwrap_or_default(),
                )
            })
            .collect();
        f.import(&blocks, std::slice::from_ref(&attached)).unwrap();
        for body in bodies {
            assert_eq!(*f.latest.get(&body.hash.0).unwrap(), 65);
            let retained = f.pending.get(&body.hash.0).unwrap();
            assert_eq!(retained.hash, body.hash);
            retained.verify_signature().unwrap();
        }
        let records = f.records();
        let mut available = std::collections::HashSet::new();
        for record in records {
            match record.kind {
                RetainedRecordKind::TransactionBody => {
                    let tx: Transaction = bincode::deserialize(&record.payload).unwrap();
                    tx.verify_signature().unwrap();
                    available.insert(tx.hash);
                }
                RetainedRecordKind::DagBlock => {
                    let block: arc_consensus::DagBlock =
                        bincode::deserialize(&record.payload).unwrap();
                    assert!(
                        block
                            .transactions
                            .iter()
                            .all(|hash| available.contains(hash))
                    );
                }
                _ => panic!("unexpected history record"),
            }
        }
        assert_eq!(available.len(), 3);
    }

    #[test]
    fn history_invalid_or_missing_preimages_do_not_mutate_dag_or_durable_log() {
        for invalid_case in 0..3 {
            let f = HistoryDurabilityFixture::new();
            let key = KeyPair::generate_ed25519();
            let mut body = signed_transfer(&key, 1, 0);
            if invalid_case == 2 {
                body.sign_in_domain(&key, &hash_bytes(b"wrong-history-transaction-domain"))
                    .unwrap();
            }
            let blocks: Vec<_> = (0..6)
                .map(|i| f.block(i, 65, vec![], vec![body.hash]))
                .collect();
            let attached = if invalid_case == 1 {
                body.signature = Signature::Ed25519 {
                    public_key: [0; 32],
                    signature: vec![0; 64],
                };
                body.sig_verified = true;
                vec![body]
            } else if invalid_case == 2 {
                vec![body]
            } else {
                vec![]
            };
            assert!(matches!(
                f.import(&blocks, &attached),
                Err(arc_consensus::ConsensusError::InvalidBlock(_))
            ));
            assert!(f.manager.engine.dag_is_empty());
            assert!(f.pending.is_empty());
            assert!(f.records().is_empty());
        }
    }

    #[test]
    fn history_thin_suffix_with_missing_body_does_not_poison_complete_prefix() {
        let f = HistoryDurabilityFixture::new();
        let mut blocks: Vec<_> = (0..6).map(|i| f.block(i, 65, vec![], vec![])).collect();
        let parents = blocks.iter().map(|b| b.hash).collect();
        let missing = hash_bytes(b"thin suffix body is unavailable");
        let thin = f.block(0, 66, parents, vec![missing]);
        blocks.push(thin.clone());
        assert_eq!(f.import(&blocks, &[]).unwrap(), 66);
        assert!(f.manager.engine.get_block(&thin.hash).is_none());
        assert_eq!(f.records().len(), 6);
        assert!(f.pending.is_empty());
    }

    #[test]
    fn history_valid_prefix_is_durable_when_later_dag_signature_is_invalid() {
        let f = HistoryDurabilityFixture::new();
        let mut blocks: Vec<_> = (0..6).map(|i| f.block(i, 65, vec![], vec![])).collect();
        blocks[5].signature = vec![0];
        assert_eq!(f.import(&blocks, &[]).unwrap(), 65);
        assert_eq!(f.manager.engine.dag_block_count(), 5);
        let persisted: Vec<_> = f.records().into_iter().map(|r| r.object_hash).collect();
        assert_eq!(
            persisted,
            blocks[..5].iter().map(|b| b.hash).collect::<Vec<_>>()
        );
    }

    #[test]
    fn history_storage_failure_stops_before_round_advance_or_child_import() {
        struct FailSecondAppend(std::sync::atomic::AtomicUsize);
        impl RecoveryDagRollover for FailSecondAppend {
            fn prepare_append(
                &self,
                _: &StateDB,
                _: &ConsensusEngine,
                writer: ActiveLogWriter,
                _: &[RetainedDagRecord],
            ) -> Result<ActiveLogWriter, String> {
                if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                    Err("injected history write failure".into())
                } else {
                    Ok(writer)
                }
            }
        }
        let mut f = HistoryDurabilityFixture::new();
        f.manager.recovery_dag_rollover = Some(Arc::new(FailSecondAppend(
            std::sync::atomic::AtomicUsize::new(0),
        )));
        let mut blocks: Vec<_> = (0..6).map(|i| f.block(i, 65, vec![], vec![])).collect();
        let parents = blocks.iter().map(|b| b.hash).collect::<Vec<_>>();
        blocks.extend((0..6).map(|i| f.block(i, 66, parents.clone(), vec![])));
        assert!(matches!(
            f.import(&blocks, &[]),
            Err(arc_consensus::ConsensusError::HistoryPersistence(_))
        ));
        assert_eq!(f.manager.engine.current_round(), 65);
        assert!(f.manager.engine.get_block(&blocks[1].hash).is_none());
        assert!(f.manager.engine.get_block(&blocks[2].hash).is_none());
        assert!(f.manager.engine.get_block(&blocks[6].hash).is_none());
        assert!(
            f.manager
                .recovery_dag_writer
                .as_ref()
                .unwrap()
                .lock()
                .is_none()
        );
        let records = f.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].object_hash, blocks[0].hash);
    }

    #[test]
    fn periodic_replay_repairs_two_round_gap_with_one_slot_transport() {
        use arc_consensus::view_change::{
            AbsenceReason, SkipCertificate, SkipVote, validator_set_hash,
        };
        let keys: Vec<_> = (0..6)
            .map(|index| {
                KeyPair::from_ed25519_secret_bytes(
                    &hash_bytes(format!("two-round-replay-{index}").as_bytes()).0,
                )
            })
            .collect();
        let set = ValidatorSet::new(
            keys.iter()
                .enumerate()
                .map(|(index, key)| {
                    Validator::new(key.address(), arc_consensus::STAKE_ARC, index as u16).unwrap()
                })
                .collect(),
            1,
        );
        let domain = arc_consensus::ConsensusDomain::new(hash_bytes(b"two-round-replay"), 1, 1);
        let engines: Vec<_> = keys
            .iter()
            .map(|key| {
                let engine =
                    ConsensusEngine::new_with_keypair(set.clone(), key.address(), key.clone());
                engine.install_consensus_domain(domain).unwrap();
                engine.install_recovery_cursor(100).unwrap();
                engine
            })
            .collect();
        let parents: Vec<_> = engines
            .iter()
            .enumerate()
            .map(|(index, engine)| engine.propose_block(vec![], 1_000 + index as u64).unwrap())
            .collect();
        for (target, engine) in engines.iter().enumerate() {
            for (source, block) in parents.iter().enumerate() {
                if target != source && !(target == 5 && source == 0) {
                    engine.receive_block(block).unwrap();
                }
            }
            assert_eq!(engine.advance_round(), target != 5);
        }
        let children: Vec<_> = engines
            .iter()
            .take(5)
            .enumerate()
            .map(|(index, engine)| engine.propose_block(vec![], 2_000 + index as u64).unwrap())
            .collect();
        for (target, engine) in engines.iter().take(5).enumerate() {
            for (source, child) in children.iter().enumerate() {
                if source != target {
                    engine.receive_block(child).unwrap();
                }
            }
        }
        let votes = keys
            .iter()
            .take(5)
            .map(|key| {
                SkipVote::sign(
                    domain,
                    validator_set_hash(&set),
                    102,
                    keys[5].address(),
                    AbsenceReason::NoBlock,
                    5 * arc_consensus::STAKE_ARC,
                    key,
                )
                .unwrap()
            })
            .collect();
        engines[0]
            .register_skip_certificate(SkipCertificate::new(
                domain,
                validator_set_hash(&set),
                102,
                keys[5].address(),
                AbsenceReason::NoBlock,
                votes,
            ))
            .unwrap();
        assert!(engines[0].advance_round());
        assert_eq!(engines[0].current_round(), 103);
        assert_eq!(engines[0].last_committed_round(), 101);
        assert!(
            engines[5].receive_block(&children[0]).is_err(),
            "the old current/parent-only replay cannot supply round101"
        );

        let pending = dashmap::DashMap::new();
        let mut rounds = std::collections::BTreeSet::new();
        let mut retry = false;
        let mut next = Instant::now();
        assert!(queue_due_recovery_replay(
            &engines[0],
            keys[0].address(),
            &mut rounds,
            &mut retry,
            next
        ));
        assert_eq!(
            rounds.iter().copied().collect::<Vec<_>>(),
            vec![101, 102, 103]
        );
        let (sender, mut receiver) = mpsc::channel(1);
        let mut sent = Vec::new();
        for _tick in 0..4 {
            let drain = try_drain_recovery_reconnect_replay(
                &engines[0],
                keys[0].address(),
                &mut rounds,
                &pending,
                Some(&sender),
                None,
            )
            .unwrap();
            if let Ok(OutboundMessage::BroadcastDagBlock {
                block,
                transactions,
            }) = receiver.try_recv()
            {
                assert!(transactions.is_empty());
                let expected = if block.round == 101 {
                    &parents[0]
                } else {
                    &children[0]
                };
                assert_eq!(
                    bincode::serialize(&block).unwrap(),
                    bincode::serialize(expected).unwrap()
                );
                engines[5].receive_block(&block).unwrap();
                sent.push(block.round);
            }
            if drain == RecoveryReplayDrain::Complete {
                complete_recovery_rebroadcast(&mut retry, &mut next);
                break;
            }
        }
        assert_eq!(sent, vec![101, 102]);
        assert!(rounds.is_empty());
        assert!(
            !queue_due_recovery_replay(
                &engines[0],
                keys[0].address(),
                &mut rounds,
                &mut retry,
                next
            ),
            "a drained window must not refill immediately"
        );
        assert!(rounds.is_empty());
        assert!(
            engines[5].advance_round(),
            "event processing resumes after the bounded replay"
        );
        for child in children.iter().skip(1) {
            engines[5].receive_block(child).unwrap();
        }
        let recovered = engines[5].propose_block(vec![], 3_000).unwrap();
        for engine in engines.iter().take(5) {
            engine.receive_block(&recovered).unwrap();
        }
        for engine in engines.iter().skip(1) {
            assert!(engine.advance_round());
        }
        let next_blocks: Vec<_> = engines
            .iter()
            .enumerate()
            .map(|(index, engine)| engine.propose_block(vec![], 4_000 + index as u64).unwrap())
            .collect();
        for (target, engine) in engines.iter().enumerate() {
            for (source, block) in next_blocks.iter().enumerate() {
                if source != target {
                    engine.receive_block(block).unwrap();
                }
            }
            assert!(
                !engine.try_commit().is_empty(),
                "all six regain certified commit progress"
            );
        }
    }

    #[test]
    fn replay_predecessor_retains_receipted_and_unexecuted_preimages() {
        let key = KeyPair::generate_ed25519();
        let executed = signed_transfer(&key, 41, 0);
        let omitted = signed_transfer(&key, 42, 1);
        let pending = dashmap::DashMap::new();
        let latest = dashmap::DashMap::new();
        retain_dag_preimages(&pending, &latest, 100, &[executed.clone(), omitted.clone()]).unwrap();
        let ran = |hash: &[u8; 32]| *hash == executed.hash.0;
        assert_eq!(prune_dag_preimages(&pending, &latest, 101, 0, ran), 0);
        assert_eq!(
            exact_dag_preimages(&pending, &[executed.hash, omitted.hash])
                .unwrap()
                .len(),
            2
        );
        assert_eq!(prune_dag_preimages(&pending, &latest, 102, 0, ran), 1);
        assert!(pending.contains_key(&omitted.hash.0));
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
                engine.install_consensus_domain(domain).unwrap();
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

        for (target, engine) in engines.iter().enumerate() {
            for (source, proposal) in bootstrap_proposals.iter().enumerate() {
                if source != target {
                    engine.receive_block(proposal).unwrap();
                }
            }
            assert!(engine.advance_round());
            assert_eq!(engine.current_round(), 102);
        }

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
            queue_recovery_reconnect_replay(&engines[source], validators[source], &mut queued);
            assert_eq!(queued.iter().copied().collect::<Vec<_>>(), vec![101, 102]);

            let (sender, mut receiver) = mpsc::channel(if source == 0 { 1 } else { 2 });
            if source == 0 {
                assert_eq!(
                    try_drain_recovery_reconnect_replay(
                        &engines[source],
                        validators[source],
                        &mut queued,
                        &pending,
                        Some(&sender),
                        None,
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
                        None,
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
                        None,
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
        assert!(!mgr.tier.can_produce_blocks());
    }

    #[test]
    fn test_consensus_manager_below_minimum_is_spark_observer() {
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

    #[tokio::test]
    async fn lifecycle_shutdown_prevents_another_consensus_tick() {
        let local_key = KeyPair::generate_ed25519();
        let manager = ConsensusManager::new_with_keypair(
            local_key.address(),
            5_000_000,
            4,
            false,
            &[],
            local_key,
        );
        let state = Arc::new(StateDB::with_genesis(&[]));
        let initial_height = state.height();
        let mempool = Arc::new(Mempool::new(16));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        shutdown_tx.send(true).unwrap();

        tokio::time::timeout(
            tokio::time::Duration::from_secs(1),
            manager.run_consensus_loop_with_shutdown(
                state.clone(),
                mempool,
                None,
                None,
                None,
                shutdown_rx,
            ),
        )
        .await
        .expect("pre-signalled consensus shutdown must return promptly");

        assert_eq!(
            state.height(),
            initial_height,
            "a pre-signalled lifecycle barrier must prevent a new proposal or commit"
        );
    }
}
