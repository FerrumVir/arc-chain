pub mod block_stm;
pub mod checkpoint_adoption;
pub mod gpu_state;
pub mod inference_contract_state;
pub use inference_contract_state::{
    AllowedExecution, InferenceAdmissionContext, IsolatedInferenceLedger, IsolatedTransitionResult,
    NativeInferencePendingSnapshot, NativeInferenceReceiptSnapshot, NativeInferenceTransactionLink,
    NativeMigrationRecord, NativeSelectionRule, validate_native_inference_activation,
};
pub mod io_backend;
pub mod jmt_store;
pub mod light_client;
pub mod mmap_state;
pub mod recovery;
pub mod simd_parse;
pub mod snapshot;
pub mod wal;

use arc_crypto::{Hash256, IncrementalMerkle, MerkleTree, hash_bytes, hash_pair};
use arc_types::economics::StateRentConfig;
use arc_types::transaction::{
    CapacityAdvertisementBody, GasMeter, InferenceEscrowOpenBody, MIN_MODEL_REGISTRATION_FEE,
    ModelRegistrationBody, ModelRequestBody, ShardCoverageClaimBody, gas_costs,
};
use arc_types::{
    Account, Address, Block, BlockHeader, Identity, IdentityLevel, Transaction, TransferBody,
    TxBody, TxReceipt, TxType,
};

use crate::jmt_store::JmtStateTree;
use dashmap::{DashMap, DashSet};
use light_client::{HeaderProof, LightSnapshot, StateProof, TxInclusionProof};
use parking_lot::RwLock;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

pub use wal::{
    PersistenceConfig, Snapshot, WalEntry, WalError, WalOp, WalWriter, find_last_checkpoint,
    latest_block_height_in_wal_dir, read_wal, read_wal_dir, read_wal_strict,
};

pub const V3_MIN_TRANSFER_FEE: u64 = 1;
pub const V3_MAX_TRANSACTION_BYTES: u64 = 64 * 1024;
pub const V3_MAX_TRANSACTIONS_PER_BLOCK: usize = 1_024;
pub const V3_MAX_TRANSACTIONS_PER_SENDER_PER_BLOCK: usize = 1;

pub const V3_COMMUNITY_REWARD_EPOCH_BLOCKS: u64 = 216_000;
pub const V3_COMMUNITY_REWARD_MAX_PER_BLOCK: u64 = 1;
pub const V3_COMMUNITY_REWARD_MAX_PER_EPOCH: u64 = 40;
pub const V3_COMMUNITY_REWARD_MAX_PER_WORKER_EPOCH: u64 = 8;
pub const V3_COMMUNITY_REWARD_MAX_PER_COORDINATOR_EPOCH: u64 = 16;

pub fn v3_fee_treasury_address() -> Address {
    hash_bytes(b"arc-treasury")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CommunityRewardIssuancePolicy {
    pub reward_amount: u64,
    pub epoch_blocks: u64,
    pub max_per_block: u64,
    pub max_per_epoch: u64,
    pub max_per_worker_epoch: u64,
    pub max_per_coordinator_epoch: u64,
}

pub const fn community_reward_issuance_policy() -> CommunityRewardIssuancePolicy {
    CommunityRewardIssuancePolicy {
        reward_amount: arc_types::economics::INFERENCE_ATTESTATION_REWARD,
        epoch_blocks: V3_COMMUNITY_REWARD_EPOCH_BLOCKS,
        max_per_block: V3_COMMUNITY_REWARD_MAX_PER_BLOCK,
        max_per_epoch: V3_COMMUNITY_REWARD_MAX_PER_EPOCH,
        max_per_worker_epoch: V3_COMMUNITY_REWARD_MAX_PER_WORKER_EPOCH,
        max_per_coordinator_epoch: V3_COMMUNITY_REWARD_MAX_PER_COORDINATOR_EPOCH,
    }
}

pub fn community_reward_issuance_policy_hash() -> Hash256 {
    let policy = community_reward_issuance_policy();
    let bytes = bincode::serialize(&policy).expect("issuance policy is serializable");
    let mut hasher = blake3::Hasher::new_derive_key("ARC-community-reward-issuance-policy-v3");
    hasher.update(&bytes);
    Hash256(*hasher.finalize().as_bytes())
}

pub const fn community_reward_epoch_index(block_height: u64) -> u64 {
    block_height / V3_COMMUNITY_REWARD_EPOCH_BLOCKS
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CommunityRewardBudgetStatus {
    pub block_height: u64,
    pub epoch: u64,
    pub issued_this_block: u64,
    pub remaining_this_block: u64,
    pub issued_this_epoch: u64,
    pub remaining_this_epoch: u64,
    pub worker_issued_this_epoch: u64,
    pub worker_remaining_this_epoch: u64,
    pub coordinator_issued_this_epoch: u64,
    pub coordinator_remaining_this_epoch: u64,
    pub policy_hash: Hash256,
}

#[derive(Debug)]
struct CommunityRewardBudgetCandidates {
    block: (Address, Account),
    epoch: (Address, Account),
    worker: (Address, Account),
    coordinator: (Address, Account),
}

#[derive(Error, Debug)]
pub enum StateError {
    #[error("account not found: {0:?}")]
    AccountNotFound(Address),
    #[error("insufficient balance: have {have}, need {need}")]
    InsufficientBalance { have: u64, need: u64 },
    #[error("invalid nonce: expected {expected}, got {got}")]
    InvalidNonce { expected: u64, got: u64 },
    #[error("execution error: {0}")]
    ExecutionError(String),
    #[error("contract not found: {0:?}")]
    ContractNotFound(Address),
    #[error("persistence error: {0}")]
    PersistenceError(String),
    #[error("chunk verification failed: BLAKE3 proof mismatch")]
    ChunkVerificationFailed,
    #[error("chunk index {index} out of range (total: {total})")]
    ChunkOutOfRange { index: u32, total: u32 },
    #[error("invalid snapshot manifest")]
    InvalidManifest,
    #[error("snapshot incomplete: {received}/{total} chunks received")]
    SnapshotIncomplete { received: u32, total: u32 },
    #[error("sync incomplete: {received}/{total} chunks received")]
    SyncIncomplete { received: u32, total: u32 },
    #[error("state root mismatch: expected {expected}, computed {computed}")]
    StateRootMismatch {
        expected: Hash256,
        computed: Hash256,
    },
    #[error(
        "unauthenticated legacy peer snapshots cannot mutate persistent state; use a quorum-certified ARCCHKPT import"
    )]
    UnauthenticatedPersistentStateSync,
}

fn uuid_staging_name(name: &str, prefix: &str, suffix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(suffix))
        .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok())
}

fn internal_wal_target_name(name: &str) -> bool {
    if name == "state.wal" || uuid_staging_name(name, ".state.wal.genesis-", ".tmp") {
        return true;
    }
    let Some(segment) = name
        .strip_prefix("wal-")
        .and_then(|value| value.strip_suffix(".bin"))
    else {
        return false;
    };
    segment.len() == 8 && segment.bytes().all(|byte| byte.is_ascii_digit())
}

fn wal_file_staging_name(name: &str, operation: &str) -> bool {
    let Some(value) = name.strip_prefix('.') else {
        return false;
    };
    let delimiter = format!(".{operation}-");
    let Some((target, identifier)) = value.rsplit_once(&delimiter) else {
        return false;
    };
    let Some(identifier) = identifier.strip_suffix(".tmp") else {
        return false;
    };
    internal_wal_target_name(target) && uuid::Uuid::parse_str(identifier).is_ok()
}

fn state_staging_name(name: &str) -> bool {
    if uuid_staging_name(name, ".state.wal.genesis-", ".tmp")
        || uuid_staging_name(name, ".state.wal.quarantine-rebarrier-", "")
        || wal_file_staging_name(name, "create")
        || wal_file_staging_name(name, "rebarrier")
    {
        return true;
    }
    let Some(value) = name.strip_prefix(".state.wal.quarantine-tmp-") else {
        return false;
    };
    let Some((pid, serial)) = value.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && pid.parse::<u32>().is_ok()
        && !serial.is_empty()
        && serial.bytes().all(|byte| byte.is_ascii_digit())
        && serial.parse::<u32>().is_ok_and(|serial| serial < 100)
}

fn cleanup_state_staging_files(wal_dir: &Path) -> Result<(), StateError> {
    wal::restore_interrupted_wal_namespace_rebarriers(wal_dir).map_err(|error| {
        StateError::PersistenceError(format!(
            "failed to restore interrupted state WAL namespace in {wal_dir:?}: {error}"
        ))
    })?;
    for entry in std::fs::read_dir(wal_dir).map_err(|error| {
        StateError::PersistenceError(format!(
            "failed to inspect state staging directory {wal_dir:?}: {error}"
        ))
    })? {
        let entry = entry.map_err(|error| StateError::PersistenceError(error.to_string()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !state_staging_name(&name) {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| StateError::PersistenceError(error.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StateError::PersistenceError(format!(
                "state WAL staging artifact is not a regular file: {path:?}"
            )));
        }
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = ?path, %error, "deferring cleanup of incomplete state WAL staging file");
            }
            continue;
        }
        if let Err(error) = arc_crypto::secret_file::sync_parent_directory(&path) {
            tracing::warn!(path = ?path, %error, "state WAL staging cleanup barrier will be retried");
        }
    }
    Ok(())
}

pub(crate) fn prepare_persistent_state_directory(
    requested: &Path,
) -> Result<arc_crypto::secret_file::PreparedPrivateDirectoryNamespaceLock, StateError> {
    let parent = requested
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match std::fs::symlink_metadata(parent) {
        Ok(_) if parent.is_dir() => {}
        Ok(_) => {
            return Err(StateError::PersistenceError(format!(
                "persistent state parent is not a directory: {parent:?}"
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            arc_crypto::secret_file::create_private_directory_tree(parent).map_err(|error| {
                StateError::PersistenceError(format!(
                    "failed to create persistent state parent {parent:?}: {error}"
                ))
            })?;
        }
        Err(error) => {
            return Err(StateError::PersistenceError(format!(
                "failed to inspect persistent state parent {parent:?}: {error}"
            )));
        }
    }
    let namespace_lock = arc_crypto::secret_file::acquire_private_directory_namespace_lock(
        requested,
    )
    .map_err(|error| {
        StateError::PersistenceError(format!(
            "failed to lock persistent state directory namespace {requested:?}: {error}"
        ))
    })?;
    namespace_lock.restore_interrupted().map_err(|error| {
        StateError::PersistenceError(format!(
            "failed to restore persistent state directory namespace {requested:?}: {error}"
        ))
    })?;
    arc_crypto::secret_file::create_private_directory_tree(namespace_lock.target()).map_err(
        |error| {
            StateError::PersistenceError(format!(
                "failed to secure persistent state directory {requested:?}: {error}"
            ))
        },
    )?;
    namespace_lock.rebarrier_into_prepared().map_err(|error| {
        StateError::PersistenceError(format!(
            "failed to rebarrier persistent state directory namespace {requested:?}: {error}"
        ))
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub version: u64,
    pub state_root: Hash256,
    pub accounts: Vec<(Address, Account)>,
    pub chunk_index: u32,
    pub total_chunks: u32,
    pub chunk_proof: Hash256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub version: u64,
    pub state_root: Hash256,
    pub total_accounts: u64,
    pub total_chunks: u32,
    pub chunk_size: usize,
    pub manifest_hash: Hash256,
}

pub struct SyncProgress {
    pub manifest: SnapshotManifest,
    pub received_chunks: Vec<bool>,
    pub verified_chunks: u32,
    pub total_accounts_imported: u64,
    pub latest_chunk_state_root: Option<Hash256>,
}

pub struct StateSummary {
    pub account_count: u64,
    pub total_balance: u128,
    pub state_root: Hash256,
    pub block_height: u64,
}

const WASM_MAGIC: &[u8; 4] = b"\0asm";

#[inline]
fn below_policy_minimum(value: u64, minimum: u64) -> bool {
    value < minimum
}

pub fn is_legacy_reserved_system_account(address: &Address) -> bool {
    [
        arc_types::transaction::faucet_pool_address(),
        arc_types::transaction::inference_reward_treasury_address(),
        hash_bytes(&[2u8]),
        hash_bytes(b"ARC-bridge-escrow"),
        v3_fee_treasury_address(),
    ]
    .contains(address)
}

pub fn is_reserved_system_account(address: &Address) -> bool {
    is_legacy_reserved_system_account(address)
        || *address == hash_bytes(b"arc-treasury")
        || arc_types::transaction::is_v3_system_account(address)
}

fn community_reward_budget_address(
    namespace: arc_types::transaction::V3SystemAccountKind,
    derivation_key: &'static str,
    height_or_epoch: u64,
    identity: Option<&Address>,
) -> Address {
    let mut hasher = blake3::Hasher::new_derive_key(derivation_key);
    hasher.update(&height_or_epoch.to_le_bytes());
    if let Some(identity) = identity {
        hasher.update(identity.as_ref());
    }
    arc_types::transaction::v3_system_account_address(
        namespace,
        &Hash256(*hasher.finalize().as_bytes()),
    )
}

fn community_reward_block_budget_address(height: u64) -> Address {
    community_reward_budget_address(
        arc_types::transaction::V3SystemAccountKind::CommunityRewardBlockBudget,
        "ARC-community-reward-block-budget-v3",
        height,
        None,
    )
}

fn community_reward_epoch_budget_address(epoch: u64) -> Address {
    community_reward_budget_address(
        arc_types::transaction::V3SystemAccountKind::CommunityRewardEpochBudget,
        "ARC-community-reward-epoch-budget-v3",
        epoch,
        None,
    )
}

fn community_reward_worker_budget_address(epoch: u64, worker: &Address) -> Address {
    community_reward_budget_address(
        arc_types::transaction::V3SystemAccountKind::CommunityRewardWorkerBudget,
        "ARC-community-reward-worker-epoch-budget-v3",
        epoch,
        Some(worker),
    )
}

fn community_reward_coordinator_budget_address(epoch: u64, coordinator: &Address) -> Address {
    community_reward_budget_address(
        arc_types::transaction::V3SystemAccountKind::CommunityRewardCoordinatorBudget,
        "ARC-community-reward-coordinator-epoch-budget-v3",
        epoch,
        Some(coordinator),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum V3TransactionFamilyPolicy {
    Allowed,
    Denied,
}

pub const TIER1_STATUS_OPEN: u8 = 0;
pub const TIER1_STATUS_VOTING: u8 = 1;
pub const TIER1_STATUS_FINALIZED: u8 = 2;
pub const TIER1_STATUS_REFUNDED: u8 = 3;

const ATTEST_ESCROW_MAGIC: [u8; 8] = *b"ARCATB2\x01";
pub const ATTEST_STATUS_OPEN: u8 = 0;
pub const ATTEST_STATUS_CHALLENGED: u8 = 1;

pub const MAX_BOND_RELEASES_PER_BLOCK: usize = 256;

#[derive(Clone, Debug)]
pub struct Tier1RequestSnapshot {
    pub request_id: [u8; 32],
    pub escrow_addr: Address,
    pub status: u8,
    pub deadline_blocks: u64,
    pub committee_size: u8,
    pub anchor_height: u64,
    pub input_blob: Vec<u8>,
    pub votes: Vec<(Address, Hash256)>,
    pub max_reward: u64,
    pub requester: Address,
}

fn compute_contract_address(deployer: &Address, nonce: u64) -> Address {
    let mut preimage = Vec::with_capacity(32 + 32 + 8);
    preimage.extend_from_slice(b"ARC-chain-contract-addr-v1\x00\x00\x00\x00\x00\x00");
    preimage.extend_from_slice(&deployer.0);
    preimage.extend_from_slice(&nonce.to_le_bytes());
    hash_bytes(&preimage)
}

#[cfg(feature = "benchmark-tools")]
pub struct IndexerBatch {
    pub block_hash: Hash256,
    pub height: u64,
    pub senders: Arc<Vec<Hash256>>,
    pub receivers: Arc<Vec<Hash256>>,
    pub nonce_start: u64,
    pub txs_per_sender: u64,
}

pub struct StateDB {
    accounts: DashMap<[u8; 32], Account>,
    storage: DashMap<[u8; 32], DashMap<Hash256, Vec<u8>>>,
    blocks: DashMap<u64, Box<Block>>,
    height: RwLock<u64>,
    pub receipts: DashMap<[u8; 32], TxReceipt>,
    pub tx_index: DashMap<[u8; 32], (u64, u32)>,
    pub account_txs: DashMap<[u8; 32], Vec<Hash256>>,
    pub contracts: DashMap<[u8; 32], Vec<u8>>,
    wal: WalWriter,
    identities: DashMap<[u8; 32], Identity>,
    pub full_transactions: DashMap<[u8; 32], Transaction>,
    snapshot_counter: AtomicU64,
    snapshot_history_window: AtomicU64,
    rebase_anchor_round: AtomicU64,
    pub benchmark_tx_count: AtomicU64,
    #[cfg(feature = "benchmark-tools")]
    indexer_tx: Option<crossbeam::channel::Sender<IndexerBatch>>,
    #[cfg(feature = "benchmark-tools")]
    benchmark_nonces: DashMap<u64, u64>,
    #[cfg(feature = "benchmark-tools")]
    benchmark_senders: parking_lot::RwLock<Option<Arc<Vec<Hash256>>>>,
    #[cfg(feature = "benchmark-tools")]
    benchmark_receivers: parking_lot::RwLock<Option<Arc<Vec<Hash256>>>>,
    #[cfg(feature = "benchmark-tools")]
    benchmark_txs_per_sender: AtomicU64,
    signed_block_data: DashMap<u64, (Vec<Transaction>, Vec<bool>, Hash256)>,
    incremental_merkle: parking_lot::Mutex<IncrementalMerkle>,
    dirty_accounts: DashSet<[u8; 32]>,
    pub event_logs: DashMap<u64, Vec<arc_types::EventLog>>,
    staking_pool: AtomicU64,
    validators: DashMap<[u8; 32], u64>,
    jmt: parking_lot::Mutex<JmtStateTree>,
    use_jmt: bool,
    gpu_cache: Option<Arc<gpu_state::GpuStateCache>>,
    pub archive_mode: bool,
    archive_history_boundary: AtomicU64,
    archive_history_verified_through: AtomicU64,
    community_rewards_v1_activation_height: AtomicU64,
    pub tier1_pending: DashMap<[u8; 32], u64>,
    pending_bond_releases: parking_lot::Mutex<BTreeMap<u64, Vec<[u8; 32]>>>,
    recovery_context: RwLock<Option<recovery::RecoveryContext>>,
    recovery_manifest_hash: RwLock<Option<Hash256>>,
    native_inference_context: RwLock<Option<InferenceAdmissionContext>>,
    pub(crate) native_migration:
        RwLock<Option<crate::inference_contract_state::NativeMigrationRecord>>,
    pub(crate) pending_migration_context: RwLock<Option<InferenceAdmissionContext>>,
    pub(crate) superseded_bindings:
        RwLock<std::collections::BTreeMap<[u8; 32], InferenceAdmissionContext>>,
    pub(crate) pending_binding_update: RwLock<Option<(u64, Hash256, InferenceAdmissionContext)>>,
    native_inference_pending: DashMap<[u8; 32], u64>,
    native_inference_execution: parking_lot::Mutex<()>,
    native_inference_publication: RwLock<()>,
}

impl StateDB {
    fn wal_state_error(error: WalError) -> StateError {
        StateError::PersistenceError(error.to_string())
    }

    fn require_healthy_wal(&self) -> Result<(), StateError> {
        self.wal.check_health().map_err(Self::wal_state_error)
    }

    fn durable_wal_barrier(&self) -> Result<(), StateError> {
        self.wal.sync().map_err(Self::wal_state_error)
    }

    pub fn is_persistent(&self) -> bool {
        self.wal.directory().is_some()
    }

    fn validate_next_protocol_block_admission(
        &self,
        transactions: &[Transaction],
    ) -> Result<(), StateError> {
        self.validate_native_inference_block_admission(transactions)?;
        if self.active_protocol_version().major == 3 {
            let height = self.height().checked_add(1).ok_or_else(|| {
                StateError::ExecutionError("prospective v3 block height overflow".to_string())
            })?;
            self.validate_v3_block_admission_at(transactions, height)?;
        }
        Ok(())
    }

    pub fn new() -> Self {
        Self {
            accounts: DashMap::new(),
            storage: DashMap::new(),
            blocks: DashMap::new(),
            height: RwLock::new(0),
            receipts: DashMap::new(),
            tx_index: DashMap::new(),
            account_txs: DashMap::new(),
            contracts: DashMap::new(),
            wal: WalWriter::null(),
            identities: DashMap::new(),
            full_transactions: DashMap::new(),
            snapshot_counter: AtomicU64::new(0),
            snapshot_history_window: AtomicU64::new(snapshot::DEFAULT_HISTORY_WINDOW),
            rebase_anchor_round: AtomicU64::new(0),
            benchmark_tx_count: AtomicU64::new(0),
            #[cfg(feature = "benchmark-tools")]
            indexer_tx: None,
            #[cfg(feature = "benchmark-tools")]
            benchmark_nonces: DashMap::new(),
            #[cfg(feature = "benchmark-tools")]
            benchmark_senders: parking_lot::RwLock::new(None),
            #[cfg(feature = "benchmark-tools")]
            benchmark_receivers: parking_lot::RwLock::new(None),
            #[cfg(feature = "benchmark-tools")]
            benchmark_txs_per_sender: AtomicU64::new(0),
            signed_block_data: DashMap::new(),
            incremental_merkle: parking_lot::Mutex::new(IncrementalMerkle::new()),
            dirty_accounts: DashSet::new(),
            event_logs: DashMap::new(),
            staking_pool: AtomicU64::new(0),
            validators: DashMap::new(),
            jmt: parking_lot::Mutex::new(JmtStateTree::new()),
            use_jmt: false,
            gpu_cache: None,
            archive_mode: false,
            archive_history_boundary: AtomicU64::new(u64::MAX),
            archive_history_verified_through: AtomicU64::new(u64::MAX),
            community_rewards_v1_activation_height: AtomicU64::new(u64::MAX),
            tier1_pending: DashMap::new(),
            pending_bond_releases: parking_lot::Mutex::new(BTreeMap::new()),
            recovery_context: RwLock::new(None),
            recovery_manifest_hash: RwLock::new(None),
            native_inference_context: RwLock::new(None),
            native_migration: RwLock::new(None),
            pending_migration_context: RwLock::new(None),
            superseded_bindings: RwLock::new(std::collections::BTreeMap::new()),
            pending_binding_update: RwLock::new(None),
            native_inference_pending: DashMap::new(),
            native_inference_execution: parking_lot::Mutex::new(()),
            native_inference_publication: RwLock::new(()),
        }
    }

    pub fn with_persistence(wal_path: impl AsRef<Path>) -> Result<Self, StateError> {
        let wal =
            WalWriter::new(wal_path).map_err(|e| StateError::PersistenceError(e.to_string()))?;
        Ok(Self {
            accounts: DashMap::new(),
            storage: DashMap::new(),
            blocks: DashMap::new(),
            height: RwLock::new(0),
            receipts: DashMap::new(),
            tx_index: DashMap::new(),
            account_txs: DashMap::new(),
            contracts: DashMap::new(),
            wal,
            identities: DashMap::new(),
            full_transactions: DashMap::new(),
            snapshot_counter: AtomicU64::new(0),
            snapshot_history_window: AtomicU64::new(snapshot::DEFAULT_HISTORY_WINDOW),
            rebase_anchor_round: AtomicU64::new(0),
            benchmark_tx_count: AtomicU64::new(0),
            #[cfg(feature = "benchmark-tools")]
            indexer_tx: None,
            #[cfg(feature = "benchmark-tools")]
            benchmark_nonces: DashMap::new(),
            #[cfg(feature = "benchmark-tools")]
            benchmark_senders: parking_lot::RwLock::new(None),
            #[cfg(feature = "benchmark-tools")]
            benchmark_receivers: parking_lot::RwLock::new(None),
            #[cfg(feature = "benchmark-tools")]
            benchmark_txs_per_sender: AtomicU64::new(0),
            signed_block_data: DashMap::new(),
            incremental_merkle: parking_lot::Mutex::new(IncrementalMerkle::new()),
            dirty_accounts: DashSet::new(),
            event_logs: DashMap::new(),
            staking_pool: AtomicU64::new(0),
            validators: DashMap::new(),
            jmt: parking_lot::Mutex::new(JmtStateTree::new()),
            use_jmt: false,
            gpu_cache: None,
            archive_mode: false,
            archive_history_boundary: AtomicU64::new(u64::MAX),
            archive_history_verified_through: AtomicU64::new(u64::MAX),
            community_rewards_v1_activation_height: AtomicU64::new(u64::MAX),
            tier1_pending: DashMap::new(),
            pending_bond_releases: parking_lot::Mutex::new(BTreeMap::new()),
            recovery_context: RwLock::new(None),
            recovery_manifest_hash: RwLock::new(None),
            native_inference_context: RwLock::new(None),
            native_migration: RwLock::new(None),
            pending_migration_context: RwLock::new(None),
            superseded_bindings: RwLock::new(std::collections::BTreeMap::new()),
            pending_binding_update: RwLock::new(None),
            native_inference_pending: DashMap::new(),
            native_inference_execution: parking_lot::Mutex::new(()),
            native_inference_publication: RwLock::new(()),
        })
    }

    pub fn with_genesis(prefunded: &[(Address, u64)]) -> Self {
        let state = Self::new();
        for (addr, balance) in prefunded {
            state.accounts.insert(addr.0, Account::new(*addr, *balance));
            state.dirty_accounts.insert(addr.0);
        }
        let genesis = Block::genesis();
        state.blocks.insert(0, Box::new(genesis));
        state
    }

    pub fn with_genesis_gpu(
        prefunded: &[(Address, u64)],
        gpu_config: gpu_state::GpuStateCacheConfig,
    ) -> Self {
        let cache = Arc::new(gpu_state::GpuStateCache::new(gpu_config));
        let mut state = Self::with_genesis(prefunded);
        for (addr, _) in prefunded {
            if let Some(acct) = state.accounts.get(&addr.0).map(|a| a.clone()) {
                cache.put_account(&acct);
            }
        }
        state.gpu_cache = Some(cache);
        state
    }

    pub fn enable_gpu_cache(&mut self, config: gpu_state::GpuStateCacheConfig) {
        let cache = Arc::new(gpu_state::GpuStateCache::new(config));
        let mut loaded = 0usize;
        for entry in self.accounts.iter() {
            cache.put_account(entry.value());
            loaded += 1;
            if loaded >= cache.gpu_count() + 1_000_000 {
                break;
            }
        }
        self.gpu_cache = Some(cache);
        tracing::info!(
            loaded = loaded,
            "GPU state cache enabled, pre-loaded accounts"
        );
    }

    pub fn gpu_cache(&self) -> Option<&Arc<gpu_state::GpuStateCache>> {
        self.gpu_cache.as_ref()
    }

    pub fn with_genesis_persistent(
        prefunded: &[(Address, u64)],
        wal_dir: impl AsRef<Path>,
        expected_genesis_hash: Hash256,
    ) -> Result<Self, StateError> {
        let namespace_lock = prepare_persistent_state_directory(wal_dir.as_ref())?;
        let result = Self::with_genesis_persistent_in_prepared_dir(
            prefunded,
            namespace_lock.target(),
            expected_genesis_hash,
        );
        drop(namespace_lock);
        result
    }

    fn with_genesis_persistent_in_prepared_dir(
        prefunded: &[(Address, u64)],
        wal_dir: &Path,
        expected_genesis_hash: Hash256,
    ) -> Result<Self, StateError> {
        cleanup_state_staging_files(wal_dir)?;

        let wal_path = wal_dir.join("state.wal");
        Self::verify_or_create_genesis_binding(wal_dir, wal_path.exists(), expected_genesis_hash)?;

        if wal_path.exists() {
            let entries = Self::prepare_normal_wal_replay(&wal_path, prefunded)?;
            let state = Self::with_persistence(&wal_path)?;
            let adopted = state.adopt_snapshot_if_trustworthy(wal_dir);
            let resume_from = adopted.map(|manifest| manifest.resume_from_sequence);
            let mut entry_count = 0usize;
            let mut history_rebuilt = 0usize;
            for entry in &entries {
                if resume_from.is_some_and(|first| entry.sequence < first) {
                    if Self::is_history_op(&entry.op) {
                        state.apply_wal_op(&entry.op);
                        history_rebuilt += 1;
                    } else if let WalOp::Rebase(record) = &entry.op {
                        state.install_history_window(&record.state);
                        state.apply_wal_op(&WalOp::SetBlock(record.height, record.tip.clone()));
                        state
                            .rebase_anchor_round
                            .fetch_max(record.anchor_round, Ordering::AcqRel);
                        history_rebuilt += 1;
                    }
                    continue;
                }
                state.apply_wal_op(&entry.op);
                entry_count += 1;
            }

            if state.blocks.get(&0).is_none() {
                state.blocks.insert(0, Box::new(Block::genesis()));
            }

            tracing::info!(
                replayed = entry_count,
                history_rebuilt,
                of_total = entries.len(),
                from_snapshot_height = ?adopted.map(|m| m.identity.height),
                accounts = state.accounts.len(),
                height = state.height(),
                "WAL recovery complete"
            );

            state.restore_native_inference_context()?;
            Ok(state)
        } else {
            let staging_path =
                wal_dir.join(format!(".state.wal.genesis-{}.tmp", uuid::Uuid::new_v4()));
            let mut state = Self::with_persistence(&staging_path)?;

            for (addr, balance) in prefunded {
                let account = Account::new(*addr, *balance);
                state.accounts.insert(addr.0, account.clone());
                state.dirty_accounts.insert(addr.0);
                state.wal.append(WalOp::SetAccount(*addr, account), 0);
            }

            let genesis = Block::genesis();
            state.blocks.insert(0, Box::new(genesis.clone()));
            state.wal.append(WalOp::SetBlock(0, genesis), 0);
            state
                .wal
                .append(WalOp::Checkpoint(state.get_state_root()), 0);
            if let Err(error) = state.durable_wal_barrier() {
                drop(state);
                let _ = std::fs::remove_file(&staging_path);
                return Err(error);
            }
            if let Err(error) = state.wal.shutdown() {
                drop(state);
                let _ = std::fs::remove_file(&staging_path);
                return Err(Self::wal_state_error(error));
            }
            if let Err(error) = arc_crypto::secret_file::durably_publish_existing_private_no_replace(
                &staging_path,
                &wal_path,
            ) {
                drop(state);
                let _ = std::fs::remove_file(&staging_path);
                return Err(StateError::PersistenceError(format!(
                    "failed to publish complete genesis WAL {:?}: {error}",
                    wal_path
                )));
            }
            state.wal = WalWriter::new(&wal_path)
                .map_err(|error| StateError::PersistenceError(error.to_string()))?;

            tracing::info!(
                "Fresh state initialized with {} genesis accounts, WAL at {:?}",
                prefunded.len(),
                wal_path
            );

            Ok(state)
        }
    }

    fn verify_or_create_genesis_binding(
        wal_dir: &Path,
        wal_exists: bool,
        expected: Hash256,
    ) -> Result<(), StateError> {
        let binding_path = wal_dir.join("genesis.network-hash");
        let verify_existing = |value: &str| -> Result<(), StateError> {
            let actual = Hash256::from_hex(value.trim()).map_err(|_| {
                StateError::PersistenceError(format!(
                    "invalid genesis binding in {:?}; refusing WAL replay",
                    binding_path
                ))
            })?;
            if actual != expected {
                return Err(StateError::PersistenceError(format!(
                    "data directory genesis mismatch: {:?} is bound to {}, configured genesis is {}; use a fresh data directory or an approved checkpoint migration",
                    wal_dir,
                    actual.to_hex(),
                    expected.to_hex()
                )));
            }
            Ok(())
        };
        match std::fs::read_to_string(&binding_path) {
            Ok(value) => {
                verify_existing(&value)?;
                let contents = format!("{}\n", expected.to_hex());
                arc_crypto::secret_file::durably_replace_private(&binding_path, contents.as_bytes())
                    .map_err(|e| {
                        StateError::PersistenceError(format!(
                            "failed to rebarrier genesis binding {:?}: {}",
                            binding_path, e
                        ))
                    })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && wal_exists => {
                Err(StateError::PersistenceError(format!(
                    "legacy data directory {:?} contains a WAL but no authenticated genesis binding; refusing replay under the configured network. Back it up, then use a fresh data directory or an approved checkpoint migration",
                    wal_dir
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let contents = format!("{}\n", expected.to_hex());
                match arc_crypto::secret_file::durably_publish_new_private(
                    &binding_path,
                    contents.as_bytes(),
                ) {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        let value = std::fs::read_to_string(&binding_path).map_err(|e| {
                            StateError::PersistenceError(format!(
                                "failed to verify concurrently-created genesis binding {:?}: {}",
                                binding_path, e
                            ))
                        })?;
                        verify_existing(&value)?;
                        arc_crypto::secret_file::durably_replace_private(
                            &binding_path,
                            contents.as_bytes(),
                        )
                        .map_err(|e| {
                            StateError::PersistenceError(format!(
                                "failed to rebarrier concurrently-created genesis binding {:?}: {}",
                                binding_path, e
                            ))
                        })
                    }
                    Err(e) => Err(StateError::PersistenceError(format!(
                        "failed to durably create genesis binding {:?}: {}",
                        binding_path, e
                    ))),
                }
            }
            Err(error) => Err(StateError::PersistenceError(format!(
                "failed to read genesis binding {:?}: {}",
                binding_path, error
            ))),
        }
    }

    fn verify_complete_genesis_wal_prefix(
        entries: &[WalEntry],
        prefunded: &[(Address, u64)],
    ) -> Result<(), StateError> {
        let required = prefunded.len().checked_add(1).ok_or_else(|| {
            StateError::PersistenceError("genesis WAL prefix length overflow".into())
        })?;
        if entries.len() < required {
            return Err(StateError::PersistenceError(format!(
                "state WAL has an incomplete genesis prefix: found {} entries, require {required}",
                entries.len()
            )));
        }
        let mut expected_accounts = prefunded
            .iter()
            .map(|(address, balance)| {
                bincode::serialize(&(*address, Account::new(*address, *balance)))
                    .map_err(|error| StateError::PersistenceError(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut actual_accounts = Vec::with_capacity(prefunded.len());
        for (index, entry) in entries.iter().take(prefunded.len()).enumerate() {
            let WalOp::SetAccount(actual_address, actual_account) = &entry.op else {
                return Err(StateError::PersistenceError(format!(
                    "state WAL genesis entry {index} is not the expected account"
                )));
            };
            if entry.block_height != 0 {
                return Err(StateError::PersistenceError(format!(
                    "state WAL genesis account entry {index} differs from configured genesis"
                )));
            }
            actual_accounts.push(
                bincode::serialize(&(*actual_address, actual_account.clone()))
                    .map_err(|error| StateError::PersistenceError(error.to_string()))?,
            );
        }
        expected_accounts.sort();
        actual_accounts.sort();
        if actual_accounts != expected_accounts {
            return Err(StateError::PersistenceError(
                "state WAL genesis account set differs from configured genesis".into(),
            ));
        }
        let genesis_entry = &entries[prefunded.len()];
        let WalOp::SetBlock(height, block) = &genesis_entry.op else {
            return Err(StateError::PersistenceError(
                "state WAL genesis prefix omits the genesis block".into(),
            ));
        };
        let actual = bincode::serialize(block)
            .map_err(|error| StateError::PersistenceError(error.to_string()))?;
        let expected = bincode::serialize(&Block::genesis())
            .map_err(|error| StateError::PersistenceError(error.to_string()))?;
        if genesis_entry.block_height != 0 || *height != 0 || actual != expected {
            return Err(StateError::PersistenceError(
                "state WAL genesis block differs from configured genesis".into(),
            ));
        }
        Ok(())
    }

    fn prepare_normal_wal_replay(
        wal_path: &Path,
        prefunded: &[(Address, u64)],
    ) -> Result<Vec<WalEntry>, StateError> {
        let mut read = wal::read_repairable_wal_prefix(wal_path).map_err(|error| {
            StateError::PersistenceError(format!(
                "state WAL contains non-repairable corruption; refusing replay: {error}"
            ))
        })?;
        Self::verify_complete_genesis_wal_prefix(&read.entries, prefunded)?;
        let genesis_boundary = prefunded
            .len()
            .checked_add(1)
            .ok_or_else(|| StateError::PersistenceError("genesis WAL boundary overflow".into()))?;
        let mut accepted_entries = genesis_boundary;
        let mut last_checkpoint_height = 0u64;
        let mut previous_block_hash = Block::genesis().hash;
        let mut pending_block: Option<(u64, Block)> = None;
        let validation = Self::new();
        for entry in read.entries.iter().take(genesis_boundary) {
            validation.apply_wal_op(&entry.op);
        }
        for (index, entry) in read.entries.iter().enumerate().skip(genesis_boundary) {
            match &entry.op {
                WalOp::SetBlock(height, block) => {
                    if pending_block.is_some() {
                        return Err(StateError::PersistenceError(
                            "state WAL contains two block records without an intervening checkpoint"
                                .into(),
                        ));
                    }
                    pending_block = Some((*height, block.clone()));
                    validation.apply_wal_op(&entry.op);
                }
                WalOp::Checkpoint(root) => {
                    let checkpoint_height = if let Some((height, block)) = pending_block.take() {
                        let expected_height =
                            last_checkpoint_height.checked_add(1).ok_or_else(|| {
                                StateError::PersistenceError("state WAL height overflow".into())
                            })?;
                        if height != expected_height
                            || entry.block_height != height
                            || block.header.height != height
                            || Block::compute_hash(&block.header) != block.hash
                            || block.header.parent_hash != previous_block_hash
                            || block.header.state_root != *root
                        {
                            return Err(StateError::PersistenceError(format!(
                                "state WAL checkpoint at height {} is not the next canonical block boundary",
                                entry.block_height
                            )));
                        }
                        previous_block_hash = block.hash;
                        last_checkpoint_height = height;
                        height
                    } else {
                        if entry.block_height != last_checkpoint_height {
                            return Err(StateError::PersistenceError(format!(
                                "state WAL standalone checkpoint height {} differs from current canonical height {}",
                                entry.block_height, last_checkpoint_height
                            )));
                        }
                        last_checkpoint_height
                    };
                    if validation.height() != checkpoint_height {
                        return Err(StateError::PersistenceError(format!(
                            "state WAL checkpoint height {checkpoint_height} differs from replayed height {}",
                            validation.height()
                        )));
                    }
                    let actual_root = validation.get_state_root();
                    if actual_root != *root {
                        return Err(StateError::PersistenceError(format!(
                            "state WAL checkpoint root mismatch: expected {root}, replayed {actual_root}"
                        )));
                    }
                    accepted_entries = index + 1;
                }
                WalOp::Rebase(record) => {
                    if pending_block.is_some()
                        || record.height <= last_checkpoint_height
                        || !record.state.receipts.is_empty()
                        || !record.state.event_logs.is_empty()
                        || !record.state.full_transactions.is_empty()
                        || !record.state.native_inference_pending.is_empty()
                        || record.state.recovery_context.is_some()
                        || record.state.height != record.height
                        || record.tip.header.height != record.height
                        || Block::compute_hash(&record.tip.header) != record.tip.hash
                        || checkpoint_adoption::accounts_bound_to_keys(&record.state).is_err()
                    {
                        return Err(StateError::PersistenceError(format!(
                            "state WAL rebase to height {} is not a forward move to a \
                             self-consistent tip",
                            record.height
                        )));
                    }
                    checkpoint_adoption::verify_history_window(&record.state.blocks, &record.tip)?;
                    validation.apply_wal_op(&entry.op);
                    let actual_root = validation.get_state_root();
                    if actual_root != record.tip.header.state_root {
                        return Err(StateError::PersistenceError(format!(
                            "state WAL rebase root mismatch: tip {} carries {}, state replays {actual_root}",
                            record.height, record.tip.header.state_root
                        )));
                    }
                    previous_block_hash = record.tip.hash;
                    last_checkpoint_height = record.height;
                    accepted_entries = index + 1;
                }
                _ => validation.apply_wal_op(&entry.op),
            }
        }
        let accepted_bytes = *read
            .frame_end_offsets
            .get(accepted_entries.saturating_sub(1))
            .ok_or_else(|| {
                StateError::PersistenceError("genesis WAL byte boundary is missing".into())
            })?;

        let original_bytes = read.original_bytes;
        let original_hash = read.original_hash;
        let tail_reason = read
            .torn_tail
            .map(|tail| tail.stable_reason())
            .unwrap_or("complete_uncheckpointed_suffix");
        read.entries.truncate(accepted_entries);
        if accepted_bytes < original_bytes {
            let quarantine = wal::quarantine_and_truncate_wal_tail(
                wal_path,
                accepted_bytes,
                original_bytes,
                original_hash,
            )
            .map_err(|error| {
                StateError::PersistenceError(format!(
                    "failed to quarantine uncommitted state WAL tail: {error}"
                ))
            })?;
            tracing::warn!(
                accepted_bytes,
                original_bytes,
                reason = tail_reason,
                quarantine = ?quarantine,
                "quarantined state WAL bytes after the last durable checkpoint"
            );
        } else {
            wal::verify_wal_file_identity(wal_path, original_bytes, original_hash).map_err(
                |error| {
                    StateError::PersistenceError(format!(
                        "state WAL changed after validation; refusing replay: {error}"
                    ))
                },
            )?;
        }

        Ok(read.entries)
    }

    pub fn recover(snapshot: Snapshot, wal_path: impl AsRef<Path>) -> Result<Self, StateError> {
        if snapshot.storage.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|(key, _)| *key == inference_contract_state::activation_key())
        }) {
            return Err(StateError::PersistenceError(
                "native inference state requires genesis-bound checkpointed WAL recovery".into(),
            ));
        }
        let state = Self::with_persistence(&wal_path)?;

        for (addr, account) in &snapshot.accounts {
            state.accounts.insert(addr.0, account.clone());
        }
        for (addr, entries) in &snapshot.storage {
            let map = DashMap::new();
            for (key, val) in entries {
                map.insert(*key, val.clone());
            }
            state.storage.insert(addr.0, map);
        }
        for (addr, bytecode) in &snapshot.contracts {
            state.contracts.insert(addr.0, bytecode.clone());
        }
        *state.height.write() = snapshot.block_height;

        let entries = wal::read_wal_from(&wal_path, snapshot.wal_sequence);
        for entry in entries {
            state.apply_wal_op(&entry.op);
        }

        tracing::info!(
            "Recovered state: {} accounts, height {}",
            state.accounts.len(),
            state.height()
        );

        if state.storage.iter().any(|entries| {
            entries
                .value()
                .contains_key(&inference_contract_state::activation_key())
        }) {
            return Err(StateError::PersistenceError(
                "native inference state requires genesis-bound checkpointed WAL recovery".into(),
            ));
        }
        state.restore_native_inference_context()?;
        Ok(state)
    }

    fn is_history_op(op: &WalOp) -> bool {
        matches!(
            op,
            WalOp::SetBlock(..)
                | WalOp::SetReceipt(..)
                | WalOp::SetFullTransaction(..)
                | WalOp::SetEventLogs(..)
        )
    }

    pub fn set_snapshot_history_window(&self, heights: u64) {
        self.snapshot_history_window
            .store(heights.max(1), Ordering::Release);
    }

    fn apply_wal_op(&self, op: &WalOp) {
        match op {
            WalOp::SetAccount(addr, account) => {
                self.accounts.insert(addr.0, account.clone());
                self.dirty_accounts.insert(addr.0);
            }
            WalOp::SetStorage(addr, key, val) => {
                self.storage
                    .entry(addr.0)
                    .or_default()
                    .insert(*key, val.clone());
            }
            WalOp::DeleteStorage(addr, key) => {
                if let Some(map) = self.storage.get(&addr.0) {
                    map.remove(key);
                }
            }
            WalOp::SetBlock(height, block) => {
                if let Some(replaced) = self.blocks.insert(*height, Box::new(block.clone())) {
                    for old_hash in replaced.tx_hashes {
                        let points_to_replaced_height = self
                            .tx_index
                            .get(&old_hash.0)
                            .is_some_and(|location| location.value().0 == *height);
                        if points_to_replaced_height {
                            self.tx_index.remove(&old_hash.0);
                        }
                    }
                }
                for (index, tx_hash) in block.tx_hashes.iter().enumerate() {
                    if let Ok(index) = u32::try_from(index) {
                        self.tx_index.insert(tx_hash.0, (*height, index));
                    }
                }
                let mut h = self.height.write();
                if *height > *h {
                    *h = *height;
                }
            }
            WalOp::SetReceipt(hash, receipt) => {
                self.receipts.insert(hash.0, receipt.clone());
            }
            WalOp::SetAgent(_addr, _name, _endpoint, _caps) => {}
            WalOp::SetContract(addr, bytecode) => {
                self.contracts.insert(addr.0, bytecode.clone());
            }
            WalOp::SetFullTransaction(hash, transaction) => {
                self.full_transactions
                    .insert(hash.0, transaction.as_ref().clone());
            }
            WalOp::SetEventLogs(height, logs) => {
                self.event_logs.insert(*height, logs.clone());
            }
            WalOp::SetIdentity(address, identity) => {
                self.identities.insert(address.0, identity.clone());
            }
            WalOp::SetValidatorState(validators, staking_pool) => {
                self.validators.clear();
                for (address, stake) in validators {
                    self.validators.insert(address.0, *stake);
                }
                self.staking_pool.store(*staking_pool, Ordering::Release);
            }
            WalOp::SetRecoveryContext(context, activation_height) => {
                *self.recovery_context.write() = Some(context.clone());
                self.community_rewards_v1_activation_height
                    .store(activation_height.unwrap_or(u64::MAX), Ordering::Release);
            }
            WalOp::InferenceTransition(record) => {
                for (address, account) in &record.account_updates {
                    self.accounts.insert(address.0, account.clone());
                    self.dirty_accounts.insert(address.0);
                    if let Some(cache) = &self.gpu_cache {
                        cache.put_account(account);
                    }
                }
                if record.metadata.status == wal::InferenceTransitionStatus::Pending {
                    self.native_inference_pending
                        .insert(record.request_id.0, record.admission_height);
                } else {
                    self.native_inference_pending.remove(&record.request_id.0);
                }
                for (address, key, value) in &record.storage_updates {
                    self.storage
                        .entry(address.0)
                        .or_default()
                        .insert(*key, value.clone());
                }
            }
            WalOp::Checkpoint(_) => {}
            WalOp::Rebase(record) => {
                self.clear_state_for_rebase();
                self.install_durable_snapshot(&record.state);
                self.apply_wal_op(&WalOp::SetBlock(record.height, record.tip.clone()));
                self.rebase_anchor_round
                    .fetch_max(record.anchor_round, Ordering::AcqRel);
            }
            WalOp::SetDagBlock(_, _) | WalOp::SetDagRound(_) | WalOp::CommitDagBlock(_) => {}
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        loop {
            let height_before = self.height();
            let accounts: Vec<(Address, Account)> = self
                .accounts
                .iter()
                .map(|e| (Hash256(*e.key()), e.value().clone()))
                .collect();

            let storage: wal::ContractStorage = self
                .storage
                .iter()
                .map(|e| {
                    let entries: Vec<(Hash256, Vec<u8>)> = e
                        .value()
                        .iter()
                        .map(|se| (*se.key(), se.value().clone()))
                        .collect();
                    (Hash256(*e.key()), entries)
                })
                .collect();

            let contracts: Vec<(Address, Vec<u8>)> = self
                .contracts
                .iter()
                .map(|e| (Hash256(*e.key()), e.value().clone()))
                .collect();

            let height_after = self.height();
            if height_before != height_after {
                tracing::warn!(
                    "Snapshot height changed ({} → {}), retrying for consistency",
                    height_before,
                    height_after
                );
                continue;
            }

            return Snapshot {
                block_height: height_after,
                state_root: self.compute_state_root(),
                wal_sequence: 0,
                accounts,
                storage,
                contracts,
            };
        }
    }

    pub fn deploy_contract(&self, address: &Address, bytecode: Vec<u8>) {
        self.contracts.insert(address.0, bytecode.clone());
        self.wal
            .append(WalOp::SetContract(*address, bytecode), self.height());
    }

    pub fn get_contract(&self, address: &Address) -> Option<Vec<u8>> {
        self.contracts.get(&address.0).map(|c| c.clone())
    }

    pub fn get_transaction(&self, tx_hash: &[u8; 32]) -> Option<Transaction> {
        if let Some(tx) = self.full_transactions.get(tx_hash).map(|t| t.clone()) {
            return Some(tx);
        }
        if let Some(&(height, idx)) = self.tx_index.get(tx_hash).as_deref()
            && let Some(block_data) = self.signed_block_data.get(&height)
        {
            let (txs_vec, _, _) = &*block_data;
            return txs_vec.get(idx as usize).cloned();
        }
        None
    }

    pub fn tier1_pending_requests(&self) -> Vec<([u8; 32], u64)> {
        self.tier1_pending
            .iter()
            .map(|kv| (*kv.key(), *kv.value()))
            .collect()
    }

    pub fn rebuild_tier1_pending(&self) -> usize {
        let key = hash_bytes(b"tier1.request_id");
        let mut rebuilt = 0usize;
        for entry in self.accounts.iter() {
            let acct = entry.value();
            let status = acct.code_hash.0[0];
            if status != TIER1_STATUS_OPEN && status != TIER1_STATUS_VOTING {
                continue;
            }
            let escrow_addr = Hash256(*entry.key());
            let raw = match self.get_storage(&escrow_addr, &key) {
                Some(v) if v.len() == 32 => v,
                _ => continue,
            };
            let mut request_id = [0u8; 32];
            request_id.copy_from_slice(&raw);
            let anchor_height = acct.nonce;
            self.tier1_pending.insert(request_id, anchor_height);
            rebuilt += 1;
        }
        rebuilt
    }

    pub fn tier1_request_snapshot(&self, request_id: &[u8; 32]) -> Option<Tier1RequestSnapshot> {
        let escrow_addr = arc_crypto::hash_bytes(&[b"arc-infreq", request_id.as_ref()].concat());
        let escrow = self.get_account(&escrow_addr)?;
        if escrow.balance == 0 && escrow.code_hash == Hash256::ZERO {
            return None;
        }
        let status = escrow.code_hash.0[0];
        let deadline_blocks =
            u64::from_le_bytes(escrow.code_hash.0[1..9].try_into().unwrap_or([0u8; 8]));
        let committee_size = escrow.code_hash.0[9];
        let anchor_height = escrow.nonce;
        let input_blob = self
            .get_storage(&escrow_addr, &arc_crypto::hash_bytes(b"tier1.input_blob"))
            .unwrap_or_default();
        let votes_bytes = self
            .get_storage(&escrow_addr, &arc_crypto::hash_bytes(b"tier1.votes"))
            .unwrap_or_default();
        let votes: Vec<(Address, Hash256)> = bincode::deserialize(&votes_bytes).unwrap_or_default();
        let requester_bytes = self
            .get_storage(&escrow_addr, &arc_crypto::hash_bytes(b"tier1.requester"))
            .unwrap_or_default();
        let requester = if requester_bytes.len() == 32 {
            let mut addr = [0u8; 32];
            addr.copy_from_slice(&requester_bytes);
            Hash256(addr)
        } else {
            escrow_addr
        };
        Some(Tier1RequestSnapshot {
            request_id: *request_id,
            escrow_addr,
            status,
            deadline_blocks,
            committee_size,
            anchor_height,
            input_blob,
            votes,
            max_reward: escrow.balance,
            requester,
        })
    }

    pub fn tier1_committee_for(
        &self,
        request_id: &[u8; 32],
        anchor_height: u64,
        committee_size: u8,
    ) -> Vec<Address> {
        let mut seed_input: Vec<u8> = Vec::with_capacity(64);
        seed_input.extend_from_slice(b"tier1-seed");
        seed_input.extend_from_slice(request_id);
        seed_input.extend_from_slice(&anchor_height.to_le_bytes());
        let seed = arc_crypto::hash_bytes(&seed_input);

        let mut eligible: Vec<Address> = self
            .validators
            .iter()
            .map(|kv| Hash256(*kv.key()))
            .collect();
        eligible.sort_by_key(|a| a.0);
        let mut scored: Vec<(Address, Hash256)> = eligible
            .into_iter()
            .map(|a| {
                let mut input = Vec::with_capacity(64);
                input.extend_from_slice(&seed.0);
                input.extend_from_slice(&a.0);
                (a, arc_crypto::hash_bytes(&input))
            })
            .collect();
        scored.sort_by_key(|a| a.1.0);
        scored
            .into_iter()
            .take(committee_size as usize)
            .map(|(a, _)| a)
            .collect()
    }

    pub fn set_storage(&self, contract: &Address, key: Hash256, value: Vec<u8>) {
        self.storage
            .entry(contract.0)
            .or_default()
            .insert(key, value.clone());
        self.wal
            .append(WalOp::SetStorage(*contract, key, value), self.height());
    }

    pub fn get_storage(&self, contract: &Address, key: &Hash256) -> Option<Vec<u8>> {
        let _publication = self.native_inference_publication.read();
        self.storage
            .get(&contract.0)
            .and_then(|map| map.get(key).map(|v| v.clone()))
    }

    pub fn get_contract_storage(&self, contract: &Address) -> HashMap<Hash256, Vec<u8>> {
        self.storage
            .get(&contract.0)
            .map(|map| map.iter().map(|e| (*e.key(), e.value().clone())).collect())
            .unwrap_or_default()
    }

    pub fn delete_storage(&self, contract: &Address, key: &Hash256) {
        if let Some(map) = self.storage.get(&contract.0) {
            map.remove(key);
        }
        self.wal
            .append(WalOp::DeleteStorage(*contract, *key), self.height());
    }

    pub fn get_account(&self, addr: &Address) -> Option<Account> {
        let _publication = self.native_inference_publication.read();
        if let Some(ref cache) = self.gpu_cache
            && let Some(acct) = cache.get_account_fast(&addr.0)
        {
            return Some(acct);
        }
        self.accounts.get(&addr.0).map(|a| a.clone())
    }

    pub fn get_or_create_account(&self, addr: &Address) -> Account {
        self.accounts
            .entry(addr.0)
            .or_insert_with(|| Account::new(*addr, 0))
            .clone()
    }

    pub fn update_account(&self, addr: &Address, account: Account) {
        self.accounts.insert(addr.0, account.clone());
        self.dirty_accounts.insert(addr.0);
        if let Some(ref cache) = self.gpu_cache {
            cache.update_account_fast(&account);
        }
    }

    pub fn is_evm_contract(&self, addr: &Address) -> bool {
        match self.get_contract(addr) {
            Some(bytecode) => bytecode.len() < 4 || &bytecode[..4] != WASM_MAGIC,
            None => false,
        }
    }

    pub fn store_event_logs(&self, height: u64, logs: Vec<arc_types::EventLog>) {
        if !logs.is_empty() {
            if self.is_persistent()
                && (self.recovery_context().is_some()
                    || self.native_inference_context.read().is_some())
            {
                tracing::error!(
                    height,
                    "standalone event-log persistence is unavailable on recovery-bound state; include logs in the canonical block commit"
                );
                return;
            }
            let combined = {
                let mut entry = self.event_logs.entry(height).or_default();
                entry.extend(logs);
                entry.clone()
            };
            let checkpoint_height = self.height();
            self.wal
                .append(WalOp::SetEventLogs(height, combined), checkpoint_height);
            self.wal
                .append(WalOp::Checkpoint(self.get_state_root()), checkpoint_height);
            if let Err(error) = self.durable_wal_barrier() {
                tracing::error!(height, error = %error, "event logs were not durably persisted");
            }
        }
    }

    pub const MIN_VALIDATOR_STAKE: u64 = 100_000;

    pub fn total_staked(&self) -> u64 {
        self.staking_pool.load(Ordering::Relaxed)
    }

    fn add_staking_pool_checked(&self, amount: u64) -> Result<(), StateError> {
        self.staking_pool
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(amount)
            })
            .map(|_| ())
            .map_err(|current| {
                StateError::ExecutionError(format!(
                    "staking pool overflow: current={current}, add={amount}"
                ))
            })
    }

    fn subtract_staking_pool_checked(&self, amount: u64) -> Result<(), StateError> {
        self.staking_pool
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(amount)
            })
            .map(|_| ())
            .map_err(|current| {
                StateError::ExecutionError(format!(
                    "staking pool underflow: current={current}, subtract={amount}"
                ))
            })
    }

    pub fn get_validator_stake(&self, addr: &Address) -> Option<u64> {
        self.validators.get(&addr.0).map(|v| *v)
    }

    pub fn is_validator(&self, addr: &Address) -> bool {
        self.validators
            .get(&addr.0)
            .map(|v| *v >= Self::MIN_VALIDATOR_STAKE)
            .unwrap_or(false)
    }

    pub fn set_community_rewards_v1_activation_height(&self, height: Option<u64>) {
        self.community_rewards_v1_activation_height
            .store(height.unwrap_or(u64::MAX), Ordering::Release);
    }

    pub fn community_rewards_v1_activation_height(&self) -> Option<u64> {
        match self
            .community_rewards_v1_activation_height
            .load(Ordering::Acquire)
        {
            u64::MAX => None,
            height => Some(height),
        }
    }

    pub fn community_rewards_v1_active(&self) -> bool {
        self.community_rewards_v1_activation_height()
            .is_some_and(|activation| self.height() >= activation)
    }

    pub fn seed_genesis_validators(&self, genesis_validators: &[(Address, u64)]) {
        self.validators.clear();
        for (addr, stake) in genesis_validators {
            self.validators.insert(addr.0, *stake);
        }
    }

    pub fn active_validators(&self) -> Vec<(Address, u64)> {
        self.validators
            .iter()
            .filter(|entry| *entry.value() >= Self::MIN_VALIDATOR_STAKE)
            .map(|entry| (Hash256(*entry.key()), *entry.value()))
            .collect()
    }

    fn verify_community_reward_validator_approvals(
        &self,
        body: &arc_types::transaction::CommunityInferenceRewardBody,
    ) -> Result<(), StateError> {
        use arc_types::transaction::MAX_COMMUNITY_REWARD_APPROVALS;

        if body.validator_approvals.len() > MAX_COMMUNITY_REWARD_APPROVALS {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: {} validator approvals exceeds protocol maximum {}",
                body.validator_approvals.len(),
                MAX_COMMUNITY_REWARD_APPROVALS
            )));
        }

        let active = self.active_validators();
        if active.is_empty() {
            return Err(StateError::ExecutionError(
                "community inference reward: active validator set is empty".to_string(),
            ));
        }
        if active.len() > MAX_COMMUNITY_REWARD_APPROVALS {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: active validator set size {} exceeds reward-v1 maximum {}",
                active.len(),
                MAX_COMMUNITY_REWARD_APPROVALS
            )));
        }
        if self.recovery_context().is_some()
            && active.len() != arc_types::transaction::COMMUNITY_REWARD_VALIDATOR_SET_SIZE
        {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: protocol-v3 committee has {} active validators; exactly {} required",
                active.len(),
                arc_types::transaction::COMMUNITY_REWARD_VALIDATOR_SET_SIZE
            )));
        }

        let active_identity_count = u64::try_from(active.len()).map_err(|_| {
            StateError::ExecutionError(
                "community inference reward: active validator count exceeds u64::MAX".to_string(),
            )
        })?;
        let required_identities = usize::try_from(arc_types::strict_supermajority_threshold(
            active_identity_count,
        ))
        .map_err(|_| {
            StateError::ExecutionError(
                "community inference reward: identity threshold exceeds usize::MAX".to_string(),
            )
        })?;
        let required_identities = if self.recovery_context().is_some() {
            arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED
        } else {
            required_identities
        };
        if body.validator_approvals.len() < required_identities {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: insufficient validator approval identities: have {}, need {} of {} active validators",
                body.validator_approvals.len(),
                required_identities,
                active.len()
            )));
        }

        let active_stakes: HashMap<[u8; 32], u64> = active
            .iter()
            .map(|(address, stake)| (address.0, *stake))
            .collect();
        let total_active_stake: u128 = active.iter().map(|(_, stake)| u128::from(*stake)).sum();
        let required_stake = total_active_stake * 2 / 3 + 1;
        let commitment = body.validator_approval_commitment();
        let mut seen = HashSet::with_capacity(body.validator_approvals.len());
        let mut approved_stake = 0u128;

        let mut parsed_approvals = Vec::with_capacity(body.validator_approvals.len());
        let mut ed_sigs = Vec::with_capacity(body.validator_approvals.len());
        let mut ed_vks = Vec::with_capacity(body.validator_approvals.len());
        let mut can_batch = true;

        for approval in &body.validator_approvals {
            if !seen.insert(approval.validator.0) {
                return Err(StateError::ExecutionError(format!(
                    "community inference reward: duplicate validator approval from {}",
                    approval.validator.to_hex()
                )));
            }
            let Some(stake) = active_stakes.get(&approval.validator.0) else {
                return Err(StateError::ExecutionError(format!(
                    "community inference reward: approval signer {} is not an active validator",
                    approval.validator.to_hex()
                )));
            };
            if arc_crypto::address_from_ed25519_pubkey(&approval.public_key) != approval.validator {
                can_batch = false;
            } else if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&approval.public_key) {
                let mut sig_bytes = [0u8; 64];
                sig_bytes[..32].copy_from_slice(&approval.signature_halves[0]);
                sig_bytes[32..].copy_from_slice(&approval.signature_halves[1]);
                ed_sigs.push(ed25519_dalek::Signature::from_bytes(&sig_bytes));
                ed_vks.push(vk);
            } else {
                can_batch = false;
            }
            parsed_approvals.push((*stake, approval));
        }

        let commit_bytes = commitment.as_bytes().as_slice();
        let msgs = vec![commit_bytes; ed_sigs.len()];

        if can_batch
            && !ed_sigs.is_empty()
            && arc_crypto::signature::batch_verify_ed25519(&msgs, &ed_sigs, &ed_vks).is_ok()
        {
            for (stake, _) in parsed_approvals {
                approved_stake += u128::from(stake);
            }
        } else {
            approved_stake = 0;
            for (stake, approval) in parsed_approvals {
                approval
                    .as_signature()
                    .verify(&commitment, &approval.validator)
                    .map_err(|_| {
                        StateError::ExecutionError(format!(
                            "community inference reward: invalid Ed25519 approval from {}",
                            approval.validator.to_hex()
                        ))
                    })?;
                approved_stake += u128::from(stake);
            }
        }

        if approved_stake < required_stake {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: insufficient approved stake: have {}, need {} of {} active stake",
                approved_stake, required_stake, total_active_stake
            )));
        }
        Ok(())
    }

    fn validate_v3_transaction_envelope(&self, tx: &Transaction) -> Result<(), StateError> {
        let native_active =
            self.native_migration().is_some() && self.native_inference_context().is_some();
        self.validate_v3_transaction_envelope_with_native(tx, native_active)
    }

    fn validate_v3_transaction_envelope_with_native(
        &self,
        tx: &Transaction,
        native_authorized: bool,
    ) -> Result<(), StateError> {
        if tx.tx_type != tx.body.tx_type() {
            return Err(StateError::ExecutionError(format!(
                "transaction type/body mismatch: envelope {:?}, body {:?}",
                tx.tx_type,
                tx.body.tx_type()
            )));
        }
        let migrated_native =
            native_authorized && inference_contract_state::is_native_body(&tx.body);
        if !Self::v3_allows_transaction(&tx.body) && !migrated_native {
            return Err(StateError::ExecutionError(format!(
                "transaction type {:?} is unavailable in recovery protocol v3",
                tx.tx_type
            )));
        }
        let serialized_size = bincode::serialize(tx)
            .map_err(|error| {
                StateError::ExecutionError(format!(
                    "transaction cannot be size-checked for v3 admission: {error}"
                ))
            })?
            .len() as u64;
        if serialized_size > V3_MAX_TRANSACTION_BYTES {
            return Err(StateError::ExecutionError(format!(
                "transaction size {serialized_size} exceeds v3 maximum {V3_MAX_TRANSACTION_BYTES}"
            )));
        }
        let operation_gas = Self::gas_cost_for_tx(tx);
        if tx.gas_limit != 0
            && (tx.gas_limit < operation_gas || tx.gas_limit > gas_costs::BLOCK_GAS_LIMIT)
        {
            return Err(StateError::ExecutionError(format!(
                "transaction gas limit {} is outside [{operation_gas}, {}]",
                tx.gas_limit,
                gas_costs::BLOCK_GAS_LIMIT
            )));
        }
        self.verify_transaction_signature(tx).map_err(|_| {
            StateError::ExecutionError(
                "invalid transaction signature for recovery protocol v3".to_string(),
            )
        })
    }

    fn validate_v3_transfer_admission(
        &self,
        tx: &Transaction,
        body: &TransferBody,
    ) -> Result<(), StateError> {
        if body.amount == 0 {
            return Err(StateError::ExecutionError(
                "v3 transfer amount must be positive".to_string(),
            ));
        }
        if tx.fee < V3_MIN_TRANSFER_FEE {
            return Err(StateError::ExecutionError(format!(
                "v3 transfer fee {} is below minimum {V3_MIN_TRANSFER_FEE}",
                tx.fee
            )));
        }
        if body.amount_commitment.is_some() {
            return Err(StateError::ExecutionError(
                "v3 transfer amount commitments are not active".to_string(),
            ));
        }
        let fee_treasury = v3_fee_treasury_address();
        if tx.from == body.to
            || is_reserved_system_account(&tx.from)
            || is_reserved_system_account(&body.to)
            || tx.from == fee_treasury
            || body.to == fee_treasury
        {
            return Err(StateError::ExecutionError(
                "v3 transfer sender/recipient/system account collision".to_string(),
            ));
        }
        let sender = self
            .get_account(&tx.from)
            .ok_or(StateError::InsufficientBalance {
                have: 0,
                need: body.amount.saturating_add(tx.fee),
            })?;
        if sender.nonce != tx.nonce {
            return Err(StateError::InvalidNonce {
                expected: sender.nonce,
                got: tx.nonce,
            });
        }
        sender
            .nonce
            .checked_add(1)
            .ok_or_else(|| StateError::ExecutionError("transfer nonce overflow".to_string()))?;
        let total_debit = body.amount.checked_add(tx.fee).ok_or_else(|| {
            StateError::ExecutionError("transfer amount plus fee overflow".to_string())
        })?;
        if sender.balance < total_debit {
            return Err(StateError::InsufficientBalance {
                have: sender.balance,
                need: total_debit,
            });
        }
        let receiver = self
            .get_account(&body.to)
            .unwrap_or_else(|| Account::new(body.to, 0));
        receiver.balance.checked_add(body.amount).ok_or_else(|| {
            StateError::ExecutionError("transfer recipient balance overflow".to_string())
        })?;
        let treasury = self
            .get_account(&fee_treasury)
            .unwrap_or_else(|| Account::new(fee_treasury, 0));
        treasury.balance.checked_add(tx.fee).ok_or_else(|| {
            StateError::ExecutionError("transfer fee treasury balance overflow".to_string())
        })?;
        Ok(())
    }

    fn validate_v3_faucet_admission(
        &self,
        tx: &Transaction,
        body: &arc_types::transaction::FaucetClaimBody,
    ) -> Result<(), StateError> {
        if tx.fee != 0 {
            return Err(StateError::ExecutionError(
                "faucet claim cannot charge an unaccounted transaction fee".to_string(),
            ));
        }
        if self.active_validators().len()
            != arc_types::transaction::COMMUNITY_REWARD_VALIDATOR_SET_SIZE
        {
            return Err(StateError::ExecutionError(format!(
                "faucet claim requires the exact {}-validator recovery authority set",
                arc_types::transaction::COMMUNITY_REWARD_VALIDATOR_SET_SIZE
            )));
        }
        if !self.is_validator(&tx.from) {
            return Err(StateError::ExecutionError(format!(
                "faucet claim: signer {} is not an active validator",
                tx.from.to_hex()
            )));
        }
        if body.amount == 0 || body.amount > arc_types::transaction::FAUCET_CLAIM_MAX {
            return Err(StateError::ExecutionError(format!(
                "faucet claim: amount {} outside [1, {}]",
                body.amount,
                arc_types::transaction::FAUCET_CLAIM_MAX
            )));
        }
        let pool = arc_types::transaction::faucet_pool_address();
        let marker = arc_types::transaction::FaucetClaimBody::v3_marker_address(&body.recipient);
        let legacy_marker =
            arc_types::transaction::FaucetClaimBody::marker_address(&body.recipient);
        let recipient_is_existing_faucet_marker =
            self.get_account(&body.recipient).is_some_and(|account| {
                account.balance == 0
                    && account.nonce == 1
                    && account.code_hash != Hash256::ZERO
                    && arc_types::transaction::FaucetClaimBody::marker_address(&account.code_hash)
                        == body.recipient
            });
        if is_reserved_system_account(&body.recipient)
            || marker == pool
            || marker == body.recipient
            || recipient_is_existing_faucet_marker
        {
            return Err(StateError::ExecutionError(
                "faucet claim: system account address collision".to_string(),
            ));
        }
        if self.accounts.contains_key(&marker.0) {
            return Err(StateError::ExecutionError(format!(
                "faucet claim: recipient {} has already claimed",
                body.recipient
            )));
        }
        if self.get_account(&legacy_marker).is_some_and(|account| {
            account.balance == 0
                && account.staked_balance == 0
                && account.nonce == 1
                && account.code_hash == body.recipient
                && account.storage_root != Hash256::ZERO
        }) {
            return Err(StateError::ExecutionError(format!(
                "faucet claim: recipient {} already claimed under legacy marker semantics",
                body.recipient
            )));
        }
        let pool_account = self.get_account(&pool).ok_or_else(|| {
            StateError::ExecutionError(
                "faucet claim: system faucet pool account is not prefunded".to_string(),
            )
        })?;
        if pool_account.balance < body.amount {
            return Err(StateError::InsufficientBalance {
                have: pool_account.balance,
                need: body.amount,
            });
        }
        let recipient = self
            .get_account(&body.recipient)
            .unwrap_or_else(|| Account::new(body.recipient, 0));
        recipient.balance.checked_add(body.amount).ok_or_else(|| {
            StateError::ExecutionError("faucet claim: recipient balance overflow".to_string())
        })?;
        Ok(())
    }

    fn next_community_reward_budget_counter(
        &self,
        address: Address,
        code_hash: Hash256,
        limit: u64,
        label: &str,
    ) -> Result<Account, StateError> {
        let policy_hash = community_reward_issuance_policy_hash();
        let mut account = self
            .get_account(&address)
            .unwrap_or_else(|| Account::new(address, 0));
        if account.nonce != 0 || account.code_hash != Hash256::ZERO {
            if account.balance != 0
                || account.staked_balance != 0
                || account.code_hash != code_hash
                || account.storage_root != policy_hash
            {
                return Err(StateError::ExecutionError(format!(
                    "community inference reward: invalid {label} budget account"
                )));
            }
        } else if account.balance != 0
            || account.staked_balance != 0
            || account.storage_root != Hash256::ZERO
        {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: {label} budget address collides with live state"
            )));
        }
        let next = account.nonce.checked_add(1).ok_or_else(|| {
            StateError::ExecutionError(format!(
                "community inference reward: {label} budget counter overflow"
            ))
        })?;
        if next > limit {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: {label} issuance limit {limit} reached"
            )));
        }
        account.nonce = next;
        account.code_hash = code_hash;
        account.storage_root = policy_hash;
        Ok(account)
    }

    fn community_reward_budget_candidates_at(
        &self,
        body: &arc_types::transaction::CommunityInferenceRewardBody,
        height: u64,
    ) -> Result<CommunityRewardBudgetCandidates, StateError> {
        let epoch = height / V3_COMMUNITY_REWARD_EPOCH_BLOCKS;
        let block_address = community_reward_block_budget_address(height);
        let epoch_address = community_reward_epoch_budget_address(epoch);
        let worker_address = community_reward_worker_budget_address(epoch, &body.worker);
        let coordinator_address =
            community_reward_coordinator_budget_address(epoch, &body.coordinator);
        let addresses = [
            block_address,
            epoch_address,
            worker_address,
            coordinator_address,
        ];
        let unique: HashSet<_> = addresses.iter().map(|address| address.0).collect();
        let treasury = arc_types::transaction::inference_reward_treasury_address();
        let job_marker = arc_types::transaction::CommunityInferenceRewardBody::v3_marker_address(
            &body.chain_domain,
            &body.job_id,
        );
        let certificate_marker =
            arc_types::transaction::CommunityInferenceRewardBody::v3_certificate_marker_address(
                &body.chain_domain,
                &body.worker,
                &body.worker_certificate.attestation_hash,
            );
        let recovery_probe_marker =
            arc_types::transaction::CommunityInferenceRewardBody::v3_recovery_probe_marker_address(
                &body.chain_domain,
                &body.assignment_epoch,
            );
        if unique.len() != addresses.len()
            || addresses
                .iter()
                .any(|address| !arc_types::transaction::is_v3_system_account(address))
            || addresses.iter().any(|address| {
                *address == treasury
                    || *address == body.worker
                    || *address == body.coordinator
                    || *address == job_marker
                    || *address == certificate_marker
                    || recovery_probe_marker == Some(*address)
            })
        {
            return Err(StateError::ExecutionError(
                "community inference reward: issuance budget address collision".to_string(),
            ));
        }
        let block = self.next_community_reward_budget_counter(
            block_address,
            hash_bytes(b"ARC-community-reward-block-budget-v3"),
            V3_COMMUNITY_REWARD_MAX_PER_BLOCK,
            "block",
        )?;
        let epoch_account = self.next_community_reward_budget_counter(
            epoch_address,
            hash_bytes(b"ARC-community-reward-epoch-budget-v3"),
            V3_COMMUNITY_REWARD_MAX_PER_EPOCH,
            "epoch",
        )?;
        let worker = self.next_community_reward_budget_counter(
            worker_address,
            body.worker,
            V3_COMMUNITY_REWARD_MAX_PER_WORKER_EPOCH,
            "worker epoch",
        )?;
        let coordinator = self.next_community_reward_budget_counter(
            coordinator_address,
            body.coordinator,
            V3_COMMUNITY_REWARD_MAX_PER_COORDINATOR_EPOCH,
            "coordinator epoch",
        )?;
        Ok(CommunityRewardBudgetCandidates {
            block: (block_address, block),
            epoch: (epoch_address, epoch_account),
            worker: (worker_address, worker),
            coordinator: (coordinator_address, coordinator),
        })
    }

    pub fn community_reward_budget_status(
        &self,
        worker: Address,
        coordinator: Address,
        block_height: u64,
    ) -> CommunityRewardBudgetStatus {
        let epoch = community_reward_epoch_index(block_height);
        let count = |address: Address| {
            self.get_account(&address)
                .map(|account| account.nonce)
                .unwrap_or(0)
        };
        let issued_this_block = count(community_reward_block_budget_address(block_height));
        let issued_this_epoch = count(community_reward_epoch_budget_address(epoch));
        let worker_issued_this_epoch =
            count(community_reward_worker_budget_address(epoch, &worker));
        let coordinator_issued_this_epoch = count(community_reward_coordinator_budget_address(
            epoch,
            &coordinator,
        ));
        CommunityRewardBudgetStatus {
            block_height,
            epoch,
            issued_this_block,
            remaining_this_block: V3_COMMUNITY_REWARD_MAX_PER_BLOCK
                .saturating_sub(issued_this_block),
            issued_this_epoch,
            remaining_this_epoch: V3_COMMUNITY_REWARD_MAX_PER_EPOCH
                .saturating_sub(issued_this_epoch),
            worker_issued_this_epoch,
            worker_remaining_this_epoch: V3_COMMUNITY_REWARD_MAX_PER_WORKER_EPOCH
                .saturating_sub(worker_issued_this_epoch),
            coordinator_issued_this_epoch,
            coordinator_remaining_this_epoch: V3_COMMUNITY_REWARD_MAX_PER_COORDINATOR_EPOCH
                .saturating_sub(coordinator_issued_this_epoch),
            policy_hash: community_reward_issuance_policy_hash(),
        }
    }

    pub fn current_community_reward_budget_status(
        &self,
        worker: Address,
        coordinator: Address,
    ) -> CommunityRewardBudgetStatus {
        self.community_reward_budget_status(worker, coordinator, self.height())
    }

    fn validate_community_reward_admission(
        &self,
        tx: &Transaction,
        body: &arc_types::transaction::CommunityInferenceRewardBody,
        execution_height: u64,
    ) -> Result<(), StateError> {
        if tx.fee != 0 {
            return Err(StateError::ExecutionError(
                "community inference reward cannot charge an unaccounted transaction fee"
                    .to_string(),
            ));
        }
        if self
            .community_rewards_v1_activation_height()
            .is_none_or(|activation| execution_height < activation)
        {
            return Err(StateError::ExecutionError(
                "community inference reward: protocol feature is not active at this height"
                    .to_string(),
            ));
        }
        if !self.is_validator(&tx.from) {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: signer {} is not an active validator",
                tx.from.to_hex()
            )));
        }
        let expected_domain =
            arc_types::transaction::CommunityInferenceRewardBody::expected_chain_domain();
        if body.chain_domain != expected_domain {
            return Err(StateError::ExecutionError(
                "community inference reward: wrong chain domain".to_string(),
            ));
        }
        if body.job_id == Hash256::ZERO {
            return Err(StateError::ExecutionError(
                "community inference reward: job_id cannot be zero".to_string(),
            ));
        }
        if body.coordinator != tx.from || !self.is_validator(&body.coordinator) {
            return Err(StateError::ExecutionError(
                "community inference reward: assignment coordinator must be the active outer signer"
                    .to_string(),
            ));
        }
        if body.assignment_epoch == Hash256::ZERO {
            return Err(StateError::ExecutionError(
                "community inference reward: assignment_epoch cannot be zero".to_string(),
            ));
        }
        match self.recovery_context() {
            Some(context)
                if body.recovery_epoch == context.recovery_epoch
                    && body.validator_set_id == context.validator_set_id
                    && body.transaction_domain == context.domain_hash() => {}
            Some(_) => {
                return Err(StateError::ExecutionError(
                    "community inference reward: recovery epoch, validator-set ID, or transaction domain does not match active state"
                        .to_string(),
                ));
            }
            None if body.recovery_epoch == 0
                && body.validator_set_id == 0
                && body.transaction_domain == Hash256::ZERO => {}
            None => {
                return Err(StateError::ExecutionError(
                    "community inference reward: recovery binding is non-zero on legacy/dev state"
                        .to_string(),
                ));
            }
        }
        let expected_job_id = arc_types::transaction::CommunityInferenceRewardBody::derive_job_id(
            &body.coordinator,
            &body.assignment_epoch,
            body.job_nonce,
            &body.model_id,
            &body.input_hash,
            body.max_tokens,
        );
        if body.job_id != expected_job_id {
            return Err(StateError::ExecutionError(
                "community inference reward: job_id does not match its exact assignment commitment"
                    .to_string(),
            ));
        }
        if body.max_tokens == 0 {
            return Err(StateError::ExecutionError(
                "community inference reward: max_tokens must be positive".to_string(),
            ));
        }
        if execution_height > body.expires_at_height {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: job expired at height {} (current {})",
                body.expires_at_height, execution_height
            )));
        }
        let worker_attestation = body.reconstruct_worker_attestation();
        self.verify_transaction_signature(&worker_attestation)
            .map_err(|_| {
                StateError::ExecutionError(
                    "community inference reward: invalid worker certificate signature".to_string(),
                )
            })?;
        let treasury = arc_types::transaction::inference_reward_treasury_address();
        let worker_stake = self.get_validator_stake(&body.worker).unwrap_or(0);
        if below_policy_minimum(
            worker_stake,
            arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE,
        ) {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: worker stake {} is below active policy minimum {}",
                worker_stake,
                arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE
            )));
        }
        if body.worker == treasury
            || is_reserved_system_account(&body.worker)
            || is_reserved_system_account(&body.coordinator)
        {
            return Err(StateError::ExecutionError(
                "community inference reward: worker/coordinator cannot be a system account"
                    .to_string(),
            ));
        }
        let job_marker = arc_types::transaction::CommunityInferenceRewardBody::v3_marker_address(
            &body.chain_domain,
            &body.job_id,
        );
        let certificate_marker =
            arc_types::transaction::CommunityInferenceRewardBody::v3_certificate_marker_address(
                &body.chain_domain,
                &body.worker,
                &body.worker_certificate.attestation_hash,
            );
        let recovery_probe_marker =
            arc_types::transaction::CommunityInferenceRewardBody::v3_recovery_probe_marker_address(
                &body.chain_domain,
                &body.assignment_epoch,
            );
        if job_marker == certificate_marker
            || job_marker == body.worker
            || certificate_marker == body.worker
            || job_marker == treasury
            || certificate_marker == treasury
            || recovery_probe_marker.is_some_and(|marker| {
                marker == job_marker
                    || marker == certificate_marker
                    || marker == body.worker
                    || marker == body.coordinator
                    || marker == treasury
            })
        {
            return Err(StateError::ExecutionError(
                "community inference reward: marker address collision".to_string(),
            ));
        }
        if self.accounts.contains_key(&job_marker.0) {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: job {} already paid",
                body.job_id.to_hex()
            )));
        }
        if self.accounts.contains_key(&certificate_marker.0) {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: worker certificate {} already paid",
                body.worker_certificate.attestation_hash.to_hex()
            )));
        }
        if let Some(marker) = recovery_probe_marker
            && self.accounts.contains_key(&marker.0)
        {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: recovery probe {} already paid",
                body.assignment_epoch.to_hex()
            )));
        }
        let legacy_job_marker =
            arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                &body.chain_domain,
                &body.job_id,
            );
        if self.get_account(&legacy_job_marker).is_some_and(|account| {
            account.balance == 0
                && account.staked_balance == 0
                && account.nonce == 1
                && account.code_hash != Hash256::ZERO
        }) {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: job {} already paid under legacy marker semantics",
                body.job_id.to_hex()
            )));
        }
        let legacy_certificate_marker =
            arc_types::transaction::CommunityInferenceRewardBody::certificate_marker_address(
                &body.chain_domain,
                &body.worker,
                &body.worker_certificate.attestation_hash,
            );
        if self
            .get_account(&legacy_certificate_marker)
            .is_some_and(|account| {
                account.balance == 0
                    && account.staked_balance == 0
                    && account.nonce == 1
                    && account.code_hash != Hash256::ZERO
            })
        {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: worker certificate {} already paid under legacy marker semantics",
                body.worker_certificate.attestation_hash.to_hex()
            )));
        }
        if let Some(legacy_probe_marker) =
            arc_types::transaction::CommunityInferenceRewardBody::recovery_probe_marker_address(
                &body.chain_domain,
                &body.assignment_epoch,
            )
            && self
                .get_account(&legacy_probe_marker)
                .is_some_and(|account| {
                    account.balance == 0
                        && account.staked_balance == 0
                        && account.nonce == 1
                        && account.code_hash != Hash256::ZERO
                })
        {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: recovery probe {} already paid under legacy marker semantics",
                body.assignment_epoch.to_hex()
            )));
        }
        self.verify_community_reward_validator_approvals(body)?;
        let reward = arc_types::economics::INFERENCE_ATTESTATION_REWARD;
        let worker = self
            .get_account(&body.worker)
            .unwrap_or_else(|| Account::new(body.worker, 0));
        worker.balance.checked_add(reward).ok_or_else(|| {
            StateError::ExecutionError(
                "community inference reward: worker balance overflow".to_string(),
            )
        })?;
        let treasury_account = self.get_account(&treasury).ok_or_else(|| {
            StateError::ExecutionError(
                "community inference reward: treasury is not funded".to_string(),
            )
        })?;
        if treasury_account.balance < reward {
            return Err(StateError::InsufficientBalance {
                have: treasury_account.balance,
                need: reward,
            });
        }
        if self.active_protocol_version().major == 3 {
            self.community_reward_budget_candidates_at(body, execution_height)?;
        }
        Ok(())
    }

    pub fn validate_v3_transaction_admission(&self, tx: &Transaction) -> Result<(), StateError> {
        let _guard = self.native_inference_execution.lock();
        let execution_height = self.height().checked_add(1).ok_or_else(|| {
            StateError::ExecutionError("prospective v3 block height overflow".to_string())
        })?;
        self.validate_v3_transaction_admission_at(tx, execution_height)
    }

    pub fn validate_v3_transaction_admission_at(
        &self,
        tx: &Transaction,
        execution_height: u64,
    ) -> Result<(), StateError> {
        if self.active_protocol_version().major != 3 {
            return Err(StateError::ExecutionError(
                "protocol-v3 admission requested without an active recovery context".to_string(),
            ));
        }
        self.validate_v3_transaction_envelope(tx)?;
        match &tx.body {
            TxBody::Transfer(body) => self.validate_v3_transfer_admission(tx, body),
            TxBody::FaucetClaim(body) => self.validate_v3_faucet_admission(tx, body),
            TxBody::CommunityInferenceReward(body) => {
                self.validate_community_reward_admission(tx, body, execution_height)
            }
            TxBody::NativeInferenceRequest(_)
            | TxBody::NativeInferenceFinalize(_)
            | TxBody::NativeInferenceRefund(_) => self
                .validate_native_inference_block_admission_at(
                    std::slice::from_ref(tx),
                    execution_height,
                ),
            TxBody::Settle(_)
            | TxBody::Swap(_)
            | TxBody::Escrow(_)
            | TxBody::Stake(_)
            | TxBody::WasmCall(_)
            | TxBody::MultiSig(_)
            | TxBody::DeployContract(_)
            | TxBody::RegisterAgent(_)
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
            | TxBody::InferenceFinalize(_) => Err(StateError::ExecutionError(
                "transaction family has no protocol-v3 admission handler".to_string(),
            )),
        }
    }

    pub fn validate_v3_dag_availability(
        &self,
        transactions: &[Transaction],
    ) -> Result<(), StateError> {
        if self.active_protocol_version().major != 3 {
            return Err(StateError::ExecutionError(
                "protocol-v3 DAG availability requires recovery protocol v3".into(),
            ));
        }
        let contains_native = transactions
            .iter()
            .any(|tx| inference_contract_state::is_native_body(&tx.body));
        if contains_native && transactions.len() != 1 {
            return Err(StateError::ExecutionError(
                "native v3 blocks contain one native transition and cannot mix transaction families".into(),
            ));
        }
        let native_authorized = contains_native
            && self.native_migration().is_some_and(|record| {
                if self.native_inference_context().is_some() {
                    return true;
                }
                record.activation_height > self.height()
                    && record.refusal_against(self).is_none()
                    && self
                        .pending_migration_context
                        .read()
                        .as_ref()
                        .is_some_and(|context| {
                            inference_contract_state::validate_native_inference_binding_shape(
                                self, context,
                            )
                            .is_ok_and(|commitment| commitment == record.context_commitment)
                        })
            });
        if transactions.len() > V3_MAX_TRANSACTIONS_PER_BLOCK {
            return Err(StateError::ExecutionError(format!(
                "v3 block contains {} transactions; maximum is {V3_MAX_TRANSACTIONS_PER_BLOCK}",
                transactions.len()
            )));
        }
        let mut senders = HashMap::<[u8; 32], usize>::new();
        let mut hashes = HashSet::new();
        let mut accessed = HashSet::new();
        for tx in transactions {
            if !hashes.insert(tx.hash.0) {
                return Err(StateError::ExecutionError(
                    "v3 block contains a duplicate transaction hash".into(),
                ));
            }
            let sender_count = senders.entry(tx.from.0).or_default();
            *sender_count += 1;
            if *sender_count > V3_MAX_TRANSACTIONS_PER_SENDER_PER_BLOCK {
                return Err(StateError::ExecutionError(
                    "v3 block exceeds per-sender transaction limit".into(),
                ));
            }
            self.validate_v3_transaction_envelope_with_native(tx, native_authorized)?;
            if !contains_native {
                for account in crate::block_stm::tx_access_set(tx).accounts {
                    if !accessed.insert(account) {
                        return Err(StateError::ExecutionError(
                            "v3 block contains overlapping transaction state access".into(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn validate_v3_block_admission(
        &self,
        transactions: &[Transaction],
    ) -> Result<(), StateError> {
        let execution_height = self.height().checked_add(1).ok_or_else(|| {
            StateError::ExecutionError("prospective v3 block height overflow".to_string())
        })?;
        self.validate_v3_block_admission_at(transactions, execution_height)
    }

    pub fn validate_v3_block_admission_at(
        &self,
        transactions: &[Transaction],
        execution_height: u64,
    ) -> Result<(), StateError> {
        if transactions
            .iter()
            .any(|tx| inference_contract_state::is_native_body(&tx.body))
        {
            if self.active_protocol_version().major != 3 {
                return Err(StateError::ExecutionError(
                    "native v3 block requires recovery protocol v3".into(),
                ));
            }
            if transactions.len() != 1 {
                return Err(StateError::ExecutionError(
                    "native v3 blocks contain one native transition and cannot mix transaction families".into(),
                ));
            }
            self.validate_v3_transaction_envelope(&transactions[0])?;
            return self
                .validate_native_inference_block_admission_at(transactions, execution_height);
        }
        if transactions.len() > V3_MAX_TRANSACTIONS_PER_BLOCK {
            return Err(StateError::ExecutionError(format!(
                "v3 block contains {} transactions; maximum is {V3_MAX_TRANSACTIONS_PER_BLOCK}",
                transactions.len()
            )));
        }
        let mut senders = HashMap::<[u8; 32], usize>::new();
        let mut hashes = HashSet::with_capacity(transactions.len());
        let mut accessed = HashSet::new();
        for tx in transactions {
            if !hashes.insert(tx.hash.0) {
                return Err(StateError::ExecutionError(
                    "v3 block contains a duplicate transaction hash".to_string(),
                ));
            }
            let sender_count = senders.entry(tx.from.0).or_default();
            *sender_count += 1;
            if *sender_count > V3_MAX_TRANSACTIONS_PER_SENDER_PER_BLOCK {
                return Err(StateError::ExecutionError(format!(
                    "v3 block exceeds per-sender transaction limit for {}",
                    tx.from.to_hex()
                )));
            }
            self.validate_v3_transaction_admission_at(tx, execution_height)?;
            for account in crate::block_stm::tx_access_set(tx).accounts {
                if !accessed.insert(account) {
                    return Err(StateError::ExecutionError(
                        "v3 block contains overlapping transaction state access".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn memory_gauges(&self) -> Vec<(&'static str, u64)> {
        let account_tx_entries: usize = self.account_txs.iter().map(|e| e.value().len()).sum();
        let event_log_entries: usize = self.event_logs.iter().map(|e| e.value().len()).sum();
        [
            ("state_accounts", self.accounts.len()),
            ("state_storage_contracts", self.storage.len()),
            ("state_blocks", self.blocks.len()),
            ("state_receipts", self.receipts.len()),
            ("state_tx_index", self.tx_index.len()),
            ("state_full_transactions", self.full_transactions.len()),
            ("state_account_txs_addresses", self.account_txs.len()),
            ("state_account_txs_entries", account_tx_entries),
            ("state_event_log_heights", self.event_logs.len()),
            ("state_event_log_entries", event_log_entries),
            ("state_contracts", self.contracts.len()),
            ("state_identities", self.identities.len()),
            ("state_signed_block_data", self.signed_block_data.len()),
            ("state_dirty_accounts", self.dirty_accounts.len()),
            ("state_validators", self.validators.len()),
            ("state_native_pending", self.native_inference_pending.len()),
            (
                "state_pending_bond_release_heights",
                self.pending_bond_releases.lock().len(),
            ),
        ]
        .into_iter()
        .map(|(name, len)| (name, len as u64))
        .collect()
    }

    pub fn height(&self) -> u64 {
        let _publication = self.native_inference_publication.read();
        *self.height.read()
    }

    pub fn evict_transactions(&self, max_entries: usize) {
        if self.archive_mode {
            return;
        }
        let current = self.full_transactions.len();
        if current <= max_entries {
            return;
        }
        let to_remove = current - max_entries;
        let keys: Vec<[u8; 32]> = self
            .full_transactions
            .iter()
            .take(to_remove)
            .map(|entry| *entry.key())
            .collect();
        for key in &keys {
            self.full_transactions.remove(key);
        }
        tracing::debug!(
            evicted = keys.len(),
            remaining = self.full_transactions.len(),
            "Evicted old transaction bodies from memory"
        );

        let wal_seq = self.wal.sequence();
        if wal_seq > 1000
            && let Err(e) = self.wal.delete_segments_before(wal_seq - 1000)
        {
            tracing::warn!("WAL segment cleanup failed: {}", e);
        }
    }

    pub fn get_block(&self, height: u64) -> Option<Block> {
        let _publication = self.native_inference_publication.read();
        self.blocks.get(&height).map(|b| Block::clone(&b))
    }

    pub fn get_block_by_hash(&self, hash: &[u8; 32]) -> Option<Block> {
        let h = self.height();
        for height in (0..=h).rev() {
            if let Some(block) = self.blocks.get(&height)
                && block.hash.0 == *hash
            {
                return Some(Block::clone(&block));
            }
        }
        None
    }

    pub fn execute_block(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        if self.native_inference_context.read().is_some() {
            return self.execute_block_verified(transactions, producer);
        }
        self.require_healthy_wal()?;
        self.validate_next_protocol_block_admission(transactions)?;
        let mut receipts = Vec::with_capacity(transactions.len());
        let mut tx_hashes = Vec::with_capacity(transactions.len());

        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        if transactions.len() >= 16 {
            let batches = block_stm::partition_batches(transactions);
            tracing::info!(
                block_height = height,
                tx_count = transactions.len(),
                batch_count = batches.len(),
                "Block-STM parallel execution: {} txs across {} batches",
                transactions.len(),
                batches.len(),
            );

            receipts.resize(
                transactions.len(),
                TxReceipt {
                    tx_hash: Hash256::ZERO,
                    block_height: height,
                    block_hash: Hash256::ZERO,
                    index: 0,
                    success: false,
                    gas_used: 0,
                    value_commitment: None,
                    inclusion_proof: None,
                    logs: vec![],
                },
            );

            for tx in transactions.iter() {
                tx_hashes.push(tx.hash);
            }

            for batch_indices in &batches {
                for &idx in batch_indices {
                    self.mark_tx_accounts_dirty(&transactions[idx]);
                }

                let results: Vec<(usize, bool, u64)> = batch_indices
                    .par_iter()
                    .map(|&idx| {
                        let result = self.execute_tx(&transactions[idx]);
                        let (success, gas_used) = match result {
                            Ok(gas) => (true, gas),
                            Err(_) => (false, Self::gas_cost_for_tx(&transactions[idx])),
                        };
                        (idx, success, gas_used)
                    })
                    .collect();

                for (idx, success, gas_used) in results {
                    receipts[idx] = TxReceipt {
                        tx_hash: transactions[idx].hash,
                        block_height: height,
                        block_hash: Hash256::ZERO,
                        index: idx as u32,
                        success,
                        gas_used,
                        value_commitment: None,
                        inclusion_proof: None,
                        logs: vec![],
                    };
                }
            }
        } else {
            for (i, tx) in transactions.iter().enumerate() {
                self.mark_tx_accounts_dirty(tx);
                let result = self.execute_tx(tx);
                let (success, gas_used) = match result {
                    Ok(gas) => (true, gas),
                    Err(_) => (false, Self::gas_cost_for_tx(tx)),
                };

                tx_hashes.push(tx.hash);

                receipts.push(TxReceipt {
                    tx_hash: tx.hash,
                    block_height: height,
                    block_hash: Hash256::ZERO,
                    index: i as u32,
                    success,
                    gas_used,
                    value_commitment: None,
                    inclusion_proof: None,
                    logs: vec![],
                });
            }
        }

        let total_gas: u64 = receipts.iter().map(|r| r.gas_used).sum();
        if total_gas > gas_costs::BLOCK_GAS_LIMIT * 80 / 100 {
            tracing::warn!(
                total_gas,
                limit = gas_costs::BLOCK_GAS_LIMIT,
                "Block nearing gas limit"
            );
        }

        self.sweep_matured_bond_releases(height);
        self.apply_due_binding_change_in_block()?;

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();

        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            parent_hash: parent,
            tx_root,
            state_root,
            proof_hash: Hash256::ZERO,
            tx_count: transactions.len() as u32,
            producer,
            protocol_version: self.active_protocol_version(),
            state_diff: None,
        };

        let block = Block::new(header, tx_hashes);

        for (i, receipt) in receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
            self.full_transactions.insert(tx.hash.0, tx.clone());
        }

        self.blocks.insert(height, Box::new(block.clone()));
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);

        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        let count = self.snapshot_counter.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count.is_multiple_of(10_000) {
            tracing::info!("Snapshot trigger at block {}", height);
        }

        Ok((block, receipts))
    }

    pub fn execute_block_adaptive(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.execute_block_adaptive_at(transactions, producer, timestamp)
    }

    pub fn execute_block_adaptive_at(
        &self,
        transactions: &[Transaction],
        producer: Address,
        timestamp: u64,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        self.execute_block_adaptive_at_with_proof(transactions, producer, timestamp, Hash256::ZERO)
    }

    pub(crate) fn execution_mode(
        &self,
        transactions: &[Transaction],
    ) -> crate::block_stm::AdaptiveMode {
        if self.native_inference_context.read().is_some() {
            return crate::block_stm::AdaptiveMode::Sequential;
        }
        if self.native_migration.read().is_some() || self.pending_binding_update.read().is_some() {
            return crate::block_stm::AdaptiveMode::Sequential;
        }
        crate::block_stm::choose_execution_mode(transactions)
    }

    pub fn execute_block_adaptive_at_with_proof(
        &self,
        transactions: &[Transaction],
        producer: Address,
        timestamp: u64,
        proof_hash: Hash256,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        let produced = match self.execution_mode(transactions) {
            crate::block_stm::AdaptiveMode::Sequential => self
                .execute_block_verified_at_with_proof(
                    transactions,
                    producer,
                    timestamp,
                    proof_hash,
                ),
            crate::block_stm::AdaptiveMode::BlockSTM => {
                self.execute_block_blockstm_at(transactions, producer, timestamp, proof_hash)
            }
        }?;
        self.apply_due_native_migration()?;
        self.apply_due_binding_update()?;
        Ok(produced)
    }

    fn execute_block_blockstm_at(
        &self,
        transactions: &[Transaction],
        producer: Address,
        timestamp: u64,
        proof_hash: Hash256,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        if self.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "protocol-4 requires canonical sequential block execution".into(),
            ));
        }
        self.require_healthy_wal()?;
        self.validate_next_protocol_block_admission(transactions)?;
        use rayon::prelude::*;

        let batches = crate::block_stm::partition_batches(transactions);
        let mut receipts = vec![None; transactions.len()];
        let mut tx_hashes = vec![Hash256::ZERO; transactions.len()];

        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        for batch in &batches {
            let batch_results: Vec<(usize, bool, u64)> = batch
                .par_iter()
                .map(|&idx| {
                    let tx = &transactions[idx];
                    self.mark_tx_accounts_dirty(tx);
                    let result = if tx.sig_verified {
                        self.execute_tx(tx)
                    } else if tx.is_unsigned() {
                        Err(StateError::ExecutionError("unsigned transaction".into()))
                    } else if self.verify_transaction_signature(tx).is_err() {
                        Err(StateError::ExecutionError("invalid signature".into()))
                    } else {
                        self.execute_tx(tx)
                    };
                    let (success, gas_used) = match result {
                        Ok(gas) => (true, gas),
                        Err(_) => (false, Self::gas_cost_for_tx(tx)),
                    };
                    (idx, success, gas_used)
                })
                .collect();

            for (idx, success, gas_used) in batch_results {
                tx_hashes[idx] = transactions[idx].hash;
                receipts[idx] = Some(TxReceipt {
                    tx_hash: transactions[idx].hash,
                    block_height: height,
                    block_hash: Hash256::ZERO,
                    index: idx as u32,
                    success,
                    gas_used,
                    value_commitment: None,
                    inclusion_proof: None,
                    logs: vec![],
                });
            }
        }

        let receipts: Vec<TxReceipt> = receipts
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                r.unwrap_or(TxReceipt {
                    tx_hash: transactions[i].hash,
                    block_height: height,
                    block_hash: Hash256::ZERO,
                    index: i as u32,
                    success: false,
                    gas_used: 0,
                    value_commitment: None,
                    inclusion_proof: None,
                    logs: vec![],
                })
            })
            .collect();

        self.sweep_matured_bond_releases(height);
        self.apply_due_binding_change_in_block()?;

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();
        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp,
            parent_hash: parent,
            tx_root,
            state_root,
            proof_hash,
            tx_count: transactions.len() as u32,
            producer,
            protocol_version: self.active_protocol_version(),
            state_diff: None,
        };

        let block = Block::new(header, tx_hashes);

        let mut final_receipts = receipts;
        for (i, receipt) in final_receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, final_receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
            self.full_transactions.insert(tx.hash.0, tx.clone());
        }

        self.blocks.insert(height, Box::new(block.clone()));
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &final_receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        Ok((block, final_receipts))
    }

    pub fn execute_block_verified(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.execute_block_verified_at(transactions, producer, timestamp)
    }

    pub fn execute_block_verified_at(
        &self,
        transactions: &[Transaction],
        producer: Address,
        timestamp: u64,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        self.execute_block_verified_at_with_proof(transactions, producer, timestamp, Hash256::ZERO)
    }

    fn execute_block_verified_at_with_proof(
        &self,
        transactions: &[Transaction],
        producer: Address,
        timestamp: u64,
        proof_hash: Hash256,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        let _native_guard = self.native_inference_execution.lock();
        if self.native_inference_context.read().is_some() {
            let migrated = self.native_migration.read().is_some();
            let native_block = transactions
                .iter()
                .any(|tx| inference_contract_state::is_native_body(&tx.body));
            if !migrated || native_block {
                return self.execute_native_inference_block_at(
                    transactions,
                    producer,
                    timestamp,
                    proof_hash,
                );
            }
        }
        self.require_healthy_wal()?;
        self.validate_next_protocol_block_admission(transactions)?;
        let mut receipts = Vec::with_capacity(transactions.len());
        let mut tx_hashes = Vec::with_capacity(transactions.len());

        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        let mut batch_sig_valid = vec![None; transactions.len()];
        let recovery_domain = self.transaction_domain_hash();
        {
            let mut ed_indices: Vec<usize> = Vec::new();
            let mut ed_msgs: Vec<&[u8]> = Vec::new();
            let mut ed_sigs: Vec<ed25519_dalek::Signature> = Vec::new();
            let mut ed_vks: Vec<ed25519_dalek::VerifyingKey> = Vec::new();

            let mut ml_indices: Vec<usize> = Vec::new();
            let mut ml_msgs: Vec<&[u8]> = Vec::new();
            let mut ml_sigs: Vec<&[u8]> = Vec::new();
            let mut ml_pks: Vec<&[u8]> = Vec::new();

            let mut falcon_indices: Vec<usize> = Vec::new();
            let mut falcon_msgs: Vec<&[u8]> = Vec::new();
            let mut falcon_sigs: Vec<&[u8]> = Vec::new();
            let mut falcon_pks: Vec<&[u8]> = Vec::new();

            for (i, tx) in transactions.iter().enumerate() {
                if tx.is_unsigned() || tx.sig_verified {
                    continue;
                }
                let expected_hash = match recovery_domain {
                    Some(domain) => tx.compute_hash_in_domain(&domain),
                    None => tx.compute_hash(),
                };
                if expected_hash != tx.hash {
                    batch_sig_valid[i] = Some(false);
                    continue;
                }
                match &tx.signature {
                    arc_crypto::signature::Signature::Ed25519 {
                        public_key,
                        signature,
                    } => {
                        if arc_crypto::address_from_ed25519_pubkey(public_key) != tx.from {
                            batch_sig_valid[i] = Some(false);
                            continue;
                        }
                        if signature.len() == 64
                            && let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(public_key)
                        {
                            let mut sig_bytes = [0u8; 64];
                            sig_bytes.copy_from_slice(signature);
                            ed_indices.push(i);
                            ed_msgs.push(tx.hash.as_bytes().as_slice());
                            ed_sigs.push(ed25519_dalek::Signature::from_bytes(&sig_bytes));
                            ed_vks.push(vk);
                            continue;
                        }
                        batch_sig_valid[i] = Some(false);
                    }
                    arc_crypto::signature::Signature::MlDsa65 {
                        public_key,
                        signature,
                    } => {
                        if arc_crypto::address_from_ml_dsa_pubkey(public_key) != tx.from {
                            batch_sig_valid[i] = Some(false);
                            continue;
                        }
                        if signature.len() == arc_crypto::ML_DSA_SIG_LEN
                            && public_key.len() == arc_crypto::ML_DSA_PK_LEN
                        {
                            ml_indices.push(i);
                            ml_msgs.push(tx.hash.as_bytes().as_slice());
                            ml_sigs.push(signature.as_slice());
                            ml_pks.push(public_key.as_slice());
                            continue;
                        }
                        batch_sig_valid[i] = Some(false);
                    }
                    arc_crypto::signature::Signature::Falcon512 {
                        public_key,
                        signature,
                    } => {
                        if arc_crypto::address_from_falcon_pubkey(public_key) != tx.from {
                            batch_sig_valid[i] = Some(false);
                            continue;
                        }
                        if public_key.len() == arc_crypto::FALCON_PK_LEN
                            && !signature.is_empty()
                            && signature.len() <= arc_crypto::FALCON_SIG_MAX_LEN
                        {
                            falcon_indices.push(i);
                            falcon_msgs.push(tx.hash.as_bytes().as_slice());
                            falcon_sigs.push(signature.as_slice());
                            falcon_pks.push(public_key.as_slice());
                            continue;
                        }
                        batch_sig_valid[i] = Some(false);
                    }
                    _ => {}
                }
            }

            if !ed_indices.is_empty() {
                match arc_crypto::signature::batch_verify_ed25519(&ed_msgs, &ed_sigs, &ed_vks) {
                    Ok(()) => {
                        for &idx in &ed_indices {
                            batch_sig_valid[idx] = Some(true);
                        }
                    }
                    Err(_) => {
                        for &idx in &ed_indices {
                            let valid = self
                                .verify_transaction_signature(&transactions[idx])
                                .is_ok();
                            batch_sig_valid[idx] = Some(valid);
                        }
                    }
                }
            }

            if !ml_indices.is_empty() {
                let results = arc_crypto::ml_dsa_batch_verify(&ml_pks, &ml_msgs, &ml_sigs);
                for (&idx, valid) in ml_indices.iter().zip(results) {
                    batch_sig_valid[idx] = Some(valid);
                }
            }

            if !falcon_indices.is_empty() {
                let results =
                    arc_crypto::falcon_batch_verify(&falcon_pks, &falcon_msgs, &falcon_sigs);
                for (&idx, valid) in falcon_indices.iter().zip(results) {
                    batch_sig_valid[idx] = Some(valid);
                }
            }
        }

        for (i, tx) in transactions.iter().enumerate() {
            self.mark_tx_accounts_dirty(tx);
            let result = if tx.sig_verified {
                self.execute_tx(tx)
            } else if tx.is_unsigned() {
                Err(StateError::ExecutionError("unsigned transaction".into()))
            } else if let Some(valid) = batch_sig_valid[i] {
                if valid {
                    self.execute_tx(tx)
                } else {
                    Err(StateError::ExecutionError("invalid signature".into()))
                }
            } else if self.verify_transaction_signature(tx).is_err() {
                Err(StateError::ExecutionError("invalid signature".into()))
            } else {
                self.execute_tx(tx)
            };
            let (success, gas_used) = match result {
                Ok(gas) => (true, gas),
                Err(_) => (false, Self::gas_cost_for_tx(tx)),
            };

            tx_hashes.push(tx.hash);

            receipts.push(TxReceipt {
                tx_hash: tx.hash,
                block_height: height,
                block_hash: Hash256::ZERO,
                index: i as u32,
                success,
                gas_used,
                value_commitment: None,
                inclusion_proof: None,
                logs: vec![],
            });
        }

        self.sweep_matured_bond_releases(height);
        self.apply_due_binding_change_locked()?;

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();

        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp,
            parent_hash: parent,
            tx_root,
            state_root,
            proof_hash,
            tx_count: transactions.len() as u32,
            producer,
            protocol_version: self.active_protocol_version(),
            state_diff: None,
        };

        let block = Block::new(header, tx_hashes);

        for (i, receipt) in receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
            self.full_transactions.insert(tx.hash.0, tx.clone());
        }

        self.blocks.insert(height, Box::new(block.clone()));
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);

        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        let count = self.snapshot_counter.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count.is_multiple_of(10_000) {
            tracing::info!("Snapshot trigger at block {}", height);
        }

        Ok((block, receipts))
    }

    pub fn execute_block_gpu_verified(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        if self.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "protocol-4 requires canonical sequential block execution".into(),
            ));
        }
        self.require_healthy_wal()?;
        self.validate_next_protocol_block_admission(transactions)?;
        use arc_gpu::metal_verify::{MetalVerifier, VerifyTask};

        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        let mut verifier = MetalVerifier::new();

        let mut ed_indices: Vec<usize> = Vec::new();
        let mut ed_tasks: Vec<VerifyTask> = Vec::new();
        let mut other_indices: Vec<usize> = Vec::new();
        let mut sig_valid = vec![false; transactions.len()];
        let recovery_domain = self.transaction_domain_hash();

        for (i, tx) in transactions.iter().enumerate() {
            let expected_hash = match recovery_domain {
                Some(domain) => tx.compute_hash_in_domain(&domain),
                None => tx.compute_hash(),
            };
            if expected_hash != tx.hash {
                continue;
            }

            match &tx.signature {
                arc_crypto::signature::Signature::Ed25519 {
                    public_key,
                    signature,
                } => {
                    if signature.len() == 64
                        && arc_crypto::address_from_ed25519_pubkey(public_key) == tx.from
                    {
                        let mut sig_bytes = [0u8; 64];
                        sig_bytes.copy_from_slice(signature);
                        ed_tasks.push(VerifyTask {
                            message: tx.hash.0.to_vec(),
                            public_key: *public_key,
                            signature: sig_bytes,
                        });
                        ed_indices.push(i);
                    }
                }
                _ => {
                    other_indices.push(i);
                }
            }
        }

        if !ed_tasks.is_empty() {
            let result = verifier.batch_verify(&ed_tasks);
            let invalid_set: std::collections::HashSet<usize> =
                result.invalid_indices.iter().copied().collect();
            for (j, &orig_idx) in ed_indices.iter().enumerate() {
                sig_valid[orig_idx] = !invalid_set.contains(&j);
            }
        }

        for &i in &other_indices {
            let tx = &transactions[i];
            sig_valid[i] = tx.signature.verify(&tx.hash, &tx.from).is_ok();
        }

        let mut receipts = Vec::with_capacity(transactions.len());
        let mut tx_hashes = Vec::with_capacity(transactions.len());

        if transactions.len() >= 16 {
            let batches = block_stm::partition_batches(transactions);

            receipts.resize(
                transactions.len(),
                TxReceipt {
                    tx_hash: Hash256::ZERO,
                    block_height: height,
                    block_hash: Hash256::ZERO,
                    index: 0,
                    success: false,
                    gas_used: 0,
                    value_commitment: None,
                    inclusion_proof: None,
                    logs: vec![],
                },
            );

            for tx in transactions.iter() {
                tx_hashes.push(tx.hash);
            }

            for batch_indices in &batches {
                for &idx in batch_indices {
                    self.mark_tx_accounts_dirty(&transactions[idx]);
                }

                let results: Vec<(usize, bool, u64)> = batch_indices
                    .par_iter()
                    .map(|&idx| {
                        if !sig_valid[idx] {
                            return (idx, false, Self::gas_cost_for_tx(&transactions[idx]));
                        }
                        let result = self.execute_tx(&transactions[idx]);
                        let (success, gas_used) = match result {
                            Ok(gas) => (true, gas),
                            Err(_) => (false, Self::gas_cost_for_tx(&transactions[idx])),
                        };
                        (idx, success, gas_used)
                    })
                    .collect();

                for (idx, success, gas_used) in results {
                    receipts[idx] = TxReceipt {
                        tx_hash: transactions[idx].hash,
                        block_height: height,
                        block_hash: Hash256::ZERO,
                        index: idx as u32,
                        success,
                        gas_used,
                        value_commitment: None,
                        inclusion_proof: None,
                        logs: vec![],
                    };
                }
            }
        } else {
            for (i, tx) in transactions.iter().enumerate() {
                self.mark_tx_accounts_dirty(tx);
                let result = if !sig_valid[i] {
                    Err(StateError::ExecutionError("invalid signature".into()))
                } else {
                    self.execute_tx(tx)
                };
                let (success, gas_used) = match result {
                    Ok(gas) => (true, gas),
                    Err(_) => (false, Self::gas_cost_for_tx(tx)),
                };

                tx_hashes.push(tx.hash);

                receipts.push(TxReceipt {
                    tx_hash: tx.hash,
                    block_height: height,
                    block_hash: Hash256::ZERO,
                    index: i as u32,
                    success,
                    gas_used,
                    value_commitment: None,
                    inclusion_proof: None,
                    logs: vec![],
                });
            }
        }

        let total_gas: u64 = receipts.iter().map(|r| r.gas_used).sum();
        if total_gas > gas_costs::BLOCK_GAS_LIMIT * 80 / 100 {
            tracing::warn!(
                total_gas,
                limit = gas_costs::BLOCK_GAS_LIMIT,
                "Block nearing gas limit"
            );
        }

        self.sweep_matured_bond_releases(height);
        self.apply_due_binding_change_in_block()?;

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();
        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            parent_hash: parent,
            tx_root,
            state_root,
            proof_hash: Hash256::ZERO,
            tx_count: transactions.len() as u32,
            producer,
            protocol_version: self.active_protocol_version(),
            state_diff: None,
        };

        let block = Block::new(header, tx_hashes);

        for (i, receipt) in receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
            self.full_transactions.insert(tx.hash.0, tx.clone());
        }

        self.blocks.insert(height, Box::new(block.clone()));
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        let count = self.snapshot_counter.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count.is_multiple_of(10_000) {
            tracing::info!("Snapshot trigger at block {}", height);
        }

        Ok((block, receipts))
    }

    pub fn execute_block_parallel(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        if self.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "protocol-4 requires canonical sequential block execution".into(),
            ));
        }
        self.require_healthy_wal()?;
        self.validate_next_protocol_block_admission(transactions)?;
        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        let mut shards: HashMap<[u8; 32], Vec<(usize, &Transaction)>> = HashMap::new();
        for (i, tx) in transactions.iter().enumerate() {
            self.mark_tx_accounts_dirty(tx);
            shards.entry(tx.from.0).or_default().push((i, tx));
        }

        let shard_results: Vec<Vec<(usize, bool, u64)>> = shards
            .into_par_iter()
            .map(|(_sender, txs)| {
                let mut results = Vec::with_capacity(txs.len());
                for (idx, tx) in txs {
                    let (success, gas_used) = match self.execute_tx(tx) {
                        Ok(gas) => (true, gas),
                        Err(_) => (false, Self::gas_cost_for_tx(tx)),
                    };
                    results.push((idx, success, gas_used));
                }
                results
            })
            .collect();

        let mut receipt_success = vec![false; transactions.len()];
        let mut receipt_gas = vec![0u64; transactions.len()];
        for shard in shard_results {
            for (idx, success, gas_used) in shard {
                receipt_success[idx] = success;
                receipt_gas[idx] = gas_used;
            }
        }

        let tx_hashes: Vec<Hash256> = transactions.iter().map(|tx| tx.hash).collect();
        let receipts: Vec<TxReceipt> = transactions
            .iter()
            .enumerate()
            .map(|(i, tx)| TxReceipt {
                tx_hash: tx.hash,
                block_height: height,
                block_hash: Hash256::ZERO,
                index: i as u32,
                success: receipt_success[i],
                gas_used: receipt_gas[i],
                value_commitment: None,
                inclusion_proof: None,
                logs: vec![],
            })
            .collect();

        self.sweep_matured_bond_releases(height);
        self.apply_due_binding_change_in_block()?;

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();
        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            parent_hash: parent,
            tx_root,
            state_root,
            proof_hash: Hash256::ZERO,
            tx_count: transactions.len() as u32,
            producer,
            protocol_version: self.active_protocol_version(),
            state_diff: None,
        };

        let block = Block::new(header, tx_hashes);

        let mut receipts = receipts;
        for (i, receipt) in receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
            self.full_transactions.insert(tx.hash.0, tx.clone());
        }

        self.blocks.insert(height, Box::new(block.clone()));
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        Ok((block, receipts))
    }

    pub fn execute_block_stm(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        if self.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "protocol-4 requires canonical sequential block execution".into(),
            ));
        }
        self.require_healthy_wal()?;
        self.validate_next_protocol_block_admission(transactions)?;
        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        for tx in transactions {
            self.mark_tx_accounts_dirty(tx);
        }

        let batches = block_stm::partition_batches(transactions);

        let mut receipt_success = vec![false; transactions.len()];
        let mut receipt_gas = vec![0u64; transactions.len()];

        for batch in &batches {
            if batch.len() == 1 {
                let idx = batch[0];
                match self.execute_tx(&transactions[idx]) {
                    Ok(gas) => {
                        receipt_success[idx] = true;
                        receipt_gas[idx] = gas;
                    }
                    Err(_) => {
                        receipt_gas[idx] = Self::gas_cost_for_tx(&transactions[idx]);
                    }
                }
            } else {
                let results: Vec<(usize, bool, u64)> = batch
                    .par_iter()
                    .map(|&idx| match self.execute_tx(&transactions[idx]) {
                        Ok(gas) => (idx, true, gas),
                        Err(_) => (idx, false, Self::gas_cost_for_tx(&transactions[idx])),
                    })
                    .collect();
                for (idx, ok, gas) in results {
                    receipt_success[idx] = ok;
                    receipt_gas[idx] = gas;
                }
            }
        }

        let tx_hashes: Vec<Hash256> = transactions.iter().map(|tx| tx.hash).collect();
        let receipts: Vec<TxReceipt> = transactions
            .iter()
            .enumerate()
            .map(|(i, tx)| TxReceipt {
                tx_hash: tx.hash,
                block_height: height,
                block_hash: Hash256::ZERO,
                index: i as u32,
                success: receipt_success[i],
                gas_used: receipt_gas[i],
                value_commitment: None,
                inclusion_proof: None,
                logs: vec![],
            })
            .collect();

        self.sweep_matured_bond_releases(height);
        self.apply_due_binding_change_in_block()?;

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();
        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            parent_hash: parent,
            tx_root,
            state_root,
            proof_hash: Hash256::ZERO,
            tx_count: transactions.len() as u32,
            producer,
            protocol_version: self.active_protocol_version(),
            state_diff: None,
        };

        let block = Block::new(header, tx_hashes);

        let mut receipts = receipts;
        for (i, receipt) in receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
            self.full_transactions.insert(tx.hash.0, tx.clone());
        }

        self.blocks.insert(height, Box::new(block.clone()));
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        Ok((block, receipts))
    }

    pub fn execute_optimistic(&self, transactions: &[Transaction]) -> (usize, usize) {
        let mut shards: HashMap<[u8; 32], Vec<&Transaction>> = HashMap::new();
        for tx in transactions {
            self.mark_tx_accounts_dirty(tx);
            shards.entry(tx.from.0).or_default().push(tx);
        }
        for shard in shards.values_mut() {
            shard.sort_unstable_by_key(|tx| tx.nonce);
        }

        let results: Vec<usize> = shards
            .into_par_iter()
            .map(|(_sender, txs)| {
                let mut ok = 0usize;
                for tx in txs {
                    if self.execute_tx(tx).is_ok() {
                        ok += 1;
                    }
                }
                ok
            })
            .collect();

        let success = results.iter().sum();
        (success, transactions.len())
    }

    #[cfg(feature = "benchmark-tools")]
    pub fn start_benchmark_indexer(self: &Arc<Self>) {
        let (tx, rx) = crossbeam::channel::unbounded::<IndexerBatch>();

        for thread_id in 0..4u32 {
            let rx = rx.clone();
            let state = Arc::clone(self);
            std::thread::Builder::new()
                .name(format!("indexer-{}", thread_id))
                .spawn(move || {
                    while let Ok(batch) = rx.recv() {
                        let mut global_idx: u32 = 0;
                        for (shard_idx, (sender, receiver)) in
                            batch.senders.iter().zip(batch.receivers.iter()).enumerate()
                        {
                            let body_bytes = bincode::serialize(&TxBody::Transfer(TransferBody {
                                to: *receiver,
                                amount: 1,
                                amount_commitment: None,
                            }))
                            .expect("serializable");
                            let mut base_hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
                            base_hasher.update(&[TxType::Transfer as u8]);
                            base_hasher.update(sender.as_ref());

                            let nonce_start =
                                batch.nonce_start + shard_idx as u64 * batch.txs_per_sender;
                            for j in 0..batch.txs_per_sender {
                                let nonce = nonce_start + j;
                                let hash =
                                    compute_benchmark_tx_hash(&base_hasher, nonce, &body_bytes);
                                state.tx_index.insert(hash.0, (batch.height, global_idx));
                                global_idx += 1;
                            }
                        }
                    }
                })
                .expect("spawn indexer thread");
        }

        #[allow(invalid_reference_casting)]
        unsafe {
            let self_mut = &mut *(Arc::as_ptr(self) as *mut Self);
            self_mut.indexer_tx = Some(tx);
        }
        tracing::info!("Benchmark indexer started (4 threads)");
    }

    #[cfg(feature = "benchmark-tools")]
    pub fn execute_block_benchmark(
        &self,
        tx_per_block: u64,
        senders: &Arc<Vec<Hash256>>,
        receivers: &Arc<Vec<Hash256>>,
        producer: Address,
        nonce_base: &mut u64,
    ) -> Result<Block, StateError> {
        if self.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "protocol-4 requires canonical sequential block execution".into(),
            ));
        }
        if self.active_protocol_version().major == 3 {
            return Err(StateError::ExecutionError(
                "unsigned benchmark execution is unavailable in recovery protocol v3".to_string(),
            ));
        }
        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        let num_senders = senders.len() as u64;
        let txs_per_sender = tx_per_block / num_senders;
        let current_nonce_base = *nonce_base;

        {
            let mut s = self.benchmark_senders.write();
            if s.is_none() {
                *s = Some(Arc::clone(senders));
            }
        }
        {
            let mut r = self.benchmark_receivers.write();
            if r.is_none() {
                *r = Some(Arc::clone(receivers));
            }
        }
        self.benchmark_txs_per_sender
            .store(txs_per_sender, Ordering::Relaxed);

        for (sender, receiver) in senders.iter().zip(receivers.iter()) {
            self.dirty_accounts.insert(sender.0);
            self.dirty_accounts.insert(receiver.0);
            if let Some(mut s) = self.accounts.get_mut(&sender.0) {
                s.balance = s.balance.saturating_sub(txs_per_sender);
                s.nonce += txs_per_sender;
            }
            if let Some(mut r) = self.accounts.get_mut(&receiver.0) {
                r.balance = txs_per_sender;
            }
        }

        let shard_data: Vec<(Hash256, Hash256, u64)> = senders
            .iter()
            .zip(receivers.iter())
            .enumerate()
            .map(|(i, (s, r))| (*s, *r, current_nonce_base + i as u64 * txs_per_sender))
            .collect();

        let all_hashes: Vec<Hash256> = shard_data
            .par_iter()
            .flat_map(|(sender, receiver, nonce_start)| {
                let body_bytes = bincode::serialize(&TxBody::Transfer(TransferBody {
                    to: *receiver,
                    amount: 1,
                    amount_commitment: None,
                }))
                .expect("serializable");

                let mut base_hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
                base_hasher.update(&[TxType::Transfer as u8]);
                base_hasher.update(sender.as_ref());

                (0..txs_per_sender)
                    .map(|j| compute_benchmark_tx_hash(&base_hasher, nonce_start + j, &body_bytes))
                    .collect::<Vec<Hash256>>()
            })
            .collect();

        let tx_root = compute_merkle_root_only(all_hashes);

        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            parent_hash: parent,
            tx_root,
            state_root,
            proof_hash: Hash256::ZERO,
            tx_count: tx_per_block as u32,
            producer,
            protocol_version: self.active_protocol_version(),
            state_diff: None,
        };

        let block = Block::new(header, vec![]);
        self.blocks.insert(height, Box::new(block.clone()));

        self.benchmark_nonces.insert(height, current_nonce_base);

        if let Some(ref indexer) = self.indexer_tx {
            let batch = IndexerBatch {
                block_hash: block.hash,
                height,
                senders: Arc::clone(senders),
                receivers: Arc::clone(receivers),
                nonce_start: current_nonce_base,
                txs_per_sender,
            };
            let _ = indexer.send(batch);
        }

        self.benchmark_tx_count
            .fetch_add(tx_per_block, Ordering::Relaxed);

        *nonce_base += tx_per_block;

        Ok(block)
    }

    pub fn execute_block_signed_benchmark(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<Block, StateError> {
        if self.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "protocol-4 requires canonical sequential block execution".into(),
            ));
        }
        self.require_healthy_wal()?;
        if self.active_protocol_version().major == 3 {
            return Err(StateError::ExecutionError(
                "non-WAL benchmark execution is unavailable in recovery protocol v3".to_string(),
            ));
        }
        self.validate_next_protocol_block_admission(transactions)?;
        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        let tx_count = transactions.len();
        let t0 = std::time::Instant::now();
        let recovery_domain = self.transaction_domain_hash();

        let sig_valid: Vec<bool> = transactions
            .par_chunks(256)
            .flat_map(|chunk| {
                let mut messages = Vec::with_capacity(chunk.len());
                let mut sigs = Vec::with_capacity(chunk.len());
                let mut vks = Vec::with_capacity(chunk.len());
                let mut valid = true;

                for tx in chunk {
                    match &tx.signature {
                        arc_crypto::Signature::Ed25519 {
                            public_key,
                            signature,
                        } => {
                            let expected_hash = match recovery_domain {
                                Some(domain) => tx.compute_hash_in_domain(&domain),
                                None => tx.compute_hash(),
                            };
                            if expected_hash != tx.hash
                                || arc_crypto::address_from_ed25519_pubkey(public_key) != tx.from
                            {
                                valid = false;
                                break;
                            }
                            if let (Ok(vk), Ok(sig)) = (
                                ed25519_dalek::VerifyingKey::from_bytes(public_key),
                                <[u8; 64]>::try_from(signature.as_slice())
                                    .map(|b| ed25519_dalek::Signature::from_bytes(&b)),
                            ) {
                                messages.push(tx.hash.as_bytes().as_slice());
                                sigs.push(sig);
                                vks.push(vk);
                            } else {
                                valid = false;
                                break;
                            }
                        }
                        _ => {
                            valid = false;
                            break;
                        }
                    }
                }

                if valid && !messages.is_empty() {
                    if arc_crypto::batch_verify_ed25519(&messages, &sigs, &vks).is_ok() {
                        vec![true; chunk.len()]
                    } else {
                        chunk
                            .iter()
                            .map(|tx| self.verify_transaction_signature(tx).is_ok())
                            .collect()
                    }
                } else {
                    chunk
                        .iter()
                        .map(|tx| self.verify_transaction_signature(tx).is_ok())
                        .collect()
                }
            })
            .collect();

        let t1 = t0.elapsed();

        let mut shards: HashMap<[u8; 32], Vec<(usize, &Transaction, bool)>> = HashMap::new();
        for (i, tx) in transactions.iter().enumerate() {
            shards
                .entry(tx.from.0)
                .or_default()
                .push((i, tx, sig_valid[i]));
        }

        let shard_results: Vec<Vec<(usize, bool)>> = shards
            .into_par_iter()
            .map(|(_sender, mut txs)| {
                txs.sort_unstable_by_key(|(_, tx, _)| tx.nonce);
                let mut results = Vec::with_capacity(txs.len());
                for (idx, tx, sig_ok) in txs {
                    let success = if !sig_ok {
                        false
                    } else {
                        self.mark_tx_accounts_dirty(tx);
                        self.execute_tx(tx).is_ok()
                    };
                    results.push((idx, success));
                }
                results
            })
            .collect();

        let mut receipt_success = vec![false; tx_count];
        for shard in shard_results {
            for (idx, success) in shard {
                receipt_success[idx] = success;
            }
        }

        let t2 = t0.elapsed();

        let tx_hashes: Vec<Hash256> = transactions.iter().map(|tx| tx.hash).collect();

        let tx_root = compute_merkle_root_only(tx_hashes);

        let t3 = t0.elapsed();

        let state_root = self.compute_state_root();

        let t4 = t0.elapsed();

        let header = BlockHeader {
            height,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            parent_hash: parent,
            tx_root,
            state_root,
            proof_hash: Hash256::ZERO,
            tx_count: tx_count as u32,
            producer,
            protocol_version: self.active_protocol_version(),
            state_diff: None,
        };

        let block_tx_hashes: Vec<Hash256> = transactions.iter().map(|tx| tx.hash).collect();
        let block = Block::new(header, block_tx_hashes);
        self.blocks.insert(height, Box::new(block.clone()));

        self.signed_block_data
            .insert(height, (vec![], receipt_success, block.hash));

        transactions.par_iter().enumerate().for_each(|(i, tx)| {
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.full_transactions.insert(tx.hash.0, tx.clone());
        });

        let t5 = t0.elapsed();

        self.benchmark_tx_count
            .fetch_add(tx_count as u64, Ordering::Relaxed);

        tracing::info!(
            txs = tx_count,
            sig_verify_ms = t1.as_millis(),
            execute_ms = (t2 - t1).as_millis(),
            merkle_ms = (t3 - t2).as_millis(),
            state_root_ms = (t4 - t3).as_millis(),
            store_index_ms = (t5 - t4).as_millis(),
            "Benchmark timing breakdown"
        );

        Ok(block)
    }

    #[cfg(feature = "benchmark-tools")]
    pub fn reconstruct_benchmark_tx(&self, height: u64, tx_index: u32) -> Option<Transaction> {
        let nonce_base = self.benchmark_nonces.get(&height)?;
        let nonce_base = *nonce_base;

        let senders = self.benchmark_senders.read();
        let receivers = self.benchmark_receivers.read();
        let senders = senders.as_ref()?;
        let receivers = receivers.as_ref()?;

        let txs_per_sender = self.benchmark_txs_per_sender.load(Ordering::Relaxed);
        if txs_per_sender == 0 {
            return None;
        }

        let shard_idx = tx_index as u64 / txs_per_sender;
        let inner_idx = tx_index as u64 % txs_per_sender;

        let sender = *senders.get(shard_idx as usize)?;
        let receiver = *receivers.get(shard_idx as usize)?;
        let nonce = nonce_base + shard_idx * txs_per_sender + inner_idx;

        let mut tx = Transaction::new_transfer(sender, receiver, 1, nonce);

        let sk = arc_crypto::benchmark_keypair(shard_idx as u8);
        use ed25519_dalek::Signer;
        let sig = sk.sign(tx.hash.as_bytes());
        let vk = sk.verifying_key();
        tx.signature = arc_crypto::Signature::Ed25519 {
            public_key: *vk.as_bytes(),
            signature: sig.to_bytes().to_vec(),
        };

        Some(tx)
    }

    #[cfg(not(feature = "benchmark-tools"))]
    pub fn reconstruct_benchmark_tx(&self, _height: u64, _tx_index: u32) -> Option<Transaction> {
        None
    }

    pub fn reconstruct_benchmark_receipt(&self, height: u64, tx_index: u32) -> Option<TxReceipt> {
        let tx = self.reconstruct_benchmark_tx(height, tx_index)?;
        let block = self.blocks.get(&height)?;

        Some(TxReceipt {
            tx_hash: tx.hash,
            block_height: height,
            block_hash: block.hash,
            index: tx_index,
            success: true,
            gas_used: 0,
            value_commitment: None,
            inclusion_proof: None,
            logs: vec![],
        })
    }

    #[cfg(feature = "benchmark-tools")]
    pub fn reconstruct_benchmark_proof(
        &self,
        height: u64,
        tx_index: u32,
    ) -> Option<arc_crypto::MerkleProof> {
        let nonce_base = self.benchmark_nonces.get(&height)?;
        let nonce_base = *nonce_base;

        let senders = self.benchmark_senders.read();
        let receivers = self.benchmark_receivers.read();
        let senders_ref = senders.as_ref()?;
        let receivers_ref = receivers.as_ref()?;

        let txs_per_sender = self.benchmark_txs_per_sender.load(Ordering::Relaxed);
        if txs_per_sender == 0 {
            return None;
        }

        let shard_data: Vec<(Hash256, Hash256, u64)> = senders_ref
            .iter()
            .zip(receivers_ref.iter())
            .enumerate()
            .map(|(i, (s, r))| (*s, *r, nonce_base + i as u64 * txs_per_sender))
            .collect();

        let all_hashes: Vec<Hash256> = shard_data
            .par_iter()
            .flat_map(|(sender, receiver, ns)| {
                let body_bytes = bincode::serialize(&TxBody::Transfer(TransferBody {
                    to: *receiver,
                    amount: 1,
                    amount_commitment: None,
                }))
                .expect("serializable");
                let mut base_hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
                base_hasher.update(&[TxType::Transfer as u8]);
                base_hasher.update(sender.as_ref());

                (0..txs_per_sender)
                    .map(|j| compute_benchmark_tx_hash(&base_hasher, ns + j, &body_bytes))
                    .collect::<Vec<Hash256>>()
            })
            .collect();

        let tree = MerkleTree::from_leaves(all_hashes);
        tree.proof(tx_index as usize)
    }

    #[cfg(not(feature = "benchmark-tools"))]
    pub fn reconstruct_benchmark_proof(
        &self,
        _height: u64,
        _tx_index: u32,
    ) -> Option<arc_crypto::MerkleProof> {
        None
    }

    pub fn get_benchmark_tx_by_hash(&self, tx_hash: &[u8; 32]) -> Option<Transaction> {
        let (height, idx) = *self.tx_index.get(tx_hash)?;
        self.reconstruct_benchmark_tx(height, idx)
    }

    pub fn get_benchmark_receipt_by_hash(&self, tx_hash: &[u8; 32]) -> Option<TxReceipt> {
        let (height, idx) = *self.tx_index.get(tx_hash)?;
        if let Some(block_data) = self.signed_block_data.get(&height) {
            let (txs_vec, success_flags, block_hash) = &*block_data;
            if let Some(tx) = txs_vec.get(idx as usize) {
                return Some(TxReceipt {
                    tx_hash: tx.hash,
                    block_height: height,
                    block_hash: *block_hash,
                    index: idx,
                    success: success_flags.get(idx as usize).copied().unwrap_or(true),
                    gas_used: 0,
                    value_commitment: None,
                    inclusion_proof: None,
                    logs: vec![],
                });
            }
        }
        self.reconstruct_benchmark_receipt(height, idx)
    }

    pub fn get_benchmark_block_txs(
        &self,
        height: u64,
        offset: u32,
        limit: u32,
    ) -> Vec<Transaction> {
        let block = match self.blocks.get(&height) {
            Some(b) => Block::clone(&b),
            None => return vec![],
        };
        let tx_count = block.header.tx_count;
        let end = (offset + limit).min(tx_count);

        if let Some(block_data) = self.signed_block_data.get(&height) {
            let (txs_vec, _, _) = &*block_data;
            return (offset as usize..end as usize)
                .filter_map(|i| txs_vec.get(i).cloned())
                .collect();
        }

        let mut txs = Vec::new();
        for idx in offset..end {
            if let Some(tx) = self.reconstruct_benchmark_tx(height, idx) {
                txs.push(tx);
            }
        }
        txs
    }

    fn gas_cost_for_tx(tx: &Transaction) -> u64 {
        match &tx.body {
            TxBody::Transfer(_) => gas_costs::TRANSFER,
            TxBody::Settle(_) => gas_costs::SETTLE,
            TxBody::Swap(_) => gas_costs::SWAP,
            TxBody::Escrow(_) => gas_costs::ESCROW,
            TxBody::Stake(_) => gas_costs::STAKE,
            TxBody::WasmCall(_) => gas_costs::CONTRACT_CALL,
            TxBody::MultiSig(_) => gas_costs::MULTI_SIG,
            TxBody::DeployContract(_) => gas_costs::DEPLOY_CONTRACT,
            TxBody::RegisterAgent(_) => gas_costs::REGISTER_AGENT,
            TxBody::JoinValidator(_) => gas_costs::JOIN_VALIDATOR,
            TxBody::LeaveValidator => gas_costs::LEAVE_VALIDATOR,
            TxBody::ClaimRewards => gas_costs::CLAIM_REWARDS,
            TxBody::UpdateStake(_) => gas_costs::UPDATE_STAKE,
            TxBody::Governance(_) => gas_costs::GOVERNANCE,
            TxBody::BridgeLock(_) => gas_costs::BRIDGE_LOCK,
            TxBody::BridgeMint(_) => gas_costs::BRIDGE_MINT,
            TxBody::BatchSettle(body) => {
                gas_costs::BATCH_SETTLE_BASE
                    + (body.entries.len() as u64) * gas_costs::BATCH_SETTLE_PER_ENTRY
            }
            TxBody::ChannelOpen(_) => gas_costs::CHANNEL_OPEN,
            TxBody::ChannelClose(_) => gas_costs::CHANNEL_CLOSE,
            TxBody::ChannelDispute(_) => gas_costs::CHANNEL_DISPUTE,
            TxBody::ShardProof(_) => gas_costs::SHARD_PROOF,
            TxBody::InferenceAttestation(_) => gas_costs::INFERENCE_ATTESTATION,
            TxBody::CommunityInferenceReward(_) => gas_costs::COMMUNITY_INFERENCE_REWARD,
            TxBody::InferenceChallenge(_) => gas_costs::INFERENCE_CHALLENGE,
            TxBody::InferenceRegister(_) => gas_costs::INFERENCE_ATTESTATION,
            TxBody::InferenceEscrowOpen(_) => gas_costs::INFERENCE_ESCROW_OPEN,
            TxBody::InferenceEscrowRelease(_) => gas_costs::INFERENCE_ESCROW_RELEASE,
            TxBody::InferenceEscrowRefund(_) => gas_costs::INFERENCE_ESCROW_REFUND,
            TxBody::ModelRegistration(_) => gas_costs::MODEL_REGISTRATION,
            TxBody::ModelRequest(_) => gas_costs::MODEL_REQUEST,
            TxBody::ShardCoverageClaim(_) => gas_costs::SHARD_COVERAGE_CLAIM,
            TxBody::CapacityAdvertisement(_) => gas_costs::CAPACITY_ADVERTISEMENT,
            TxBody::ShardAssignmentProposal(_) => gas_costs::SHARD_ASSIGNMENT_PROPOSAL,
            TxBody::FaucetClaim(_) => gas_costs::FAUCET_CLAIM,
            TxBody::InferenceRequest(_) => gas_costs::TIER1_INFERENCE_REQUEST,
            TxBody::InferenceVote(_) => gas_costs::TIER1_INFERENCE_VOTE,
            TxBody::InferenceFinalize(_) => gas_costs::TIER1_INFERENCE_FINALIZE,
            TxBody::NativeInferenceRequest(_) => gas_costs::NATIVE_INFERENCE_REQUEST,
            TxBody::NativeInferenceFinalize(_) => gas_costs::NATIVE_INFERENCE_FINALIZE,
            TxBody::NativeInferenceRefund(_) => gas_costs::NATIVE_INFERENCE_REFUND,
        }
    }

    fn v3_transaction_family_policy(tx_type: TxType) -> V3TransactionFamilyPolicy {
        match tx_type {
            TxType::Transfer | TxType::CommunityInferenceReward | TxType::FaucetClaim => {
                V3TransactionFamilyPolicy::Allowed
            }
            TxType::InferenceAttestation
            | TxType::Settle
            | TxType::Swap
            | TxType::Escrow
            | TxType::Stake
            | TxType::WasmCall
            | TxType::MultiSig
            | TxType::DeployContract
            | TxType::RegisterAgent
            | TxType::JoinValidator
            | TxType::LeaveValidator
            | TxType::ClaimRewards
            | TxType::UpdateStake
            | TxType::Governance
            | TxType::BridgeLock
            | TxType::BridgeMint
            | TxType::BatchSettle
            | TxType::ChannelOpen
            | TxType::ChannelClose
            | TxType::ChannelDispute
            | TxType::ShardProof
            | TxType::InferenceChallenge
            | TxType::InferenceRegister
            | TxType::InferenceEscrowOpen
            | TxType::InferenceEscrowRelease
            | TxType::InferenceEscrowRefund
            | TxType::ModelRegistration
            | TxType::ModelRequest
            | TxType::ShardCoverageClaim
            | TxType::CapacityAdvertisement
            | TxType::ShardAssignmentProposal
            | TxType::InferenceRequest
            | TxType::InferenceVote
            | TxType::InferenceFinalize
            | TxType::NativeInferenceRequest
            | TxType::NativeInferenceFinalize
            | TxType::NativeInferenceRefund => V3TransactionFamilyPolicy::Denied,
        }
    }

    fn v3_allows_transaction(body: &TxBody) -> bool {
        match Self::v3_transaction_family_policy(body.tx_type()) {
            V3TransactionFamilyPolicy::Allowed => true,
            V3TransactionFamilyPolicy::Denied => false,
        }
    }

    pub fn refuse_registry_change_under_native_binding(&self) -> Result<(), StateError> {
        if self.native_migration.read().is_some() {
            return Ok(());
        }
        if self.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "the validator registry is frozen by the active native inference binding; \
                 changing the committee requires an explicit migration to a fresh activation"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn is_registry_change(tx: &Transaction) -> bool {
        matches!(
            tx.body,
            TxBody::Stake(_) | TxBody::JoinValidator(_) | TxBody::UpdateStake(_)
        )
    }

    fn execute_tx(&self, tx: &Transaction) -> Result<u64, StateError> {
        if tx.tx_type != tx.body.tx_type() {
            return Err(StateError::ExecutionError(format!(
                "transaction type/body mismatch: envelope {:?}, body {:?}",
                tx.tx_type,
                tx.body.tx_type()
            )));
        }
        if self.active_protocol_version().major == 3 {
            self.validate_v3_transaction_envelope(tx)?;
        }
        let effective_limit = if tx.gas_limit > 0 {
            tx.gas_limit
        } else {
            gas_costs::BLOCK_GAS_LIMIT
        };
        let mut gas = GasMeter::new(effective_limit);

        let op_cost = Self::gas_cost_for_tx(tx);
        if let Err(e) = gas.charge(op_cost) {
            return Err(StateError::ExecutionError(format!("gas: {}", e)));
        }

        match &tx.body {
            TxBody::Transfer(body) => {
                if self.active_protocol_version().major == 3 {
                    self.validate_v3_transfer_admission(tx, body)?;
                    let fee_treasury_addr = v3_fee_treasury_address();
                    let total_debit = body.amount.checked_add(tx.fee).ok_or_else(|| {
                        StateError::ExecutionError("transfer amount plus fee overflow".to_string())
                    })?;
                    let mut sender = self.get_account(&tx.from).ok_or_else(|| {
                        StateError::ExecutionError(
                            "v3 transfer sender disappeared after preflight".to_string(),
                        )
                    })?;
                    let mut receiver = self
                        .get_account(&body.to)
                        .unwrap_or_else(|| Account::new(body.to, 0));
                    let mut fee_treasury = self
                        .get_account(&fee_treasury_addr)
                        .unwrap_or_else(|| Account::new(fee_treasury_addr, 0));
                    sender.balance = sender.balance.checked_sub(total_debit).ok_or_else(|| {
                        StateError::ExecutionError("transfer sender balance underflow".to_string())
                    })?;
                    sender.nonce = sender.nonce.checked_add(1).ok_or_else(|| {
                        StateError::ExecutionError("transfer nonce overflow".to_string())
                    })?;
                    receiver.balance =
                        receiver.balance.checked_add(body.amount).ok_or_else(|| {
                            StateError::ExecutionError(
                                "transfer recipient balance overflow".to_string(),
                            )
                        })?;
                    fee_treasury.balance =
                        fee_treasury.balance.checked_add(tx.fee).ok_or_else(|| {
                            StateError::ExecutionError(
                                "transfer fee treasury balance overflow".to_string(),
                            )
                        })?;

                    self.accounts.insert(tx.from.0, sender.clone());
                    self.accounts.insert(body.to.0, receiver.clone());
                    self.accounts
                        .insert(fee_treasury_addr.0, fee_treasury.clone());
                    if self.use_jmt {
                        let mut jmt = self.jmt.lock();
                        for (address, account) in [
                            (tx.from, &sender),
                            (body.to, &receiver),
                            (fee_treasury_addr, &fee_treasury),
                        ] {
                            let value =
                                hash_bytes(&bincode::serialize(account).unwrap_or_default());
                            jmt.update_leaf(address.0, value);
                        }
                    }
                    if self.wal.is_active() {
                        self.wal
                            .append(WalOp::SetAccount(tx.from, sender), self.height());
                        self.wal
                            .append(WalOp::SetAccount(body.to, receiver), self.height());
                        self.wal.append(
                            WalOp::SetAccount(fee_treasury_addr, fee_treasury),
                            self.height(),
                        );
                    }
                    return Ok(gas.consumed);
                }

                let mut sender =
                    self.get_account(&tx.from)
                        .ok_or(StateError::InsufficientBalance {
                            have: 0,
                            need: body.amount,
                        })?;
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                let next_nonce = sender.nonce.checked_add(1).ok_or_else(|| {
                    StateError::ExecutionError("transfer nonce overflow".to_string())
                })?;
                if sender.balance < body.amount {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.amount,
                    });
                }

                if body.to == tx.from {
                    sender.nonce = next_nonce;
                    self.accounts.insert(tx.from.0, sender.clone());
                    if self.use_jmt {
                        let value = hash_bytes(&bincode::serialize(&sender).unwrap_or_default());
                        self.jmt.lock().update_leaf(tx.from.0, value);
                    }
                    if self.wal.is_active() {
                        self.wal
                            .append(WalOp::SetAccount(tx.from, sender), self.height());
                    }
                    return Ok(gas.consumed);
                }

                let mut receiver = self
                    .get_account(&body.to)
                    .unwrap_or_else(|| Account::new(body.to, 0));
                let sender_balance = sender.balance.checked_sub(body.amount).ok_or_else(|| {
                    StateError::ExecutionError("transfer sender balance underflow".to_string())
                })?;
                let receiver_balance =
                    receiver.balance.checked_add(body.amount).ok_or_else(|| {
                        StateError::ExecutionError(
                            "transfer recipient balance overflow".to_string(),
                        )
                    })?;
                sender.balance = sender_balance;
                sender.nonce = next_nonce;
                receiver.balance = receiver_balance;

                self.accounts.insert(tx.from.0, sender.clone());
                self.accounts.insert(body.to.0, receiver.clone());
                if self.use_jmt {
                    let mut jmt = self.jmt.lock();
                    for (address, account) in [(tx.from, &sender), (body.to, &receiver)] {
                        let value = hash_bytes(&bincode::serialize(account).unwrap_or_default());
                        jmt.update_leaf(address.0, value);
                    }
                }
                if self.wal.is_active() {
                    self.wal
                        .append(WalOp::SetAccount(tx.from, sender), self.height());
                    self.wal
                        .append(WalOp::SetAccount(body.to, receiver), self.height());
                }

                Ok(gas.consumed)
            }
            TxBody::Settle(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if sender.balance < body.amount {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.amount,
                    });
                }
                sender.balance -= body.amount;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let mut agent = self.get_or_create_account(&body.agent_id);
                agent.balance = agent.balance.saturating_add(body.amount);
                self.accounts.insert(body.agent_id.0, agent.clone());
                self.wal
                    .append(WalOp::SetAccount(body.agent_id, agent), self.height());

                Ok(gas.consumed)
            }
            TxBody::Swap(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if sender.balance < body.offer_amount {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.offer_amount,
                    });
                }
                let mut counterparty = self.get_or_create_account(&body.counterparty);
                if counterparty.balance < body.receive_amount {
                    return Err(StateError::InsufficientBalance {
                        have: counterparty.balance,
                        need: body.receive_amount,
                    });
                }
                sender.balance -= body.offer_amount;
                sender.balance = sender.balance.saturating_add(body.receive_amount);
                sender.nonce += 1;
                counterparty.balance -= body.receive_amount;
                counterparty.balance = counterparty.balance.saturating_add(body.offer_amount);

                self.accounts.insert(tx.from.0, sender);
                self.accounts.insert(body.counterparty.0, counterparty);
                Ok(gas.consumed)
            }
            TxBody::Escrow(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if body.is_create {
                    if sender.balance < body.amount {
                        return Err(StateError::InsufficientBalance {
                            have: sender.balance,
                            need: body.amount,
                        });
                    }
                    sender.balance -= body.amount;
                } else {
                    let mut beneficiary = self.get_or_create_account(&body.beneficiary);
                    beneficiary.balance = beneficiary.balance.saturating_add(body.amount);
                    self.accounts.insert(body.beneficiary.0, beneficiary);
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender);
                Ok(gas.consumed)
            }
            TxBody::Stake(body) => {
                self.refuse_registry_change_under_native_binding()?;
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                let next_nonce = sender
                    .nonce
                    .checked_add(1)
                    .ok_or_else(|| StateError::ExecutionError("stake nonce overflow".into()))?;

                if body.is_stake {
                    if sender.balance < body.amount {
                        return Err(StateError::InsufficientBalance {
                            have: sender.balance,
                            need: body.amount,
                        });
                    }
                    let prev_stake = self.validators.get(&tx.from.0).map(|v| *v).unwrap_or(0);
                    if sender.staked_balance < prev_stake {
                        return Err(StateError::ExecutionError(format!(
                            "validator account stake {} is below tracked validator stake {prev_stake}",
                            sender.staked_balance
                        )));
                    }
                    let new_balance = sender.balance.checked_sub(body.amount).ok_or_else(|| {
                        StateError::ExecutionError("stake balance underflow".into())
                    })?;
                    let new_account_stake = sender
                        .staked_balance
                        .checked_add(body.amount)
                        .ok_or_else(|| {
                            StateError::ExecutionError("account stake overflow".into())
                        })?;
                    let new_stake = prev_stake.checked_add(body.amount).ok_or_else(|| {
                        StateError::ExecutionError("validator stake overflow".into())
                    })?;
                    self.add_staking_pool_checked(body.amount)?;

                    sender.balance = new_balance;
                    sender.staked_balance = new_account_stake;
                    self.validators.insert(tx.from.0, new_stake);

                    if prev_stake < Self::MIN_VALIDATOR_STAKE
                        && new_stake >= Self::MIN_VALIDATOR_STAKE
                    {
                        tracing::info!(
                            validator = ?tx.from,
                            stake = new_stake,
                            "new validator registered (above minimum stake)"
                        );
                    }
                } else {
                    let prev_stake = self.validators.get(&tx.from.0).map(|v| *v).unwrap_or(0);
                    if prev_stake < body.amount {
                        return Err(StateError::InsufficientBalance {
                            have: prev_stake,
                            need: body.amount,
                        });
                    }
                    if sender.staked_balance < prev_stake {
                        return Err(StateError::ExecutionError(format!(
                            "validator account stake {} is below tracked validator stake {prev_stake}",
                            sender.staked_balance
                        )));
                    }
                    let new_account_stake = sender
                        .staked_balance
                        .checked_sub(body.amount)
                        .ok_or_else(|| {
                            StateError::ExecutionError("account stake underflow".into())
                        })?;
                    let new_balance = sender.balance.checked_add(body.amount).ok_or_else(|| {
                        StateError::ExecutionError("unstake balance overflow".into())
                    })?;
                    let new_stake = prev_stake.checked_sub(body.amount).ok_or_else(|| {
                        StateError::ExecutionError("validator stake underflow".into())
                    })?;
                    self.subtract_staking_pool_checked(body.amount)?;

                    sender.staked_balance = new_account_stake;
                    sender.balance = new_balance;
                    if new_stake == 0 {
                        self.validators.remove(&tx.from.0);
                    } else {
                        self.validators.insert(tx.from.0, new_stake);
                    }

                    if prev_stake >= Self::MIN_VALIDATOR_STAKE
                        && new_stake < Self::MIN_VALIDATOR_STAKE
                    {
                        tracing::info!(
                            validator = ?tx.from,
                            remaining_stake = new_stake,
                            "validator removed (below minimum stake)"
                        );
                    }
                }

                sender.nonce = next_nonce;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());
                Ok(gas.consumed)
            }
            TxBody::WasmCall(body) => {
                let bytecode = self
                    .get_contract(&body.contract)
                    .ok_or(StateError::ContractNotFound(body.contract))?;

                let is_evm = bytecode.len() < 4 || &bytecode[..4] != WASM_MAGIC;

                if is_evm {
                    return Err(StateError::ExecutionError(
                        "state-changing EVM calls are unavailable until canonical pre-root execution is activated"
                            .to_string(),
                    ));
                } else {
                    let mut sender = self
                        .get_account(&tx.from)
                        .unwrap_or_else(|| Account::new(tx.from, 0));
                    if sender.nonce != tx.nonce {
                        return Err(StateError::InvalidNonce {
                            expected: sender.nonce,
                            got: tx.nonce,
                        });
                    }
                    if body.value > 0 {
                        if sender.balance < body.value {
                            return Err(StateError::InsufficientBalance {
                                have: sender.balance,
                                need: body.value,
                            });
                        }
                        sender.balance -= body.value;

                        let mut contract_acct = self.get_or_create_account(&body.contract);
                        contract_acct.balance = body.value;
                        self.accounts.insert(body.contract.0, contract_acct.clone());
                        self.wal.append(
                            WalOp::SetAccount(body.contract, contract_acct),
                            self.height(),
                        );
                    }

                    {
                        use std::sync::Mutex as StdMutex;
                        use wasmer::{
                            Function, FunctionEnv, FunctionEnvMut, Instance, Memory,
                            Module as WasmModule, Store, imports,
                        };

                        struct WasmHostState {
                            gas_used: std::sync::atomic::AtomicU64,
                            gas_limit: u64,
                            out_of_gas: StdMutex<bool>,
                            logs: StdMutex<Vec<String>>,
                            storage_cache: StdMutex<HashMap<[u8; 32], Option<Vec<u8>>>>,
                            storage_writes: StdMutex<Vec<([u8; 32], Vec<u8>)>>,
                            caller: [u8; 32],
                            self_address: [u8; 32],
                            call_value: u64,
                            block_height: u64,
                            memory: StdMutex<Option<Memory>>,
                        }

                        let wasm_gas_limit = if body.gas_limit > 0 {
                            body.gas_limit
                        } else {
                            10_000_000
                        };
                        let host = WasmHostState {
                            gas_used: std::sync::atomic::AtomicU64::new(0),
                            gas_limit: wasm_gas_limit,
                            out_of_gas: StdMutex::new(false),
                            logs: StdMutex::new(Vec::new()),
                            storage_cache: StdMutex::new(HashMap::new()),
                            storage_writes: StdMutex::new(Vec::new()),
                            caller: tx.from.0,
                            self_address: body.contract.0,
                            call_value: body.value,
                            block_height: self.height(),
                            memory: StdMutex::new(None),
                        };

                        let mut store = Store::default();
                        let func_env = FunctionEnv::new(&mut store, host);

                        let h_use_gas = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>, amount: i64| {
                                if amount <= 0 {
                                    return;
                                }
                                let data = env.data_mut();
                                let prev = data
                                    .gas_used
                                    .fetch_add(amount as u64, std::sync::atomic::Ordering::Relaxed);
                                if prev + amount as u64 > data.gas_limit {
                                    *data.out_of_gas.lock().unwrap() = true;
                                }
                            },
                        );

                        let h_log = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>, ptr: i32, len: i32| {
                                let (data, wstore) = env.data_and_store_mut();
                                if let Some(ref mem) = *data.memory.lock().unwrap() {
                                    let view = mem.view(&wstore);
                                    let mut buf = vec![0u8; len as usize];
                                    if view.read(ptr as u64, &mut buf).is_ok() {
                                        data.logs
                                            .lock()
                                            .unwrap()
                                            .push(String::from_utf8_lossy(&buf).to_string());
                                    }
                                }
                            },
                        );

                        let h_storage_get = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>,
                             key_ptr: i32,
                             val_ptr: i32|
                             -> i32 {
                                let (data, wstore) = env.data_and_store_mut();
                                let mem_guard = data.memory.lock().unwrap();
                                let mem = match *mem_guard {
                                    Some(ref m) => m.clone(),
                                    None => return -1,
                                };
                                drop(mem_guard);
                                let view = mem.view(&wstore);
                                let mut key = [0u8; 32];
                                if view.read(key_ptr as u64, &mut key).is_err() {
                                    return -1;
                                }
                                let cache = data.storage_cache.lock().unwrap();
                                match cache.get(&key) {
                                    Some(Some(val)) => {
                                        let val = val.clone();
                                        drop(cache);
                                        let view2 = mem.view(&wstore);
                                        if view2.write(val_ptr as u64, &val).is_err() {
                                            return -1;
                                        }
                                        val.len() as i32
                                    }
                                    Some(None) => -1,
                                    None => -1,
                                }
                            },
                        );

                        let h_storage_set = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>,
                             key_ptr: i32,
                             val_ptr: i32,
                             val_len: i32| {
                                let (data, wstore) = env.data_and_store_mut();
                                let mem_guard = data.memory.lock().unwrap();
                                let mem = match *mem_guard {
                                    Some(ref m) => m.clone(),
                                    None => return,
                                };
                                drop(mem_guard);
                                let view = mem.view(&wstore);
                                let mut key = [0u8; 32];
                                if view.read(key_ptr as u64, &mut key).is_err() {
                                    return;
                                }
                                let mut val = vec![0u8; val_len as usize];
                                if view.read(val_ptr as u64, &mut val).is_err() {
                                    return;
                                }
                                data.storage_cache
                                    .lock()
                                    .unwrap()
                                    .insert(key, Some(val.clone()));
                                data.storage_writes.lock().unwrap().push((key, val));
                            },
                        );

                        let h_caller = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>, ptr: i32| {
                                let (data, wstore) = env.data_and_store_mut();
                                if let Some(ref mem) = *data.memory.lock().unwrap() {
                                    let view = mem.view(&wstore);
                                    let _ = view.write(ptr as u64, &data.caller);
                                }
                            },
                        );

                        let h_self_address = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>, ptr: i32| {
                                let (data, wstore) = env.data_and_store_mut();
                                if let Some(ref mem) = *data.memory.lock().unwrap() {
                                    let view = mem.view(&wstore);
                                    let _ = view.write(ptr as u64, &data.self_address);
                                }
                            },
                        );

                        let h_block_height = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |env: FunctionEnvMut<'_, WasmHostState>| -> i64 {
                                env.data().block_height as i64
                            },
                        );

                        let h_tx_value = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |env: FunctionEnvMut<'_, WasmHostState>| -> i64 {
                                env.data().call_value as i64
                            },
                        );

                        let h_gas_remaining = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |env: FunctionEnvMut<'_, WasmHostState>| -> i64 {
                                let data = env.data();
                                let used = data.gas_used.load(std::sync::atomic::Ordering::Relaxed);
                                data.gas_limit.saturating_sub(used) as i64
                            },
                        );

                        let import_object = imports! {
                            "env" => {
                                "use_gas" => h_use_gas,
                                "log" => h_log,
                                "storage_get" => h_storage_get,
                                "storage_set" => h_storage_set,
                                "caller" => h_caller,
                                "self_address" => h_self_address,
                                "block_height" => h_block_height,
                                "tx_value" => h_tx_value,
                                "gas_remaining" => h_gas_remaining,
                            }
                        };

                        let module = WasmModule::new(&store, &bytecode).map_err(|e| {
                            StateError::ExecutionError(format!("WASM compile: {}", e))
                        })?;
                        let instance =
                            Instance::new(&mut store, &module, &import_object).map_err(|e| {
                                StateError::ExecutionError(format!("WASM instantiate: {}", e))
                            })?;

                        if let Ok(memory) = instance.exports.get_memory("memory") {
                            *func_env.as_mut(&mut store).memory.lock().unwrap() =
                                Some(memory.clone());
                        }

                        if let Some(contract_storage) = self.storage.get(&body.contract.0) {
                            let mut cache = func_env.as_ref(&store).storage_cache.lock().unwrap();
                            for entry in contract_storage.iter() {
                                cache.insert(entry.key().0, Some(entry.value().clone()));
                            }
                        }

                        let func = instance.exports.get_function(&body.function).map_err(|e| {
                            StateError::ExecutionError(format!(
                                "function '{}' not found: {}",
                                body.function, e
                            ))
                        })?;

                        let call_result = func.call(&mut store, &[]);

                        let host_state = func_env.as_ref(&store);
                        let wasm_gas_used = host_state
                            .gas_used
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let was_out_of_gas = *host_state.out_of_gas.lock().unwrap();

                        if was_out_of_gas || wasm_gas_used > wasm_gas_limit {
                            return Err(StateError::ExecutionError("WASM out of gas".into()));
                        }

                        let wasm_gas_charge = wasm_gas_used / 100;
                        let _ = gas.charge(wasm_gas_charge);

                        match call_result {
                            Ok(_) => {
                                let writes =
                                    std::mem::take(&mut *host_state.storage_writes.lock().unwrap());
                                for (key, value) in writes {
                                    self.set_storage(&body.contract, Hash256(key), value);
                                }

                                tracing::debug!(
                                    contract = ?body.contract,
                                    function = %body.function,
                                    gas_used = wasm_gas_used,
                                    "WASM contract call succeeded"
                                );
                            }
                            Err(e) => {
                                if body.value > 0 {
                                    sender.balance = body.value;
                                    self.accounts.insert(tx.from.0, sender.clone());

                                    if let Some(mut contract_acct) =
                                        self.accounts.get_mut(&body.contract.0)
                                    {
                                        contract_acct.balance -= body.value;
                                    }
                                }
                                return Err(StateError::ExecutionError(format!(
                                    "WASM exec failed: {}",
                                    e
                                )));
                            }
                        }
                    }

                    sender.nonce += 1;
                    self.accounts.insert(tx.from.0, sender.clone());
                    self.wal
                        .append(WalOp::SetAccount(tx.from, sender), self.height());
                }

                Ok(gas.consumed)
            }
            TxBody::MultiSig(_body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender);
                Ok(gas.consumed)
            }
            TxBody::DeployContract(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                let total_cost = tx.fee + body.state_rent_deposit;
                if sender.balance < total_cost {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: total_cost,
                    });
                }

                if body.bytecode.len() < 4 || &body.bytecode[..4] != WASM_MAGIC {
                    return Err(StateError::ExecutionError(
                        "invalid WASM bytecode: missing \0asm magic header".into(),
                    ));
                }

                let bytecode_gas = body.bytecode.len() as u64 * gas_costs::TX_DATA_BYTE;
                if let Err(e) = gas.charge(bytecode_gas) {
                    return Err(StateError::ExecutionError(format!("gas: {}", e)));
                }

                let contract_addr = compute_contract_address(&tx.from, tx.nonce);

                self.deploy_contract(&contract_addr, body.bytecode.clone());

                let code_hash = hash_bytes(&body.bytecode);
                let contract_acct = Account::new_contract(contract_addr, code_hash);
                self.accounts.insert(contract_addr.0, contract_acct.clone());
                self.wal.append(
                    WalOp::SetAccount(contract_addr, contract_acct),
                    self.height(),
                );

                if !body.constructor_args.is_empty() {
                    use std::sync::Mutex as StdMutex;
                    use wasmer::{
                        Function, FunctionEnv, FunctionEnvMut, Instance, Memory,
                        Module as WasmModule, Store, imports,
                    };

                    struct InitHostState {
                        gas_used: std::sync::atomic::AtomicU64,
                        gas_limit: u64,
                        out_of_gas: StdMutex<bool>,
                        logs: StdMutex<Vec<String>>,
                        storage_writes: StdMutex<Vec<([u8; 32], Vec<u8>)>>,
                        storage_cache: StdMutex<HashMap<[u8; 32], Option<Vec<u8>>>>,
                        deployer: [u8; 32],
                        self_address: [u8; 32],
                        block_height: u64,
                        memory: StdMutex<Option<Memory>>,
                    }

                    let init_gas_limit: u64 = 5_000_000;
                    let init_host = InitHostState {
                        gas_used: std::sync::atomic::AtomicU64::new(0),
                        gas_limit: init_gas_limit,
                        out_of_gas: StdMutex::new(false),
                        logs: StdMutex::new(Vec::new()),
                        storage_writes: StdMutex::new(Vec::new()),
                        storage_cache: StdMutex::new(HashMap::new()),
                        deployer: tx.from.0,
                        self_address: contract_addr.0,
                        block_height: self.height(),
                        memory: StdMutex::new(None),
                    };

                    let mut store = Store::default();
                    let func_env = FunctionEnv::new(&mut store, init_host);

                    let h_use_gas = Function::new_typed_with_env(
                        &mut store,
                        &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, amount: i64| {
                            if amount <= 0 {
                                return;
                            }
                            let data = env.data_mut();
                            let prev = data
                                .gas_used
                                .fetch_add(amount as u64, std::sync::atomic::Ordering::Relaxed);
                            if prev + amount as u64 > data.gas_limit {
                                *data.out_of_gas.lock().unwrap() = true;
                            }
                        },
                    );
                    let h_log = Function::new_typed_with_env(
                        &mut store,
                        &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, ptr: i32, len: i32| {
                            let (data, wstore) = env.data_and_store_mut();
                            if let Some(ref mem) = *data.memory.lock().unwrap() {
                                let view = mem.view(&wstore);
                                let mut buf = vec![0u8; len as usize];
                                if view.read(ptr as u64, &mut buf).is_ok() {
                                    data.logs
                                        .lock()
                                        .unwrap()
                                        .push(String::from_utf8_lossy(&buf).to_string());
                                }
                            }
                        },
                    );
                    let h_storage_get = Function::new_typed_with_env(
                        &mut store,
                        &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>,
                         key_ptr: i32,
                         val_ptr: i32|
                         -> i32 {
                            let (data, wstore) = env.data_and_store_mut();
                            let mem_guard = data.memory.lock().unwrap();
                            let mem = match *mem_guard {
                                Some(ref m) => m.clone(),
                                None => return -1,
                            };
                            drop(mem_guard);
                            let view = mem.view(&wstore);
                            let mut key = [0u8; 32];
                            if view.read(key_ptr as u64, &mut key).is_err() {
                                return -1;
                            }
                            let cache = data.storage_cache.lock().unwrap();
                            match cache.get(&key) {
                                Some(Some(val)) => {
                                    let val = val.clone();
                                    drop(cache);
                                    let view2 = mem.view(&wstore);
                                    if view2.write(val_ptr as u64, &val).is_err() {
                                        return -1;
                                    }
                                    val.len() as i32
                                }
                                _ => -1,
                            }
                        },
                    );
                    let h_storage_set = Function::new_typed_with_env(
                        &mut store,
                        &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>,
                         key_ptr: i32,
                         val_ptr: i32,
                         val_len: i32| {
                            let (data, wstore) = env.data_and_store_mut();
                            let mem_guard = data.memory.lock().unwrap();
                            let mem = match *mem_guard {
                                Some(ref m) => m.clone(),
                                None => return,
                            };
                            drop(mem_guard);
                            let view = mem.view(&wstore);
                            let mut key = [0u8; 32];
                            if view.read(key_ptr as u64, &mut key).is_err() {
                                return;
                            }
                            let mut val = vec![0u8; val_len as usize];
                            if view.read(val_ptr as u64, &mut val).is_err() {
                                return;
                            }
                            data.storage_cache
                                .lock()
                                .unwrap()
                                .insert(key, Some(val.clone()));
                            data.storage_writes.lock().unwrap().push((key, val));
                        },
                    );
                    let h_caller = Function::new_typed_with_env(
                        &mut store,
                        &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, ptr: i32| {
                            let (data, wstore) = env.data_and_store_mut();
                            if let Some(ref mem) = *data.memory.lock().unwrap() {
                                let view = mem.view(&wstore);
                                let _ = view.write(ptr as u64, &data.deployer);
                            }
                        },
                    );
                    let h_self_address = Function::new_typed_with_env(
                        &mut store,
                        &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, ptr: i32| {
                            let (data, wstore) = env.data_and_store_mut();
                            if let Some(ref mem) = *data.memory.lock().unwrap() {
                                let view = mem.view(&wstore);
                                let _ = view.write(ptr as u64, &data.self_address);
                            }
                        },
                    );
                    let h_block_height = Function::new_typed_with_env(
                        &mut store,
                        &func_env,
                        |env: FunctionEnvMut<'_, InitHostState>| -> i64 {
                            env.data().block_height as i64
                        },
                    );
                    let h_tx_value = Function::new_typed_with_env(
                        &mut store,
                        &func_env,
                        |_env: FunctionEnvMut<'_, InitHostState>| -> i64 { 0i64 },
                    );
                    let h_gas_remaining = Function::new_typed_with_env(
                        &mut store,
                        &func_env,
                        |env: FunctionEnvMut<'_, InitHostState>| -> i64 {
                            let data = env.data();
                            let used = data.gas_used.load(std::sync::atomic::Ordering::Relaxed);
                            data.gas_limit.saturating_sub(used) as i64
                        },
                    );

                    let import_object = imports! {
                        "env" => {
                            "use_gas" => h_use_gas,
                            "log" => h_log,
                            "storage_get" => h_storage_get,
                            "storage_set" => h_storage_set,
                            "caller" => h_caller,
                            "self_address" => h_self_address,
                            "block_height" => h_block_height,
                            "tx_value" => h_tx_value,
                            "gas_remaining" => h_gas_remaining,
                        }
                    };

                    let module = WasmModule::new(&store, &body.bytecode)
                        .map_err(|e| StateError::ExecutionError(format!("WASM compile: {}", e)))?;
                    let instance =
                        Instance::new(&mut store, &module, &import_object).map_err(|e| {
                            StateError::ExecutionError(format!("WASM instantiate: {}", e))
                        })?;

                    if let Ok(memory) = instance.exports.get_memory("memory") {
                        *func_env.as_mut(&mut store).memory.lock().unwrap() = Some(memory.clone());
                    }

                    if let Ok(init_fn) = instance.exports.get_function("init") {
                        let call_result = init_fn.call(&mut store, &[]);

                        let host_state = func_env.as_ref(&store);
                        let was_out_of_gas = *host_state.out_of_gas.lock().unwrap();
                        if was_out_of_gas {
                            return Err(StateError::ExecutionError(
                                "constructor out of gas".into(),
                            ));
                        }

                        match call_result {
                            Ok(_) => {
                                let writes =
                                    std::mem::take(&mut *host_state.storage_writes.lock().unwrap());
                                for (key, value) in writes {
                                    self.set_storage(&contract_addr, Hash256(key), value);
                                }
                                tracing::debug!(
                                    contract = ?contract_addr,
                                    "constructor executed successfully"
                                );
                            }
                            Err(e) => {
                                self.contracts.remove(&contract_addr.0);
                                self.accounts.remove(&contract_addr.0);
                                return Err(StateError::ExecutionError(format!(
                                    "constructor exec: {}",
                                    e
                                )));
                            }
                        }
                    }
                }

                sender.balance -= total_cost;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                tracing::info!(
                    contract = ?contract_addr,
                    deployer = ?tx.from,
                    bytecode_len = body.bytecode.len(),
                    "contract deployed"
                );

                Ok(gas.consumed)
            }
            TxBody::RegisterAgent(_body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if sender.balance < tx.fee {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: tx.fee,
                    });
                }
                sender.balance -= tx.fee;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender);
                Ok(gas.consumed)
            }
            TxBody::JoinValidator(body) => {
                self.refuse_registry_change_under_native_binding()?;
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                let next_nonce = sender.nonce.checked_add(1).ok_or_else(|| {
                    StateError::ExecutionError("validator join nonce overflow".into())
                })?;
                if sender.balance < body.initial_stake {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.initial_stake,
                    });
                }
                if body.initial_stake < Self::MIN_VALIDATOR_STAKE {
                    return Err(StateError::ExecutionError(format!(
                        "initial stake {} below minimum {}",
                        body.initial_stake,
                        Self::MIN_VALIDATOR_STAKE
                    )));
                }
                let current_stake = self
                    .validators
                    .get(&tx.from.0)
                    .map(|value| *value)
                    .unwrap_or(0);
                if current_stake != 0 {
                    return Err(StateError::ExecutionError(format!(
                        "validator already has {current_stake} tracked stake; use UpdateStake"
                    )));
                }

                let new_balance = sender
                    .balance
                    .checked_sub(body.initial_stake)
                    .ok_or_else(|| StateError::ExecutionError("join balance underflow".into()))?;
                let new_account_stake = sender
                    .staked_balance
                    .checked_add(body.initial_stake)
                    .ok_or_else(|| StateError::ExecutionError("account stake overflow".into()))?;
                self.add_staking_pool_checked(body.initial_stake)?;

                sender.balance = new_balance;
                sender.staked_balance = new_account_stake;
                sender.nonce = next_nonce;

                self.validators.insert(tx.from.0, body.initial_stake);

                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                tracing::info!(
                    validator = ?tx.from,
                    initial_stake = body.initial_stake,
                    "validator joined"
                );
                Ok(gas.consumed)
            }
            TxBody::LeaveValidator => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                let next_nonce = sender.nonce.checked_add(1).ok_or_else(|| {
                    StateError::ExecutionError("validator exit nonce overflow".into())
                })?;

                let staked = self
                    .validators
                    .get(&tx.from.0)
                    .map(|value| *value)
                    .unwrap_or(0);
                if staked > 0 {
                    if sender.staked_balance < staked {
                        return Err(StateError::ExecutionError(format!(
                            "validator account stake {} is below tracked validator stake {staked}",
                            sender.staked_balance
                        )));
                    }
                    let new_account_stake =
                        sender.staked_balance.checked_sub(staked).ok_or_else(|| {
                            StateError::ExecutionError("account stake underflow".into())
                        })?;
                    let new_balance = sender.balance.checked_add(staked).ok_or_else(|| {
                        StateError::ExecutionError("validator exit balance overflow".into())
                    })?;
                    self.subtract_staking_pool_checked(staked)?;
                    sender.staked_balance = new_account_stake;
                    sender.balance = new_balance;
                    self.validators.remove(&tx.from.0);
                }

                sender.nonce = next_nonce;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                tracing::info!(
                    validator = ?tx.from,
                    returned_stake = staked,
                    "validator left"
                );
                Ok(gas.consumed)
            }
            TxBody::ClaimRewards => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());
                Ok(gas.consumed)
            }
            TxBody::UpdateStake(body) => {
                self.refuse_registry_change_under_native_binding()?;
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                let next_nonce = sender.nonce.checked_add(1).ok_or_else(|| {
                    StateError::ExecutionError("validator stake nonce overflow".into())
                })?;

                let current_stake = self.validators.get(&tx.from.0).map(|v| *v).unwrap_or(0);
                if sender.staked_balance < current_stake {
                    return Err(StateError::ExecutionError(format!(
                        "validator account stake {} is below tracked validator stake {current_stake}",
                        sender.staked_balance
                    )));
                }

                if body.new_stake > current_stake {
                    let diff = body.new_stake - current_stake;
                    if sender.balance < diff {
                        return Err(StateError::InsufficientBalance {
                            have: sender.balance,
                            need: diff,
                        });
                    }
                    let new_balance = sender.balance.checked_sub(diff).ok_or_else(|| {
                        StateError::ExecutionError("stake increase balance underflow".into())
                    })?;
                    let new_account_stake =
                        sender.staked_balance.checked_add(diff).ok_or_else(|| {
                            StateError::ExecutionError("account stake overflow".into())
                        })?;
                    self.add_staking_pool_checked(diff)?;
                    sender.balance = new_balance;
                    sender.staked_balance = new_account_stake;
                } else if body.new_stake < current_stake {
                    let diff = current_stake - body.new_stake;
                    let new_account_stake =
                        sender.staked_balance.checked_sub(diff).ok_or_else(|| {
                            StateError::ExecutionError("account stake underflow".into())
                        })?;
                    let new_balance = sender.balance.checked_add(diff).ok_or_else(|| {
                        StateError::ExecutionError("stake decrease balance overflow".into())
                    })?;
                    self.subtract_staking_pool_checked(diff)?;
                    sender.staked_balance = new_account_stake;
                    sender.balance = new_balance;
                }

                if body.new_stake == 0 {
                    self.validators.remove(&tx.from.0);
                } else {
                    self.validators.insert(tx.from.0, body.new_stake);
                }

                sender.nonce = next_nonce;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                tracing::debug!(
                    validator = ?tx.from,
                    old_stake = current_stake,
                    new_stake = body.new_stake,
                    "validator stake updated"
                );
                Ok(gas.consumed)
            }
            TxBody::Governance(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                match body.action {
                    arc_types::transaction::GovernanceAction::Execute => {
                        tracing::info!(
                            proposal_id = body.proposal_id,
                            executor = ?tx.from,
                            "governance proposal execution recorded on-chain"
                        );
                    }
                }
                Ok(gas.consumed)
            }
            TxBody::BridgeLock(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if sender.balance < body.amount {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.amount,
                    });
                }

                sender.balance -= body.amount;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let escrow_addr = hash_bytes(b"ARC-bridge-escrow");
                let mut escrow = self.get_or_create_account(&escrow_addr);
                escrow.balance = body.amount;
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                tracing::info!(
                    from = ?tx.from,
                    amount = body.amount,
                    dest_chain = body.destination_chain,
                    "bridge lock: tokens escrowed for cross-chain transfer"
                );
                Ok(gas.consumed)
            }
            TxBody::BridgeMint(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if body.merkle_proof.is_empty() {
                    return Err(StateError::ExecutionError(
                        "bridge mint requires a non-empty merkle proof".into(),
                    ));
                }

                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let mut recipient = self.get_or_create_account(&body.recipient);
                recipient.balance = body.amount;
                self.accounts.insert(body.recipient.0, recipient.clone());
                self.wal
                    .append(WalOp::SetAccount(body.recipient, recipient), self.height());

                tracing::info!(
                    recipient = ?body.recipient,
                    amount = body.amount,
                    source_chain = body.source_chain,
                    source_tx = ?body.source_tx_hash,
                    "bridge mint: tokens credited from cross-chain transfer"
                );
                Ok(gas.consumed)
            }
            TxBody::BatchSettle(body) => {
                if body.entries.len() > gas_costs::BATCH_SETTLE_MAX_ENTRIES {
                    return Err(StateError::ExecutionError(format!(
                        "BatchSettle exceeds max entries: {} > {}",
                        body.entries.len(),
                        gas_costs::BATCH_SETTLE_MAX_ENTRIES
                    )));
                }
                if body.entries.is_empty() {
                    return Err(StateError::ExecutionError(
                        "BatchSettle with zero entries".to_string(),
                    ));
                }
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                let total_amount: u64 = body.entries.iter().map(|e| e.amount).sum();
                if sender.balance < total_amount {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: total_amount,
                    });
                }

                let mut net_credits: std::collections::HashMap<[u8; 32], u64> =
                    std::collections::HashMap::new();
                for entry in &body.entries {
                    *net_credits.entry(entry.agent_id.0).or_insert(0) += entry.amount;
                }

                sender.balance -= total_amount;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                for (agent_addr, net_amount) in &net_credits {
                    let agent_address = Hash256(*agent_addr);
                    let mut agent = self.get_or_create_account(&agent_address);
                    agent.balance = agent.balance.saturating_add(*net_amount);
                    self.accounts.insert(*agent_addr, agent.clone());
                    self.wal
                        .append(WalOp::SetAccount(agent_address, agent), self.height());
                }

                Ok(gas.consumed)
            }
            TxBody::ChannelOpen(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if sender.balance < body.deposit {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.deposit,
                    });
                }

                sender.balance -= body.deposit;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let escrow_addr = hash_bytes(&[b"arc-channel", body.channel_id.as_ref()].concat());
                let mut escrow = self.get_or_create_account(&escrow_addr);
                escrow.balance = body.deposit;
                escrow.code_hash = tx.from;
                escrow.storage_root = body.counterparty;
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                Ok(gas.consumed)
            }
            TxBody::ChannelClose(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                sender.nonce += 1;

                let escrow_addr = hash_bytes(&[b"arc-channel", body.channel_id.as_ref()].concat());
                let escrow = self.get_or_create_account(&escrow_addr);
                let total_locked = escrow.balance;

                let opener_addr = escrow.code_hash;
                let counterparty_addr_stored = escrow.storage_root;
                if tx.from != opener_addr && tx.from != counterparty_addr_stored {
                    return Err(StateError::ExecutionError(
                        "channel close: sender is neither opener nor counterparty".to_string(),
                    ));
                }

                if escrow.staked_balance > 0 && self.height() < escrow.staked_balance {
                    return Err(StateError::ExecutionError(
                        "channel close: active dispute in progress, wait for challenge period to expire".to_string(),
                    ));
                }

                let claimed_total = body
                    .opener_balance
                    .saturating_add(body.counterparty_balance);
                if claimed_total > total_locked {
                    return Err(StateError::ExecutionError(format!(
                        "channel close exceeds locked funds: claimed={}, locked={}",
                        claimed_total, total_locked
                    )));
                }

                let mut escrow_mut = self.get_or_create_account(&escrow_addr);
                escrow_mut.balance = 0;
                self.accounts.insert(escrow_addr.0, escrow_mut.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow_mut), self.height());

                sender.balance += body.opener_balance;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let counterparty_addr = escrow.storage_root;
                if body.counterparty_balance > 0 && counterparty_addr != Hash256::ZERO {
                    let mut counterparty = self.get_or_create_account(&counterparty_addr);
                    counterparty.balance += body.counterparty_balance;
                    self.accounts
                        .insert(counterparty_addr.0, counterparty.clone());
                    self.wal.append(
                        WalOp::SetAccount(counterparty_addr, counterparty),
                        self.height(),
                    );
                }

                Ok(gas.consumed)
            }
            TxBody::ChannelDispute(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let escrow_addr = hash_bytes(&[b"arc-channel", body.channel_id.as_ref()].concat());
                let mut escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance == 0 {
                    return Err(StateError::ExecutionError(
                        "channel dispute: no funds locked in channel".to_string(),
                    ));
                }

                if tx.from != escrow.code_hash && tx.from != escrow.storage_root {
                    return Err(StateError::ExecutionError(
                        "channel dispute: sender is neither opener nor counterparty".to_string(),
                    ));
                }

                if body.challenge_period == 0 || body.challenge_period > 100_000 {
                    return Err(StateError::ExecutionError(
                        "channel dispute: challenge_period must be 1..=100000".to_string(),
                    ));
                }

                if escrow.staked_balance > 0 && self.height() >= escrow.staked_balance {
                    return Err(StateError::ExecutionError(
                        "channel dispute: challenge period has expired, state is finalized"
                            .to_string(),
                    ));
                }

                if escrow.staked_balance > 0 && body.state_nonce <= escrow.nonce {
                    return Err(StateError::ExecutionError(format!(
                        "channel dispute: state_nonce {} must exceed previously disputed nonce {}",
                        body.state_nonce, escrow.nonce
                    )));
                }

                let claimed_total = body
                    .opener_balance
                    .saturating_add(body.counterparty_balance);
                if claimed_total > escrow.balance {
                    return Err(StateError::ExecutionError(format!(
                        "channel dispute: claimed balances ({}) exceed locked funds ({})",
                        claimed_total, escrow.balance
                    )));
                }

                let challenge_expiry = self.height() + body.challenge_period;
                escrow.nonce = body.state_nonce;
                escrow.staked_balance = challenge_expiry;
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                Ok(gas.consumed)
            }
            TxBody::ShardProof(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                if body.proof_data.is_empty() {
                    return Err(StateError::ExecutionError(
                        "shard proof: empty proof data".to_string(),
                    ));
                }

                if body.prev_state_root == body.post_state_root && body.tx_count > 0 {
                    return Err(StateError::ExecutionError(
                        "shard proof: state root unchanged despite transactions".to_string(),
                    ));
                }

                #[cfg(feature = "stwo-prover")]
                {
                    let recursive_input = arc_crypto::stwo_air::RecursiveVerifierInput {
                        child_hashes: vec![body.block_hash.0],
                        child_start_states: vec![body.prev_state_root.0],
                        child_end_states: vec![body.post_state_root.0],
                        merkle_siblings: vec![vec![]],
                        expected_merkle_root: body.block_hash.0,
                    };
                    if !arc_crypto::stwo_air::verify_recursive_proof(
                        &recursive_input,
                        &body.proof_data,
                    ) {
                        return Err(StateError::ExecutionError(
                            "shard proof: STARK proof verification failed".to_string(),
                        ));
                    }
                }

                let mut proof_input = Vec::new();
                proof_input.extend_from_slice(b"arc-shard-proof");
                proof_input.extend_from_slice(&body.shard_id.to_le_bytes());
                proof_input.extend_from_slice(&body.block_height.to_le_bytes());
                let proof_key = hash_bytes(&proof_input);
                let proof_hash = hash_bytes(&body.proof_data);
                let mut proof_record = self.get_or_create_account(&proof_key);
                proof_record.balance =
                    u64::from_le_bytes(proof_hash.0[..8].try_into().unwrap_or([0u8; 8]));
                proof_record.nonce = body.block_height;
                self.accounts.insert(proof_key.0, proof_record.clone());
                self.wal
                    .append(WalOp::SetAccount(proof_key, proof_record), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceAttestation(body) => {
                let mut sender = self
                    .get_account(&tx.from)
                    .unwrap_or_else(|| Account::new(tx.from, 0));
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                let next_nonce = sender.nonce.checked_add(1).ok_or_else(|| {
                    StateError::ExecutionError("inference attestation nonce overflow".to_string())
                })?;

                if sender.balance < body.bond {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.bond,
                    });
                }

                let escrow_candidate = if body.bond > 0 {
                    let escrow_addr = hash_bytes(&[b"arc-inference", tx.hash.as_ref()].concat());
                    if self.accounts.contains_key(&escrow_addr.0) {
                        return Err(StateError::ExecutionError(
                            "inference attestation escrow already exists".to_string(),
                        ));
                    }
                    let release_height = self
                        .height()
                        .checked_add(body.challenge_period)
                        .ok_or_else(|| {
                            StateError::ExecutionError(
                                "inference attestation release height overflow".to_string(),
                            )
                        })?;
                    let mut escrow = Account::new(escrow_addr, 0);
                    escrow.balance = body.bond;
                    escrow.code_hash = tx.from;
                    escrow.nonce = release_height;
                    let mut sr = [0u8; 32];
                    sr[..8].copy_from_slice(&ATTEST_ESCROW_MAGIC);
                    sr[8] = ATTEST_STATUS_OPEN;
                    escrow.storage_root = Hash256(sr);
                    Some((escrow_addr, release_height, escrow))
                } else {
                    None
                };

                sender.balance = sender.balance.checked_sub(body.bond).ok_or_else(|| {
                    StateError::ExecutionError(
                        "inference attestation balance underflow".to_string(),
                    )
                })?;
                sender.nonce = next_nonce;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                if let Some((escrow_addr, release_height, escrow)) = escrow_candidate {
                    self.accounts.insert(escrow_addr.0, escrow.clone());
                    self.wal
                        .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                    let mut q = self.pending_bond_releases.lock();
                    let bucket = q.entry(release_height).or_default();
                    if let Err(pos) = bucket.binary_search(&escrow_addr.0) {
                        bucket.insert(pos, escrow_addr.0);
                    }
                }

                Ok(gas.consumed)
            }
            TxBody::CommunityInferenceReward(body) => {
                if self.active_protocol_version().major == 3 {
                    self.validate_community_reward_admission(tx, body, self.height())?;
                }
                if !self.community_rewards_v1_active() {
                    return Err(StateError::ExecutionError(
                        "community inference reward: protocol feature is not active at this height"
                            .to_string(),
                    ));
                }
                if !self.is_validator(&tx.from) {
                    return Err(StateError::ExecutionError(format!(
                        "community inference reward: signer {} is not an active validator",
                        tx.from.to_hex()
                    )));
                }
                self.verify_transaction_signature(tx).map_err(|_| {
                    StateError::ExecutionError(
                        "community inference reward: invalid validator signature".to_string(),
                    )
                })?;
                let expected_domain =
                    arc_types::transaction::CommunityInferenceRewardBody::expected_chain_domain();
                if body.chain_domain != expected_domain {
                    return Err(StateError::ExecutionError(
                        "community inference reward: wrong chain domain".to_string(),
                    ));
                }
                if body.job_id == Hash256::ZERO {
                    return Err(StateError::ExecutionError(
                        "community inference reward: job_id cannot be zero".to_string(),
                    ));
                }
                if body.coordinator != tx.from || !self.is_validator(&body.coordinator) {
                    return Err(StateError::ExecutionError(
                        "community inference reward: assignment coordinator must be the active outer signer"
                            .to_string(),
                    ));
                }
                if body.assignment_epoch == Hash256::ZERO {
                    return Err(StateError::ExecutionError(
                        "community inference reward: assignment_epoch cannot be zero".to_string(),
                    ));
                }
                match self.recovery_context() {
                    Some(context)
                        if body.recovery_epoch == context.recovery_epoch
                            && body.validator_set_id == context.validator_set_id
                            && body.transaction_domain == context.domain_hash() => {}
                    Some(_) => {
                        return Err(StateError::ExecutionError(
                            "community inference reward: recovery epoch, validator-set ID, or transaction domain does not match active state"
                                .to_string(),
                        ));
                    }
                    None if body.recovery_epoch == 0
                        && body.validator_set_id == 0
                        && body.transaction_domain == Hash256::ZERO => {}
                    None => {
                        return Err(StateError::ExecutionError(
                            "community inference reward: recovery binding is non-zero on legacy/dev state"
                                .to_string(),
                        ));
                    }
                }
                let expected_job_id =
                    arc_types::transaction::CommunityInferenceRewardBody::derive_job_id(
                        &body.coordinator,
                        &body.assignment_epoch,
                        body.job_nonce,
                        &body.model_id,
                        &body.input_hash,
                        body.max_tokens,
                    );
                if body.job_id != expected_job_id {
                    return Err(StateError::ExecutionError(
                        "community inference reward: job_id does not match its exact assignment commitment"
                            .to_string(),
                    ));
                }
                if body.max_tokens == 0 {
                    return Err(StateError::ExecutionError(
                        "community inference reward: max_tokens must be positive".to_string(),
                    ));
                }
                if self.height() > body.expires_at_height {
                    return Err(StateError::ExecutionError(format!(
                        "community inference reward: job expired at height {} (current {})",
                        body.expires_at_height,
                        self.height()
                    )));
                }

                let worker_attestation = body.reconstruct_worker_attestation();
                self.verify_transaction_signature(&worker_attestation)
                    .map_err(|_| {
                        StateError::ExecutionError(
                            "community inference reward: invalid worker certificate signature"
                                .to_string(),
                        )
                    })?;

                let treasury_addr = arc_types::transaction::inference_reward_treasury_address();
                let worker_stake = self.get_validator_stake(&body.worker).unwrap_or(0);
                if below_policy_minimum(
                    worker_stake,
                    arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE,
                ) {
                    return Err(StateError::ExecutionError(format!(
                        "community inference reward: worker stake {} is below active policy minimum {}",
                        worker_stake,
                        arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE
                    )));
                }
                if body.worker == treasury_addr {
                    return Err(StateError::ExecutionError(
                        "community inference reward: treasury cannot be the worker".to_string(),
                    ));
                }
                let protocol_v3 = self.active_protocol_version().major == 3;
                let job_marker_addr = if protocol_v3 {
                    arc_types::transaction::CommunityInferenceRewardBody::v3_marker_address(
                        &body.chain_domain,
                        &body.job_id,
                    )
                } else {
                    arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                        &body.chain_domain,
                        &body.job_id,
                    )
                };
                if self.accounts.contains_key(&job_marker_addr.0) {
                    return Err(StateError::ExecutionError(format!(
                        "community inference reward: job {} already paid",
                        body.job_id.to_hex()
                    )));
                }
                let certificate_marker_addr = if protocol_v3 {
                    arc_types::transaction::CommunityInferenceRewardBody::v3_certificate_marker_address(
                        &body.chain_domain,
                        &body.worker,
                        &body.worker_certificate.attestation_hash,
                    )
                } else {
                    arc_types::transaction::CommunityInferenceRewardBody::certificate_marker_address(
                        &body.chain_domain,
                        &body.worker,
                        &body.worker_certificate.attestation_hash,
                    )
                };
                if self.accounts.contains_key(&certificate_marker_addr.0) {
                    return Err(StateError::ExecutionError(format!(
                        "community inference reward: worker certificate {} already paid",
                        body.worker_certificate.attestation_hash.to_hex()
                    )));
                }
                let recovery_probe_marker_addr = if protocol_v3 {
                    arc_types::transaction::CommunityInferenceRewardBody::v3_recovery_probe_marker_address(
                        &body.chain_domain,
                        &body.assignment_epoch,
                    )
                } else {
                    arc_types::transaction::CommunityInferenceRewardBody::recovery_probe_marker_address(
                        &body.chain_domain,
                        &body.assignment_epoch,
                    )
                };
                if let Some(marker) = recovery_probe_marker_addr
                    && self.accounts.contains_key(&marker.0)
                {
                    return Err(StateError::ExecutionError(format!(
                        "community inference reward: recovery probe {} already paid",
                        body.assignment_epoch.to_hex()
                    )));
                }

                self.verify_community_reward_validator_approvals(body)?;

                let reward = arc_types::economics::INFERENCE_ATTESTATION_REWARD;
                let mut worker = self
                    .get_account(&body.worker)
                    .unwrap_or_else(|| Account::new(body.worker, 0));
                worker.balance = worker.balance.checked_add(reward).ok_or_else(|| {
                    StateError::ExecutionError(
                        "community inference reward: worker balance overflow".to_string(),
                    )
                })?;
                let mut treasury = self.get_account(&treasury_addr).ok_or_else(|| {
                    StateError::ExecutionError(
                        "community inference reward: treasury is not funded".to_string(),
                    )
                })?;
                if treasury.balance < reward {
                    return Err(StateError::InsufficientBalance {
                        have: treasury.balance,
                        need: reward,
                    });
                }
                treasury.balance = treasury.balance.checked_sub(reward).ok_or_else(|| {
                    StateError::ExecutionError(
                        "community inference reward: treasury balance underflow".to_string(),
                    )
                })?;

                if job_marker_addr == certificate_marker_addr
                    || job_marker_addr == body.worker
                    || certificate_marker_addr == body.worker
                    || job_marker_addr == treasury_addr
                    || certificate_marker_addr == treasury_addr
                    || recovery_probe_marker_addr.is_some_and(|marker| {
                        marker == job_marker_addr
                            || marker == certificate_marker_addr
                            || marker == body.worker
                            || marker == body.coordinator
                            || marker == treasury_addr
                    })
                {
                    return Err(StateError::ExecutionError(
                        "community inference reward: marker address collision".to_string(),
                    ));
                }
                let mut job_marker = Account::new(job_marker_addr, 0);
                job_marker.nonce = 1;
                job_marker.code_hash = body.worker;
                job_marker.storage_root = body.output_hash;
                let mut certificate_marker = Account::new(certificate_marker_addr, 0);
                certificate_marker.nonce = 1;
                certificate_marker.code_hash = body.worker;
                certificate_marker.storage_root = body.job_id;
                let recovery_probe_marker = recovery_probe_marker_addr.map(|address| {
                    let mut marker = Account::new(address, 0);
                    marker.nonce = 1;
                    marker.code_hash = body.coordinator;
                    marker.storage_root = body.job_id;
                    (address, marker)
                });
                let budget_candidates = if protocol_v3 {
                    Some(self.community_reward_budget_candidates_at(body, self.height())?)
                } else {
                    None
                };

                self.accounts.insert(treasury_addr.0, treasury.clone());
                self.accounts.insert(body.worker.0, worker.clone());
                self.accounts.insert(job_marker_addr.0, job_marker.clone());
                self.accounts
                    .insert(certificate_marker_addr.0, certificate_marker.clone());
                if let Some((address, marker)) = &recovery_probe_marker {
                    self.accounts.insert(address.0, marker.clone());
                }
                if let Some(budget) = &budget_candidates {
                    for (address, account) in [
                        &budget.block,
                        &budget.epoch,
                        &budget.worker,
                        &budget.coordinator,
                    ] {
                        self.accounts.insert(address.0, account.clone());
                    }
                }
                if self.use_jmt {
                    let mut jmt = self.jmt.lock();
                    for (address, account) in [
                        (treasury_addr, &treasury),
                        (body.worker, &worker),
                        (job_marker_addr, &job_marker),
                        (certificate_marker_addr, &certificate_marker),
                    ] {
                        let value = hash_bytes(&bincode::serialize(account).unwrap_or_default());
                        jmt.update_leaf(address.0, value);
                    }
                    if let Some((address, marker)) = &recovery_probe_marker {
                        let value = hash_bytes(&bincode::serialize(marker).unwrap_or_default());
                        jmt.update_leaf(address.0, value);
                    }
                    if let Some(budget) = &budget_candidates {
                        for (address, account) in [
                            &budget.block,
                            &budget.epoch,
                            &budget.worker,
                            &budget.coordinator,
                        ] {
                            let value =
                                hash_bytes(&bincode::serialize(account).unwrap_or_default());
                            jmt.update_leaf(address.0, value);
                        }
                    }
                }
                self.wal
                    .append(WalOp::SetAccount(treasury_addr, treasury), self.height());
                self.wal
                    .append(WalOp::SetAccount(body.worker, worker), self.height());
                self.wal.append(
                    WalOp::SetAccount(job_marker_addr, job_marker),
                    self.height(),
                );
                self.wal.append(
                    WalOp::SetAccount(certificate_marker_addr, certificate_marker),
                    self.height(),
                );
                if let Some((address, marker)) = recovery_probe_marker {
                    self.wal
                        .append(WalOp::SetAccount(address, marker), self.height());
                }
                if let Some(budget) = budget_candidates {
                    for (address, account) in [
                        budget.block,
                        budget.epoch,
                        budget.worker,
                        budget.coordinator,
                    ] {
                        self.wal
                            .append(WalOp::SetAccount(address, account), self.height());
                    }
                }

                Ok(gas.consumed)
            }
            TxBody::InferenceChallenge(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                if sender.balance < body.challenger_bond {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.challenger_bond,
                    });
                }

                let escrow_addr =
                    hash_bytes(&[b"arc-inference", body.attestation_hash.as_ref()].concat());
                let escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance == 0 {
                    return Err(StateError::ExecutionError(
                        "inference challenge: attestation escrow not found or already resolved"
                            .to_string(),
                    ));
                }

                sender.balance -= body.challenger_bond;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let total_bond = escrow.balance + body.challenger_bond;
                let mut escrow = escrow.clone();
                escrow.balance = total_bond;
                let mut sr = escrow.storage_root.0;
                if sr[..8] == ATTEST_ESCROW_MAGIC {
                    sr[8] = ATTEST_STATUS_CHALLENGED;
                    escrow.storage_root = Hash256(sr);
                }
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceRegister(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                if body.tier == 0 || body.tier > 4 {
                    return Err(StateError::ExecutionError(format!(
                        "inference register: invalid tier {}, must be 1-4",
                        body.tier
                    )));
                }

                if sender.balance < body.stake_bond {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.stake_bond,
                    });
                }

                let min_stakes = [0u64, 1_000, 5_000, 10_000, 25_000];
                let min_stake = min_stakes[body.tier as usize];
                if body.stake_bond < min_stake {
                    return Err(StateError::ExecutionError(format!(
                        "inference register: stake {} below minimum {} for tier {}",
                        body.stake_bond, min_stake, body.tier
                    )));
                }

                sender.balance -= body.stake_bond;
                sender.staked_balance += body.stake_bond;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceEscrowOpen(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if sender.balance < body.max_fee {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.max_fee,
                    });
                }

                let escrow_addr_bytes = InferenceEscrowOpenBody::escrow_address(&body.request_id);
                let escrow_addr = Hash256(escrow_addr_bytes);
                let existing = self.get_or_create_account(&escrow_addr);
                if existing.balance != 0 {
                    return Err(StateError::ExecutionError(format!(
                        "inference escrow open: request_id already has open \
                         escrow (balance={})",
                        existing.balance
                    )));
                }

                sender.balance -= body.max_fee;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let commitment = InferenceEscrowOpenBody::metadata_commitment(
                    &tx.from,
                    &body.model_id,
                    body.max_tokens,
                    body.timeout_blocks,
                );
                let mut escrow = existing;
                escrow.balance = body.max_fee;
                escrow.nonce = self.height();
                escrow.storage_root = Hash256(commitment);
                self.accounts.insert(escrow_addr_bytes, escrow.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceEscrowRelease(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if body.replicas.is_empty() {
                    return Err(StateError::ExecutionError(
                        "inference escrow release: must name at least one replica".into(),
                    ));
                }

                let escrow_addr_bytes = InferenceEscrowOpenBody::escrow_address(&body.request_id);
                let escrow_addr = Hash256(escrow_addr_bytes);
                let escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance == 0 {
                    return Err(StateError::ExecutionError(
                        "inference escrow release: no open escrow for this \
                         request_id"
                            .into(),
                    ));
                }

                let expected = InferenceEscrowOpenBody::metadata_commitment(
                    &body.payer,
                    &body.model_id,
                    body.max_tokens,
                    body.timeout_blocks,
                );
                if escrow.storage_root.0 != expected {
                    return Err(StateError::ExecutionError(
                        "inference escrow release: metadata commitment mismatch".into(),
                    ));
                }

                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let total = escrow.balance;
                let proposer_share = total * 40 / 100;
                let replicas_pool = total * 25 / 100;
                let observer_share = total * 15 / 100;
                let per_replica = replicas_pool / body.replicas.len() as u64;
                let replicas_paid = per_replica * body.replicas.len() as u64;
                let treasury_share = total - proposer_share - replicas_paid - observer_share;

                let mut proposer_acc = self.get_or_create_account(&body.proposer);
                proposer_acc.balance += proposer_share;
                self.accounts.insert(body.proposer.0, proposer_acc.clone());
                self.wal.append(
                    WalOp::SetAccount(body.proposer, proposer_acc),
                    self.height(),
                );

                for r in &body.replicas {
                    let mut rep = self.get_or_create_account(r);
                    rep.balance += per_replica;
                    self.accounts.insert(r.0, rep.clone());
                    self.wal.append(WalOp::SetAccount(*r, rep), self.height());
                }

                let mut obs = self.get_or_create_account(&body.observer_pool);
                obs.balance += observer_share;
                self.accounts.insert(body.observer_pool.0, obs.clone());
                self.wal
                    .append(WalOp::SetAccount(body.observer_pool, obs), self.height());

                let mut tre = self.get_or_create_account(&body.treasury);
                tre.balance += treasury_share;
                self.accounts.insert(body.treasury.0, tre.clone());
                self.wal
                    .append(WalOp::SetAccount(body.treasury, tre), self.height());

                let mut released = escrow;
                released.balance = 0;
                released.storage_root = Hash256::ZERO;
                self.accounts.insert(escrow_addr_bytes, released.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, released), self.height());

                Ok(gas.consumed)
            }
            TxBody::ModelRegistration(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if body.quantization.len() > 32 {
                    return Err(StateError::ExecutionError(
                        "model registration: quantization tag > 32 bytes".into(),
                    ));
                }
                let fee = body.registration_fee.max(MIN_MODEL_REGISTRATION_FEE);
                if sender.balance < fee {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: fee,
                    });
                }
                let registry_addr_bytes = ModelRegistrationBody::registry_account(&body.model_id);
                let registry_addr = Hash256(registry_addr_bytes);
                let existing = self.get_or_create_account(&registry_addr);
                if existing.storage_root != Hash256::ZERO {
                    return Err(StateError::ExecutionError(
                        "model registration: model_id already registered".into(),
                    ));
                }

                sender.balance -= fee;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let treasury = Hash256(arc_crypto::hash_bytes(b"arc-treasury").0);
                let mut tre = self.get_or_create_account(&treasury);
                tre.balance += fee;
                self.accounts.insert(treasury.0, tre.clone());
                self.wal
                    .append(WalOp::SetAccount(treasury, tre), self.height());

                let commitment = ModelRegistrationBody::metadata_commitment(
                    body.n_layers,
                    body.d_model,
                    &body.quantization,
                    &body.chunk_tree_root,
                    &body.royalty_recipient,
                );
                let mut reg = existing;
                reg.nonce = self.height();
                reg.storage_root = Hash256(commitment);
                reg.balance = 0;
                self.accounts.insert(registry_addr_bytes, reg.clone());
                self.wal
                    .append(WalOp::SetAccount(registry_addr, reg), self.height());

                Ok(gas.consumed)
            }
            TxBody::ModelRequest(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let req_addr_bytes = ModelRequestBody::request_account(&body.request_id);
                let req_addr = Hash256(req_addr_bytes);
                let mut req = self.get_or_create_account(&req_addr);
                req.balance = body.bond_per_layer_epoch;
                req.nonce = self.height();
                let mut meta = Vec::new();
                meta.extend_from_slice(&body.model_id.0);
                meta.extend_from_slice(&body.target_k_replication.to_le_bytes());
                meta.extend_from_slice(&body.max_wait_secs.to_le_bytes());
                meta.extend_from_slice(&tx.from.0);
                req.storage_root = arc_crypto::hash_bytes(&meta);
                self.accounts.insert(req_addr_bytes, req.clone());
                self.wal
                    .append(WalOp::SetAccount(req_addr, req), self.height());

                Ok(gas.consumed)
            }
            TxBody::ShardCoverageClaim(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if body.bond == 0 {
                    return Err(StateError::ExecutionError(
                        "shard coverage claim: bond must be > 0".into(),
                    ));
                }
                if body.ranges.is_empty() {
                    return Err(StateError::ExecutionError(
                        "shard coverage claim: must claim at least one range".into(),
                    ));
                }
                if sender.balance < body.bond {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.bond,
                    });
                }

                sender.balance -= body.bond;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let claim_addr_bytes =
                    ShardCoverageClaimBody::claim_account(&body.model_id, &body.node_pubkey);
                let claim_addr = Hash256(claim_addr_bytes);
                let mut claim = self.get_or_create_account(&claim_addr);
                if claim.balance != 0 {
                    return Err(StateError::ExecutionError(
                        "shard coverage claim: prior claim still active - \
                         release or wait for epoch end before re-claiming"
                            .into(),
                    ));
                }
                claim.balance = body.bond;
                claim.nonce = self.height();
                let mut meta = Vec::new();
                for (s, e) in &body.ranges {
                    meta.extend_from_slice(&s.to_le_bytes());
                    meta.extend_from_slice(&e.to_le_bytes());
                }
                meta.extend_from_slice(&body.epoch_blocks.to_le_bytes());
                meta.extend_from_slice(&tx.from.0);
                claim.storage_root = arc_crypto::hash_bytes(&meta);
                self.accounts.insert(claim_addr_bytes, claim.clone());
                self.wal
                    .append(WalOp::SetAccount(claim_addr, claim), self.height());

                Ok(gas.consumed)
            }
            TxBody::CapacityAdvertisement(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if body.region.len() > 8 {
                    return Err(StateError::ExecutionError(
                        "capacity advertisement: region tag > 8 bytes".into(),
                    ));
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let cap_addr_bytes = CapacityAdvertisementBody::capacity_account(&body.node_pubkey);
                let cap_addr = Hash256(cap_addr_bytes);
                let mut cap = self.get_or_create_account(&cap_addr);
                let mut meta = Vec::new();
                meta.extend_from_slice(&body.ram_bytes.to_le_bytes());
                meta.extend_from_slice(&body.vram_bytes.to_le_bytes());
                meta.extend_from_slice(&body.bandwidth_mbps.to_le_bytes());
                meta.extend_from_slice(&body.uptime_hint_mins.to_le_bytes());
                meta.extend_from_slice(&body.stake.to_le_bytes());
                meta.extend_from_slice(&(body.region.len() as u32).to_le_bytes());
                meta.extend_from_slice(body.region.as_bytes());
                cap.nonce = self.height();
                cap.storage_root = arc_crypto::hash_bytes(&meta);
                self.accounts.insert(cap_addr_bytes, cap.clone());
                self.wal
                    .append(WalOp::SetAccount(cap_addr, cap), self.height());

                Ok(gas.consumed)
            }
            TxBody::ShardAssignmentProposal(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if body.assignments.is_empty() {
                    return Err(StateError::ExecutionError(
                        "shard assignment proposal: must include at least one entry".into(),
                    ));
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let prop_addr = Hash256(
                    arc_crypto::hash_bytes(
                        &[b"arc-planner-proposal", body.input_snapshot_hash.0.as_ref()].concat(),
                    )
                    .0,
                );
                let mut prop = self.get_or_create_account(&prop_addr);
                let mut buf = Vec::new();
                buf.extend_from_slice(&body.epoch_blocks.to_le_bytes());
                buf.extend_from_slice(&body.input_snapshot_hash.0);
                for a in &body.assignments {
                    buf.extend_from_slice(&a.node_pubkey);
                    buf.extend_from_slice(&a.model_id.0);
                    for (s, e) in &a.ranges {
                        buf.extend_from_slice(&s.to_le_bytes());
                        buf.extend_from_slice(&e.to_le_bytes());
                    }
                }
                prop.nonce = self.height();
                prop.storage_root = arc_crypto::hash_bytes(&buf);
                self.accounts.insert(prop_addr.0, prop.clone());
                self.wal
                    .append(WalOp::SetAccount(prop_addr, prop), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceEscrowRefund(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                let escrow_addr_bytes = InferenceEscrowOpenBody::escrow_address(&body.request_id);
                let escrow_addr = Hash256(escrow_addr_bytes);
                let escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance == 0 {
                    return Err(StateError::ExecutionError(
                        "inference escrow refund: no open escrow for this \
                         request_id"
                            .into(),
                    ));
                }
                let expected = InferenceEscrowOpenBody::metadata_commitment(
                    &tx.from,
                    &body.model_id,
                    body.max_tokens,
                    body.timeout_blocks,
                );
                if escrow.storage_root.0 != expected {
                    return Err(StateError::ExecutionError(
                        "inference escrow refund: caller is not the original \
                         payer (metadata mismatch)"
                            .into(),
                    ));
                }
                let opened_at = escrow.nonce;
                let now = self.height();
                if now < opened_at + body.timeout_blocks {
                    return Err(StateError::ExecutionError(format!(
                        "inference escrow refund: timeout not elapsed \
                         (now={}, opened_at={}, timeout={})",
                        now, opened_at, body.timeout_blocks
                    )));
                }

                sender.balance += escrow.balance;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let mut refunded = escrow;
                refunded.balance = 0;
                refunded.storage_root = Hash256::ZERO;
                self.accounts.insert(escrow_addr_bytes, refunded.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, refunded), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceRequest(body) => {
                use arc_types::transaction as ttx;

                if body.input_blob.len() > ttx::TIER1_INPUT_BLOB_MAX {
                    return Err(StateError::ExecutionError(format!(
                        "tier1 request: input_blob {} > max {}",
                        body.input_blob.len(),
                        ttx::TIER1_INPUT_BLOB_MAX
                    )));
                }
                if body.max_tokens == 0 || body.max_tokens > ttx::TIER1_MAX_TOKENS {
                    return Err(StateError::ExecutionError(format!(
                        "tier1 request: max_tokens {} outside [1, {}]",
                        body.max_tokens,
                        ttx::TIER1_MAX_TOKENS
                    )));
                }
                if body.deadline_blocks < ttx::TIER1_MIN_DEADLINE_BLOCKS
                    || body.deadline_blocks > ttx::TIER1_MAX_DEADLINE_BLOCKS
                {
                    return Err(StateError::ExecutionError(format!(
                        "tier1 request: deadline_blocks {} outside [{}, {}]",
                        body.deadline_blocks,
                        ttx::TIER1_MIN_DEADLINE_BLOCKS,
                        ttx::TIER1_MAX_DEADLINE_BLOCKS
                    )));
                }
                if body.committee_size == 0 || body.committee_size > 32 {
                    return Err(StateError::ExecutionError(format!(
                        "tier1 request: committee_size {} outside [1, 32]",
                        body.committee_size
                    )));
                }
                if body.tier != 1 {
                    return Err(StateError::ExecutionError(format!(
                        "tier1 request: only tier=1 supported in this phase, got {}",
                        body.tier
                    )));
                }
                let computed = hash_bytes(&body.input_blob);
                if computed != body.input_hash {
                    return Err(StateError::ExecutionError(
                        "tier1 request: input_hash does not match blob".into(),
                    ));
                }

                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                if sender.balance < body.max_reward {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.max_reward,
                    });
                }

                sender.balance -= body.max_reward;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                let mut escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance != 0 || escrow.code_hash != Hash256::ZERO {
                    return Err(StateError::ExecutionError(
                        "tier1 request: request_id collides with existing escrow".into(),
                    ));
                }
                escrow.balance = body.max_reward;
                escrow.nonce = self.height();
                let mut status_bytes = [0u8; 32];
                status_bytes[0] = TIER1_STATUS_OPEN;
                status_bytes[1..9].copy_from_slice(&body.deadline_blocks.to_le_bytes());
                status_bytes[9] = body.committee_size;
                escrow.code_hash = Hash256(status_bytes);
                let mut meta = Vec::new();
                meta.extend_from_slice(&body.model_id.0);
                meta.extend_from_slice(&body.input_hash.0);
                meta.extend_from_slice(&body.tier.to_le_bytes());
                meta.extend_from_slice(&body.max_tokens.to_le_bytes());
                meta.extend_from_slice(&body.committee_size.to_le_bytes());
                meta.extend_from_slice(&body.deadline_blocks.to_le_bytes());
                meta.extend_from_slice(&body.max_reward.to_le_bytes());
                meta.extend_from_slice(&self.height().to_le_bytes());
                escrow.storage_root = hash_bytes(&meta);
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                self.set_storage(
                    &escrow_addr,
                    hash_bytes(b"tier1.requester"),
                    tx.from.0.to_vec(),
                );
                self.set_storage(
                    &escrow_addr,
                    hash_bytes(b"tier1.input_blob"),
                    body.input_blob.clone(),
                );
                self.set_storage(
                    &escrow_addr,
                    hash_bytes(b"tier1.votes"),
                    bincode::serialize(&Vec::<(Address, Hash256)>::new()).unwrap_or_default(),
                );
                self.set_storage(
                    &escrow_addr,
                    hash_bytes(b"tier1.request_id"),
                    body.request_id.to_vec(),
                );

                self.tier1_pending.insert(body.request_id, self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceVote(body) => {
                use arc_types::transaction as ttx;

                if let Some(blob) = &body.output_blob {
                    if blob.len() > ttx::TIER1_OUTPUT_BLOB_MAX {
                        return Err(StateError::ExecutionError(format!(
                            "tier1 vote: output_blob {} > max {}",
                            blob.len(),
                            ttx::TIER1_OUTPUT_BLOB_MAX
                        )));
                    }
                    let computed = hash_bytes(blob);
                    if computed != body.output_hash {
                        return Err(StateError::ExecutionError(
                            "tier1 vote: output_hash does not match attached blob".into(),
                        ));
                    }
                }

                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                let escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance == 0 {
                    return Err(StateError::ExecutionError(
                        "tier1 vote: request not found or already settled".into(),
                    ));
                }
                let status = escrow.code_hash.0[0];
                if status != TIER1_STATUS_OPEN && status != TIER1_STATUS_VOTING {
                    return Err(StateError::ExecutionError(format!(
                        "tier1 vote: request status {} disallows voting",
                        status
                    )));
                }
                let committee_size = escrow.code_hash.0[9] as usize;

                let mut seed_input: Vec<u8> = Vec::with_capacity(64);
                seed_input.extend_from_slice(b"tier1-seed");
                seed_input.extend_from_slice(&body.request_id);
                seed_input.extend_from_slice(&escrow.nonce.to_le_bytes());
                let canonical_seed = hash_bytes(&seed_input);

                let mut eligible: Vec<Address> = self
                    .validators
                    .iter()
                    .map(|kv| Hash256(*kv.key()))
                    .collect();
                eligible.sort_by_key(|a| a.0);
                let mut scored: Vec<(Address, Hash256)> = eligible
                    .into_iter()
                    .map(|a| {
                        let mut input = Vec::with_capacity(64);
                        input.extend_from_slice(&canonical_seed.0);
                        input.extend_from_slice(&a.0);
                        (a, hash_bytes(&input))
                    })
                    .collect();
                scored.sort_by_key(|a| a.1.0);
                let members: Vec<Address> = scored
                    .into_iter()
                    .take(committee_size)
                    .map(|(a, _)| a)
                    .collect();
                if !members.iter().any(|m| m.0 == tx.from.0) {
                    return Err(StateError::ExecutionError(format!(
                        "tier1 vote: signer {} not in committee",
                        tx.from.to_hex()
                    )));
                }

                let key = hash_bytes(b"tier1.votes");
                let existing = self.get_storage(&escrow_addr, &key).unwrap_or_default();
                let mut votes: Vec<(Address, Hash256)> =
                    bincode::deserialize(&existing).unwrap_or_default();
                if votes.iter().any(|(v, _)| v.0 == tx.from.0) {
                    return Err(StateError::ExecutionError(
                        "tier1 vote: duplicate vote from this validator".into(),
                    ));
                }
                votes.push((tx.from, body.output_hash));
                let encoded = bincode::serialize(&votes).map_err(|e| {
                    StateError::ExecutionError(format!("tier1 vote: serialize votes: {}", e))
                })?;
                self.set_storage(&escrow_addr, key, encoded);

                if let Some(blob) = &body.output_blob {
                    let blob_key = hash_bytes(b"tier1.output_blob");
                    if self.get_storage(&escrow_addr, &blob_key).is_none() {
                        self.set_storage(&escrow_addr, blob_key, blob.clone());
                    }
                }

                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                if status == TIER1_STATUS_OPEN {
                    let mut updated = escrow.clone();
                    updated.code_hash.0[0] = TIER1_STATUS_VOTING;
                    self.accounts.insert(escrow_addr.0, updated.clone());
                    self.wal
                        .append(WalOp::SetAccount(escrow_addr, updated), self.height());
                }

                Ok(gas.consumed)
            }
            TxBody::InferenceFinalize(body) => {
                use arc_types::transaction as ttx;

                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                let escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance == 0 {
                    return Err(StateError::ExecutionError(
                        "tier1 finalize: request not found or already settled".into(),
                    ));
                }
                let status = escrow.code_hash.0[0];
                if status != TIER1_STATUS_OPEN && status != TIER1_STATUS_VOTING {
                    return Err(StateError::ExecutionError(format!(
                        "tier1 finalize: request status {} already terminal",
                        status
                    )));
                }
                let deadline_blocks =
                    u64::from_le_bytes(escrow.code_hash.0[1..9].try_into().unwrap_or([0u8; 8]));
                let committee_size = escrow.code_hash.0[9] as usize;
                let anchor_height = escrow.nonce;
                let now = self.height();

                let key = hash_bytes(b"tier1.votes");
                let votes_bytes = self.get_storage(&escrow_addr, &key).unwrap_or_default();
                let votes: Vec<(Address, Hash256)> =
                    bincode::deserialize(&votes_bytes).unwrap_or_default();
                let vote_count = votes.len();

                let timeout_reached = now >= anchor_height.saturating_add(deadline_blocks);
                let all_voted = vote_count >= committee_size;
                if !timeout_reached && !all_voted {
                    return Err(StateError::ExecutionError(format!(
                        "tier1 finalize: not yet eligible (votes {}/{}, height {} < deadline {})",
                        vote_count,
                        committee_size,
                        now,
                        anchor_height.saturating_add(deadline_blocks)
                    )));
                }

                let min_agreement: usize = (committee_size / 2 + 1).max(1);
                let mut tally: std::collections::HashMap<[u8; 32], usize> =
                    std::collections::HashMap::new();
                for (_, oh) in &votes {
                    *tally.entry(oh.0).or_insert(0) += 1;
                }
                let majority = tally
                    .iter()
                    .max_by_key(|(_, c)| **c)
                    .map(|(h, c)| (Hash256(*h), *c))
                    .unwrap_or((Hash256::ZERO, 0));

                let requester_bytes = self
                    .get_storage(&escrow_addr, &hash_bytes(b"tier1.requester"))
                    .unwrap_or_default();
                let requester: Address = if requester_bytes.len() == 32 {
                    let mut a = [0u8; 32];
                    a.copy_from_slice(&requester_bytes);
                    Hash256(a)
                } else {
                    return Err(StateError::ExecutionError(
                        "tier1 finalize: requester not recorded in escrow storage".into(),
                    ));
                };

                let max_reward = escrow.balance;
                let new_status: u8;
                if majority.1 >= min_agreement {
                    let voters_pool = max_reward * ttx::TIER1_REWARD_SHARE_VOTERS_BPS / 10_000;
                    let refund = max_reward * ttx::TIER1_REWARD_SHARE_REFUND_BPS / 10_000;
                    let treasury = max_reward * ttx::TIER1_REWARD_SHARE_TREASURY_BPS / 10_000;
                    let remainder = max_reward.saturating_sub(voters_pool + refund + treasury);

                    let agreeing: Vec<Address> = votes
                        .iter()
                        .filter(|(_, oh)| oh.0 == majority.0.0)
                        .map(|(v, _)| *v)
                        .collect();
                    let per_voter = if agreeing.is_empty() {
                        0
                    } else {
                        voters_pool / agreeing.len() as u64
                    };
                    let voter_rem = voters_pool.saturating_sub(per_voter * agreeing.len() as u64);

                    for v in &agreeing {
                        let mut acct = self.get_or_create_account(v);
                        acct.balance = acct.balance.saturating_add(per_voter);
                        self.accounts.insert(v.0, acct.clone());
                        self.wal.append(WalOp::SetAccount(*v, acct), self.height());
                    }
                    {
                        let mut acct = self.get_or_create_account(&requester);
                        acct.balance = acct.balance.saturating_add(refund);
                        self.accounts.insert(requester.0, acct.clone());
                        self.wal
                            .append(WalOp::SetAccount(requester, acct), self.height());
                    }
                    let treasury_addr = arc_types::transaction::faucet_pool_address();
                    {
                        let mut acct = self.get_or_create_account(&treasury_addr);
                        acct.balance = acct
                            .balance
                            .saturating_add(treasury + remainder + voter_rem);
                        self.accounts.insert(treasury_addr.0, acct.clone());
                        self.wal
                            .append(WalOp::SetAccount(treasury_addr, acct), self.height());
                    }
                    new_status = TIER1_STATUS_FINALIZED;
                    self.set_storage(
                        &escrow_addr,
                        hash_bytes(b"tier1.final_output_hash"),
                        majority.0.0.to_vec(),
                    );
                } else {
                    let fee = ttx::TIER1_ANTI_SPAM_FEE.min(max_reward);
                    let refund = max_reward - fee;
                    {
                        let mut acct = self.get_or_create_account(&requester);
                        acct.balance = acct.balance.saturating_add(refund);
                        self.accounts.insert(requester.0, acct.clone());
                        self.wal
                            .append(WalOp::SetAccount(requester, acct), self.height());
                    }
                    if fee > 0 {
                        let treasury_addr = arc_types::transaction::faucet_pool_address();
                        let mut acct = self.get_or_create_account(&treasury_addr);
                        acct.balance = acct.balance.saturating_add(fee);
                        self.accounts.insert(treasury_addr.0, acct.clone());
                        self.wal
                            .append(WalOp::SetAccount(treasury_addr, acct), self.height());
                    }
                    new_status = TIER1_STATUS_REFUNDED;
                }

                let mut closed = escrow.clone();
                closed.balance = 0;
                closed.code_hash.0[0] = new_status;
                self.accounts.insert(escrow_addr.0, closed.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, closed), self.height());

                self.tier1_pending.remove(&body.request_id);

                Ok(gas.consumed)
            }
            TxBody::FaucetClaim(body) => {
                if self.active_protocol_version().major == 3 {
                    self.validate_v3_faucet_admission(tx, body)?;
                }
                if !self.is_validator(&tx.from) {
                    return Err(StateError::ExecutionError(format!(
                        "faucet claim: signer {} is not an active validator",
                        tx.from.to_hex()
                    )));
                }
                if body.amount == 0 || body.amount > arc_types::transaction::FAUCET_CLAIM_MAX {
                    return Err(StateError::ExecutionError(format!(
                        "faucet claim: amount {} outside [1, {}]",
                        body.amount,
                        arc_types::transaction::FAUCET_CLAIM_MAX
                    )));
                }

                let pool_addr = arc_types::transaction::faucet_pool_address();
                let protocol_v3 = self.active_protocol_version().major == 3;
                let marker_addr = if protocol_v3 {
                    arc_types::transaction::FaucetClaimBody::v3_marker_address(&body.recipient)
                } else {
                    arc_types::transaction::FaucetClaimBody::marker_address(&body.recipient)
                };
                let recipient_is_existing_faucet_marker =
                    self.get_account(&body.recipient).is_some_and(|account| {
                        account.balance == 0
                            && account.nonce == 1
                            && account.code_hash != Hash256::ZERO
                            && arc_types::transaction::FaucetClaimBody::marker_address(
                                &account.code_hash,
                            ) == body.recipient
                    });
                let recipient_is_reserved = if protocol_v3 {
                    is_reserved_system_account(&body.recipient)
                } else {
                    is_legacy_reserved_system_account(&body.recipient)
                };
                if recipient_is_reserved
                    || marker_addr == pool_addr
                    || marker_addr == body.recipient
                    || recipient_is_existing_faucet_marker
                {
                    return Err(StateError::ExecutionError(
                        "faucet claim: system account address collision".to_string(),
                    ));
                }
                if self.accounts.contains_key(&marker_addr.0) {
                    return Err(StateError::ExecutionError(format!(
                        "faucet claim: recipient {} has already claimed",
                        body.recipient
                    )));
                }

                let mut pool = self.get_account(&pool_addr).ok_or_else(|| {
                    StateError::ExecutionError(
                        "faucet claim: system faucet pool account is not prefunded".into(),
                    )
                })?;
                if pool.balance < body.amount {
                    return Err(StateError::InsufficientBalance {
                        have: pool.balance,
                        need: body.amount,
                    });
                }
                let mut recipient = self
                    .get_account(&body.recipient)
                    .unwrap_or_else(|| Account::new(body.recipient, 0));
                recipient.balance =
                    recipient.balance.checked_add(body.amount).ok_or_else(|| {
                        StateError::ExecutionError(
                            "faucet claim: recipient balance overflow".into(),
                        )
                    })?;
                pool.balance -= body.amount;
                let mut marker = Account::new(marker_addr, 0);
                marker.nonce = 1;
                marker.code_hash = body.recipient;
                marker.storage_root = hash_bytes(&body.amount.to_be_bytes());

                self.accounts.insert(pool_addr.0, pool.clone());
                self.accounts.insert(body.recipient.0, recipient.clone());
                self.accounts.insert(marker_addr.0, marker.clone());
                if self.use_jmt {
                    let mut jmt = self.jmt.lock();
                    for (address, account) in [
                        (pool_addr, &pool),
                        (body.recipient, &recipient),
                        (marker_addr, &marker),
                    ] {
                        let value = hash_bytes(&bincode::serialize(account).unwrap_or_default());
                        jmt.update_leaf(address.0, value);
                    }
                }
                self.wal
                    .append(WalOp::SetAccount(pool_addr, pool), self.height());
                self.wal
                    .append(WalOp::SetAccount(body.recipient, recipient), self.height());
                self.wal
                    .append(WalOp::SetAccount(marker_addr, marker), self.height());

                Ok(gas.consumed)
            }
            TxBody::NativeInferenceRequest(_)
            | TxBody::NativeInferenceFinalize(_)
            | TxBody::NativeInferenceRefund(_) => Err(StateError::ExecutionError(
                "native inference requires the canonical block publisher".into(),
            )),
        }
    }

    pub fn execute_tx_pub(&self, tx: &Transaction) -> Result<u64, StateError> {
        if self.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "protocol-4 requires canonical sequential block execution".into(),
            ));
        }
        if self.active_protocol_version().major == 3 {
            return Err(StateError::ExecutionError(
                "pipeline/direct transaction execution is unavailable in recovery protocol v3"
                    .to_string(),
            ));
        }
        self.execute_tx(tx)
    }

    pub fn mark_tx_accounts_dirty_pub(&self, tx: &Transaction) {
        self.mark_tx_accounts_dirty(tx);
    }

    pub fn commit_executed_block(
        &self,
        transactions: &[Transaction],
        receipt_success: &[bool],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        if self.native_inference_context.read().is_some() {
            return Err(StateError::ExecutionError(
                "protocol-4 requires canonical sequential block execution".into(),
            ));
        }
        self.require_healthy_wal()?;
        if self.active_protocol_version().major == 3 {
            return Err(StateError::ExecutionError(
                "pre-executed pipeline commit is unavailable in recovery protocol v3".to_string(),
            ));
        }
        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self
            .blocks
            .get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        let tx_hashes: Vec<Hash256> = transactions.iter().map(|tx| tx.hash).collect();
        let receipts: Vec<TxReceipt> = transactions
            .iter()
            .enumerate()
            .map(|(i, tx)| TxReceipt {
                tx_hash: tx.hash,
                block_height: height,
                block_hash: Hash256::ZERO,
                index: i as u32,
                success: receipt_success[i],
                gas_used: 0,
                value_commitment: None,
                inclusion_proof: None,
                logs: vec![],
            })
            .collect();

        self.sweep_matured_bond_releases(height);
        self.apply_due_binding_change_in_block()?;

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();
        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            parent_hash: parent,
            tx_root,
            state_root,
            proof_hash: Hash256::ZERO,
            tx_count: transactions.len() as u32,
            producer,
            protocol_version: self.active_protocol_version(),
            state_diff: None,
        };

        let block = Block::new(header, tx_hashes);

        let mut receipts = receipts;
        for (i, receipt) in receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
            self.full_transactions.insert(tx.hash.0, tx.clone());
        }

        self.blocks.insert(height, Box::new(block.clone()));
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        if !self.archive_mode && height % 100 == 0 {
            self.prune_old_state(1000);
            self.prune_old_receipts(1000);
        }

        Ok((block, receipts))
    }

    pub fn sweep_matured_bond_releases(&self, current_height: u64) -> usize {
        let due: Vec<[u8; 32]> = {
            let mut q = self.pending_bond_releases.lock();
            let mut picked: Vec<[u8; 32]> = Vec::new();
            let mut empty_heights: Vec<u64> = Vec::new();
            for (&h, bucket) in q.range_mut(..=current_height) {
                if picked.len() >= MAX_BOND_RELEASES_PER_BLOCK {
                    break;
                }
                let remaining = MAX_BOND_RELEASES_PER_BLOCK - picked.len();
                let take = remaining.min(bucket.len());
                picked.extend(bucket.drain(..take));
                if bucket.is_empty() {
                    empty_heights.push(h);
                }
            }
            for h in empty_heights {
                q.remove(&h);
            }
            picked
        };

        let mut refunded = 0usize;
        for escrow_key in due {
            let escrow_addr = Hash256(escrow_key);
            let escrow = match self.accounts.get(&escrow_key) {
                Some(e) => e.clone(),
                None => continue,
            };
            if escrow.balance == 0 {
                continue;
            }
            if escrow.storage_root.0[..8] != ATTEST_ESCROW_MAGIC {
                continue;
            }
            if escrow.storage_root.0[8] != ATTEST_STATUS_OPEN {
                continue;
            }
            if current_height < escrow.nonce {
                let mut q = self.pending_bond_releases.lock();
                let bucket = q.entry(escrow.nonce).or_default();
                if let Err(pos) = bucket.binary_search(&escrow_key) {
                    bucket.insert(pos, escrow_key);
                }
                continue;
            }

            let attester = escrow.code_hash;
            let amount = escrow.balance;

            {
                let mut acct = self
                    .accounts
                    .entry(attester.0)
                    .or_insert_with(|| Account::new(attester, 0));
                acct.balance = acct.balance.saturating_add(amount);
                if self.use_jmt {
                    let h = hash_bytes(&bincode::serialize(acct.value()).unwrap_or_default());
                    self.jmt.lock().update_leaf(attester.0, h);
                }
                if self.wal.is_active() {
                    let snap = acct.clone();
                    drop(acct);
                    self.wal
                        .append(WalOp::SetAccount(attester, snap), current_height);
                }
            }
            self.dirty_accounts.insert(attester.0);

            {
                let mut esc = self
                    .accounts
                    .entry(escrow_key)
                    .or_insert_with(|| Account::new(escrow_addr, 0));
                esc.balance = 0;
                esc.storage_root = Hash256::ZERO;
                if self.use_jmt {
                    let h = hash_bytes(&bincode::serialize(esc.value()).unwrap_or_default());
                    self.jmt.lock().update_leaf(escrow_key, h);
                }
                if self.wal.is_active() {
                    let snap = esc.clone();
                    drop(esc);
                    self.wal
                        .append(WalOp::SetAccount(escrow_addr, snap), current_height);
                }
            }
            self.dirty_accounts.insert(escrow_key);

            refunded += 1;
        }
        refunded
    }

    pub fn rebuild_pending_bond_releases(&self) -> usize {
        let mut q = self.pending_bond_releases.lock();
        q.clear();
        let mut rebuilt = 0usize;
        for entry in self.accounts.iter() {
            let acct = entry.value();
            if acct.balance == 0 {
                continue;
            }
            if acct.storage_root.0[..8] != ATTEST_ESCROW_MAGIC {
                continue;
            }
            if acct.storage_root.0[8] != ATTEST_STATUS_OPEN {
                continue;
            }
            let release_height = acct.nonce;
            let escrow_key = *entry.key();
            let bucket = q.entry(release_height).or_default();
            if let Err(pos) = bucket.binary_search(&escrow_key) {
                bucket.insert(pos, escrow_key);
            }
            rebuilt += 1;
        }
        rebuilt
    }

    #[cfg(test)]
    fn pending_bond_release_count(&self) -> usize {
        self.pending_bond_releases
            .lock()
            .values()
            .map(|v| v.len())
            .sum()
    }

    pub fn enable_archive_mode(&mut self) -> bool {
        self.archive_mode = true;
        self.archive_history_boundary
            .store(u64::MAX, Ordering::Release);
        self.archive_history_verified_through
            .store(u64::MAX, Ordering::Release);

        if self.recovery_context().is_none() {
            return false;
        }
        let current = self.height();
        let boundary = (0..=current).find(|height| {
            self.get_block(*height)
                .is_some_and(|block| block.header.protocol_version.major >= 3)
        });
        let Some(boundary) = boundary else {
            return false;
        };
        if !self.verify_archive_history_range(boundary, current) {
            return false;
        }
        self.archive_history_boundary
            .store(boundary, Ordering::Release);
        self.archive_history_verified_through
            .store(current, Ordering::Release);
        true
    }

    pub fn archive_reward_history_complete_since_v3_recovery(&self) -> bool {
        if !self.archive_mode || self.recovery_context().is_none() {
            return false;
        }
        let boundary = self.archive_history_boundary.load(Ordering::Acquire);
        let verified = self
            .archive_history_verified_through
            .load(Ordering::Acquire);
        if boundary == u64::MAX || verified == u64::MAX || verified < boundary {
            return false;
        }
        let current = self.height();
        if current < verified {
            return false;
        }
        if current == verified {
            return true;
        }
        if !self.verify_archive_history_range(verified.saturating_add(1), current) {
            return false;
        }
        self.archive_history_verified_through
            .store(current, Ordering::Release);
        true
    }

    fn verify_archive_history_range(&self, from: u64, through: u64) -> bool {
        if from > through {
            return true;
        }
        let mut recovery_domains = HashSet::new();
        if let Some(domain) = self.transaction_domain_hash() {
            recovery_domains.insert(domain);
        }
        for transaction in self.full_transactions.iter() {
            if let TxBody::CommunityInferenceReward(body) = &transaction.body
                && body.transaction_domain != Hash256::ZERO
            {
                recovery_domains.insert(body.transaction_domain);
            }
        }
        for height in from..=through {
            let Some(block) = self.get_block(height) else {
                return false;
            };
            if block.header.height != height
                || block.hash != Block::compute_hash(&block.header)
                || block.header.tx_count as usize != block.tx_hashes.len()
                || MerkleTree::from_leaves(block.tx_hashes.clone()).root() != block.header.tx_root
            {
                return false;
            }
            for (index, tx_hash) in block.tx_hashes.iter().enumerate() {
                let Ok(index_u32) = u32::try_from(index) else {
                    return false;
                };
                if self
                    .tx_index
                    .get(&tx_hash.0)
                    .is_none_or(|location| *location.value() != (height, index_u32))
                {
                    return false;
                }
                let Some(transaction) = self.full_transactions.get(&tx_hash.0) else {
                    return false;
                };
                if transaction.hash != *tx_hash
                    || transaction.tx_type != transaction.body.tx_type()
                    || !recovery_domains
                        .iter()
                        .any(|domain| transaction.compute_hash_in_domain(domain) == *tx_hash)
                {
                    return false;
                }
                if self.receipts.get(&tx_hash.0).is_none_or(|receipt| {
                    receipt.tx_hash != *tx_hash
                        || receipt.block_height != height
                        || receipt.block_hash != block.hash
                        || receipt.index != index_u32
                }) {
                    return false;
                }
            }
        }
        true
    }

    pub fn prune_old_state(&self, keep_versions: u64) {
        if self.archive_mode {
            return;
        }
        let mut jmt = self.jmt.lock();
        let current = jmt.version();
        if current > keep_versions {
            jmt.prune_versions_before(current - keep_versions);
        }
    }

    pub fn prune_old_receipts(&self, keep_blocks: u64) {
        if self.archive_mode {
            return;
        }
        let current = self.height();
        if current <= keep_blocks {
            return;
        }
        let cutoff = current - keep_blocks;

        self.receipts
            .retain(|_, receipt| receipt.block_height > cutoff);

        self.tx_index
            .retain(|_, &mut (block_height, _)| block_height > cutoff);

        self.full_transactions
            .retain(|hash, _| self.tx_index.contains_key(hash));
    }

    pub fn collect_rent(&self, config: &StateRentConfig) -> (u64, u64) {
        let rent = config.rent_per_epoch();
        if rent == 0 {
            return (0, 0);
        }

        let mut total_collected: u64 = 0;
        let mut dormant_count: u64 = 0;

        let keys: Vec<[u8; 32]> = self.accounts.iter().map(|e| *e.key()).collect();
        for key in keys {
            if let Some(mut entry) = self.accounts.get_mut(&key) {
                let account = entry.value_mut();

                if config.is_dormant(account.balance) {
                    dormant_count += 1;
                    continue;
                }

                let deducted = account.balance.min(rent);
                account.balance = account.balance.saturating_sub(rent);
                total_collected += deducted;

                if config.is_dormant(account.balance) {
                    dormant_count += 1;
                }

                self.dirty_accounts.insert(key);
            }
        }

        (total_collected, dormant_count)
    }

    pub fn get_receipt(&self, tx_hash: &[u8; 32]) -> Option<TxReceipt> {
        let _publication = self.native_inference_publication.read();
        self.receipts.get(tx_hash).map(|r| r.clone())
    }

    pub fn get_tx_location(&self, tx_hash: &[u8; 32]) -> Option<(u64, u32)> {
        let _publication = self.native_inference_publication.read();
        self.tx_index.get(tx_hash).map(|r| *r)
    }

    pub fn get_account_txs(&self, address: &[u8; 32]) -> Vec<Hash256> {
        self.account_txs
            .get(address)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub fn get_block_range(&self, from: u64, to: u64, limit: usize) -> Vec<Block> {
        let mut blocks = Vec::new();
        let end = to.min(from.saturating_add(limit as u64).saturating_sub(1));
        for h in from..=end {
            if let Some(block) = self.blocks.get(&h) {
                blocks.push(Block::clone(&block));
            }
        }
        blocks
    }

    fn persist_restart_artifacts(
        &self,
        transactions: &[Transaction],
        receipts: &[TxReceipt],
        height: u64,
    ) {
        for (transaction, receipt) in transactions.iter().zip(receipts) {
            self.wal
                .append(WalOp::SetReceipt(transaction.hash, receipt.clone()), height);
            self.wal.append(
                WalOp::SetFullTransaction(transaction.hash, Box::new(transaction.clone())),
                height,
            );
        }
        let mut validators: Vec<_> = self
            .validators
            .iter()
            .map(|entry| (Hash256(*entry.key()), *entry.value()))
            .collect();
        validators.sort_by_key(|entry| entry.0.0);
        self.wal.append(
            WalOp::SetValidatorState(validators, self.staking_pool.load(Ordering::Acquire)),
            height,
        );
        if let Some(context) = self.recovery_context() {
            self.wal.append(
                WalOp::SetRecoveryContext(context, self.community_rewards_v1_activation_height()),
                height,
            );
        }
    }

    fn index_account_tx(&self, tx: &Transaction) {
        const MAX_TX_HISTORY: usize = 10_000;
        {
            let mut entry = self.account_txs.entry(tx.from.0).or_default();
            if entry.len() >= MAX_TX_HISTORY {
                let drain_count = MAX_TX_HISTORY / 10;
                entry.drain(..drain_count);
            }
            entry.push(tx.hash);
        }

        match &tx.body {
            TxBody::Transfer(body) => {
                self.account_txs.entry(body.to.0).or_default().push(tx.hash);
                if self.active_protocol_version().major == 3 {
                    self.account_txs
                        .entry(v3_fee_treasury_address().0)
                        .or_default()
                        .push(tx.hash);
                }
            }
            TxBody::Settle(body) => {
                self.account_txs
                    .entry(body.agent_id.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::Swap(body) => {
                self.account_txs
                    .entry(body.counterparty.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::Escrow(body) => {
                self.account_txs
                    .entry(body.beneficiary.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::Stake(body) => {
                self.account_txs
                    .entry(body.validator.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::WasmCall(body) => {
                self.account_txs
                    .entry(body.contract.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::MultiSig(_) | TxBody::DeployContract(_) | TxBody::RegisterAgent(_) => {}
            TxBody::JoinValidator(_)
            | TxBody::LeaveValidator
            | TxBody::ClaimRewards
            | TxBody::UpdateStake(_) => {}
            TxBody::Governance(_) => {}
            TxBody::BridgeLock(_) => {
                let escrow_addr = hash_bytes(b"ARC-bridge-escrow");
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::BridgeMint(body) => {
                self.account_txs
                    .entry(body.recipient.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::BatchSettle(body) => {
                for entry in &body.entries {
                    self.account_txs
                        .entry(entry.agent_id.0)
                        .or_default()
                        .push(tx.hash);
                }
            }
            TxBody::ChannelOpen(_) | TxBody::ChannelClose(_) | TxBody::ChannelDispute(_) => {}
            TxBody::ShardProof(_) => {}
            TxBody::InferenceAttestation(_) => {
                let escrow_addr = hash_bytes(&[b"arc-inference", tx.hash.as_ref()].concat());
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::CommunityInferenceReward(body) => {
                let protocol_v3 = self.active_protocol_version().major == 3;
                let job_marker = if protocol_v3 {
                    arc_types::transaction::CommunityInferenceRewardBody::v3_marker_address(
                        &body.chain_domain,
                        &body.job_id,
                    )
                } else {
                    arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                        &body.chain_domain,
                        &body.job_id,
                    )
                };
                let certificate_marker = if protocol_v3 {
                    arc_types::transaction::CommunityInferenceRewardBody::v3_certificate_marker_address(
                        &body.chain_domain,
                        &body.worker,
                        &body.worker_certificate.attestation_hash,
                    )
                } else {
                    arc_types::transaction::CommunityInferenceRewardBody::certificate_marker_address(
                        &body.chain_domain,
                        &body.worker,
                        &body.worker_certificate.attestation_hash,
                    )
                };
                self.account_txs
                    .entry(body.worker.0)
                    .or_default()
                    .push(tx.hash);
                self.account_txs
                    .entry(job_marker.0)
                    .or_default()
                    .push(tx.hash);
                self.account_txs
                    .entry(certificate_marker.0)
                    .or_default()
                    .push(tx.hash);
                let recovery_marker = if protocol_v3 {
                    arc_types::transaction::CommunityInferenceRewardBody::v3_recovery_probe_marker_address(
                        &body.chain_domain,
                        &body.assignment_epoch,
                    )
                } else {
                    arc_types::transaction::CommunityInferenceRewardBody::recovery_probe_marker_address(
                        &body.chain_domain,
                        &body.assignment_epoch,
                    )
                };
                if let Some(marker) = recovery_marker {
                    self.account_txs.entry(marker.0).or_default().push(tx.hash);
                }
                self.account_txs
                    .entry(arc_types::transaction::inference_reward_treasury_address().0)
                    .or_default()
                    .push(tx.hash);
                if protocol_v3 {
                    let epoch = self.height() / V3_COMMUNITY_REWARD_EPOCH_BLOCKS;
                    for address in [
                        community_reward_block_budget_address(self.height()),
                        community_reward_epoch_budget_address(epoch),
                        community_reward_worker_budget_address(epoch, &body.worker),
                        community_reward_coordinator_budget_address(epoch, &body.coordinator),
                    ] {
                        self.account_txs.entry(address.0).or_default().push(tx.hash);
                    }
                }
            }
            TxBody::InferenceChallenge(body) => {
                let escrow_addr =
                    hash_bytes(&[b"arc-inference", body.attestation_hash.as_ref()].concat());
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::InferenceRegister(_) => {}
            TxBody::InferenceEscrowOpen(body) => {
                let escrow_addr = InferenceEscrowOpenBody::escrow_address(&body.request_id);
                self.account_txs
                    .entry(escrow_addr)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::InferenceEscrowRelease(body) => {
                let escrow_addr = InferenceEscrowOpenBody::escrow_address(&body.request_id);
                self.account_txs
                    .entry(escrow_addr)
                    .or_default()
                    .push(tx.hash);
                self.account_txs
                    .entry(body.proposer.0)
                    .or_default()
                    .push(tx.hash);
                for r in &body.replicas {
                    self.account_txs.entry(r.0).or_default().push(tx.hash);
                }
                self.account_txs
                    .entry(body.observer_pool.0)
                    .or_default()
                    .push(tx.hash);
                self.account_txs
                    .entry(body.treasury.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::InferenceEscrowRefund(body) => {
                let escrow_addr = InferenceEscrowOpenBody::escrow_address(&body.request_id);
                self.account_txs
                    .entry(escrow_addr)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::ModelRegistration(body) => {
                let reg_addr = ModelRegistrationBody::registry_account(&body.model_id);
                self.account_txs.entry(reg_addr).or_default().push(tx.hash);
            }
            TxBody::ModelRequest(body) => {
                let req_addr = ModelRequestBody::request_account(&body.request_id);
                self.account_txs.entry(req_addr).or_default().push(tx.hash);
            }
            TxBody::ShardCoverageClaim(body) => {
                let claim_addr =
                    ShardCoverageClaimBody::claim_account(&body.model_id, &body.node_pubkey);
                self.account_txs
                    .entry(claim_addr)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::CapacityAdvertisement(body) => {
                let cap_addr = CapacityAdvertisementBody::capacity_account(&body.node_pubkey);
                self.account_txs.entry(cap_addr).or_default().push(tx.hash);
            }
            TxBody::ShardAssignmentProposal(_) => {}
            TxBody::FaucetClaim(body) => {
                self.account_txs
                    .entry(body.recipient.0)
                    .or_default()
                    .push(tx.hash);
                let pool_addr = arc_types::transaction::faucet_pool_address();
                self.account_txs
                    .entry(pool_addr.0)
                    .or_default()
                    .push(tx.hash);
                let marker_addr = if self.active_protocol_version().major == 3 {
                    arc_types::transaction::FaucetClaimBody::v3_marker_address(&body.recipient)
                } else {
                    arc_types::transaction::FaucetClaimBody::marker_address(&body.recipient)
                };
                self.account_txs
                    .entry(marker_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::InferenceRequest(body) => {
                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::InferenceVote(body) => {
                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::InferenceFinalize(body) => {
                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::NativeInferenceRequest(body) => {
                let escrow_addr =
                    inference_contract_state::escrow_address(body.request.job.request_id());
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::NativeInferenceFinalize(body) => {
                self.index_native_inference_payees(Hash256(body.request_id), tx);
                let escrow_addr =
                    inference_contract_state::escrow_address(Hash256(body.request_id));
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::NativeInferenceRefund(body) => {
                self.index_native_inference_payees(Hash256(body.request_id), tx);
                let escrow_addr =
                    inference_contract_state::escrow_address(Hash256(body.request_id));
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
        }
    }

    fn mark_tx_accounts_dirty(&self, tx: &Transaction) {
        self.dirty_accounts.insert(tx.from.0);
        match &tx.body {
            TxBody::Transfer(body) => {
                self.dirty_accounts.insert(body.to.0);
                if self.active_protocol_version().major == 3 {
                    self.dirty_accounts.insert(v3_fee_treasury_address().0);
                }
            }
            TxBody::Settle(body) => {
                self.dirty_accounts.insert(body.agent_id.0);
            }
            TxBody::Swap(body) => {
                self.dirty_accounts.insert(body.counterparty.0);
            }
            TxBody::Stake(body) => {
                self.dirty_accounts.insert(body.validator.0);
            }
            TxBody::WasmCall(body) => {
                self.dirty_accounts.insert(body.contract.0);
            }
            TxBody::Escrow(body) => {
                self.dirty_accounts.insert(body.beneficiary.0);
            }
            TxBody::DeployContract(_) => {
                let contract_addr = compute_contract_address(&tx.from, tx.nonce);
                self.dirty_accounts.insert(contract_addr.0);
            }
            TxBody::RegisterAgent(_) | TxBody::MultiSig(_) => {}
            TxBody::JoinValidator(_)
            | TxBody::LeaveValidator
            | TxBody::ClaimRewards
            | TxBody::UpdateStake(_) => {}
            TxBody::Governance(_) => {}
            TxBody::BridgeLock(_) => {
                let escrow_addr = hash_bytes(b"ARC-bridge-escrow");
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::BridgeMint(body) => {
                self.dirty_accounts.insert(body.recipient.0);
            }
            TxBody::BatchSettle(body) => {
                for entry in &body.entries {
                    self.dirty_accounts.insert(entry.agent_id.0);
                }
            }
            TxBody::ChannelOpen(body) => {
                self.dirty_accounts.insert(body.counterparty.0);
                let escrow_addr = hash_bytes(&[b"arc-channel", body.channel_id.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::ChannelClose(body) => {
                let escrow_addr = hash_bytes(&[b"arc-channel", body.channel_id.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
                if let Some(escrow) = self.accounts.get(&escrow_addr.0)
                    && escrow.storage_root != Hash256::ZERO
                {
                    self.dirty_accounts.insert(escrow.storage_root.0);
                }
            }
            TxBody::ChannelDispute(body) => {
                let escrow_addr = hash_bytes(&[b"arc-channel", body.channel_id.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::ShardProof(body) => {
                let mut proof_input = Vec::new();
                proof_input.extend_from_slice(b"arc-shard-proof");
                proof_input.extend_from_slice(&body.shard_id.to_le_bytes());
                proof_input.extend_from_slice(&body.block_height.to_le_bytes());
                let proof_key = hash_bytes(&proof_input);
                self.dirty_accounts.insert(proof_key.0);
            }
            TxBody::InferenceAttestation(_) => {
                let escrow_addr = hash_bytes(&[b"arc-inference", tx.hash.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::CommunityInferenceReward(body) => {
                let protocol_v3 = self.active_protocol_version().major == 3;
                self.dirty_accounts.insert(body.worker.0);
                self.dirty_accounts
                    .insert(arc_types::transaction::inference_reward_treasury_address().0);
                let job_marker = if protocol_v3 {
                    arc_types::transaction::CommunityInferenceRewardBody::v3_marker_address(
                        &body.chain_domain,
                        &body.job_id,
                    )
                } else {
                    arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                        &body.chain_domain,
                        &body.job_id,
                    )
                };
                self.dirty_accounts.insert(job_marker.0);
                let certificate_marker = if protocol_v3 {
                    arc_types::transaction::CommunityInferenceRewardBody::v3_certificate_marker_address(
                        &body.chain_domain,
                        &body.worker,
                        &body.worker_certificate.attestation_hash,
                    )
                } else {
                    arc_types::transaction::CommunityInferenceRewardBody::certificate_marker_address(
                        &body.chain_domain,
                        &body.worker,
                        &body.worker_certificate.attestation_hash,
                    )
                };
                self.dirty_accounts.insert(certificate_marker.0);
                let recovery_marker = if protocol_v3 {
                    arc_types::transaction::CommunityInferenceRewardBody::v3_recovery_probe_marker_address(
                        &body.chain_domain,
                        &body.assignment_epoch,
                    )
                } else {
                    arc_types::transaction::CommunityInferenceRewardBody::recovery_probe_marker_address(
                        &body.chain_domain,
                        &body.assignment_epoch,
                    )
                };
                if let Some(marker) = recovery_marker {
                    self.dirty_accounts.insert(marker.0);
                }
                if protocol_v3 {
                    let epoch = self.height() / V3_COMMUNITY_REWARD_EPOCH_BLOCKS;
                    for address in [
                        community_reward_block_budget_address(self.height()),
                        community_reward_epoch_budget_address(epoch),
                        community_reward_worker_budget_address(epoch, &body.worker),
                        community_reward_coordinator_budget_address(epoch, &body.coordinator),
                    ] {
                        self.dirty_accounts.insert(address.0);
                    }
                }
            }
            TxBody::InferenceChallenge(body) => {
                let escrow_addr =
                    hash_bytes(&[b"arc-inference", body.attestation_hash.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::InferenceRegister(_) => {}
            TxBody::InferenceEscrowOpen(body) => {
                let escrow_addr = InferenceEscrowOpenBody::escrow_address(&body.request_id);
                self.dirty_accounts.insert(escrow_addr);
            }
            TxBody::InferenceEscrowRelease(body) => {
                let escrow_addr = InferenceEscrowOpenBody::escrow_address(&body.request_id);
                self.dirty_accounts.insert(escrow_addr);
                self.dirty_accounts.insert(body.proposer.0);
                for r in &body.replicas {
                    self.dirty_accounts.insert(r.0);
                }
                self.dirty_accounts.insert(body.observer_pool.0);
                self.dirty_accounts.insert(body.treasury.0);
            }
            TxBody::InferenceEscrowRefund(body) => {
                let escrow_addr = InferenceEscrowOpenBody::escrow_address(&body.request_id);
                self.dirty_accounts.insert(escrow_addr);
            }
            TxBody::ModelRegistration(body) => {
                let reg_addr = ModelRegistrationBody::registry_account(&body.model_id);
                self.dirty_accounts.insert(reg_addr);
                let treasury = arc_crypto::hash_bytes(b"arc-treasury").0;
                self.dirty_accounts.insert(treasury);
            }
            TxBody::ModelRequest(body) => {
                let req_addr = ModelRequestBody::request_account(&body.request_id);
                self.dirty_accounts.insert(req_addr);
            }
            TxBody::ShardCoverageClaim(body) => {
                let claim_addr =
                    ShardCoverageClaimBody::claim_account(&body.model_id, &body.node_pubkey);
                self.dirty_accounts.insert(claim_addr);
            }
            TxBody::CapacityAdvertisement(body) => {
                let cap_addr = CapacityAdvertisementBody::capacity_account(&body.node_pubkey);
                self.dirty_accounts.insert(cap_addr);
            }
            TxBody::ShardAssignmentProposal(body) => {
                let prop_addr = arc_crypto::hash_bytes(
                    &[b"arc-planner-proposal", body.input_snapshot_hash.0.as_ref()].concat(),
                )
                .0;
                self.dirty_accounts.insert(prop_addr);
            }
            TxBody::FaucetClaim(body) => {
                self.dirty_accounts.insert(body.recipient.0);
                let pool_addr = arc_types::transaction::faucet_pool_address();
                self.dirty_accounts.insert(pool_addr.0);
                let marker = if self.active_protocol_version().major == 3 {
                    arc_types::transaction::FaucetClaimBody::v3_marker_address(&body.recipient)
                } else {
                    arc_types::transaction::FaucetClaimBody::marker_address(&body.recipient)
                };
                self.dirty_accounts.insert(marker.0);
            }
            TxBody::InferenceRequest(body) => {
                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::InferenceVote(_body) => {}
            TxBody::InferenceFinalize(body) => {
                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
                let treasury_addr = arc_types::transaction::faucet_pool_address();
                self.dirty_accounts.insert(treasury_addr.0);
            }
            TxBody::NativeInferenceRequest(body) => {
                let escrow_addr =
                    inference_contract_state::escrow_address(body.request.job.request_id());
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::NativeInferenceFinalize(body) => {
                let escrow_addr =
                    inference_contract_state::escrow_address(Hash256(body.request_id));
                self.dirty_accounts.insert(escrow_addr.0);
                self.dirty_accounts
                    .insert(arc_types::transaction::faucet_pool_address().0);
            }
            TxBody::NativeInferenceRefund(body) => {
                let escrow_addr =
                    inference_contract_state::escrow_address(Hash256(body.request_id));
                self.dirty_accounts.insert(escrow_addr.0);
                self.dirty_accounts
                    .insert(arc_types::transaction::faucet_pool_address().0);
            }
        }
    }

    fn compute_state_root(&self) -> Hash256 {
        if let Some(context) = self.recovery_context() {
            return self.compute_recovery_state_root(&context);
        }

        if self.use_jmt {
            return self.compute_state_root_jmt();
        }

        let mut tree = self.incremental_merkle.lock();
        let dirty_keys: Vec<[u8; 32]> = {
            let keys: Vec<[u8; 32]> = self.dirty_accounts.iter().map(|k| *k).collect();
            for k in &keys {
                self.dirty_accounts.remove(k);
            }
            keys
        };

        let cold_start = tree.is_empty() && !self.accounts.is_empty();

        if cold_start {
            let pairs: Vec<([u8; 32], Hash256)> = self
                .accounts
                .iter()
                .map(|entry| {
                    let bytes = bincode::serialize(entry.value()).expect("serializable");
                    (*entry.key(), hash_bytes(&bytes))
                })
                .collect();
            *tree = IncrementalMerkle::from_keyed_leaves(pairs);
            return tree.root();
        }

        if dirty_keys.is_empty() {
            return tree.root();
        }

        let mut changed_indices: Vec<usize> = Vec::with_capacity(dirty_keys.len());
        let mut structure_changed = false;

        for key in &dirty_keys {
            if let Some(account) = self.accounts.get(key) {
                let bytes = bincode::serialize(account.value()).expect("serializable");
                let h = hash_bytes(&bytes);
                let (idx, is_new) = tree.update(*key, h);
                changed_indices.push(idx);
                if is_new {
                    structure_changed = true;
                }
            } else if tree.remove(key) {
                structure_changed = true;
            }
        }

        if structure_changed {
            tree.rebuild();
        } else {
            tree.recompute_paths(&changed_indices);
        }

        tree.root()
    }

    fn compute_state_root_jmt(&self) -> Hash256 {
        let mut jmt = self.jmt.lock();
        let dirty_keys: Vec<[u8; 32]> = {
            let keys: Vec<[u8; 32]> = self.dirty_accounts.iter().map(|k| *k).collect();
            for k in &keys {
                self.dirty_accounts.remove(k);
            }
            keys
        };

        if dirty_keys.is_empty() && !jmt.is_empty() {
            return jmt.root_hash();
        }

        if jmt.is_empty() && !self.accounts.is_empty() {
            for entry in self.accounts.iter() {
                let addr = *entry.key();
                let account = entry.value();
                let hash = hash_bytes(&bincode::serialize(account).unwrap_or_default());
                jmt.update_leaf(addr, hash);
            }
            return jmt.root_hash();
        }

        for key in &dirty_keys {
            if let Some(account) = self.accounts.get(key) {
                let hash = hash_bytes(&bincode::serialize(account.value()).unwrap_or_default());
                jmt.update_leaf(*key, hash);
            } else {
                jmt.remove_leaf(key);
            }
        }

        jmt.root_hash()
    }

    pub fn enable_jmt(&mut self) {
        if self.native_inference_context.read().is_some() {
            tracing::warn!("JMT root switch ignored for the activated native protocol-4 candidate");
            return;
        }
        self.use_jmt = true;
        let mut jmt = self.jmt.lock();
        for entry in self.accounts.iter() {
            let addr = *entry.key();
            let account = entry.value();
            let hash = hash_bytes(&bincode::serialize(account).unwrap_or_default());
            jmt.update_leaf(addr, hash);
        }
    }

    pub fn jmt_root(&self) -> Hash256 {
        let mut jmt = self.jmt.lock();
        jmt.root_hash()
    }

    pub fn is_jmt_enabled(&self) -> bool {
        self.use_jmt
    }

    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    pub fn get_staked_accounts(&self) -> Vec<(Address, Account)> {
        self.accounts
            .iter()
            .filter(|entry| entry.value().staked_balance > 0)
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect()
    }

    pub fn block_count(&self) -> u64 {
        *self.height.read()
    }

    pub fn wal_failure(&self) -> Option<WalError> {
        self.wal.failure()
    }

    pub fn persistence_dir(&self) -> Option<std::path::PathBuf> {
        self.wal.directory().map(Path::to_path_buf)
    }

    pub fn try_sync_wal(&self) -> Result<(), StateError> {
        self.durable_wal_barrier()
    }

    pub fn sync_wal(&self) {
        if let Err(error) = self.try_sync_wal() {
            tracing::error!(error = %error, "WAL shutdown sync failed");
        }
    }

    pub fn export_durable_snapshot(&self) -> snapshot::SnapshotPayload {
        let height = self.height();
        let window = self.snapshot_history_window.load(Ordering::Acquire).max(1);
        let first = height.saturating_sub(window - 1);
        let blocks: Vec<(u64, Block)> = (first..=height)
            .filter_map(|h| self.blocks.get(&h).map(|b| (h, Block::clone(&b))))
            .collect();
        let in_window: Vec<[u8; 32]> = blocks
            .iter()
            .flat_map(|(_, block)| block.tx_hashes.iter().map(|hash| hash.0))
            .collect();
        let mut payload = snapshot::SnapshotPayload {
            height,
            accounts: self
                .accounts
                .iter()
                .map(|e| (Hash256(*e.key()), e.value().clone()))
                .collect(),
            storage: self
                .storage
                .iter()
                .map(|e| {
                    (
                        Hash256(*e.key()),
                        e.value()
                            .iter()
                            .map(|i| (*i.key(), i.value().clone()))
                            .collect(),
                    )
                })
                .collect(),
            contracts: self
                .contracts
                .iter()
                .map(|e| (Hash256(*e.key()), e.value().clone()))
                .collect(),
            identities: self
                .identities
                .iter()
                .map(|e| (Hash256(*e.key()), e.value().clone()))
                .collect(),
            receipts: in_window
                .iter()
                .filter_map(|hash| {
                    self.receipts
                        .get(hash)
                        .map(|r| (Hash256(*hash), r.value().clone()))
                })
                .collect(),
            full_transactions: in_window
                .iter()
                .filter_map(|hash| {
                    self.full_transactions
                        .get(hash)
                        .map(|t| (Hash256(*hash), t.value().clone()))
                })
                .collect(),
            event_logs: (first..=height)
                .filter_map(|h| self.event_logs.get(&h).map(|l| (h, l.value().clone())))
                .collect(),
            blocks,
            validators: self
                .validators
                .iter()
                .map(|e| (Hash256(*e.key()), *e.value()))
                .collect(),
            staking_pool: self.staking_pool.load(Ordering::Acquire),
            recovery_context: self.recovery_context.read().clone(),
            community_rewards_activation_height: self
                .community_rewards_v1_activation_height
                .load(Ordering::Acquire),
            native_inference_pending: self
                .native_inference_pending
                .iter()
                .map(|e| (Hash256(*e.key()), *e.value()))
                .collect(),
        };
        payload.canonicalize();
        payload
    }

    pub fn install_durable_snapshot(&self, payload: &snapshot::SnapshotPayload) {
        for (address, account) in &payload.accounts {
            self.apply_wal_op(&WalOp::SetAccount(*address, account.clone()));
        }
        for (address, entries) in &payload.storage {
            for (key, value) in entries {
                self.apply_wal_op(&WalOp::SetStorage(*address, *key, value.clone()));
            }
        }
        for (address, bytecode) in &payload.contracts {
            self.apply_wal_op(&WalOp::SetContract(*address, bytecode.clone()));
        }
        for (address, identity) in &payload.identities {
            self.apply_wal_op(&WalOp::SetIdentity(*address, identity.clone()));
        }
        self.install_history_window(payload);
        if !payload.validators.is_empty() || payload.staking_pool != 0 {
            self.apply_wal_op(&WalOp::SetValidatorState(
                payload.validators.clone(),
                payload.staking_pool,
            ));
        }
        if let Some(context) = &payload.recovery_context {
            let activation = (payload.community_rewards_activation_height != u64::MAX)
                .then_some(payload.community_rewards_activation_height);
            self.apply_wal_op(&WalOp::SetRecoveryContext(context.clone(), activation));
        }
        for (request_id, admission_height) in &payload.native_inference_pending {
            self.native_inference_pending
                .insert(request_id.0, *admission_height);
        }
        let mut height = self.height.write();
        if payload.height > *height {
            *height = payload.height;
        }
    }

    fn install_history_window(&self, payload: &snapshot::SnapshotPayload) {
        for (height, block) in &payload.blocks {
            self.apply_wal_op(&WalOp::SetBlock(*height, block.clone()));
        }
        for (hash, receipt) in &payload.receipts {
            self.apply_wal_op(&WalOp::SetReceipt(*hash, receipt.clone()));
        }
        for (hash, transaction) in &payload.full_transactions {
            self.apply_wal_op(&WalOp::SetFullTransaction(
                *hash,
                Box::new(transaction.clone()),
            ));
        }
        for (height, logs) in &payload.event_logs {
            self.apply_wal_op(&WalOp::SetEventLogs(*height, logs.clone()));
        }
    }

    pub fn rebase_anchor_round(&self) -> Option<u64> {
        let round = self.rebase_anchor_round.load(Ordering::Acquire);
        (round > 0).then_some(round)
    }

    fn clear_state_for_rebase(&self) {
        self.accounts.clear();
        self.storage.clear();
        self.contracts.clear();
        self.identities.clear();
        self.validators.clear();
        self.native_inference_pending.clear();
        self.dirty_accounts.clear();
        self.staking_pool.store(0, Ordering::Release);
        *self.incremental_merkle.lock() = IncrementalMerkle::new();
    }

    pub fn rebuild_after_rebase(&self) -> Result<(), StateError> {
        let context = self.native_inference_context.read().clone();
        if let Some(context) = context {
            self.rebuild_native_inference_pending(context.commitment()?)?;
        }
        self.tier1_pending.clear();
        self.rebuild_tier1_pending();
        self.rebuild_pending_bond_releases();
        Ok(())
    }

    pub fn rebase_onto_checkpoint(
        &self,
        payload: &snapshot::SnapshotPayload,
        tip: &Block,
        anchor_hash: Hash256,
        anchor_round: u64,
    ) -> Result<(), StateError> {
        let _guard = self.native_inference_execution.lock();
        self.require_healthy_wal()?;
        if !payload.receipts.is_empty()
            || !payload.event_logs.is_empty()
            || !payload.full_transactions.is_empty()
            || !payload.native_inference_pending.is_empty()
            || payload.recovery_context.is_some()
        {
            return Err(StateError::PersistenceError(
                "a rebase carries only planned state and verified blocks".into(),
            ));
        }
        let height = payload.height;
        if tip.header.height != height || Block::compute_hash(&tip.header) != tip.hash {
            return Err(StateError::PersistenceError(
                "checkpoint tip does not describe the checkpoint height".into(),
            ));
        }
        if height <= self.height() {
            return Err(StateError::PersistenceError(format!(
                "a checkpoint at height {height} does not move this node (at {}) forward",
                self.height()
            )));
        }
        checkpoint_adoption::adoption_preconditions(self, payload)?;
        checkpoint_adoption::verify_history_window(&payload.blocks, tip)?;
        let scratch = Self::new();
        scratch.install_durable_snapshot(payload);
        if scratch.get_state_root() != tip.header.state_root {
            return Err(StateError::PersistenceError(
                "checkpoint payload does not reproduce its tip's state root".into(),
            ));
        }
        if let Some(context) = self.native_inference_context.read().clone() {
            let commitment = context.commitment()?;
            *scratch.native_inference_context.write() = Some(context);
            scratch.rebuild_native_inference_pending(commitment)?;
        }
        let record = wal::RebaseRecord {
            height,
            tip: tip.clone(),
            state: payload.clone(),
            anchor_hash,
            anchor_round,
        };
        self.wal
            .append(WalOp::Rebase(Box::new(record.clone())), height);
        self.durable_wal_barrier()?;
        {
            let _publication = self.native_inference_publication.write();
            self.apply_wal_op(&WalOp::Rebase(Box::new(record)));
            let adopted = self.compute_state_root();
            if adopted != tip.header.state_root {
                return Err(StateError::PersistenceError(format!(
                    "adopted checkpoint produced root {adopted}, expected {}",
                    tip.header.state_root
                )));
            }
        }
        self.rebuild_after_rebase()
    }

    fn clear_replayable_state(&self) {
        self.accounts.clear();
        self.storage.clear();
        self.contracts.clear();
        self.identities.clear();
        self.blocks.clear();
        self.receipts.clear();
        self.tx_index.clear();
        self.full_transactions.clear();
        self.event_logs.clear();
        self.validators.clear();
        self.native_inference_pending.clear();
        self.dirty_accounts.clear();
        self.staking_pool.store(0, Ordering::Release);
        self.community_rewards_v1_activation_height
            .store(u64::MAX, Ordering::Release);
        *self.recovery_context.write() = None;
        *self.height.write() = 0;
        *self.incremental_merkle.lock() = IncrementalMerkle::new();
    }

    fn adopt_snapshot_if_trustworthy(&self, dir: &Path) -> Option<snapshot::SnapshotManifestV1> {
        let verified = match snapshot::load(dir) {
            Ok(verified) => verified,
            Err(snapshot::SnapshotError::Absent) => return None,
            Err(error) => {
                tracing::warn!(%error, "Ignoring an unusable state snapshot; replaying the full WAL");
                return None;
            }
        };
        self.install_durable_snapshot(&verified.payload);
        let found = self.get_state_root();
        if found == verified.manifest.identity.state_root {
            tracing::info!(
                height = verified.manifest.identity.height,
                resume_from = verified.manifest.resume_from_sequence,
                "Installed a verified state snapshot; replaying only the WAL tail"
            );
            return Some(verified.manifest);
        }
        tracing::error!(
            expected = %verified.manifest.identity.state_root,
            %found,
            "State snapshot does not reproduce its own state root; discarding it"
        );
        self.clear_replayable_state();
        None
    }

    pub fn publish_durable_snapshot(
        &self,
    ) -> Result<snapshot::SnapshotManifestV1, snapshot::SnapshotError> {
        let dir = self.persistence_dir().ok_or_else(|| {
            snapshot::SnapshotError::Io("state has no persistence directory".into())
        })?;
        self.try_sync_wal()
            .map_err(|error| snapshot::SnapshotError::Io(error.to_string()))?;
        let payload = self.export_durable_snapshot();
        let height = payload.height;
        let block_hash = self
            .get_block(height)
            .map(|block| block.hash)
            .unwrap_or(Hash256::ZERO);
        let sequence = self.wal.sequence();
        snapshot::publish(&dir, payload, self.get_state_root(), sequence, block_hash)
    }

    pub fn export_snapshot(&self) -> Snapshot {
        let accounts: Vec<(Address, Account)> = self
            .accounts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();

        let storage: wal::ContractStorage = self
            .storage
            .iter()
            .map(|entry| {
                let key = Hash256(*entry.key());
                let values: Vec<(Hash256, Vec<u8>)> = entry
                    .value()
                    .iter()
                    .map(|inner| (*inner.key(), inner.value().clone()))
                    .collect();
                (key, values)
            })
            .collect();

        let contracts: Vec<(Address, Vec<u8>)> = self
            .contracts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();

        Snapshot {
            block_height: self.height(),
            state_root: self.get_state_root(),
            wal_sequence: 0,
            accounts,
            storage,
            contracts,
        }
    }

    pub fn import_snapshot(
        &self,
        snapshot: &Snapshot,
        expected_state_root: Hash256,
    ) -> Result<(), StateError> {
        self.accounts.clear();
        self.contracts.clear();
        self.storage.clear();
        *self.incremental_merkle.lock() = IncrementalMerkle::new();
        self.dirty_accounts.clear();

        for (addr, account) in &snapshot.accounts {
            self.accounts.insert(addr.0, account.clone());
        }

        for (addr, storage_entries) in &snapshot.storage {
            let storage_map = DashMap::new();
            for (key, value) in storage_entries {
                storage_map.insert(*key, value.clone());
            }
            self.storage.insert(addr.0, storage_map);
        }

        for (addr, bytecode) in &snapshot.contracts {
            self.contracts.insert(addr.0, bytecode.clone());
        }

        let computed_root = self.compute_state_root();
        if computed_root != expected_state_root {
            self.accounts.clear();
            self.contracts.clear();
            self.storage.clear();
            *self.incremental_merkle.lock() = IncrementalMerkle::new();
            self.dirty_accounts.clear();
            return Err(StateError::PersistenceError(format!(
                "state root mismatch after import: expected {}, computed {}",
                expected_state_root, computed_root
            )));
        }

        *self.height.write() = snapshot.block_height;
        Ok(())
    }

    pub fn export_state_diff(&self, dirty_keys: &[Address]) -> arc_types::StateDiff {
        use arc_types::{AccountChange, StateDiff};

        let changes: Vec<AccountChange> = dirty_keys
            .iter()
            .filter_map(|addr| {
                self.accounts.get(&addr.0).map(|acct| AccountChange {
                    address: *addr,
                    account: acct.clone(),
                })
            })
            .collect();

        let new_root = self.compute_state_root();

        StateDiff { changes, new_root }
    }

    pub fn apply_state_diff(&self, diff: &arc_types::StateDiff) -> Result<Hash256, StateError> {
        use std::collections::HashSet;

        if self.is_persistent()
            && (self.recovery_context().is_some() || self.native_inference_context.read().is_some())
        {
            return Err(StateError::PersistenceError(
                "standalone state-diff persistence is unavailable on recovery-bound state; authenticate and commit the matching canonical block"
                    .into(),
            ));
        }
        self.require_healthy_wal()?;

        let original_root = self.compute_state_root();
        let mut seen = HashSet::with_capacity(diff.changes.len());
        for change in &diff.changes {
            if change.address != change.account.address {
                return Err(StateError::ExecutionError(format!(
                    "state diff account key {} does not match embedded address {}",
                    change.address, change.account.address
                )));
            }
            if !seen.insert(change.address.0) {
                return Err(StateError::ExecutionError(format!(
                    "state diff contains duplicate account {}",
                    change.address
                )));
            }
        }

        let originals: Vec<([u8; 32], Option<Account>)> = diff
            .changes
            .iter()
            .map(|change| {
                (
                    change.address.0,
                    self.accounts.get(&change.address.0).map(|a| a.clone()),
                )
            })
            .collect();

        for change in &diff.changes {
            self.accounts
                .insert(change.address.0, change.account.clone());
            self.dirty_accounts.insert(change.address.0);
        }
        let computed_root = self.compute_state_root();
        if computed_root != diff.new_root {
            for (key, original) in originals {
                match original {
                    Some(account) => {
                        self.accounts.insert(key, account);
                    }
                    None => {
                        self.accounts.remove(&key);
                    }
                }
                self.dirty_accounts.insert(key);
            }
            let restored_root = self.compute_state_root();
            if restored_root != original_root {
                return Err(StateError::PersistenceError(format!(
                    "state diff rollback failed: expected root {}, restored {}",
                    original_root, restored_root
                )));
            }
            return Err(StateError::ExecutionError(format!(
                "state diff root mismatch: declared {}, computed {}",
                diff.new_root, computed_root
            )));
        }

        let height = self.height();
        for change in &diff.changes {
            self.wal.append(
                WalOp::SetAccount(change.address, change.account.clone()),
                height,
            );
        }
        self.wal.append(WalOp::Checkpoint(computed_root), height);
        self.durable_wal_barrier()?;
        Ok(computed_root)
    }

    pub fn verify_state_diff(&self, diff: &arc_types::StateDiff) -> bool {
        self.apply_state_diff(diff).is_ok()
    }

    pub fn drain_dirty_addresses(&self) -> Vec<Address> {
        let keys: Vec<[u8; 32]> = self.dirty_accounts.iter().map(|k| *k).collect();
        keys.into_iter().map(Hash256).collect()
    }

    pub fn register_identity(&self, identity: Identity) {
        self.identities.insert(identity.address.0, identity.clone());
        self.wal.append(
            WalOp::SetIdentity(identity.address, identity),
            self.height(),
        );
    }

    pub fn get_identity(&self, address: &Address) -> Option<Identity> {
        self.identities.get(&address.0).map(|i| i.clone())
    }

    pub fn is_compliant(&self, address: &Address) -> bool {
        match self.get_identity(address) {
            Some(id) => {
                let level_ok = matches!(
                    id.level,
                    IdentityLevel::Verified | IdentityLevel::Institutional
                );
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                level_ok && !id.is_expired(now) && !id.is_sanctioned_country()
            }
            None => false,
        }
    }

    pub fn identity_count(&self) -> usize {
        self.identities.len()
    }

    pub fn get_state_root(&self) -> Hash256 {
        let _publication = self.native_inference_publication.read();
        self.compute_state_root()
    }

    pub fn generate_state_proof(&self, address: &Hash256) -> Result<StateProof, StateError> {
        let account = self
            .get_account(address)
            .ok_or(StateError::AccountNotFound(*address))?;

        let _root = self.compute_state_root();

        let tree = self.incremental_merkle.lock();
        let index = tree
            .get_index(&address.0)
            .ok_or(StateError::AccountNotFound(*address))?;

        let merkle_proof = tree
            .proof(index)
            .ok_or_else(|| StateError::ExecutionError("failed to generate Merkle proof".into()))?;

        let height = self.height();
        let timestamp = self
            .get_block(height)
            .map(|b| b.header.timestamp)
            .unwrap_or(0);

        Ok(StateProof {
            account_address: *address,
            account,
            merkle_proof,
            block_height: height,
            state_root: tree.root(),
            timestamp,
        })
    }

    pub fn generate_header_proof(&self, height: u64) -> Result<HeaderProof, StateError> {
        let block = self
            .get_block(height)
            .ok_or_else(|| StateError::ExecutionError(format!("block {} not found", height)))?;

        Ok(HeaderProof {
            parent_hash: block.header.parent_hash,
            header: block.header,
            validator_signature: None,
        })
    }

    pub fn generate_tx_inclusion_proof(
        &self,
        tx_hash: &Hash256,
    ) -> Result<TxInclusionProof, StateError> {
        let (block_height, tx_index) = self
            .get_tx_location(&tx_hash.0)
            .ok_or_else(|| StateError::ExecutionError("transaction not found".into()))?;

        let block = self.get_block(block_height).ok_or_else(|| {
            StateError::ExecutionError(format!("block {} not found", block_height))
        })?;

        let tree = MerkleTree::from_leaves(block.tx_hashes.clone());
        let merkle_proof = tree.proof(tx_index as usize).ok_or_else(|| {
            StateError::ExecutionError("failed to generate tx Merkle proof".into())
        })?;

        Ok(TxInclusionProof {
            tx_hash: *tx_hash,
            block_height,
            merkle_proof,
            block_tx_root: block.header.tx_root,
        })
    }

    pub fn generate_light_snapshot(&self) -> LightSnapshot {
        let height = self.height();
        let state_root = self.compute_state_root();
        let account_count = self.accounts.len() as u64;
        let total_supply: u64 = self
            .accounts
            .iter()
            .map(|entry| entry.value().balance)
            .sum();
        let latest_block_hash = self
            .get_block(height)
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        LightSnapshot {
            height,
            state_root,
            account_count,
            total_supply,
            latest_block_hash,
        }
    }

    pub fn export_chunked_snapshot(
        &self,
        chunk_size: usize,
    ) -> (SnapshotManifest, Vec<StateSnapshot>) {
        let chunk_size = chunk_size.max(1);
        let version = self.height();
        let state_root = self.compute_state_root();

        let mut all_accounts: Vec<(Address, Account)> = self
            .accounts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        all_accounts.sort_by_key(|(addr, _)| addr.0);

        let total_accounts = all_accounts.len() as u64;
        let total_chunks = if all_accounts.is_empty() {
            1
        } else {
            all_accounts.len().div_ceil(chunk_size) as u32
        };

        let mut chunks = Vec::with_capacity(total_chunks as usize);
        let mut chunk_proofs = Vec::with_capacity(total_chunks as usize);

        for (i, accounts_slice) in all_accounts.chunks(chunk_size).enumerate() {
            let chunk_data = bincode::serialize(accounts_slice).expect("serializable");
            let chunk_proof = hash_bytes(&chunk_data);
            chunk_proofs.push(chunk_proof);

            chunks.push(StateSnapshot {
                version,
                state_root,
                accounts: accounts_slice.to_vec(),
                chunk_index: i as u32,
                total_chunks,
                chunk_proof,
            });
        }

        if chunks.is_empty() {
            let empty_data =
                bincode::serialize(&Vec::<(Address, Account)>::new()).expect("serializable");
            let chunk_proof = hash_bytes(&empty_data);
            chunk_proofs.push(chunk_proof);
            chunks.push(StateSnapshot {
                version,
                state_root,
                accounts: vec![],
                chunk_index: 0,
                total_chunks: 1,
                chunk_proof,
            });
        }

        let manifest_hash = Self::compute_manifest_hash(
            version,
            &state_root,
            total_accounts,
            total_chunks,
            chunk_size,
            &chunk_proofs,
        );

        let manifest = SnapshotManifest {
            version,
            state_root,
            total_accounts,
            total_chunks,
            chunk_size,
            manifest_hash,
        };

        (manifest, chunks)
    }

    pub fn export_snapshot_chunk(
        &self,
        chunk_index: u32,
        chunk_size: usize,
    ) -> Option<StateSnapshot> {
        let chunk_size = chunk_size.max(1);
        let version = self.height();
        let state_root = self.compute_state_root();

        let mut all_accounts: Vec<(Address, Account)> = self
            .accounts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        all_accounts.sort_by_key(|(addr, _)| addr.0);

        let total_chunks = if all_accounts.is_empty() {
            1u32
        } else {
            all_accounts.len().div_ceil(chunk_size) as u32
        };

        if chunk_index >= total_chunks {
            return None;
        }

        let start = chunk_index as usize * chunk_size;
        let end = (start + chunk_size).min(all_accounts.len());
        let accounts_slice = if start >= all_accounts.len() {
            vec![]
        } else {
            all_accounts[start..end].to_vec()
        };

        let chunk_data = bincode::serialize(&accounts_slice).expect("serializable");
        let chunk_proof = hash_bytes(&chunk_data);

        Some(StateSnapshot {
            version,
            state_root,
            accounts: accounts_slice,
            chunk_index,
            total_chunks,
            chunk_proof,
        })
    }

    pub fn import_snapshot_chunk(&self, chunk: &StateSnapshot) -> Result<u32, StateError> {
        if self.is_persistent() {
            return Err(StateError::UnauthenticatedPersistentStateSync);
        }

        let chunk_data = bincode::serialize(&chunk.accounts).expect("serializable");
        let computed_proof = hash_bytes(&chunk_data);
        if computed_proof != chunk.chunk_proof {
            return Err(StateError::ChunkVerificationFailed);
        }

        if chunk.chunk_index == 0 && self.height() == 0 {
            self.accounts.clear();
            self.contracts.clear();
            self.storage.clear();
            *self.incremental_merkle.lock() = IncrementalMerkle::new();
            self.dirty_accounts.clear();
        }

        let count = chunk.accounts.len() as u32;
        for (addr, account) in &chunk.accounts {
            self.accounts.insert(addr.0, account.clone());
            self.dirty_accounts.insert(addr.0);
        }

        Ok(count)
    }

    pub fn verify_snapshot_integrity(&self, manifest: &SnapshotManifest) -> bool {
        let computed = self.compute_state_root();
        computed == manifest.state_root
    }

    pub fn begin_sync(manifest: SnapshotManifest) -> SyncProgress {
        let total = manifest.total_chunks as usize;
        SyncProgress {
            manifest,
            received_chunks: vec![false; total],
            verified_chunks: 0,
            total_accounts_imported: 0,
            latest_chunk_state_root: None,
        }
    }

    pub fn is_sync_complete(progress: &SyncProgress) -> bool {
        progress.received_chunks.iter().all(|&received| received)
    }

    pub fn record_chunk(
        progress: &mut SyncProgress,
        chunk: &StateSnapshot,
        accounts_imported: u32,
    ) -> Result<(), StateError> {
        let idx = chunk.chunk_index as usize;
        if idx >= progress.received_chunks.len() {
            return Err(StateError::ChunkOutOfRange {
                index: chunk.chunk_index,
                total: progress.manifest.total_chunks,
            });
        }
        progress.received_chunks[idx] = true;
        progress.verified_chunks += 1;
        progress.total_accounts_imported += accounts_imported as u64;
        progress.latest_chunk_state_root = Some(chunk.state_root);
        Ok(())
    }

    const DEFAULT_CHUNK_SIZE: usize = 1000;

    pub fn export_snapshot_manifest(&self) -> SnapshotManifest {
        let chunk_size = Self::DEFAULT_CHUNK_SIZE;
        let total_accounts = self.accounts.len() as u64;
        let total_chunks = if total_accounts == 0 {
            1u32
        } else {
            (total_accounts as usize).div_ceil(chunk_size) as u32
        };

        let version = self.height();
        let state_root = self.compute_state_root();

        let pre_hash_data = bincode::serialize(&(
            version,
            &state_root,
            total_accounts,
            total_chunks,
            chunk_size,
        ))
        .expect("serializable");
        let manifest_hash = hash_bytes(&pre_hash_data);

        SnapshotManifest {
            version,
            state_root,
            total_accounts,
            total_chunks,
            chunk_size,
            manifest_hash,
        }
    }

    pub fn finalize_sync(&self, progress: &SyncProgress) -> Result<(), StateError> {
        if self.is_persistent() {
            return Err(StateError::UnauthenticatedPersistentStateSync);
        }
        if !Self::is_sync_complete(progress) {
            return Err(StateError::SyncIncomplete {
                received: progress.verified_chunks,
                total: progress.manifest.total_chunks,
            });
        }

        let expected = progress
            .latest_chunk_state_root
            .unwrap_or(progress.manifest.state_root);
        let computed_root = self.compute_state_root();
        if computed_root != expected {
            return Err(StateError::StateRootMismatch {
                expected,
                computed: computed_root,
            });
        }

        *self.height.write() = progress.manifest.version;

        Ok(())
    }

    pub fn state_summary(&self) -> StateSummary {
        let total_balance: u128 = self
            .accounts
            .iter()
            .map(|entry| entry.value().balance as u128)
            .sum();
        StateSummary {
            account_count: self.accounts.len() as u64,
            total_balance,
            state_root: self.compute_state_root(),
            block_height: self.height(),
        }
    }

    fn compute_manifest_hash(
        version: u64,
        state_root: &Hash256,
        total_accounts: u64,
        total_chunks: u32,
        chunk_size: usize,
        chunk_proofs: &[Hash256],
    ) -> Hash256 {
        let mut data = Vec::new();
        data.extend_from_slice(&version.to_le_bytes());
        data.extend_from_slice(&state_root.0);
        data.extend_from_slice(&total_accounts.to_le_bytes());
        data.extend_from_slice(&total_chunks.to_le_bytes());
        data.extend_from_slice(&(chunk_size as u64).to_le_bytes());
        for proof in chunk_proofs {
            data.extend_from_slice(&proof.0);
        }
        hash_bytes(&data)
    }
}

impl Default for StateDB {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "benchmark-tools")]
#[inline]
fn compute_benchmark_tx_hash(
    base_hasher: &blake3::Hasher,
    nonce: u64,
    body_bytes: &[u8],
) -> Hash256 {
    let mut h = base_hasher.clone();
    h.update(&nonce.to_le_bytes());
    h.update(body_bytes);
    h.update(&0u64.to_le_bytes());
    h.update(&0u64.to_le_bytes());
    Hash256(*h.finalize().as_bytes())
}

fn compute_merkle_root_only(mut leaves: Vec<Hash256>) -> Hash256 {
    if leaves.is_empty() {
        return Hash256::ZERO;
    }
    if leaves.len() == 1 {
        return leaves[0];
    }
    if !leaves.len().is_multiple_of(2) {
        leaves.push(*leaves.last().unwrap());
    }
    while leaves.len() > 1 {
        leaves = leaves
            .par_chunks(2)
            .map(|pair| hash_pair(&pair[0], &pair[1]))
            .collect();
        if leaves.len() > 1 && !leaves.len().is_multiple_of(2) {
            leaves.push(*leaves.last().unwrap());
        }
    }
    leaves[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    fn persistent_test_dir(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("arc-state-{name}-{}-{unique}", std::process::id()))
    }

    #[test]
    fn persistent_state_is_bound_to_the_authenticated_genesis() {
        let dir = persistent_test_dir("genesis-binding");
        let genesis_a = hash_bytes(b"genesis-a");
        let genesis_b = hash_bytes(b"genesis-b");
        let prefunded = [(addr(1), 123)];

        let state = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_a).unwrap();
        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 123);
        drop(state);

        let binding = std::fs::read_to_string(dir.join("genesis.network-hash")).unwrap();
        assert_eq!(binding.trim(), genesis_a.to_hex());

        let recovered = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_a).unwrap();
        assert_eq!(recovered.get_account(&addr(1)).unwrap().balance, 123);
        drop(recovered);

        let error = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_b)
            .err()
            .expect("mismatched genesis must fail")
            .to_string();
        assert!(error.contains("data directory genesis mismatch"), "{error}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn persistent_constructor_restores_a_staged_full_root_before_wal_discovery() {
        let parent = persistent_test_dir("staged-full-root-parent");
        arc_crypto::secret_file::create_private_directory_tree(&parent).unwrap();
        let dir = parent.join("state");
        let genesis_hash = hash_bytes(b"staged-full-root-genesis");
        let prefunded = [(addr(1), 123)];

        let state = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap();
        let (block, _) = state.execute_block(&[], addr(99)).unwrap();
        let expected_root = state.get_state_root();
        drop(state);

        let digest = arc_crypto::secret_file::namespace_path_digest(&dir).unwrap();
        let staged = parent.join(format!(
            ".arc-private-directory-namespace-{digest}.rebarrier"
        ));
        arc_crypto::secret_file::windows_move_path_write_through(&dir, &staged, false).unwrap();
        assert!(!dir.exists());

        let restarted = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap();
        assert_eq!(restarted.height(), 1);
        assert_eq!(restarted.get_block(1).unwrap().hash, block.hash);
        assert_eq!(restarted.get_state_root(), expected_root);
        assert!(!staged.exists());
        drop(restarted);

        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn persistent_startup_reclaims_only_exact_state_wal_staging_files() {
        let dir = persistent_test_dir("genesis-staging-cleanup");
        arc_crypto::secret_file::create_private_directory_tree(&dir).unwrap();
        let genesis_identifier = uuid::Uuid::new_v4();
        let stale = [
            dir.join(format!(".state.wal.genesis-{}.tmp", uuid::Uuid::new_v4())),
            dir.join(format!(
                ".state.wal.quarantine-rebarrier-{}",
                uuid::Uuid::new_v4()
            )),
            dir.join(".state.wal.quarantine-tmp-1-0"),
            dir.join(".state.wal.quarantine-tmp-4294967295-99"),
            dir.join(format!(".state.wal.create-{}.tmp", uuid::Uuid::new_v4())),
            dir.join(format!(
                "..state.wal.genesis-{genesis_identifier}.tmp.create-{}.tmp",
                uuid::Uuid::new_v4()
            )),
            dir.join(format!(
                ".wal-00000000.bin.create-{}.tmp",
                uuid::Uuid::new_v4()
            )),
            dir.join(format!(".state.wal.rebarrier-{}.tmp", uuid::Uuid::new_v4())),
        ];
        let unrelated = [
            dir.join(".state.wal.genesis-not-a-uuid.tmp"),
            dir.join(".state.wal.quarantine-rebarrier-not-a-uuid"),
            dir.join(".state.wal.quarantine-tmp-1-100"),
            dir.join(".state.wal.quarantine-tmp-pid-0"),
            dir.join(format!(".operator.create-{}.tmp", uuid::Uuid::new_v4())),
            dir.join(format!(
                ".wal-000000000.bin.create-{}.tmp",
                uuid::Uuid::new_v4()
            )),
        ];
        for path in &stale {
            std::fs::write(path, b"incomplete internal state WAL staging").unwrap();
        }
        for path in &unrelated {
            std::fs::write(path, b"operator file").unwrap();
        }

        let state = StateDB::with_genesis_persistent(
            &[(addr(1), 123)],
            &dir,
            hash_bytes(b"genesis-staging-cleanup"),
        )
        .unwrap();
        assert!(stale.iter().all(|path| !path.exists()));
        assert!(unrelated.iter().all(|path| path.exists()));
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistent_state_rejects_every_incomplete_genesis_wal_prefix() {
        let prefunded = [(addr(1), 123), (addr(2), 456), (addr(3), 789)];
        let genesis_hash = hash_bytes(b"genesis-prefix-crash");

        for account_prefix in 0..=prefunded.len() {
            let dir = persistent_test_dir(&format!("genesis-prefix-{account_prefix}"));
            arc_crypto::secret_file::create_private_directory_tree(&dir).unwrap();
            StateDB::verify_or_create_genesis_binding(&dir, false, genesis_hash).unwrap();
            let wal_path = dir.join("state.wal");
            let writer = WalWriter::new(&wal_path).unwrap();
            for (address, balance) in prefunded.iter().take(account_prefix) {
                writer.append(
                    WalOp::SetAccount(*address, Account::new(*address, *balance)),
                    0,
                );
            }
            writer.sync().unwrap();
            drop(writer);

            let error = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash)
                .err()
                .expect("an incomplete genesis prefix must fail closed")
                .to_string();
            assert!(error.contains("incomplete genesis prefix"), "{error}");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn persistent_genesis_account_order_is_configuration_order_independent() {
        let dir = persistent_test_dir("genesis-order");
        let genesis_hash = hash_bytes(b"genesis-order");
        let original = [(addr(1), 123), (addr(2), 456), (addr(3), 789)];
        let reordered = [original[2], original[0], original[1]];

        drop(StateDB::with_genesis_persistent(&original, &dir, genesis_hash).unwrap());
        let restarted = StateDB::with_genesis_persistent(&reordered, &dir, genesis_hash).unwrap();
        for (address, balance) in original {
            assert_eq!(restarted.get_account(&address).unwrap().balance, balance);
        }
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistent_legacy_genesis_without_height_zero_checkpoint_still_restarts() {
        let dir = persistent_test_dir("legacy-genesis-no-checkpoint");
        let genesis_hash = hash_bytes(b"legacy-genesis-no-checkpoint");
        let prefunded = [(addr(1), 123), (addr(2), 456)];
        arc_crypto::secret_file::create_private_directory_tree(&dir).unwrap();
        StateDB::verify_or_create_genesis_binding(&dir, false, genesis_hash).unwrap();
        let writer = WalWriter::new(dir.join("state.wal")).unwrap();
        for (address, balance) in prefunded {
            writer.append(
                WalOp::SetAccount(address, Account::new(address, balance)),
                0,
            );
        }
        writer.append(WalOp::SetBlock(0, Block::genesis()), 0);
        writer.sync().unwrap();
        drop(writer);

        let restarted = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap();
        assert_eq!(restarted.height(), 0);
        assert_eq!(restarted.get_account(&addr(1)).unwrap().balance, 123);
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistent_restart_keeps_a_complete_checkpointed_block() {
        let dir = persistent_test_dir("checkpointed-block-restart");
        let genesis_hash = hash_bytes(b"checkpointed-block-restart");
        let state =
            StateDB::with_genesis_persistent(&[(addr(1), 123)], &dir, genesis_hash).unwrap();
        let (block, _) = state.execute_block(&[], addr(99)).unwrap();
        let root = state.get_state_root();
        drop(state);

        let restarted =
            StateDB::with_genesis_persistent(&[(addr(1), 123)], &dir, genesis_hash).unwrap();
        assert_eq!(restarted.height(), 1);
        assert_eq!(restarted.get_block(1).unwrap().hash, block.hash);
        assert_eq!(restarted.get_state_root(), root);
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn block_bytes(blocks: &[Block]) -> Vec<Vec<u8>> {
        blocks
            .iter()
            .map(|block| bincode::serialize(block).expect("block serializes"))
            .collect()
    }

    #[test]
    fn block_table_entry_is_a_height_and_a_pointer() {
        fn entry_size<K, V, S>(_: &dashmap::DashMap<K, V, S>) -> usize {
            std::mem::size_of::<(K, V)>()
        }
        let state = StateDB::new();
        assert!(entry_size(&state.blocks) <= 16);
    }

    #[test]
    fn block_table_returns_stored_blocks_after_insert_overwrite_and_clear() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000)]);
        let mut stored = vec![Block::genesis()];
        for _ in 0..3 {
            stored.push(state.execute_block(&[], addr(99)).unwrap().0);
        }
        let expected = block_bytes(&stored);
        let by_height: Vec<Block> = (0..=3).map(|h| state.get_block(h).unwrap()).collect();
        assert_eq!(block_bytes(&by_height), expected);
        let by_hash: Vec<Block> = stored
            .iter()
            .map(|block| state.get_block_by_hash(&block.hash.0).unwrap())
            .collect();
        assert_eq!(block_bytes(&by_hash), expected);
        assert_eq!(block_bytes(&state.get_block_range(0, 3, 10)), expected);
        let window: Vec<Block> = state
            .export_durable_snapshot()
            .blocks
            .into_iter()
            .map(|(_, block)| block)
            .collect();
        assert_eq!(block_bytes(&window), expected);

        let old_tx = hash_bytes(b"block-table-old-tx");
        let new_tx = hash_bytes(b"block-table-new-tx");
        let mut header = stored[3].header.clone();
        header.tx_count = 1;
        let older = Block::new(header.clone(), vec![old_tx]);
        header.timestamp += 1;
        let newer = Block::new(header, vec![new_tx]);
        state.apply_wal_op(&WalOp::SetBlock(3, older));
        assert_eq!(state.get_tx_location(&old_tx.0), Some((3, 0)));
        state.apply_wal_op(&WalOp::SetBlock(3, newer.clone()));
        stored[3] = newer;
        let expected = block_bytes(&stored);
        assert_eq!(block_bytes(&state.get_block_range(0, 3, 10)), expected);
        assert_eq!(state.get_tx_location(&old_tx.0), None);
        assert_eq!(state.get_tx_location(&new_tx.0), Some((3, 0)));
        assert_eq!(state.blocks.len(), 4);

        let payload = state.export_durable_snapshot();
        state.clear_replayable_state();
        assert!(state.blocks.is_empty());
        assert!(state.get_block(0).is_none());
        assert!(state.get_block_range(0, 3, 10).is_empty());
        state.install_history_window(&payload);
        assert_eq!(block_bytes(&state.get_block_range(0, 3, 10)), expected);
        assert_eq!(state.get_tx_location(&new_tx.0), Some((3, 0)));
    }

    #[test]
    fn block_table_after_restart_holds_the_same_blocks() {
        let dir = persistent_test_dir("block-table-restart");
        let genesis_hash = hash_bytes(b"block-table-restart");
        let prefunded = [(addr(1), 123)];
        let state = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap();
        for _ in 0..3 {
            state.execute_block(&[], addr(99)).unwrap();
        }
        let before = block_bytes(&state.get_block_range(0, 3, 10));
        assert_eq!(before.len(), 4);
        drop(state);

        let restarted = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap();
        assert_eq!(restarted.height(), 3);
        assert_eq!(restarted.blocks.len(), 4);
        assert_eq!(block_bytes(&restarted.get_block_range(0, 3, 10)), before);
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistent_restart_keeps_a_same_height_state_diff_checkpoint() {
        let dir = persistent_test_dir("same-height-state-diff-restart");
        let genesis_hash = hash_bytes(b"same-height-state-diff-restart");
        let prefunded = [(addr(1), 123)];
        let state = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap();

        let proposer = StateDB::with_genesis(&prefunded);
        let mut changed = proposer.get_account(&addr(1)).unwrap();
        changed.balance = 987;
        proposer.update_account(&addr(1), changed);
        let diff = proposer.export_state_diff(&[addr(1)]);
        assert_eq!(state.apply_state_diff(&diff).unwrap(), diff.new_root);
        drop(state);

        let restarted = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap();
        assert_eq!(restarted.height(), 0);
        assert_eq!(restarted.get_account(&addr(1)).unwrap().balance, 987);
        assert_eq!(restarted.get_state_root(), diff.new_root);
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistent_restart_keeps_post_checkpoint_event_logs() {
        let dir = persistent_test_dir("post-checkpoint-event-logs-restart");
        let genesis_hash = hash_bytes(b"post-checkpoint-event-logs-restart");
        let state = StateDB::with_genesis_persistent(&[], &dir, genesis_hash).unwrap();
        let (block, _) = state.execute_block(&[], addr(99)).unwrap();
        let log = arc_types::EventLog {
            address: addr(7),
            topics: vec![hash_bytes(b"event-topic")],
            data: b"durable-event-data".to_vec(),
            block_height: block.header.height,
            tx_hash: hash_bytes(b"event-transaction"),
            log_index: 3,
        };
        state.store_event_logs(block.header.height, vec![log.clone()]);
        drop(state);

        let restarted = StateDB::with_genesis_persistent(&[], &dir, genesis_hash).unwrap();
        let restored = restarted
            .event_logs
            .get(&block.header.height)
            .expect("event logs survive restart");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].address, log.address);
        assert_eq!(restored[0].topics, log.topics);
        assert_eq!(restored[0].data, log.data);
        assert_eq!(restored[0].block_height, log.block_height);
        assert_eq!(restored[0].tx_hash, log.tx_hash);
        assert_eq!(restored[0].log_index, log.log_index);
        drop(restored);
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistent_restart_quarantines_complete_uncheckpointed_suffix() {
        let dir = persistent_test_dir("complete-uncheckpointed-suffix");
        let genesis_hash = hash_bytes(b"complete-uncheckpointed-suffix");
        let prefunded = [(addr(1), 123)];
        drop(StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap());
        let wal_path = dir.join("state.wal");
        let accepted_bytes = std::fs::metadata(&wal_path).unwrap().len();
        let writer = WalWriter::new(&wal_path).unwrap();
        writer.append(WalOp::SetAccount(addr(9), Account::new(addr(9), 999)), 1);
        writer.sync().unwrap();
        drop(writer);
        assert!(std::fs::metadata(&wal_path).unwrap().len() > accepted_bytes);

        let restarted = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap();
        assert!(restarted.get_account(&addr(9)).is_none());
        assert_eq!(restarted.height(), 0);
        assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), accepted_bytes);
        assert_eq!(
            std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("state.wal.quarantine-")
                })
                .count(),
            1
        );
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistent_restart_quarantines_a_torn_final_frame() {
        use std::io::Write as _;

        let dir = persistent_test_dir("torn-final-frame");
        let genesis_hash = hash_bytes(b"torn-final-frame");
        let prefunded = [(addr(1), 123)];
        drop(StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap());
        let wal_path = dir.join("state.wal");
        let accepted_bytes = std::fs::metadata(&wal_path).unwrap().len();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wal_path)
            .unwrap();
        file.write_all(&[0, 0]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let restarted = StateDB::with_genesis_persistent(&prefunded, &dir, genesis_hash).unwrap();
        assert_eq!(restarted.height(), 0);
        assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), accepted_bytes);
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unauthenticated_snapshot_sync_cannot_mutate_persistent_state_or_wal() {
        let dir = persistent_test_dir("persistent-snapshot-rejection");
        let genesis_hash = hash_bytes(b"persistent-snapshot-rejection");
        let state =
            StateDB::with_genesis_persistent(&[(addr(1), 123)], &dir, genesis_hash).unwrap();
        let source = StateDB::with_genesis(&[(addr(9), 999)]);
        let (manifest, chunks) = source.export_chunked_snapshot(100);
        let before_root = state.get_state_root();
        let before_wal = std::fs::read(dir.join("state.wal")).unwrap();

        assert!(matches!(
            state.import_snapshot_chunk(&chunks[0]),
            Err(StateError::UnauthenticatedPersistentStateSync)
        ));
        let progress = StateDB::begin_sync(manifest);
        assert!(matches!(
            state.finalize_sync(&progress),
            Err(StateError::UnauthenticatedPersistentStateSync)
        ));
        assert_eq!(state.height(), 0);
        assert_eq!(state.get_state_root(), before_root);
        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 123);
        assert!(state.get_account(&addr(9)).is_none());
        state.sync_wal();
        assert_eq!(std::fs::read(dir.join("state.wal")).unwrap(), before_wal);

        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistence_directory_is_exposed_only_for_durable_state() {
        assert_eq!(StateDB::new().persistence_dir(), None);

        let dir = persistent_test_dir("persistence-directory");
        std::fs::create_dir_all(&dir).unwrap();
        let state = StateDB::with_persistence(dir.join("state.wal")).unwrap();
        assert_eq!(state.persistence_dir().as_deref(), Some(dir.as_path()));
        drop(state);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn block_is_not_acknowledged_when_wal_fsync_fails() {
        let dir = persistent_test_dir("wal-boundary-failure");
        std::fs::create_dir_all(&dir).unwrap();
        let state = StateDB::with_persistence(dir.join("state.wal")).unwrap();
        state.wal.inject_failure(crate::wal::WalFaultPoint::Fsync);

        let error = state
            .execute_block(&[], addr(99))
            .expect_err("block boundary must propagate fsync failure");
        assert!(matches!(error, StateError::PersistenceError(_)));
        assert_eq!(state.wal_failure().unwrap().operation(), "fsync");

        let failed_height = state.height();
        let retry = state
            .execute_block(&[], addr(99))
            .expect_err("a failed WAL remains fatal");
        assert!(matches!(retry, StateError::PersistenceError(_)));
        assert_eq!(state.height(), failed_height);

        drop(state);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_unbound_wal_fails_closed() {
        let dir = persistent_test_dir("legacy-unbound");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.wal"), []).unwrap();

        let error = StateDB::with_genesis_persistent(&[(addr(1), 123)], &dir, hash_bytes(b"g"))
            .err()
            .expect("unbound legacy WAL must fail")
            .to_string();
        assert!(
            error.contains("no authenticated genesis binding"),
            "{error}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_transfer_execution() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);

        let tx = Transaction::new_transfer(addr(1), addr(2), 500, 0);
        let (block, receipts) = state.execute_block(&[tx], addr(99)).unwrap();

        assert_eq!(block.header.height, 1);
        assert_eq!(receipts.len(), 1);
        assert!(receipts[0].success);

        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 999_500);
        assert_eq!(sender.nonce, 1);

        let receiver = state.get_account(&addr(2)).unwrap();
        assert_eq!(receiver.balance, 500);
    }

    #[test]
    fn test_insufficient_balance() {
        let state = StateDB::with_genesis(&[(addr(1), 100)]);

        let tx = Transaction::new_transfer(addr(1), addr(2), 200, 0);
        let (_, receipts) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(!receipts[0].success);
    }

    #[test]
    fn test_nonce_enforcement() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);

        let tx1 = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let (_, r1) = state.execute_block(&[tx1], addr(99)).unwrap();
        assert!(r1[0].success);

        let tx2 = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let (_, r2) = state.execute_block(&[tx2], addr(99)).unwrap();
        assert!(!r2[0].success);

        let tx3 = Transaction::new_transfer(addr(1), addr(2), 100, 1);
        let (_, r3) = state.execute_block(&[tx3], addr(99)).unwrap();
        assert!(r3[0].success);
    }

    #[test]
    fn test_large_block() {
        let state = StateDB::with_genesis(&[(addr(1), u64::MAX)]);

        let txns: Vec<Transaction> = (0..10_000u64)
            .map(|i| Transaction::new_transfer(addr(1), addr(2), 1, i))
            .collect();

        let (block, receipts) = state.execute_block(&txns, addr(99)).unwrap();
        assert_eq!(block.header.tx_count, 10_000);
        assert!(receipts.iter().all(|r| r.success));

        let receiver = state.get_account(&addr(2)).unwrap();
        assert_eq!(receiver.balance, 10_000);
    }

    #[test]
    fn test_contract_storage() {
        let state = StateDB::new();
        let contract = addr(10);
        let key = hash_bytes(b"counter");

        state.deploy_contract(&contract, vec![0x00, 0x61, 0x73, 0x6d]);
        assert!(state.get_contract(&contract).is_some());

        state.set_storage(&contract, key, 42u64.to_le_bytes().to_vec());
        let val = state.get_storage(&contract, &key).unwrap();
        assert_eq!(u64::from_le_bytes(val[..8].try_into().unwrap()), 42);

        state.delete_storage(&contract, &key);
        assert!(state.get_storage(&contract, &key).is_none());
    }

    fn signed_evm_call(
        signer: &arc_crypto::signature::KeyPair,
        contract: Address,
        nonce: u64,
        tag: u8,
    ) -> Transaction {
        let mut transaction = Transaction::new_wasm_call(
            signer.address(),
            contract,
            String::new(),
            vec![tag],
            7,
            1_000_000,
            nonce,
        );
        transaction.sign(signer).unwrap();
        transaction
    }

    fn assert_evm_call_left_no_state(
        state: &StateDB,
        sender: Address,
        contract: Address,
        storage_key: Hash256,
        expected_root: Hash256,
    ) {
        let account = state.get_account(&sender).expect("funded sender remains");
        assert_eq!(account.balance, 1_000_000);
        assert_eq!(account.nonce, 0, "rejected EVM call cannot consume nonce");
        assert_eq!(
            state.get_storage(&contract, &storage_key),
            Some(b"before".to_vec()),
            "rejected EVM call cannot write storage"
        );
        assert!(
            state.event_logs.is_empty(),
            "rejected call cannot emit logs"
        );
        assert_eq!(
            state.compute_state_root(),
            expected_root,
            "rejected call cannot change the authenticated state"
        );
    }

    #[test]
    fn state_changing_evm_call_fails_before_any_mutation() {
        use arc_crypto::signature::KeyPair;

        let signer = KeyPair::generate_ed25519();
        let contract = addr(81);
        let storage_key = hash_bytes(b"evm-atomic-storage");
        let state = StateDB::with_genesis(&[(signer.address(), 1_000_000)]);
        state.deploy_contract(&contract, vec![0x60, 0x00]);
        state.set_storage(&contract, storage_key, b"before".to_vec());
        let before_root = state.compute_state_root();

        let error = state
            .execute_tx_pub(&signed_evm_call(&signer, contract, 0, 1))
            .expect_err("state-changing EVM execution must fail closed");
        assert!(
            error
                .to_string()
                .contains("state-changing EVM calls are unavailable"),
            "{error}"
        );
        assert_evm_call_left_no_state(&state, signer.address(), contract, storage_key, before_root);
    }

    #[test]
    fn rejected_evm_call_matches_sequential_and_blockstm_roots() {
        use arc_crypto::signature::KeyPair;

        let signer = KeyPair::generate_ed25519();
        let contract = addr(82);
        let storage_key = hash_bytes(b"evm-engine-storage");
        let genesis = [(signer.address(), 1_000_000)];
        let sequential = StateDB::with_genesis(&genesis);
        let blockstm = StateDB::with_genesis(&genesis);
        for state in [&sequential, &blockstm] {
            state.deploy_contract(&contract, vec![0x60, 0x01]);
            state.set_storage(&contract, storage_key, b"before".to_vec());
        }
        let before_root = sequential.compute_state_root();
        assert_eq!(before_root, blockstm.compute_state_root());
        let transaction = signed_evm_call(&signer, contract, 0, 2);
        let timestamp = 1_800_000_001_000;

        let (sequential_block, sequential_receipts) = sequential
            .execute_block_verified_at(std::slice::from_ref(&transaction), addr(99), timestamp)
            .unwrap();
        let (blockstm_block, blockstm_receipts) = blockstm
            .execute_block_blockstm_at(&[transaction], addr(99), timestamp, Hash256::ZERO)
            .unwrap();

        assert!(!sequential_receipts[0].success);
        assert!(!blockstm_receipts[0].success);
        assert_eq!(sequential_block.header.state_root, before_root);
        assert_eq!(blockstm_block.header.state_root, before_root);
        assert_eq!(sequential_block.hash, blockstm_block.hash);
        assert_evm_call_left_no_state(
            &sequential,
            signer.address(),
            contract,
            storage_key,
            before_root,
        );
        assert_evm_call_left_no_state(
            &blockstm,
            signer.address(),
            contract,
            storage_key,
            before_root,
        );
    }

    #[test]
    fn rejected_evm_calls_do_not_disturb_following_nonce_order() {
        use arc_crypto::signature::KeyPair;

        let signer = KeyPair::generate_ed25519();
        let contract = addr(83);
        let recipient = addr(84);
        let state = StateDB::with_genesis(&[(signer.address(), 1_000_000), (recipient, 0)]);
        state.deploy_contract(&contract, vec![0x60, 0x02]);

        let first = signed_evm_call(&signer, contract, 0, 3);
        let second = signed_evm_call(&signer, contract, 1, 4);
        let mut transfer = Transaction::new_transfer(signer.address(), recipient, 11, 0);
        transfer.sign(&signer).unwrap();
        let (_, receipts) = state
            .execute_block_blockstm_at(
                &[first, second, transfer],
                addr(99),
                1_800_000_001_001,
                Hash256::ZERO,
            )
            .unwrap();

        assert_eq!(
            receipts
                .iter()
                .map(|receipt| receipt.success)
                .collect::<Vec<_>>(),
            vec![false, false, true]
        );
        let sender = state.get_account(&signer.address()).unwrap();
        assert_eq!(sender.nonce, 1);
        assert_eq!(sender.balance, 1_000_000 - 11);
        assert_eq!(state.get_account(&recipient).unwrap().balance, 11);
        assert!(state.event_logs.is_empty());
    }

    #[test]
    fn rejected_evm_call_is_restart_stable() {
        use arc_crypto::signature::KeyPair;

        let signer = KeyPair::generate_ed25519();
        let contract = addr(85);
        let storage_key = hash_bytes(b"evm-restart-storage");
        let directory = persistent_test_dir("evm-reject-restart");
        let genesis_hash = hash_bytes(b"evm-reject-genesis");
        let prefunded = [(signer.address(), 1_000_000)];
        let state = StateDB::with_genesis_persistent(&prefunded, &directory, genesis_hash).unwrap();
        state.deploy_contract(&contract, vec![0x60, 0x03]);
        state.set_storage(&contract, storage_key, b"before".to_vec());

        let (block, receipts) = state
            .execute_block_verified_at(
                &[signed_evm_call(&signer, contract, 0, 5)],
                addr(99),
                1_800_000_001_002,
            )
            .unwrap();
        assert!(!receipts[0].success);
        let durable_root = block.header.state_root;
        assert_evm_call_left_no_state(
            &state,
            signer.address(),
            contract,
            storage_key,
            durable_root,
        );
        drop(state);

        let recovered =
            StateDB::with_genesis_persistent(&prefunded, &directory, genesis_hash).unwrap();
        assert_eq!(recovered.height(), 1);
        assert_eq!(
            recovered.get_block(1).unwrap().header.state_root,
            durable_root
        );
        assert_evm_call_left_no_state(
            &recovered,
            signer.address(),
            contract,
            storage_key,
            durable_root,
        );
        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);

        state.deploy_contract(&addr(10), vec![0x00, 0x61, 0x73, 0x6d]);
        state.set_storage(&addr(10), hash_bytes(b"k"), b"v".to_vec());

        let snapshot = state.snapshot();
        assert_eq!(snapshot.accounts.len(), 2);
        assert_eq!(snapshot.contracts.len(), 1);
        assert_eq!(snapshot.storage.len(), 1);
    }

    #[test]
    fn test_verified_execution() {
        use arc_crypto::signature::KeyPair;

        let kp = KeyPair::generate_ed25519();
        let address = kp.address();
        let state = StateDB::with_genesis(&[(address, 1_000_000)]);

        let mut tx = Transaction::new_transfer(address, addr(2), 500, 0);
        tx.sign(&kp).unwrap();
        let (_, receipts) = state.execute_block_verified(&[tx], addr(99)).unwrap();
        assert!(receipts[0].success);

        let tx2 = Transaction::new_transfer(address, addr(2), 500, 1);
        let (_, receipts2) = state.execute_block_verified(&[tx2], addr(99)).unwrap();
        assert!(!receipts2[0].success);
    }

    #[test]
    fn batch_verified_ed25519_cannot_spend_from_a_different_address() {
        use arc_crypto::signature::KeyPair;

        let victim = KeyPair::generate_ed25519();
        let attacker = KeyPair::generate_ed25519();
        let recipient = addr(73);
        let starting_balance = 1_000_000;
        let state = StateDB::with_genesis(&[(victim.address(), starting_balance)]);

        let mut forged = Transaction::new_transfer(victim.address(), recipient, 500, 0);
        forged.sign(&attacker).unwrap();
        assert!(forged.verify_signature().is_err());

        let (_, receipts) = state.execute_block_verified(&[forged], addr(99)).unwrap();
        assert!(!receipts[0].success);
        assert_eq!(
            state.get_account(&victim.address()).unwrap().balance,
            starting_balance
        );
        assert_eq!(
            state
                .get_account(&recipient)
                .map(|account| account.balance)
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn gpu_verified_rejects_ed25519_signature_from_a_different_address() {
        use arc_crypto::signature::KeyPair;

        let victim = KeyPair::generate_ed25519();
        let attacker = KeyPair::generate_ed25519();
        let recipient = addr(74);
        let starting_balance = 1_000_000;
        let state = StateDB::with_genesis(&[(victim.address(), starting_balance)]);

        let mut forged = Transaction::new_transfer(victim.address(), recipient, 500, 0);
        forged.sign(&attacker).unwrap();
        assert!(forged.verify_signature().is_err());

        let (_, receipts) = state
            .execute_block_gpu_verified(&[forged], addr(99))
            .unwrap();
        assert!(!receipts[0].success);
        assert_eq!(
            state.get_account(&victim.address()).unwrap().balance,
            starting_balance
        );
        assert_eq!(
            state
                .get_account(&recipient)
                .map(|account| account.balance)
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn gpu_verified_rejects_stale_transaction_hash() {
        use arc_crypto::signature::KeyPair;

        let sender = KeyPair::generate_ed25519();
        let recipient = addr(75);
        let starting_balance = 1_000_000;
        let state = StateDB::with_genesis(&[(sender.address(), starting_balance)]);

        let mut stale = Transaction::new_transfer(sender.address(), recipient, 500, 0);
        stale.sign(&sender).unwrap();
        match &mut stale.body {
            TxBody::Transfer(body) => body.amount = 750,
            _ => unreachable!("new_transfer must create a transfer body"),
        }
        assert_ne!(stale.compute_hash(), stale.hash);

        let (_, receipts) = state
            .execute_block_gpu_verified(&[stale], addr(99))
            .unwrap();
        assert!(!receipts[0].success);
        assert_eq!(
            state.get_account(&sender.address()).unwrap().balance,
            starting_balance
        );
        assert_eq!(
            state
                .get_account(&recipient)
                .map(|account| account.balance)
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn batch_verified_mixed_algorithms_execute_successfully() {
        use arc_crypto::signature::KeyPair;

        let kp_ed = KeyPair::generate_ed25519();
        let kp_ml = KeyPair::generate_ml_dsa();
        let kp_falcon = KeyPair::generate_falcon512();

        let recipient = addr(88);
        let starting_balance = 500_000;
        let state = StateDB::with_genesis(&[
            (kp_ed.address(), starting_balance),
            (kp_ml.address(), starting_balance),
            (kp_falcon.address(), starting_balance),
        ]);

        let mut tx_ed = Transaction::new_transfer(kp_ed.address(), recipient, 1_000, 0);
        tx_ed.sign(&kp_ed).unwrap();

        let mut tx_ml = Transaction::new_transfer(kp_ml.address(), recipient, 2_000, 0);
        tx_ml.sign(&kp_ml).unwrap();

        let mut tx_falcon = Transaction::new_transfer(kp_falcon.address(), recipient, 3_000, 0);
        tx_falcon.sign(&kp_falcon).unwrap();

        let (block, receipts) = state
            .execute_block_verified(&[tx_ed, tx_ml, tx_falcon], addr(99))
            .unwrap();
        assert_eq!(receipts.len(), 3);
        assert!(receipts[0].success, "Ed25519 transfer should succeed");
        assert!(receipts[1].success, "ML-DSA-65 transfer should succeed");
        assert!(receipts[2].success, "Falcon-512 transfer should succeed");
        assert_eq!(block.header.height, 1);
        assert_eq!(state.get_account(&recipient).unwrap().balance, 6_000);
    }

    #[test]
    fn batch_verified_ml_dsa_and_falcon_isolated_failures() {
        use arc_crypto::signature::KeyPair;

        let kp_ed = KeyPair::generate_ed25519();
        let kp_ml = KeyPair::generate_ml_dsa();
        let kp_falcon = KeyPair::generate_falcon512();
        let bad_signer = KeyPair::generate_ml_dsa();

        let recipient = addr(89);
        let starting_balance = 500_000;
        let state = StateDB::with_genesis(&[
            (kp_ed.address(), starting_balance),
            (kp_ml.address(), starting_balance),
            (kp_falcon.address(), starting_balance),
        ]);

        let mut tx_ed = Transaction::new_transfer(kp_ed.address(), recipient, 1_000, 0);
        tx_ed.sign(&kp_ed).unwrap();

        let mut tx_ml_bad = Transaction::new_transfer(kp_ml.address(), recipient, 2_000, 0);
        tx_ml_bad.sign(&bad_signer).unwrap();

        let mut tx_falcon = Transaction::new_transfer(kp_falcon.address(), recipient, 3_000, 0);
        tx_falcon.sign(&kp_falcon).unwrap();

        let (_, receipts) = state
            .execute_block_verified(&[tx_ed, tx_ml_bad, tx_falcon], addr(99))
            .unwrap();
        assert_eq!(receipts.len(), 3);
        assert!(receipts[0].success, "Ed25519 tx should succeed");
        assert!(!receipts[1].success, "Bad ML-DSA tx should fail");
        assert!(receipts[2].success, "Falcon tx should succeed");
        assert_eq!(state.get_account(&recipient).unwrap().balance, 4_000);
    }

    #[test]
    fn signed_benchmark_rejects_ed25519_signature_from_a_different_address() {
        use arc_crypto::signature::KeyPair;

        let victim = KeyPair::generate_ed25519();
        let attacker = KeyPair::generate_ed25519();
        let recipient = addr(76);
        let starting_balance = 1_000_000;
        let state = StateDB::with_genesis(&[(victim.address(), starting_balance)]);

        let mut forged = Transaction::new_transfer(victim.address(), recipient, 500, 0);
        forged.sign(&attacker).unwrap();
        assert!(forged.verify_signature().is_err());

        let block = state
            .execute_block_signed_benchmark(&[forged], addr(99))
            .unwrap();
        let stored = state
            .signed_block_data
            .get(&block.header.height)
            .expect("benchmark block must retain its success flags");
        assert_eq!(stored.value().1.as_slice(), &[false]);
        assert_eq!(
            state.get_account(&victim.address()).unwrap().balance,
            starting_balance
        );
        assert_eq!(
            state
                .get_account(&recipient)
                .map(|account| account.balance)
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn signed_benchmark_rejects_stale_transaction_hash() {
        use arc_crypto::signature::KeyPair;

        let sender = KeyPair::generate_ed25519();
        let recipient = addr(77);
        let starting_balance = 1_000_000;
        let state = StateDB::with_genesis(&[(sender.address(), starting_balance)]);

        let mut stale = Transaction::new_transfer(sender.address(), recipient, 500, 0);
        stale.sign(&sender).unwrap();
        match &mut stale.body {
            TxBody::Transfer(body) => body.amount = 750,
            _ => unreachable!("new_transfer must create a transfer body"),
        }
        assert_ne!(stale.compute_hash(), stale.hash);

        let block = state
            .execute_block_signed_benchmark(&[stale], addr(99))
            .unwrap();
        let stored = state
            .signed_block_data
            .get(&block.header.height)
            .expect("benchmark block must retain its success flags");
        assert_eq!(stored.value().1.as_slice(), &[false]);
        assert_eq!(
            state.get_account(&sender.address()).unwrap().balance,
            starting_balance
        );
        assert_eq!(
            state
                .get_account(&recipient)
                .map(|account| account.balance)
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn consensus_timestamp_makes_linear_block_hashes_cross_node_deterministic() {
        use arc_crypto::signature::KeyPair;

        let sender = KeyPair::generate_ed25519();
        let producer = addr(91);
        let genesis = [(sender.address(), 1_000_000)];
        let first = StateDB::with_genesis(&genesis);
        let second = StateDB::with_genesis(&genesis);
        let mut tx = Transaction::new_transfer(sender.address(), addr(92), 500, 0);
        tx.sign(&sender).unwrap();

        let (first_block, first_receipts) = first
            .execute_block_adaptive_at(std::slice::from_ref(&tx), producer, 1_800_000_000_123)
            .unwrap();
        let (second_block, second_receipts) = second
            .execute_block_adaptive_at(&[tx], producer, 1_800_000_000_123)
            .unwrap();

        assert!(first_receipts[0].success && second_receipts[0].success);
        assert_eq!(first_block.header.timestamp, 1_800_000_000_123);
        assert_eq!(
            first_block.header.state_root,
            second_block.header.state_root
        );
        assert_eq!(first_block.hash, second_block.hash);

        let (first_next, _) = first
            .execute_block_adaptive_at(&[], producer, 1_800_000_000_456)
            .unwrap();
        let (second_next, _) = second
            .execute_block_adaptive_at(&[], producer, 1_800_000_000_456)
            .unwrap();
        assert_eq!(first_next.header.parent_hash, first_block.hash);
        assert_eq!(second_next.header.parent_hash, second_block.hash);
        assert_eq!(first_next.hash, second_next.hash);
    }

    #[test]
    fn consensus_proof_hash_is_persisted_in_the_canonical_block_identity() {
        let producer = addr(93);
        let proof = hash_bytes(b"recovery-domain DAG decision round 42");
        let first = StateDB::with_genesis(&[]);
        let second = StateDB::with_genesis(&[]);

        let (bound, _) = first
            .execute_block_adaptive_at_with_proof(&[], producer, 1_800_000_000_500, proof)
            .unwrap();
        let (unbound, _) = second
            .execute_block_adaptive_at(&[], producer, 1_800_000_000_500)
            .unwrap();

        assert_eq!(bound.header.proof_hash, proof);
        assert_eq!(unbound.header.proof_hash, Hash256::ZERO);
        assert_ne!(bound.hash, unbound.hash);
        let stored = first.get_block(bound.header.height).unwrap();
        assert_eq!(stored.hash, bound.hash);
        assert_eq!(stored.header.proof_hash, proof);
    }

    #[test]
    fn consensus_timestamp_makes_blockstm_hashes_cross_node_deterministic() {
        use arc_crypto::signature::KeyPair;

        const TX_COUNT: usize = 128;
        const TIMESTAMP: u64 = 1_800_000_000_789;

        let senders: Vec<KeyPair> = (0..TX_COUNT).map(|_| KeyPair::generate_ed25519()).collect();
        let genesis: Vec<(Address, u64)> = senders
            .iter()
            .map(|sender| (sender.address(), 1_000))
            .collect();
        let transactions: Vec<Transaction> = senders
            .iter()
            .enumerate()
            .map(|(index, sender)| {
                let recipient = hash_bytes(&[0xb1, index as u8]);
                let mut tx = Transaction::new_transfer(sender.address(), recipient, 1, 0);
                tx.sign(sender).unwrap();
                tx
            })
            .collect();

        assert_eq!(
            crate::block_stm::choose_execution_mode(&transactions),
            crate::block_stm::AdaptiveMode::BlockSTM,
            "the regression must exercise execute_block_blockstm_at"
        );

        let producer = addr(93);
        let first = StateDB::with_genesis(&genesis);
        let second = StateDB::with_genesis(&genesis);
        let (first_block, first_receipts) = first
            .execute_block_adaptive_at(&transactions, producer, TIMESTAMP)
            .unwrap();
        let (second_block, second_receipts) = second
            .execute_block_adaptive_at(&transactions, producer, TIMESTAMP)
            .unwrap();

        assert!(first_receipts.iter().all(|receipt| receipt.success));
        assert!(second_receipts.iter().all(|receipt| receipt.success));
        assert_eq!(first_block.header.timestamp, TIMESTAMP);
        assert_eq!(second_block.header.timestamp, TIMESTAMP);
        assert_eq!(first_block.header.tx_root, second_block.header.tx_root);
        assert_eq!(
            first_block.header.state_root,
            second_block.header.state_root
        );
        assert_eq!(first_block.hash, second_block.hash);
    }

    #[test]
    fn test_identity_registry() {
        use arc_types::{Identity, IdentityLevel};

        let state = StateDB::new();
        let user = addr(1);
        let attestor = addr(99);

        assert!(state.get_identity(&user).is_none());
        assert!(!state.is_compliant(&user));
        assert_eq!(state.identity_count(), 0);

        let id = Identity {
            address: user,
            level: IdentityLevel::Verified,
            attestor,
            proof_hash: hash_bytes(b"kyc-proof-001"),
            country_code: *b"US",
            attested_at: 1_000_000,
            expires_at: 0,
        };
        state.register_identity(id.clone());

        assert_eq!(state.identity_count(), 1);
        let fetched = state.get_identity(&user).unwrap();
        assert_eq!(fetched.level, IdentityLevel::Verified);
        assert_eq!(fetched.country_code, *b"US");
        assert!(state.is_compliant(&user));

        let anon = Identity {
            address: addr(2),
            level: IdentityLevel::Anonymous,
            attestor,
            proof_hash: hash_bytes(b"anon"),
            country_code: *b"CH",
            attested_at: 1_000_000,
            expires_at: 0,
        };
        state.register_identity(anon);
        assert!(!state.is_compliant(&addr(2)));

        let sanctioned = Identity {
            address: addr(3),
            level: IdentityLevel::Institutional,
            attestor,
            proof_hash: hash_bytes(b"inst"),
            country_code: *b"KP",
            attested_at: 1_000_000,
            expires_at: 0,
        };
        state.register_identity(sanctioned);
        assert!(!state.is_compliant(&addr(3)));

        let expired = Identity {
            address: addr(4),
            level: IdentityLevel::Verified,
            attestor,
            proof_hash: hash_bytes(b"exp"),
            country_code: *b"DE",
            attested_at: 1_000_000,
            expires_at: 1,
        };
        state.register_identity(expired);
        assert!(!state.is_compliant(&addr(4)));

        assert_eq!(state.identity_count(), 4);
    }

    #[test]
    fn test_propose_verify_state_diff() {
        let proposer_state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 500, 0);
        let (block, _receipts) = proposer_state.execute_block(&[tx], addr(99)).unwrap();

        let affected = vec![addr(1), addr(2)];
        let diff = proposer_state.export_state_diff(&affected);
        assert_eq!(diff.new_root, block.header.state_root);

        let verifier_state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let verifier_root = verifier_state.apply_state_diff(&diff).unwrap();

        assert_eq!(verifier_root, diff.new_root);

        let sender = verifier_state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 999_500);
        let receiver = verifier_state.get_account(&addr(2)).unwrap();
        assert_eq!(receiver.balance, 500);
    }

    #[test]
    fn test_propose_verify_detects_fraud() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let affected = vec![addr(1), addr(2)];
        let mut diff = state.export_state_diff(&affected);
        diff.new_root = Hash256([0xDE; 32]);

        let verifier = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let before_root = verifier.get_state_root();
        let before_sender = verifier.get_account(&addr(1)).unwrap();
        assert!(verifier.apply_state_diff(&diff).is_err());
        assert_eq!(verifier.get_state_root(), before_root);
        let restored_sender = verifier.get_account(&addr(1)).unwrap();
        assert_eq!(restored_sender.balance, before_sender.balance);
        assert_eq!(restored_sender.nonce, before_sender.nonce);
        assert!(verifier.get_account(&addr(2)).is_none());
    }

    #[test]
    fn test_export_state_diff() {
        let db = StateDB::new();
        let addr1 = hash_bytes(&[1]);
        let addr2 = hash_bytes(&[2]);

        db.accounts.insert(addr1.0, Account::new(addr1, 1000));
        db.accounts.insert(
            addr2.0,
            Account {
                address: addr2,
                balance: 2000,
                nonce: 5,
                code_hash: Hash256::ZERO,
                storage_root: Hash256::ZERO,
                staked_balance: 0,
            },
        );

        db.dirty_accounts.insert(addr1.0);
        db.dirty_accounts.insert(addr2.0);

        let affected = vec![addr1, addr2];
        let diff = db.export_state_diff(&affected);
        assert_eq!(diff.changes.len(), 2);
        assert_ne!(diff.new_root, Hash256::ZERO);
    }

    #[test]
    fn test_apply_and_verify_state_diff() {
        let proposer_db = StateDB::new();
        let verifier_db = StateDB::new();

        let addr1 = hash_bytes(&[1]);
        let addr2 = hash_bytes(&[2]);

        proposer_db.accounts.insert(
            addr1.0,
            Account {
                address: addr1,
                balance: 900,
                nonce: 1,
                code_hash: Hash256::ZERO,
                storage_root: Hash256::ZERO,
                staked_balance: 0,
            },
        );
        proposer_db
            .accounts
            .insert(addr2.0, Account::new(addr2, 100));
        proposer_db.dirty_accounts.insert(addr1.0);
        proposer_db.dirty_accounts.insert(addr2.0);

        let affected = vec![addr1, addr2];
        let diff = proposer_db.export_state_diff(&affected);

        assert!(verifier_db.verify_state_diff(&diff));
    }

    #[test]
    fn test_fraud_detection_wrong_root() {
        let proposer_db = StateDB::new();
        let verifier_db = StateDB::new();

        let addr1 = hash_bytes(&[1]);

        proposer_db.accounts.insert(
            addr1.0,
            Account {
                address: addr1,
                balance: 900,
                nonce: 1,
                code_hash: Hash256::ZERO,
                storage_root: Hash256::ZERO,
                staked_balance: 0,
            },
        );
        proposer_db.dirty_accounts.insert(addr1.0);

        let affected = vec![addr1];
        let mut diff = proposer_db.export_state_diff(&affected);

        diff.new_root = Hash256([0xFF; 32]);

        assert!(!verifier_db.verify_state_diff(&diff));
        assert!(verifier_db.get_account(&addr1).is_none());
    }

    #[test]
    fn state_diff_rejects_duplicate_and_mismatched_account_keys_without_mutation() {
        use arc_types::{AccountChange, StateDiff};

        let state = StateDB::with_genesis(&[(addr(1), 1_000)]);
        let before_root = state.get_state_root();
        let changed = Account::new(addr(1), 123);

        let duplicate = StateDiff {
            changes: vec![
                AccountChange {
                    address: addr(1),
                    account: changed.clone(),
                },
                AccountChange {
                    address: addr(1),
                    account: changed,
                },
            ],
            new_root: Hash256::ZERO,
        };
        assert!(state.apply_state_diff(&duplicate).is_err());
        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 1_000);
        assert_eq!(state.get_state_root(), before_root);

        let mismatched_key = StateDiff {
            changes: vec![AccountChange {
                address: addr(2),
                account: Account::new(addr(3), 999_999),
            }],
            new_root: Hash256::ZERO,
        };
        assert!(state.apply_state_diff(&mismatched_key).is_err());
        assert!(state.get_account(&addr(2)).is_none());
        assert!(state.get_account(&addr(3)).is_none());
        assert_eq!(state.get_state_root(), before_root);
    }

    #[test]
    fn state_diff_root_mismatch_rolls_back_jmt_cache() {
        use arc_types::{AccountChange, StateDiff};

        let mut state = StateDB::with_genesis(&[(addr(1), 1_000)]);
        state.enable_jmt();
        let before_root = state.get_state_root();
        let diff = StateDiff {
            changes: vec![AccountChange {
                address: addr(2),
                account: Account::new(addr(2), u64::MAX),
            }],
            new_root: hash_bytes(b"attacker-declared-root"),
        };

        assert!(state.apply_state_diff(&diff).is_err());
        assert!(state.get_account(&addr(2)).is_none());
        assert_eq!(state.get_state_root(), before_root);
    }

    #[test]
    fn test_chunked_snapshot_export_single_chunk() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000), (addr(2), 2_000)]);

        let (manifest, chunks) = state.export_chunked_snapshot(100);

        assert_eq!(manifest.total_chunks, 1);
        assert_eq!(manifest.total_accounts, 2);
        assert_eq!(manifest.chunk_size, 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].total_chunks, 1);
        assert_eq!(chunks[0].accounts.len(), 2);
        assert_eq!(chunks[0].version, 0);
        assert_eq!(chunks[0].state_root, manifest.state_root);
    }

    #[test]
    fn test_chunked_snapshot_export_multiple_chunks() {
        let prefunded: Vec<(Address, u64)> = (0u8..10)
            .map(|i| (addr(i), (i as u64 + 1) * 1_000))
            .collect();
        let state = StateDB::with_genesis(&prefunded);

        let (manifest, chunks) = state.export_chunked_snapshot(3);

        assert_eq!(manifest.total_chunks, 4);
        assert_eq!(manifest.total_accounts, 10);
        assert_eq!(chunks.len(), 4);

        assert_eq!(chunks[0].accounts.len(), 3);
        assert_eq!(chunks[1].accounts.len(), 3);
        assert_eq!(chunks[2].accounts.len(), 3);
        assert_eq!(chunks[3].accounts.len(), 1);

        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.chunk_index, i as u32);
            assert_eq!(chunk.total_chunks, 4);
        }
    }

    #[test]
    fn test_chunked_snapshot_import_roundtrip() {
        let prefunded: Vec<(Address, u64)> = (0u8..5)
            .map(|i| (addr(i), (i as u64 + 1) * 1_000))
            .collect();
        let source = StateDB::with_genesis(&prefunded);
        let source_root = source.compute_state_root();

        let (_manifest, chunks) = source.export_chunked_snapshot(2);

        let dest = StateDB::new();
        let mut total_imported = 0u32;
        for chunk in &chunks {
            let count = dest.import_snapshot_chunk(chunk).unwrap();
            total_imported += count;
        }
        assert_eq!(total_imported, 5);

        for i in 0u8..5 {
            let acct = dest.get_account(&addr(i)).expect("account should exist");
            assert_eq!(acct.balance, (i as u64 + 1) * 1_000);
        }

        let dest_root = dest.compute_state_root();
        assert_eq!(dest_root, source_root);
    }

    #[test]
    fn test_chunked_snapshot_chunk_verification() {
        let state = StateDB::with_genesis(&[(addr(1), 5_000)]);

        let (_manifest, chunks) = state.export_chunked_snapshot(10);
        assert_eq!(chunks.len(), 1);

        let dest = StateDB::new();
        assert!(dest.import_snapshot_chunk(&chunks[0]).is_ok());

        let mut tampered = StateSnapshot {
            version: chunks[0].version,
            state_root: chunks[0].state_root,
            accounts: chunks[0].accounts.clone(),
            chunk_index: chunks[0].chunk_index,
            total_chunks: chunks[0].total_chunks,
            chunk_proof: Hash256([0xFF; 32]),
        };
        let dest2 = StateDB::new();
        let err = dest2.import_snapshot_chunk(&tampered).unwrap_err();
        assert!(matches!(err, StateError::ChunkVerificationFailed));

        tampered.accounts[0].1.balance = 999_999_999;
        tampered.chunk_proof = chunks[0].chunk_proof;
        let dest3 = StateDB::new();
        let err2 = dest3.import_snapshot_chunk(&tampered).unwrap_err();
        assert!(matches!(err2, StateError::ChunkVerificationFailed));
    }

    #[test]
    fn test_chunked_snapshot_manifest_hash_deterministic() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000), (addr(2), 2_000), (addr(3), 3_000)]);

        let (m1, _) = state.export_chunked_snapshot(2);
        let (m2, _) = state.export_chunked_snapshot(2);

        assert_eq!(m1.manifest_hash, m2.manifest_hash);
        assert_eq!(m1.state_root, m2.state_root);
        assert_eq!(m1.total_chunks, m2.total_chunks);

        let (m3, _) = state.export_chunked_snapshot(1);
        assert_ne!(m3.manifest_hash, m1.manifest_hash);
    }

    #[test]
    fn test_sync_progress_tracking() {
        let state = StateDB::with_genesis(&[
            (addr(1), 100),
            (addr(2), 200),
            (addr(3), 300),
            (addr(4), 400),
            (addr(5), 500),
        ]);

        let (manifest, chunks) = state.export_chunked_snapshot(2);
        assert_eq!(manifest.total_chunks, 3);

        let mut progress = StateDB::begin_sync(manifest);
        assert!(!StateDB::is_sync_complete(&progress));
        assert_eq!(progress.verified_chunks, 0);
        assert_eq!(progress.total_accounts_imported, 0);

        let dest = StateDB::new();

        let count1 = dest.import_snapshot_chunk(&chunks[2]).unwrap();
        StateDB::record_chunk(&mut progress, &chunks[2], count1).unwrap();
        assert!(!StateDB::is_sync_complete(&progress));
        assert_eq!(progress.verified_chunks, 1);

        let count2 = dest.import_snapshot_chunk(&chunks[0]).unwrap();
        StateDB::record_chunk(&mut progress, &chunks[0], count2).unwrap();
        assert!(!StateDB::is_sync_complete(&progress));
        assert_eq!(progress.verified_chunks, 2);

        let count3 = dest.import_snapshot_chunk(&chunks[1]).unwrap();
        StateDB::record_chunk(&mut progress, &chunks[1], count3).unwrap();
        assert!(StateDB::is_sync_complete(&progress));
        assert_eq!(progress.verified_chunks, 3);
        assert_eq!(
            progress.total_accounts_imported, 5,
            "all 5 accounts should be tracked"
        );
    }

    #[test]
    fn test_chunked_snapshot_empty_state() {
        let state = StateDB::new();
        let (manifest, chunks) = state.export_chunked_snapshot(10);

        assert_eq!(manifest.total_accounts, 0);
        assert_eq!(manifest.total_chunks, 1);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].accounts.len(), 0);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].total_chunks, 1);

        let dest = StateDB::new();
        let count = dest.import_snapshot_chunk(&chunks[0]).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_state_summary() {
        let state =
            StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000), (addr(3), 250_000)]);

        let summary = state.state_summary();
        assert_eq!(summary.account_count, 3);
        assert_eq!(summary.total_balance, 1_750_000);
        assert_eq!(summary.block_height, 0);
        assert_ne!(summary.state_root, Hash256::ZERO);

        let tx = Transaction::new_transfer(addr(1), addr(4), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let summary2 = state.state_summary();
        assert_eq!(summary2.account_count, 4);
        assert_eq!(summary2.total_balance, 1_750_000);
        assert_eq!(summary2.block_height, 1);
    }

    #[test]
    fn test_prune_old_receipts() {
        let state = StateDB::with_genesis(&[(addr(1), 10_000_000), (addr(2), 10_000_000)]);

        for i in 0..5 {
            let tx = Transaction::new_transfer(addr(1), addr(2), 100, i);
            state.execute_block(&[tx], addr(99)).unwrap();
        }

        assert_eq!(state.height(), 5);
        assert_eq!(state.receipts.len(), 5);

        state.prune_old_receipts(2);

        assert_eq!(state.receipts.len(), 2);
        for entry in state.receipts.iter() {
            assert!(
                entry.value().block_height > 3,
                "receipt at height {} should have been pruned",
                entry.value().block_height
            );
        }
    }

    #[test]
    fn test_prune_old_receipts_noop_when_young() {
        let state = StateDB::with_genesis(&[(addr(1), 10_000_000), (addr(2), 10_000_000)]);

        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        state.prune_old_receipts(1000);
        assert_eq!(state.receipts.len(), 1);
    }

    #[test]
    fn archive_mode_disables_receipt_transaction_and_wal_pruning_across_restart() {
        let directory = persistent_test_dir("archive-history-restart");
        let genesis_hash = hash_bytes(b"archive-history-genesis");
        let prefunded = [(addr(1), 10_000_000), (addr(2), 10_000_000)];
        let mut state =
            StateDB::with_genesis_persistent(&prefunded, &directory, genesis_hash).unwrap();
        state.enable_archive_mode();

        let mut hashes = Vec::new();
        for nonce in 0..3 {
            let transaction = Transaction::new_transfer(addr(1), addr(2), 100, nonce);
            hashes.push(transaction.hash.0);
            state.execute_block(&[transaction], addr(99)).unwrap();
        }
        state.sync_wal();
        let wal_bytes = std::fs::metadata(directory.join("state.wal"))
            .unwrap()
            .len();

        state.prune_old_receipts(0);
        state.evict_transactions(0);
        assert_eq!(state.receipts.len(), 3);
        assert_eq!(state.full_transactions.len(), 3);
        assert_eq!(
            std::fs::metadata(directory.join("state.wal"))
                .unwrap()
                .len(),
            wal_bytes
        );
        drop(state);

        let mut recovered =
            StateDB::with_genesis_persistent(&prefunded, &directory, genesis_hash).unwrap();
        recovered.enable_archive_mode();
        for hash in hashes {
            assert!(recovered.get_transaction(&hash).is_some());
            assert!(recovered.get_receipt(&hash).is_some());
        }
        recovered.evict_transactions(0);
        recovered.prune_old_receipts(0);
        assert_eq!(recovered.receipts.len(), 3);
        assert_eq!(recovered.full_transactions.len(), 3);
        drop(recovered);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn archive_lifetime_history_requires_complete_v3_block_body_and_receipt_bindings() {
        let mut state = StateDB::new();
        let recovery_context = recovery::RecoveryContext::new(
            "arc-history-test",
            hash_bytes(b"archive-history-recovery-genesis"),
            1,
            1,
        );
        *state.recovery_context.write() = Some(recovery_context.clone());
        *state.recovery_manifest_hash.write() = Some(hash_bytes(b"archive-history-manifest"));

        let mut transaction = Transaction::new_transfer(addr(1), addr(2), 1, 0);
        transaction.hash = transaction.compute_hash_in_domain(&recovery_context.domain_hash());
        let transaction_hash = transaction.hash;
        let tx_hashes = vec![transaction_hash];
        let header = BlockHeader {
            height: 0,
            timestamp: 1,
            parent_hash: Hash256::ZERO,
            tx_root: MerkleTree::from_leaves(tx_hashes.clone()).root(),
            state_root: Hash256::ZERO,
            proof_hash: Hash256::ZERO,
            tx_count: 1,
            producer: addr(9),
            protocol_version: recovery::RECOVERY_PROTOCOL_VERSION,
            state_diff: None,
        };
        let block = Block::new(header, tx_hashes);
        state.blocks.insert(0, Box::new(block.clone()));
        state.tx_index.insert(transaction_hash.0, (0, 0));
        state
            .full_transactions
            .insert(transaction_hash.0, transaction);
        *state.height.write() = 0;

        assert!(
            !state.enable_archive_mode(),
            "archive mode cannot manufacture a missing historical receipt"
        );
        assert!(!state.archive_reward_history_complete_since_v3_recovery());

        state.receipts.insert(
            transaction_hash.0,
            TxReceipt {
                tx_hash: transaction_hash,
                block_height: 0,
                block_hash: block.hash,
                index: 0,
                success: true,
                gas_used: 0,
                value_commitment: None,
                inclusion_proof: None,
                logs: Vec::new(),
            },
        );
        assert!(state.enable_archive_mode());
        assert!(state.archive_reward_history_complete_since_v3_recovery());

        state.archive_mode = false;
        state.receipts.remove(&transaction_hash.0);
        assert!(!state.enable_archive_mode());
        assert!(!state.archive_reward_history_complete_since_v3_recovery());
    }

    fn install_historical_archive_reward(
        state: &StateDB,
        historical_context: &recovery::RecoveryContext,
        current_context: &recovery::RecoveryContext,
        height: u64,
        parent_hash: Hash256,
        tag: &[u8],
    ) -> (Hash256, Block, TxReceipt) {
        let validators: Vec<_> = (0..6)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        *state.recovery_context.write() = Some(historical_context.clone());
        let transaction = make_recovery_signed_community_reward(
            state,
            &validators[0],
            &worker,
            &validators,
            height,
            tag,
        );
        *state.recovery_context.write() = Some(current_context.clone());

        let transaction_hash = transaction.hash;
        let tx_hashes = vec![transaction_hash];
        let header = BlockHeader {
            height,
            timestamp: 1_787_857_623_010 + height,
            parent_hash,
            tx_root: MerkleTree::from_leaves(tx_hashes.clone()).root(),
            state_root: Hash256::ZERO,
            proof_hash: Hash256::ZERO,
            tx_count: 1,
            producer: validators[0].address(),
            protocol_version: recovery::RECOVERY_PROTOCOL_VERSION,
            state_diff: None,
        };
        let block = Block::new(header, tx_hashes);
        let receipt = TxReceipt {
            tx_hash: transaction_hash,
            block_height: height,
            block_hash: block.hash,
            index: 0,
            success: true,
            gas_used: 0,
            value_commitment: None,
            inclusion_proof: None,
            logs: Vec::new(),
        };
        state.blocks.insert(height, Box::new(block.clone()));
        state.tx_index.insert(transaction_hash.0, (height, 0));
        state
            .full_transactions
            .insert(transaction_hash.0, transaction);
        state.receipts.insert(transaction_hash.0, receipt.clone());
        *state.height.write() = height;
        (transaction_hash, block, receipt)
    }

    #[test]
    fn archive_history_completeness_recomputes_from_durable_v3_evidence_after_restart() {
        let directory = persistent_test_dir("archive-completeness-restart");
        let genesis_hash = hash_bytes(b"archive-completeness-restart-genesis");
        let historical_context =
            recovery::RecoveryContext::new("arc-history-restart", genesis_hash, 1, 7);
        let current_context =
            recovery::RecoveryContext::new("arc-history-restart", genesis_hash, 2, 8);
        let mut state = StateDB::with_genesis_persistent(&[], &directory, genesis_hash).unwrap();
        let parent_hash = state.get_block(0).expect("durable genesis block").hash;
        let (transaction_hash, block, receipt) = install_historical_archive_reward(
            &state,
            &historical_context,
            &current_context,
            1,
            parent_hash,
            b"durable-historical-reward",
        );
        state.wal.append(WalOp::SetBlock(1, block.clone()), 1);
        state.wal.append(
            WalOp::SetFullTransaction(
                transaction_hash,
                Box::new(
                    state
                        .get_transaction(&transaction_hash.0)
                        .expect("installed full transaction"),
                ),
            ),
            1,
        );
        state
            .wal
            .append(WalOp::SetReceipt(transaction_hash, receipt.clone()), 1);
        state.wal.append(WalOp::Checkpoint(Hash256::ZERO), 1);
        state.sync_wal();

        assert!(state.enable_archive_mode());
        assert!(state.archive_reward_history_complete_since_v3_recovery());
        drop(state);

        let mut restarted =
            StateDB::with_genesis_persistent(&[], &directory, genesis_hash).unwrap();
        *restarted.recovery_context.write() = Some(current_context.clone());
        assert!(
            !restarted.archive_reward_history_complete_since_v3_recovery(),
            "the in-memory completeness marker itself must not survive restart"
        );
        assert_eq!(restarted.get_block(1).unwrap().hash, block.hash);
        assert_eq!(
            restarted.get_transaction(&transaction_hash.0).unwrap().hash,
            transaction_hash
        );
        let restored_receipt = restarted.get_receipt(&transaction_hash.0).unwrap();
        assert_eq!(restored_receipt.tx_hash, receipt.tx_hash);
        assert_eq!(restored_receipt.block_height, receipt.block_height);
        assert_eq!(restored_receipt.block_hash, receipt.block_hash);
        assert_eq!(restored_receipt.index, receipt.index);
        assert_eq!(restored_receipt.success, receipt.success);
        assert_eq!(restarted.get_tx_location(&transaction_hash.0), Some((1, 0)));
        assert!(
            restarted.enable_archive_mode(),
            "restored canonical block/body/receipt/index evidence must re-establish completeness"
        );
        assert!(restarted.archive_reward_history_complete_since_v3_recovery());
        drop(restarted);

        std::fs::remove_dir_all(directory).unwrap();
    }

    fn historical_archive_state(tag: &[u8]) -> (StateDB, Hash256, Hash256) {
        let state = StateDB::new();
        let genesis_hash = hash_bytes(b"archive-historical-domain-corruption");
        let historical_context =
            recovery::RecoveryContext::new("arc-history-corruption", genesis_hash, 1, 7);
        let current_context =
            recovery::RecoveryContext::new("arc-history-corruption", genesis_hash, 2, 8);
        let historical_domain = historical_context.domain_hash();
        let (transaction_hash, _, _) = install_historical_archive_reward(
            &state,
            &historical_context,
            &current_context,
            0,
            Hash256::ZERO,
            tag,
        );
        (state, transaction_hash, historical_domain)
    }

    #[test]
    fn archive_history_rejects_discriminator_and_body_hash_corruption_in_historical_domain() {
        let (mut valid, valid_hash, historical_domain) =
            historical_archive_state(b"historical-valid");
        assert_ne!(
            valid.transaction_domain_hash().unwrap(),
            historical_domain,
            "the fixture must exercise an earlier recovery domain"
        );
        assert_eq!(
            valid
                .get_transaction(&valid_hash.0)
                .unwrap()
                .compute_hash_in_domain(&historical_domain),
            valid_hash
        );
        assert!(valid.enable_archive_mode());

        let (mut wrong_discriminator, discriminator_hash, _) =
            historical_archive_state(b"historical-wrong-discriminator");
        wrong_discriminator
            .full_transactions
            .get_mut(&discriminator_hash.0)
            .unwrap()
            .tx_type = TxType::Transfer;
        assert!(
            !wrong_discriminator.enable_archive_mode(),
            "an envelope discriminator that differs from the retained body must fail closed"
        );
        assert!(!wrong_discriminator.archive_reward_history_complete_since_v3_recovery());

        let (mut wrong_body, body_hash, historical_domain) =
            historical_archive_state(b"historical-wrong-body-hash");
        {
            let mut transaction = wrong_body.full_transactions.get_mut(&body_hash.0).unwrap();
            let TxBody::CommunityInferenceReward(body) = &mut transaction.body else {
                panic!("historical archive fixture must contain a 0x25 body");
            };
            body.output_hash = hash_bytes(b"corrupted-retained-output");
            assert_ne!(
                transaction.compute_hash_in_domain(&historical_domain),
                body_hash
            );
        }
        assert!(
            !wrong_body.enable_archive_mode(),
            "a retained body that no longer hashes to its historical-domain key must fail closed"
        );
        assert!(!wrong_body.archive_reward_history_complete_since_v3_recovery());
    }

    #[test]
    fn test_collect_rent_deducts_balance() {
        let state = StateDB::with_genesis(&[(addr(1), 5_000_000), (addr(2), 10_000_000)]);

        let config = StateRentConfig::default();
        let rent = config.rent_per_epoch();

        let (collected, dormant) = state.collect_rent(&config);

        assert_eq!(collected, rent * 2);
        assert_eq!(dormant, 0);

        let acct1 = state.get_account(&addr(1)).unwrap();
        assert_eq!(acct1.balance, 5_000_000 - rent);

        let acct2 = state.get_account(&addr(2)).unwrap();
        assert_eq!(acct2.balance, 10_000_000 - rent);
    }

    #[test]
    fn test_collect_rent_marks_dormant() {
        let balance = 1_000_100;
        let state = StateDB::with_genesis(&[(addr(1), balance)]);

        let config = StateRentConfig::default();
        let (collected, dormant) = state.collect_rent(&config);

        assert_eq!(collected, config.rent_per_epoch());
        assert_eq!(dormant, 1);

        let acct = state.get_account(&addr(1)).unwrap();
        assert!(config.is_dormant(acct.balance));
    }

    #[test]
    fn test_collect_rent_skips_already_dormant() {
        let state = StateDB::with_genesis(&[(addr(1), 500)]);

        let config = StateRentConfig::default();
        let (collected, dormant) = state.collect_rent(&config);

        assert_eq!(collected, 0);
        assert_eq!(dormant, 1);

        let acct = state.get_account(&addr(1)).unwrap();
        assert_eq!(acct.balance, 500);
    }

    #[test]
    fn test_collect_rent_zero_rent_noop() {
        let state = StateDB::with_genesis(&[(addr(1), 5_000_000)]);

        let config = StateRentConfig {
            cost_per_byte_per_epoch: 0,
            ..Default::default()
        };

        let (collected, dormant) = state.collect_rent(&config);
        assert_eq!(collected, 0);
        assert_eq!(dormant, 0);
    }

    use arc_types::transaction::{
        BridgeLockBody, BridgeMintBody, ChannelCloseBody, ChannelDisputeBody, ChannelOpenBody,
        DeployBody, EscrowBody, InferenceRegisterBody, JoinValidatorBody, ShardProofBody,
        StakeBody, SwapBody, UpdateStakeBody, WasmCallBody,
    };

    fn make_channel_tx(from: Address, nonce: u64, body: TxBody, tx_type: TxType) -> Transaction {
        let mut tx = Transaction {
            tx_type,
            from,
            nonce,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    #[test]
    fn test_channel_open_creates_escrow() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let channel_id = hash_bytes(b"test-channel-1");

        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ChannelOpen(ChannelOpenBody {
                channel_id,
                counterparty: addr(2),
                deposit: 100_000,
                timeout_blocks: 100,
            }),
            TxType::ChannelOpen,
        );

        let (_, receipts) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(receipts[0].success, "ChannelOpen should succeed");

        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 900_000);

        let escrow_addr = hash_bytes(&[b"arc-channel", channel_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 100_000);
        assert_eq!(escrow.code_hash, addr(1));
        assert_eq!(escrow.storage_root, addr(2));
    }

    #[test]
    fn test_channel_close_releases_funds() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let channel_id = hash_bytes(b"test-channel-2");

        let open_tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ChannelOpen(ChannelOpenBody {
                channel_id,
                counterparty: addr(2),
                deposit: 100_000,
                timeout_blocks: 100,
            }),
            TxType::ChannelOpen,
        );
        let (_, r) = state.execute_block(&[open_tx], addr(99)).unwrap();
        assert!(r[0].success);

        let close_tx = make_channel_tx(
            addr(1),
            1,
            TxBody::ChannelClose(ChannelCloseBody {
                channel_id,
                opener_balance: 60_000,
                counterparty_balance: 40_000,
                counterparty_sig: vec![0u8; 64],
                state_nonce: 1,
            }),
            TxType::ChannelClose,
        );
        let (_, r) = state.execute_block(&[close_tx], addr(99)).unwrap();
        assert!(r[0].success, "ChannelClose should succeed");

        let escrow_addr = hash_bytes(&[b"arc-channel", channel_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 0);

        let opener = state.get_account(&addr(1)).unwrap();
        assert_eq!(opener.balance, 960_000);

        let counterparty = state.get_account(&addr(2)).unwrap();
        assert_eq!(counterparty.balance, 40_000);
    }

    #[test]
    fn test_channel_dispute_tracks_nonce_and_expiry() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let channel_id = hash_bytes(b"test-channel-3");

        let open_tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ChannelOpen(ChannelOpenBody {
                channel_id,
                counterparty: addr(2),
                deposit: 100_000,
                timeout_blocks: 100,
            }),
            TxType::ChannelOpen,
        );
        state.execute_block(&[open_tx], addr(99)).unwrap();

        let dispute_tx = make_channel_tx(
            addr(2),
            0,
            TxBody::ChannelDispute(ChannelDisputeBody {
                channel_id,
                opener_balance: 70_000,
                counterparty_balance: 30_000,
                other_party_sig: vec![0u8; 64],
                state_nonce: 5,
                challenge_period: 100,
            }),
            TxType::ChannelDispute,
        );
        let (_, r) = state.execute_block(&[dispute_tx], addr(99)).unwrap();
        assert!(r[0].success, "ChannelDispute should succeed");

        let escrow_addr = hash_bytes(&[b"arc-channel", channel_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.nonce, 5);
        assert!(escrow.staked_balance > 0);
        assert_eq!(escrow.balance, 100_000);
    }

    #[test]
    fn test_channel_dispute_rejects_lower_nonce() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let channel_id = hash_bytes(b"test-channel-4");

        let open_tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ChannelOpen(ChannelOpenBody {
                channel_id,
                counterparty: addr(2),
                deposit: 100_000,
                timeout_blocks: 100,
            }),
            TxType::ChannelOpen,
        );
        state.execute_block(&[open_tx], addr(99)).unwrap();

        let d1 = make_channel_tx(
            addr(2),
            0,
            TxBody::ChannelDispute(ChannelDisputeBody {
                channel_id,
                opener_balance: 60_000,
                counterparty_balance: 40_000,
                other_party_sig: vec![0u8; 64],
                state_nonce: 10,
                challenge_period: 100,
            }),
            TxType::ChannelDispute,
        );
        let (_, r) = state.execute_block(&[d1], addr(99)).unwrap();
        assert!(r[0].success);

        let d2 = make_channel_tx(
            addr(1),
            1,
            TxBody::ChannelDispute(ChannelDisputeBody {
                channel_id,
                opener_balance: 80_000,
                counterparty_balance: 20_000,
                other_party_sig: vec![0u8; 64],
                state_nonce: 5,
                challenge_period: 100,
            }),
            TxType::ChannelDispute,
        );
        let (_, r) = state.execute_block(&[d2], addr(99)).unwrap();
        assert!(!r[0].success, "Dispute with lower nonce should be rejected");
    }

    #[test]
    fn test_channel_close_blocked_during_dispute() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let channel_id = hash_bytes(b"test-channel-5");

        let open_tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ChannelOpen(ChannelOpenBody {
                channel_id,
                counterparty: addr(2),
                deposit: 100_000,
                timeout_blocks: 100,
            }),
            TxType::ChannelOpen,
        );
        state.execute_block(&[open_tx], addr(99)).unwrap();

        let dispute_tx = make_channel_tx(
            addr(2),
            0,
            TxBody::ChannelDispute(ChannelDisputeBody {
                channel_id,
                opener_balance: 60_000,
                counterparty_balance: 40_000,
                other_party_sig: vec![0u8; 64],
                state_nonce: 1,
                challenge_period: 100_000,
            }),
            TxType::ChannelDispute,
        );
        let (_, r) = state.execute_block(&[dispute_tx], addr(99)).unwrap();
        assert!(r[0].success);

        let close_tx = make_channel_tx(
            addr(1),
            1,
            TxBody::ChannelClose(ChannelCloseBody {
                channel_id,
                opener_balance: 100_000,
                counterparty_balance: 0,
                counterparty_sig: vec![0u8; 64],
                state_nonce: 1,
            }),
            TxType::ChannelClose,
        );
        let (_, r) = state.execute_block(&[close_tx], addr(99)).unwrap();
        assert!(
            !r[0].success,
            "Close should be blocked during active dispute"
        );
    }

    #[test]
    fn test_channel_dispute_balance_conservation() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let channel_id = hash_bytes(b"test-channel-6");

        let open_tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ChannelOpen(ChannelOpenBody {
                channel_id,
                counterparty: addr(2),
                deposit: 100_000,
                timeout_blocks: 100,
            }),
            TxType::ChannelOpen,
        );
        state.execute_block(&[open_tx], addr(99)).unwrap();

        let dispute_tx = make_channel_tx(
            addr(2),
            0,
            TxBody::ChannelDispute(ChannelDisputeBody {
                channel_id,
                opener_balance: 80_000,
                counterparty_balance: 40_000,
                other_party_sig: vec![0u8; 64],
                state_nonce: 1,
                challenge_period: 100,
            }),
            TxType::ChannelDispute,
        );
        let (_, r) = state.execute_block(&[dispute_tx], addr(99)).unwrap();
        assert!(
            !r[0].success,
            "Dispute exceeding deposit should be rejected"
        );
    }

    #[test]
    fn test_inference_register_locks_stake() {
        let state = StateDB::with_genesis(&[(addr(1), 100_000)]);

        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceRegister(InferenceRegisterBody {
                tier: 2,
                stake_bond: 5_000,
            }),
            TxType::InferenceRegister,
        );

        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success, "InferenceRegister should succeed");

        let acct = state.get_account(&addr(1)).unwrap();
        assert_eq!(acct.balance, 95_000);
        assert_eq!(acct.staked_balance, 5_000);
    }

    #[test]
    fn test_inference_register_rejects_insufficient_stake() {
        let state = StateDB::with_genesis(&[(addr(1), 100_000)]);

        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceRegister(InferenceRegisterBody {
                tier: 2,
                stake_bond: 1_000,
            }),
            TxType::InferenceRegister,
        );

        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(
            !r[0].success,
            "InferenceRegister with insufficient stake should fail"
        );
    }

    #[test]
    fn validator_exit_paths_preserve_overlapping_provider_bond_and_supply() {
        let worker = addr(31);
        let state = StateDB::with_genesis(&[(worker, 1_000_000)]);
        let initial_supply = 1_000_000u128;
        let assert_state = |balance, locked, validator_stake, pool, nonce| {
            let account = state.get_account(&worker).unwrap();
            assert_eq!(account.balance, balance);
            assert_eq!(account.staked_balance, locked);
            assert_eq!(account.nonce, nonce);
            assert_eq!(
                state.get_validator_stake(&worker).unwrap_or(0),
                validator_stake
            );
            assert_eq!(state.total_staked(), pool);
            assert_eq!(
                u128::from(account.balance) + u128::from(account.staked_balance),
                initial_supply
            );
        };

        let register = make_channel_tx(
            worker,
            0,
            TxBody::InferenceRegister(InferenceRegisterBody {
                tier: 4,
                stake_bond: 25_000,
            }),
            TxType::InferenceRegister,
        );
        assert!(state.execute_tx(&register).is_ok());
        assert_state(975_000, 25_000, 0, 0, 1);

        let steal_provider_bond = make_channel_tx(
            worker,
            1,
            TxBody::Stake(StakeBody {
                amount: 25_000,
                is_stake: false,
                validator: worker,
            }),
            TxType::Stake,
        );
        assert!(state.execute_tx(&steal_provider_bond).is_err());
        assert_state(975_000, 25_000, 0, 0, 1);

        let join = make_channel_tx(
            worker,
            1,
            TxBody::JoinValidator(JoinValidatorBody {
                pubkey: [0u8; 32],
                initial_stake: 100_000,
            }),
            TxType::JoinValidator,
        );
        assert!(state.execute_tx(&join).is_ok());
        assert_state(875_000, 125_000, 100_000, 100_000, 2);

        let partial_unstake = make_channel_tx(
            worker,
            2,
            TxBody::Stake(StakeBody {
                amount: 40_000,
                is_stake: false,
                validator: worker,
            }),
            TxType::Stake,
        );
        assert!(state.execute_tx(&partial_unstake).is_ok());
        assert_state(915_000, 85_000, 60_000, 60_000, 3);

        let update = make_channel_tx(
            worker,
            3,
            TxBody::UpdateStake(UpdateStakeBody { new_stake: 50_000 }),
            TxType::UpdateStake,
        );
        assert!(state.execute_tx(&update).is_ok());
        assert_state(925_000, 75_000, 50_000, 50_000, 4);

        let leave = make_channel_tx(worker, 4, TxBody::LeaveValidator, TxType::LeaveValidator);
        assert!(state.execute_tx(&leave).is_ok());
        assert_state(975_000, 25_000, 0, 0, 5);

        let second_steal = make_channel_tx(
            worker,
            5,
            TxBody::Stake(StakeBody {
                amount: 25_000,
                is_stake: false,
                validator: worker,
            }),
            TxType::Stake,
        );
        assert!(state.execute_tx(&second_steal).is_err());
        assert_state(975_000, 25_000, 0, 0, 5);
    }

    #[test]
    fn validator_exit_arithmetic_failure_is_atomic() {
        let validator = addr(32);
        let state = StateDB::with_genesis(&[(validator, u64::MAX)]);
        let mut account = state.get_account(&validator).unwrap();
        account.staked_balance = StateDB::MIN_VALIDATOR_STAKE;
        state.accounts.insert(validator.0, account.clone());
        state
            .validators
            .insert(validator.0, StateDB::MIN_VALIDATOR_STAKE);
        state
            .staking_pool
            .store(StateDB::MIN_VALIDATOR_STAKE, Ordering::Release);
        let leave = make_channel_tx(validator, 0, TxBody::LeaveValidator, TxType::LeaveValidator);
        let error = state.execute_tx(&leave).unwrap_err();
        assert!(error.to_string().contains("balance overflow"));
        let unchanged = state.get_account(&validator).unwrap();
        assert_eq!(unchanged.balance, u64::MAX);
        assert_eq!(unchanged.staked_balance, StateDB::MIN_VALIDATOR_STAKE);
        assert_eq!(unchanged.nonce, 0);
        assert_eq!(
            state.get_validator_stake(&validator),
            Some(StateDB::MIN_VALIDATOR_STAKE)
        );
        assert_eq!(state.total_staked(), StateDB::MIN_VALIDATOR_STAKE);

        state.staking_pool.store(0, Ordering::Release);
        let error = state.execute_tx(&leave).unwrap_err();
        assert!(error.to_string().contains("balance overflow"));
        let mut account = state.get_account(&validator).unwrap();
        account.balance = 0;
        state.accounts.insert(validator.0, account);
        let error = state.execute_tx(&leave).unwrap_err();
        assert!(error.to_string().contains("staking pool underflow"));
        let unchanged = state.get_account(&validator).unwrap();
        assert_eq!(unchanged.balance, 0);
        assert_eq!(unchanged.staked_balance, StateDB::MIN_VALIDATOR_STAKE);
        assert_eq!(unchanged.nonce, 0);
        assert_eq!(
            state.get_validator_stake(&validator),
            Some(StateDB::MIN_VALIDATOR_STAKE)
        );
        assert_eq!(state.total_staked(), 0);
    }

    #[test]
    fn recovery_v3_allowlist_classifies_every_family_and_rejects_unsafe_mutations() {
        use V3TransactionFamilyPolicy::{Allowed, Denied};

        let policies = [
            (TxType::Transfer, Allowed),
            (TxType::Settle, Denied),
            (TxType::Swap, Denied),
            (TxType::Escrow, Denied),
            (TxType::Stake, Denied),
            (TxType::WasmCall, Denied),
            (TxType::MultiSig, Denied),
            (TxType::DeployContract, Denied),
            (TxType::RegisterAgent, Denied),
            (TxType::JoinValidator, Denied),
            (TxType::LeaveValidator, Denied),
            (TxType::ClaimRewards, Denied),
            (TxType::UpdateStake, Denied),
            (TxType::Governance, Denied),
            (TxType::BridgeLock, Denied),
            (TxType::BridgeMint, Denied),
            (TxType::BatchSettle, Denied),
            (TxType::ChannelOpen, Denied),
            (TxType::ChannelClose, Denied),
            (TxType::ChannelDispute, Denied),
            (TxType::ShardProof, Denied),
            (TxType::InferenceAttestation, Denied),
            (TxType::InferenceChallenge, Denied),
            (TxType::InferenceRegister, Denied),
            (TxType::InferenceEscrowOpen, Denied),
            (TxType::InferenceEscrowRelease, Denied),
            (TxType::InferenceEscrowRefund, Denied),
            (TxType::ModelRegistration, Denied),
            (TxType::ModelRequest, Denied),
            (TxType::ShardCoverageClaim, Denied),
            (TxType::CapacityAdvertisement, Denied),
            (TxType::ShardAssignmentProposal, Denied),
            (TxType::FaucetClaim, Allowed),
            (TxType::InferenceRequest, Denied),
            (TxType::InferenceVote, Denied),
            (TxType::InferenceFinalize, Denied),
            (TxType::CommunityInferenceReward, Allowed),
        ];
        assert_eq!(policies.len(), 0x25);
        let mut discriminants: Vec<_> =
            policies.iter().map(|(tx_type, _)| *tx_type as u8).collect();
        discriminants.sort_unstable();
        assert_eq!(discriminants, (1u8..=0x25).collect::<Vec<_>>());
        for (tx_type, expected) in policies {
            assert_eq!(
                StateDB::v3_transaction_family_policy(tx_type),
                expected,
                "unexpected v3 policy for {tx_type:?}"
            );
        }

        let directory = persistent_test_dir("v3-denylist");
        let signer = arc_crypto::KeyPair::generate_ed25519();
        let counterparty = addr(41);
        let validators: Vec<_> = (0..6)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let mut prefunded = vec![(signer.address(), 1_000_000), (counterparty, 500_000)];
        prefunded.extend(validators.iter().map(|validator| (validator.address(), 0)));
        let state = StateDB::with_genesis_persistent(
            &prefunded,
            &directory,
            hash_bytes(b"v3-denylist-genesis"),
        )
        .unwrap();
        let validator_set: Vec<_> = validators
            .iter()
            .enumerate()
            .map(|(index, validator)| {
                (
                    validator.address(),
                    if index < 4 { 6_666_667 } else { 6_666_666 },
                )
            })
            .collect();
        state.seed_genesis_validators(&validator_set);
        state.staking_pool.store(
            validator_set.iter().map(|entry| entry.1).sum(),
            Ordering::Release,
        );
        *state.recovery_context.write() = Some(crate::recovery::RecoveryContext::new(
            "0x415243",
            hash_bytes(b"v3-denylist-genesis"),
            1,
            1,
        ));
        state.wal.sync().unwrap();

        let channel_id = hash_bytes(b"unsafe-channel");
        let denied = vec![
            (
                "swap",
                TxBody::Swap(SwapBody {
                    counterparty,
                    offer_amount: 1,
                    receive_amount: 500_000,
                    offer_asset: Hash256::ZERO,
                    receive_asset: Hash256::ZERO,
                }),
            ),
            (
                "escrow-release",
                TxBody::Escrow(EscrowBody {
                    beneficiary: signer.address(),
                    amount: u64::MAX,
                    conditions_hash: Hash256::ZERO,
                    is_create: false,
                }),
            ),
            (
                "stake",
                TxBody::Stake(StakeBody {
                    amount: 100_000,
                    is_stake: true,
                    validator: signer.address(),
                }),
            ),
            (
                "wasm-call",
                TxBody::WasmCall(WasmCallBody {
                    contract: addr(42),
                    function: "trap".into(),
                    calldata: vec![0xff],
                    value: 1,
                    gas_limit: 1,
                }),
            ),
            (
                "deploy-invalid-compile",
                TxBody::DeployContract(DeployBody {
                    bytecode: b"\0asm-invalid".to_vec(),
                    constructor_args: vec![0xff],
                    state_rent_deposit: 1,
                }),
            ),
            (
                "join-validator",
                TxBody::JoinValidator(JoinValidatorBody {
                    pubkey: [1u8; 32],
                    initial_stake: 100_000,
                }),
            ),
            ("leave-validator", TxBody::LeaveValidator),
            (
                "update-stake",
                TxBody::UpdateStake(UpdateStakeBody { new_stake: 1 }),
            ),
            (
                "bridge-lock",
                TxBody::BridgeLock(BridgeLockBody {
                    destination_chain: 1,
                    destination_address: [2u8; 32],
                    amount: 10,
                }),
            ),
            (
                "bridge-mint",
                TxBody::BridgeMint(BridgeMintBody {
                    source_chain: 1,
                    source_tx_hash: hash_bytes(b"replayable-source"),
                    recipient: signer.address(),
                    amount: u64::MAX,
                    merkle_proof: vec![0],
                }),
            ),
            (
                "channel-open",
                TxBody::ChannelOpen(ChannelOpenBody {
                    channel_id,
                    counterparty,
                    deposit: 10,
                    timeout_blocks: 10,
                }),
            ),
            (
                "channel-close",
                TxBody::ChannelClose(ChannelCloseBody {
                    channel_id,
                    opener_balance: u64::MAX,
                    counterparty_balance: 0,
                    counterparty_sig: vec![],
                    state_nonce: 1,
                }),
            ),
            (
                "channel-dispute",
                TxBody::ChannelDispute(ChannelDisputeBody {
                    channel_id,
                    opener_balance: 1,
                    counterparty_balance: 1,
                    other_party_sig: vec![],
                    state_nonce: 1,
                    challenge_period: 1,
                }),
            ),
            (
                "shard-proof",
                TxBody::ShardProof(ShardProofBody {
                    shard_id: 0,
                    block_height: 1,
                    block_hash: hash_bytes(b"forged-shard-block"),
                    prev_state_root: Hash256::ZERO,
                    post_state_root: hash_bytes(b"forged-root"),
                    tx_count: 1,
                    proof_data: vec![0],
                }),
            ),
        ];

        let before_account =
            bincode::serialize(&state.get_account(&signer.address()).unwrap()).unwrap();
        let before_counterparty =
            bincode::serialize(&state.get_account(&counterparty).unwrap()).unwrap();
        let before_root = state.get_state_root();
        let before_height = state.height();
        let before_accounts = state.accounts.len();
        let mut before_validators = state.active_validators();
        before_validators.sort_by_key(|entry| entry.0.0);
        let before_wal = std::fs::metadata(directory.join("state.wal"))
            .unwrap()
            .len();
        let before_supply: u128 = state
            .accounts
            .iter()
            .map(|account| {
                u128::from(account.value().balance) + u128::from(account.value().staked_balance)
            })
            .sum();

        for (label, body) in denied {
            let mut transaction =
                make_channel_tx(signer.address(), 0, body.clone(), body.tx_type());
            state.sign_transaction(&mut transaction, &signer).unwrap();
            state.verify_transaction_signature(&transaction).unwrap();
            let error = state
                .execute_tx(&transaction)
                .expect_err("v3 denylisted transaction must fail before execution");
            assert!(
                error.to_string().contains("unavailable"),
                "{label}: {error}"
            );
            state.wal.sync().unwrap();
            assert_eq!(
                bincode::serialize(&state.get_account(&signer.address()).unwrap()).unwrap(),
                before_account,
                "{label} changed signer"
            );
            assert_eq!(
                bincode::serialize(&state.get_account(&counterparty).unwrap()).unwrap(),
                before_counterparty,
                "{label} changed counterparty"
            );
            assert_eq!(state.get_state_root(), before_root, "{label} changed root");
            assert_eq!(state.height(), before_height, "{label} changed height");
            assert_eq!(
                state.accounts.len(),
                before_accounts,
                "{label} created account"
            );
            assert_eq!(
                std::fs::metadata(directory.join("state.wal"))
                    .unwrap()
                    .len(),
                before_wal,
                "{label} wrote WAL"
            );
            assert_eq!(
                state.active_validators().len(),
                6,
                "{label} changed committee"
            );
            let mut current_validators = state.active_validators();
            current_validators.sort_by_key(|entry| entry.0.0);
            assert_eq!(
                current_validators, before_validators,
                "{label} changed validator identities/stakes"
            );
            assert_eq!(
                state
                    .accounts
                    .iter()
                    .map(|account| {
                        u128::from(account.value().balance)
                            + u128::from(account.value().staked_balance)
                    })
                    .sum::<u128>(),
                before_supply,
                "{label} changed supply"
            );
        }

        for index in 0..32u8 {
            let fresh_worker = arc_crypto::KeyPair::generate_ed25519();
            assert!(state.get_account(&fresh_worker.address()).is_none());
            let mut attestation = make_attestation(
                fresh_worker.address(),
                0,
                0,
                100,
                &[b"v3-attestation-flood-".as_slice(), &[index]].concat(),
            );
            state
                .sign_transaction(&mut attestation, &fresh_worker)
                .unwrap();
            let error = state.execute_tx(&attestation).unwrap_err();
            assert!(error.to_string().contains("unavailable"));
            assert!(
                state.get_account(&fresh_worker.address()).is_none(),
                "raw attestation {index} created a sender account"
            );
        }
        state.wal.sync().unwrap();
        assert_eq!(state.accounts.len(), before_accounts);
        assert_eq!(state.get_state_root(), before_root);
        assert_eq!(
            std::fs::metadata(directory.join("state.wal"))
                .unwrap()
                .len(),
            before_wal,
            "rejected raw-attestation flood wrote WAL"
        );

        let member = &validators[0];
        for body in [
            TxBody::LeaveValidator,
            TxBody::UpdateStake(UpdateStakeBody { new_stake: 0 }),
        ] {
            let mut transaction =
                make_channel_tx(member.address(), 0, body.clone(), body.tx_type());
            state.sign_transaction(&mut transaction, member).unwrap();
            assert!(state.execute_tx(&transaction).is_err());
        }
        assert_eq!(state.active_validators().len(), 6);
        let mut current_validators = state.active_validators();
        current_validators.sort_by_key(|entry| entry.0.0);
        assert_eq!(current_validators, before_validators);
        assert_eq!(state.get_state_root(), before_root);

        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recovery_v3_transfer_is_checked_atomic_and_domain_authenticated() {
        let directory = persistent_test_dir("v3-transfer-atomic");
        let sender = arc_crypto::KeyPair::generate_ed25519();
        let receiver = arc_crypto::KeyPair::generate_ed25519();
        let overflow_receiver = arc_crypto::KeyPair::generate_ed25519();
        let second_sender = arc_crypto::KeyPair::generate_ed25519();
        let second_receiver = arc_crypto::KeyPair::generate_ed25519();
        let wrong_signer = arc_crypto::KeyPair::generate_ed25519();
        let state = StateDB::with_genesis_persistent(
            &[
                (sender.address(), 100),
                (receiver.address(), 0),
                (overflow_receiver.address(), u64::MAX),
                (second_sender.address(), 100),
                (second_receiver.address(), 0),
            ],
            &directory,
            hash_bytes(b"v3-transfer-atomic-genesis"),
        )
        .unwrap();
        *state.recovery_context.write() = Some(crate::recovery::RecoveryContext::new(
            "0x415243",
            hash_bytes(b"v3-transfer-atomic-genesis"),
            1,
            1,
        ));
        state.wal.sync().unwrap();

        let chain_domain =
            arc_types::transaction::CommunityInferenceRewardBody::expected_chain_domain();
        let digest = hash_bytes(b"v3-transfer-dust-target");
        let mut probe_id = [0u8; 32];
        probe_id[..arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX.len()]
            .copy_from_slice(&arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX);
        probe_id[arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX.len()..].fill(4);
        let reserved_targets = [
            arc_types::transaction::CommunityInferenceRewardBody::v3_marker_address(
                &chain_domain,
                &digest,
            ),
            arc_types::transaction::CommunityInferenceRewardBody::v3_certificate_marker_address(
                &chain_domain,
                &receiver.address(),
                &digest,
            ),
            arc_types::transaction::CommunityInferenceRewardBody::v3_recovery_probe_marker_address(
                &chain_domain,
                &Hash256(probe_id),
            )
            .unwrap(),
            community_reward_block_budget_address(7),
            community_reward_epoch_budget_address(8),
            community_reward_worker_budget_address(8, &receiver.address()),
            community_reward_coordinator_budget_address(8, &sender.address()),
            arc_types::transaction::FaucetClaimBody::v3_marker_address(&receiver.address()),
        ];
        for target in reserved_targets {
            assert!(is_reserved_system_account(&target));
            let mut dust = make_channel_tx(
                sender.address(),
                0,
                TxBody::Transfer(TransferBody {
                    to: target,
                    amount: 1,
                    amount_commitment: None,
                }),
                TxType::Transfer,
            );
            dust.fee = V3_MIN_TRANSFER_FEE;
            state.sign_transaction(&mut dust, &sender).unwrap();
            let error = state.validate_v3_transaction_admission(&dust).unwrap_err();
            assert!(error.to_string().contains("collision"), "{error}");
            assert!(state.execute_tx(&dust).is_err());
            assert!(state.get_account(&target).is_none());
        }
        assert_eq!(state.get_account(&sender.address()).unwrap().balance, 100);
        assert_eq!(state.get_account(&sender.address()).unwrap().nonce, 0);

        let mut zero_fee = make_channel_tx(
            sender.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: receiver.address(),
                amount: 1,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        state.sign_transaction(&mut zero_fee, &sender).unwrap();
        let error = state
            .validate_v3_transaction_admission(&zero_fee)
            .unwrap_err();
        assert!(error.to_string().contains("below minimum"), "{error}");
        assert!(state.execute_tx(&zero_fee).is_err());
        assert_eq!(state.get_account(&sender.address()).unwrap().balance, 100);

        let mut self_transfer = make_channel_tx(
            sender.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: sender.address(),
                amount: 75,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        self_transfer.fee = V3_MIN_TRANSFER_FEE;
        state.sign_transaction(&mut self_transfer, &sender).unwrap();
        assert!(
            state
                .execute_tx(&self_transfer)
                .unwrap_err()
                .to_string()
                .contains("collision")
        );
        let after_self = state.get_account(&sender.address()).unwrap();
        assert_eq!(after_self.balance, 100);
        assert_eq!(after_self.nonce, 0);

        state.wal.sync().unwrap();
        let before_sender = state.get_account(&sender.address()).unwrap();
        let before_overflow = state.get_account(&overflow_receiver.address()).unwrap();
        let before_accounts = state.accounts.len();
        let before_wal = std::fs::metadata(directory.join("state.wal"))
            .unwrap()
            .len();
        let mut overflow = make_channel_tx(
            sender.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: overflow_receiver.address(),
                amount: 1,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        overflow.fee = V3_MIN_TRANSFER_FEE;
        state.sign_transaction(&mut overflow, &sender).unwrap();
        assert!(
            state
                .execute_tx(&overflow)
                .unwrap_err()
                .to_string()
                .contains("overflow")
        );
        state.wal.sync().unwrap();
        assert_eq!(
            bincode::serialize(&state.get_account(&sender.address()).unwrap()).unwrap(),
            bincode::serialize(&before_sender).unwrap()
        );
        assert_eq!(
            bincode::serialize(&state.get_account(&overflow_receiver.address()).unwrap()).unwrap(),
            bincode::serialize(&before_overflow).unwrap()
        );
        assert_eq!(state.accounts.len(), before_accounts);
        assert_eq!(
            std::fs::metadata(directory.join("state.wal"))
                .unwrap()
                .len(),
            before_wal
        );

        let missing = arc_crypto::KeyPair::generate_ed25519();
        let mut missing_tx = make_channel_tx(
            missing.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: receiver.address(),
                amount: 1,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        missing_tx.fee = V3_MIN_TRANSFER_FEE;
        state.sign_transaction(&mut missing_tx, &missing).unwrap();
        assert!(matches!(
            state.execute_tx(&missing_tx),
            Err(StateError::InsufficientBalance { have: 0, need: 2 })
        ));
        assert!(state.get_account(&missing.address()).is_none());

        let mut forged = make_channel_tx(
            sender.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: receiver.address(),
                amount: 1,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        forged.fee = V3_MIN_TRANSFER_FEE;
        forged.sig_verified = true;
        assert!(
            state
                .execute_tx(&forged)
                .unwrap_err()
                .to_string()
                .contains("signature")
        );
        assert_eq!(
            bincode::serialize(&state.get_account(&sender.address()).unwrap()).unwrap(),
            bincode::serialize(&before_sender).unwrap()
        );

        let mut transfer = make_channel_tx(
            sender.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: receiver.address(),
                amount: 40,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        transfer.fee = V3_MIN_TRANSFER_FEE;
        state.sign_transaction(&mut transfer, &sender).unwrap();
        state
            .validate_v3_transaction_admission_at(&transfer, state.height() + 1)
            .unwrap();
        state
            .validate_v3_block_admission_at(std::slice::from_ref(&transfer), state.height() + 1)
            .unwrap();

        let mut denied_attestation =
            make_attestation(sender.address(), 0, 0, 100, b"denied-block-attestation");
        state
            .sign_transaction(&mut denied_attestation, &sender)
            .unwrap();

        let mut bad_nonce = make_channel_tx(
            sender.address(),
            9,
            TxBody::Transfer(TransferBody {
                to: receiver.address(),
                amount: 1,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        bad_nonce.fee = V3_MIN_TRANSFER_FEE;
        state.sign_transaction(&mut bad_nonce, &sender).unwrap();

        let mut invalid_signature = make_channel_tx(
            sender.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: receiver.address(),
                amount: 1,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        invalid_signature.fee = V3_MIN_TRANSFER_FEE;
        state
            .sign_transaction(&mut invalid_signature, &wrong_signer)
            .unwrap();

        let mut wrong_domain = make_channel_tx(
            sender.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: receiver.address(),
                amount: 1,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        wrong_domain.fee = V3_MIN_TRANSFER_FEE;
        wrong_domain
            .sign(&sender)
            .expect("legacy-domain signature is well-formed but wrong for v3");

        let mut same_sender_second = make_channel_tx(
            sender.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: second_receiver.address(),
                amount: 2,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        same_sender_second.fee = V3_MIN_TRANSFER_FEE;
        state
            .sign_transaction(&mut same_sender_second, &sender)
            .unwrap();

        let mut overlapping_treasury = make_channel_tx(
            second_sender.address(),
            0,
            TxBody::Transfer(TransferBody {
                to: second_receiver.address(),
                amount: 3,
                amount_commitment: None,
            }),
            TxType::Transfer,
        );
        overlapping_treasury.fee = V3_MIN_TRANSFER_FEE;
        state
            .sign_transaction(&mut overlapping_treasury, &second_sender)
            .unwrap();
        state
            .validate_v3_transaction_admission_at(&overlapping_treasury, state.height() + 1)
            .unwrap();

        state.wal.sync().unwrap();
        let fingerprint = || {
            let mut accounts: Vec<_> = state
                .accounts
                .iter()
                .map(|entry| {
                    (
                        *entry.key(),
                        bincode::serialize(entry.value()).expect("account serializes"),
                    )
                })
                .collect();
            accounts.sort_by_key(|entry| entry.0);
            (
                state.height(),
                state.get_state_root(),
                accounts,
                state.blocks.len(),
                state.receipts.len(),
                state.tx_index.len(),
                state.full_transactions.len(),
                std::fs::metadata(directory.join("state.wal"))
                    .unwrap()
                    .len(),
            )
        };
        let before_rejections = fingerprint();
        let oversized = vec![transfer.clone(); V3_MAX_TRANSACTIONS_PER_BLOCK + 1];
        let rejected_blocks = vec![
            ("denied family", vec![denied_attestation]),
            ("bad nonce", vec![bad_nonce]),
            ("invalid signature", vec![invalid_signature]),
            ("wrong recovery domain", vec![wrong_domain]),
            ("duplicate hash", vec![transfer.clone(), transfer.clone()]),
            ("same sender", vec![transfer.clone(), same_sender_second]),
            (
                "overlapping state access",
                vec![transfer.clone(), overlapping_treasury],
            ),
            ("too many transactions", oversized),
        ];
        for (label, candidate) in rejected_blocks {
            let error = state
                .execute_block_verified_at(&candidate, sender.address(), 1_787_857_623_001)
                .expect_err("invalid v3 block must fail before state mutation");
            state.wal.sync().unwrap();
            assert_eq!(fingerprint(), before_rejections, "{label}: {error}");
        }

        let (block, receipts) = state
            .execute_block_verified_at(
                std::slice::from_ref(&transfer),
                sender.address(),
                1_787_857_623_002,
            )
            .unwrap();
        assert_eq!(block.header.height, 1);
        assert!(receipts[0].success);
        assert_eq!(state.get_account(&sender.address()).unwrap().balance, 59);
        assert_eq!(state.get_account(&sender.address()).unwrap().nonce, 1);
        assert_eq!(state.get_account(&receiver.address()).unwrap().balance, 40);
        assert_eq!(
            state
                .get_account(&v3_fee_treasury_address())
                .unwrap()
                .balance,
            V3_MIN_TRANSFER_FEE
        );

        drop(state);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recovery_v3_faucet_rejects_reserved_and_marker_aliases_without_minting() {
        let validators: Vec<_> = (0..6)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let validator = &validators[0];
        let ordinary_recipient = arc_crypto::KeyPair::generate_ed25519().address();
        let pool = arc_types::transaction::faucet_pool_address();
        let reward_treasury = arc_types::transaction::inference_reward_treasury_address();
        let recovery_reserve = hash_bytes(&[2u8]);
        let legacy_bridge = hash_bytes(b"ARC-bridge-escrow");
        let legacy_model_treasury = hash_bytes(b"arc-treasury");
        let chain_domain =
            arc_types::transaction::CommunityInferenceRewardBody::expected_chain_domain();
        let namespace_digest = hash_bytes(b"v3-reserved-namespace-admission-test");
        let mut probe_id = [0u8; 32];
        probe_id[..arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX.len()]
            .copy_from_slice(&arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX);
        probe_id[arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX.len()..].fill(3);
        let dynamic_reserved = [
            arc_types::transaction::CommunityInferenceRewardBody::v3_marker_address(
                &chain_domain,
                &namespace_digest,
            ),
            arc_types::transaction::CommunityInferenceRewardBody::v3_certificate_marker_address(
                &chain_domain,
                &ordinary_recipient,
                &namespace_digest,
            ),
            arc_types::transaction::CommunityInferenceRewardBody::v3_recovery_probe_marker_address(
                &chain_domain,
                &Hash256(probe_id),
            )
            .unwrap(),
            community_reward_block_budget_address(42),
            community_reward_epoch_budget_address(9),
            community_reward_worker_budget_address(9, &ordinary_recipient),
            community_reward_coordinator_budget_address(9, &validator.address()),
            FaucetClaimBody::v3_marker_address(&ordinary_recipient),
        ];
        assert!(
            dynamic_reserved.iter().all(is_reserved_system_account),
            "every dynamic state-owned v3 family must be admission-recognizable"
        );
        let mut genesis = vec![
            (pool, 10 * arc_types::transaction::FAUCET_CLAIM_MAX),
            (reward_treasury, 20),
            (recovery_reserve, 30),
            (legacy_bridge, 40),
            (legacy_model_treasury, 50),
        ];
        genesis.extend(validators.iter().map(|key| (key.address(), 0)));
        let state = StateDB::with_genesis(&genesis);
        state.seed_genesis_validators(
            &validators
                .iter()
                .map(|key| (key.address(), StateDB::MIN_VALIDATOR_STAKE))
                .collect::<Vec<_>>(),
        );
        *state.recovery_context.write() = Some(crate::recovery::RecoveryContext::new(
            "0x415243",
            hash_bytes(b"v3-faucet-alias-genesis"),
            1,
            1,
        ));
        let initial_supply = sum_all_balances(&state);

        let mut forbidden_recipients = vec![
            pool,
            reward_treasury,
            recovery_reserve,
            legacy_bridge,
            legacy_model_treasury,
        ];
        forbidden_recipients.extend(dynamic_reserved);
        for recipient in forbidden_recipients {
            let mut claim = make_channel_tx(
                validator.address(),
                0,
                TxBody::FaucetClaim(FaucetClaimBody {
                    recipient,
                    amount: 1,
                }),
                TxType::FaucetClaim,
            );
            state.sign_transaction(&mut claim, validator).unwrap();
            assert!(
                state
                    .execute_tx(&claim)
                    .unwrap_err()
                    .to_string()
                    .contains("collision")
            );
            assert_eq!(sum_all_balances(&state), initial_supply);
        }

        let mut valid = make_channel_tx(
            validator.address(),
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient: ordinary_recipient,
                amount: 1,
            }),
            TxType::FaucetClaim,
        );
        state.sign_transaction(&mut valid, validator).unwrap();
        let legacy_marker = FaucetClaimBody::marker_address(&ordinary_recipient);
        let mut genuine_legacy_marker = Account::new(legacy_marker, 0);
        genuine_legacy_marker.nonce = 1;
        genuine_legacy_marker.code_hash = ordinary_recipient;
        genuine_legacy_marker.storage_root = hash_bytes(&1u64.to_be_bytes());
        state
            .accounts
            .insert(legacy_marker.0, genuine_legacy_marker);
        let legacy_replay = state.execute_tx(&valid).unwrap_err();
        assert!(
            legacy_replay
                .to_string()
                .contains("legacy marker semantics")
        );

        let mut pool_for_dust = state.get_account(&pool).unwrap();
        pool_for_dust.balance -= 1;
        state.accounts.insert(pool.0, pool_for_dust);
        state
            .accounts
            .insert(legacy_marker.0, Account::new(legacy_marker, 1));
        state.execute_tx(&valid).unwrap();
        let marker = FaucetClaimBody::v3_marker_address(&ordinary_recipient);
        assert_eq!(state.get_account(&legacy_marker).unwrap().balance, 1);
        let supply_after_valid = sum_all_balances(&state);
        assert_eq!(supply_after_valid, initial_supply);
        let pool_after_valid = state.get_account(&pool).unwrap().balance;

        let mut marker_claim = make_channel_tx(
            validator.address(),
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient: marker,
                amount: 1,
            }),
            TxType::FaucetClaim,
        );
        state
            .sign_transaction(&mut marker_claim, validator)
            .unwrap();
        assert!(
            state
                .execute_tx(&marker_claim)
                .unwrap_err()
                .to_string()
                .contains("collision")
        );
        assert_eq!(state.get_account(&pool).unwrap().balance, pool_after_valid);
        assert_eq!(sum_all_balances(&state), supply_after_valid);
    }

    use arc_types::transaction::{
        InferenceEscrowOpenBody, InferenceEscrowRefundBody, InferenceEscrowReleaseBody,
    };

    fn req(tag: &[u8]) -> [u8; 32] {
        hash_bytes(&[b"req-", tag].concat()).0
    }

    fn model_id() -> Hash256 {
        hash_bytes(b"llama-2-7b-test-model")
    }

    #[test]
    fn test_escrow_open_debits_payer_credits_escrow() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let request_id = req(b"open-happy");

        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 10_000,
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowOpen,
        );
        let (_, receipts) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(receipts[0].success, "InferenceEscrowOpen should succeed");

        let payer = state.get_account(&addr(1)).unwrap();
        assert_eq!(payer.balance, 990_000);
        assert_eq!(payer.nonce, 1);

        let escrow_addr = Hash256(InferenceEscrowOpenBody::escrow_address(&request_id));
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 10_000);
        let expected = InferenceEscrowOpenBody::metadata_commitment(&addr(1), &model_id(), 32, 10);
        assert_eq!(escrow.storage_root.0, expected);
    }

    #[test]
    fn test_escrow_open_rejects_insufficient_balance() {
        let state = StateDB::with_genesis(&[(addr(1), 100)]);
        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id: req(b"too-broke"),
                model_id: model_id(),
                max_fee: 10_000,
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowOpen,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(!r[0].success, "open should reject insufficient balance");
        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 100);
    }

    #[test]
    fn test_escrow_open_rejects_double_open_same_request_id() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let request_id = req(b"double-open");
        let tx1 = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 10_000,
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowOpen,
        );
        let (_, r1) = state.execute_block(&[tx1], addr(99)).unwrap();
        assert!(r1[0].success);

        let tx2 = make_channel_tx(
            addr(1),
            1,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 10_000,
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowOpen,
        );
        let (_, r2) = state.execute_block(&[tx2], addr(99)).unwrap();
        assert!(!r2[0].success, "second open on same request_id must fail");
    }

    #[test]
    fn test_escrow_release_distributes_40_25_15_20_and_conserves_total() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let request_id = req(b"release-split");

        let open = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 10_000,
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowOpen,
        );
        let release = make_channel_tx(
            addr(2),
            0,
            TxBody::InferenceEscrowRelease(InferenceEscrowReleaseBody {
                request_id,
                payer: addr(1),
                model_id: model_id(),
                max_tokens: 32,
                timeout_blocks: 10,
                output_hash: hash_bytes(b"sample-output"),
                proposer: addr(2),
                replicas: vec![addr(3), addr(4), addr(5)],
                observer_pool: addr(10),
                treasury: addr(11),
            }),
            TxType::InferenceEscrowRelease,
        );
        let (_, rs) = state.execute_block(&[open, release], addr(99)).unwrap();
        assert!(rs[0].success, "open must succeed");
        assert!(rs[1].success, "release must succeed");

        let payer = state.get_account(&addr(1)).unwrap();
        let proposer = state.get_account(&addr(2)).unwrap();
        let r1 = state.get_account(&addr(3)).unwrap();
        let r2 = state.get_account(&addr(4)).unwrap();
        let r3 = state.get_account(&addr(5)).unwrap();
        let obs = state.get_account(&addr(10)).unwrap();
        let tre = state.get_account(&addr(11)).unwrap();
        let escrow_addr = Hash256(InferenceEscrowOpenBody::escrow_address(&request_id));
        let escrow = state.get_account(&escrow_addr).unwrap();

        assert_eq!(proposer.balance, 4_000);
        assert_eq!(r1.balance, 833);
        assert_eq!(r2.balance, 833);
        assert_eq!(r3.balance, 833);
        assert_eq!(obs.balance, 1_500);
        assert_eq!(tre.balance, 2_001);

        let credited =
            proposer.balance + r1.balance + r2.balance + r3.balance + obs.balance + tre.balance;
        assert_eq!(credited, 10_000);
        assert_eq!(payer.balance, 990_000);

        assert_eq!(escrow.balance, 0);
        assert_eq!(escrow.storage_root, Hash256::ZERO);
    }

    #[test]
    fn test_escrow_release_rejects_wrong_payer() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let request_id = req(b"wrong-payer");
        let open = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 5_000,
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowOpen,
        );
        let bad_release = make_channel_tx(
            addr(2),
            0,
            TxBody::InferenceEscrowRelease(InferenceEscrowReleaseBody {
                request_id,
                payer: addr(42),
                model_id: model_id(),
                max_tokens: 32,
                timeout_blocks: 10,
                output_hash: hash_bytes(b"x"),
                proposer: addr(2),
                replicas: vec![addr(3)],
                observer_pool: addr(10),
                treasury: addr(11),
            }),
            TxType::InferenceEscrowRelease,
        );
        let (_, rs) = state.execute_block(&[open, bad_release], addr(99)).unwrap();
        assert!(rs[0].success);
        assert!(!rs[1].success, "release with wrong payer must fail");
        let escrow_addr = Hash256(InferenceEscrowOpenBody::escrow_address(&request_id));
        assert_eq!(state.get_account(&escrow_addr).unwrap().balance, 5_000);
    }

    #[test]
    fn test_escrow_release_rejects_empty_replicas() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let request_id = req(b"no-replicas");
        let open = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 5_000,
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowOpen,
        );
        let bad = make_channel_tx(
            addr(2),
            0,
            TxBody::InferenceEscrowRelease(InferenceEscrowReleaseBody {
                request_id,
                payer: addr(1),
                model_id: model_id(),
                max_tokens: 32,
                timeout_blocks: 10,
                output_hash: hash_bytes(b"x"),
                proposer: addr(2),
                replicas: vec![],
                observer_pool: addr(10),
                treasury: addr(11),
            }),
            TxType::InferenceEscrowRelease,
        );
        let (_, rs) = state.execute_block(&[open, bad], addr(99)).unwrap();
        assert!(rs[0].success);
        assert!(!rs[1].success, "empty replicas must be rejected");
    }

    #[test]
    fn test_escrow_refund_after_timeout_returns_funds_to_payer() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let request_id = req(b"refund-timeout");
        let open = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 7_500,
                max_tokens: 32,
                timeout_blocks: 2,
            }),
            TxType::InferenceEscrowOpen,
        );
        let (_, r) = state.execute_block(&[open], addr(99)).unwrap();
        assert!(r[0].success);
        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 992_500);

        let _ = state.execute_block(&[], addr(99)).unwrap();
        let _ = state.execute_block(&[], addr(99)).unwrap();
        let _ = state.execute_block(&[], addr(99)).unwrap();

        let refund = make_channel_tx(
            addr(1),
            1,
            TxBody::InferenceEscrowRefund(InferenceEscrowRefundBody {
                request_id,
                model_id: model_id(),
                max_tokens: 32,
                timeout_blocks: 2,
            }),
            TxType::InferenceEscrowRefund,
        );
        let (_, rs) = state.execute_block(&[refund], addr(99)).unwrap();
        assert!(rs[0].success, "refund after timeout should succeed");

        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 1_000_000);
        let escrow_addr = Hash256(InferenceEscrowOpenBody::escrow_address(&request_id));
        assert_eq!(state.get_account(&escrow_addr).unwrap().balance, 0);
    }

    #[test]
    fn test_escrow_refund_rejected_before_timeout() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let request_id = req(b"too-early");
        let open = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 5_000,
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowOpen,
        );
        let refund = make_channel_tx(
            addr(1),
            1,
            TxBody::InferenceEscrowRefund(InferenceEscrowRefundBody {
                request_id,
                model_id: model_id(),
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowRefund,
        );
        let (_, rs) = state.execute_block(&[open, refund], addr(99)).unwrap();
        assert!(rs[0].success);
        assert!(!rs[1].success, "refund before timeout must fail");
    }

    #[test]
    fn test_escrow_refund_rejects_non_payer() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 100_000)]);
        let request_id = req(b"not-your-money");
        let open = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 5_000,
                max_tokens: 32,
                timeout_blocks: 1,
            }),
            TxType::InferenceEscrowOpen,
        );
        let (_, r) = state.execute_block(&[open], addr(99)).unwrap();
        assert!(r[0].success);

        let _ = state.execute_block(&[], addr(99)).unwrap();
        let _ = state.execute_block(&[], addr(99)).unwrap();

        let refund = make_channel_tx(
            addr(2),
            0,
            TxBody::InferenceEscrowRefund(InferenceEscrowRefundBody {
                request_id,
                model_id: model_id(),
                max_tokens: 32,
                timeout_blocks: 1,
            }),
            TxType::InferenceEscrowRefund,
        );
        let (_, rs) = state.execute_block(&[refund], addr(99)).unwrap();
        assert!(!rs[0].success, "non-payer must not be able to refund");
    }

    use arc_types::transaction::{
        CapacityAdvertisementBody, MIN_MODEL_REGISTRATION_FEE, ModelRegistrationBody,
        ModelRequestBody, ShardCoverageClaimBody,
    };

    fn test_model_id() -> Hash256 {
        hash_bytes(b"test-model-7b")
    }

    #[test]
    fn test_model_registration_charges_fee_to_treasury() {
        let state = StateDB::with_genesis(&[(addr(1), 10_000)]);
        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ModelRegistration(ModelRegistrationBody {
                model_id: test_model_id(),
                metadata_hash: hash_bytes(b"meta"),
                chunk_tree_root: hash_bytes(b"chunks"),
                n_layers: 32,
                d_model: 4096,
                quantization: "int16".into(),
                registration_fee: MIN_MODEL_REGISTRATION_FEE,
                royalty_recipient: addr(1),
            }),
            TxType::ModelRegistration,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success, "model registration should succeed");

        assert_eq!(
            state.get_account(&addr(1)).unwrap().balance,
            10_000 - MIN_MODEL_REGISTRATION_FEE
        );
        let treasury = hash_bytes(b"arc-treasury");
        assert_eq!(
            state.get_account(&treasury).unwrap().balance,
            MIN_MODEL_REGISTRATION_FEE
        );

        let reg_addr = Hash256(ModelRegistrationBody::registry_account(&test_model_id()));
        let reg = state.get_account(&reg_addr).unwrap();
        assert_ne!(reg.storage_root, Hash256::ZERO);
    }

    #[test]
    fn test_model_registration_rejects_duplicate() {
        let state = StateDB::with_genesis(&[(addr(1), 10_000)]);
        let body = ModelRegistrationBody {
            model_id: test_model_id(),
            metadata_hash: hash_bytes(b"m"),
            chunk_tree_root: hash_bytes(b"c"),
            n_layers: 32,
            d_model: 4096,
            quantization: "int16".into(),
            registration_fee: MIN_MODEL_REGISTRATION_FEE,
            royalty_recipient: addr(1),
        };
        let tx1 = make_channel_tx(
            addr(1),
            0,
            TxBody::ModelRegistration(body.clone()),
            TxType::ModelRegistration,
        );
        let tx2 = make_channel_tx(
            addr(1),
            1,
            TxBody::ModelRegistration(body),
            TxType::ModelRegistration,
        );
        let (_, r1) = state.execute_block(&[tx1], addr(99)).unwrap();
        assert!(r1[0].success);
        let (_, r2) = state.execute_block(&[tx2], addr(99)).unwrap();
        assert!(
            !r2[0].success,
            "second registration for same model_id must fail"
        );
    }

    #[test]
    fn test_model_registration_fee_floors_at_min() {
        let state = StateDB::with_genesis(&[(addr(1), 10_000)]);
        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ModelRegistration(ModelRegistrationBody {
                model_id: test_model_id(),
                metadata_hash: hash_bytes(b"m"),
                chunk_tree_root: hash_bytes(b"c"),
                n_layers: 32,
                d_model: 4096,
                quantization: "int16".into(),
                registration_fee: 1,
                royalty_recipient: addr(1),
            }),
            TxType::ModelRegistration,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success);
        assert_eq!(
            state.get_account(&addr(1)).unwrap().balance,
            10_000 - MIN_MODEL_REGISTRATION_FEE
        );
    }

    #[test]
    fn test_model_request_records_demand() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let request_id = hash_bytes(b"demand-1").0;
        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ModelRequest(ModelRequestBody {
                request_id,
                model_id: test_model_id(),
                target_k_replication: 3,
                bond_per_layer_epoch: 500,
                max_wait_secs: 300,
            }),
            TxType::ModelRequest,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success);

        let req_addr = Hash256(ModelRequestBody::request_account(&request_id));
        let req = state.get_account(&req_addr).unwrap();
        assert_eq!(req.balance, 500);
        assert_ne!(req.storage_root, Hash256::ZERO);
    }

    #[test]
    fn test_shard_coverage_claim_locks_bond() {
        let state = StateDB::with_genesis(&[(addr(1), 100_000)]);
        let node_pubkey = [7u8; 32];
        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ShardCoverageClaim(ShardCoverageClaimBody {
                model_id: test_model_id(),
                node_pubkey,
                ranges: vec![(0, 6), (6, 12)],
                bond: 5_000,
                epoch_blocks: 1_000,
            }),
            TxType::ShardCoverageClaim,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success);

        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 95_000);
        let claim_addr = Hash256(ShardCoverageClaimBody::claim_account(
            &test_model_id(),
            &node_pubkey,
        ));
        assert_eq!(state.get_account(&claim_addr).unwrap().balance, 5_000);
    }

    #[test]
    fn test_shard_coverage_claim_rejects_empty_ranges() {
        let state = StateDB::with_genesis(&[(addr(1), 100_000)]);
        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::ShardCoverageClaim(ShardCoverageClaimBody {
                model_id: test_model_id(),
                node_pubkey: [0u8; 32],
                ranges: vec![],
                bond: 5_000,
                epoch_blocks: 1_000,
            }),
            TxType::ShardCoverageClaim,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(!r[0].success);
        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 100_000);
    }

    #[test]
    fn test_capacity_advertisement_records_metadata() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000)]);
        let node_pubkey = [11u8; 32];
        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::CapacityAdvertisement(CapacityAdvertisementBody {
                node_pubkey,
                ram_bytes: 16 * 1024 * 1024 * 1024,
                vram_bytes: 8 * 1024 * 1024 * 1024,
                bandwidth_mbps: 100,
                uptime_hint_mins: 1440,
                stake: 5_000_000,
                region: "US".into(),
            }),
            TxType::CapacityAdvertisement,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success);

        let cap_addr = Hash256(CapacityAdvertisementBody::capacity_account(&node_pubkey));
        let cap = state.get_account(&cap_addr).unwrap();
        assert_ne!(cap.storage_root, Hash256::ZERO);
    }

    #[test]
    fn test_escrow_release_cannot_be_replayed() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let request_id = req(b"no-replay");
        let open = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceEscrowOpen(InferenceEscrowOpenBody {
                request_id,
                model_id: model_id(),
                max_fee: 1_000,
                max_tokens: 32,
                timeout_blocks: 10,
            }),
            TxType::InferenceEscrowOpen,
        );
        let release_body = InferenceEscrowReleaseBody {
            request_id,
            payer: addr(1),
            model_id: model_id(),
            max_tokens: 32,
            timeout_blocks: 10,
            output_hash: hash_bytes(b"x"),
            proposer: addr(2),
            replicas: vec![addr(3)],
            observer_pool: addr(10),
            treasury: addr(11),
        };
        let release1 = make_channel_tx(
            addr(2),
            0,
            TxBody::InferenceEscrowRelease(release_body.clone()),
            TxType::InferenceEscrowRelease,
        );
        let release2 = make_channel_tx(
            addr(2),
            1,
            TxBody::InferenceEscrowRelease(release_body),
            TxType::InferenceEscrowRelease,
        );
        let (_, rs) = state
            .execute_block(&[open, release1, release2], addr(99))
            .unwrap();
        assert!(rs[0].success);
        assert!(rs[1].success);
        assert!(!rs[2].success, "second release must fail (escrow cleared)");
    }

    use arc_types::transaction::{FAUCET_CLAIM_MAX, FaucetClaimBody};

    fn seed_validator(state: &StateDB, signer: Address) {
        state
            .validators
            .insert(signer.0, StateDB::MIN_VALIDATOR_STAKE);
    }

    fn activate_community_rewards(state: &StateDB) {
        state.set_community_rewards_v1_activation_height(Some(0));
        assert!(state.community_rewards_v1_active());
    }

    #[test]
    fn test_faucet_claim_validator_authorized_credits_recipient() {
        let pool_addr = arc_types::transaction::faucet_pool_address();
        let validator = addr(7);
        let recipient = addr(42);
        let state = StateDB::with_genesis(&[(pool_addr, 1_000_000_000), (validator, 0)]);
        seed_validator(&state, validator);

        let tx = make_channel_tx(
            validator,
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient,
                amount: 10_000,
            }),
            TxType::FaucetClaim,
        );

        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success, "FaucetClaim from validator should succeed");

        let recv = state.get_account(&recipient).expect("recipient created");
        assert_eq!(recv.balance, 10_000, "recipient credited 10_000");
        let pool = state.get_account(&pool_addr).expect("pool exists");
        assert_eq!(pool.balance, 999_990_000, "pool debited 10_000");
        if let Some(signer) = state.get_account(&validator) {
            assert_eq!(signer.balance, 0, "signer balance untouched");
        }
    }

    #[test]
    fn test_faucet_claim_rejects_non_validator_signer() {
        let pool_addr = arc_types::transaction::faucet_pool_address();
        let not_a_validator = addr(8);
        let state = StateDB::with_genesis(&[(pool_addr, 1_000_000), (not_a_validator, 0)]);

        let tx = make_channel_tx(
            not_a_validator,
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient: addr(43),
                amount: 1_000,
            }),
            TxType::FaucetClaim,
        );

        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(
            !r[0].success,
            "FaucetClaim signed by non-validator must be rejected"
        );

        let pool = state.get_account(&pool_addr).unwrap();
        assert_eq!(pool.balance, 1_000_000);
        assert!(
            state.get_account(&addr(43)).is_none()
                || state.get_account(&addr(43)).unwrap().balance == 0
        );
    }

    #[test]
    fn test_faucet_claim_rejects_amount_over_max() {
        let pool_addr = arc_types::transaction::faucet_pool_address();
        let validator = addr(9);
        let state = StateDB::with_genesis(&[(pool_addr, 1_000_000_000)]);
        seed_validator(&state, validator);

        let tx = make_channel_tx(
            validator,
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient: addr(44),
                amount: FAUCET_CLAIM_MAX + 1,
            }),
            TxType::FaucetClaim,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(!r[0].success, "amount above FAUCET_CLAIM_MAX must reject");
    }

    #[test]
    fn test_faucet_claim_rejects_zero_amount() {
        let pool_addr = arc_types::transaction::faucet_pool_address();
        let validator = addr(10);
        let state = StateDB::with_genesis(&[(pool_addr, 1_000_000)]);
        seed_validator(&state, validator);

        let tx = make_channel_tx(
            validator,
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient: addr(45),
                amount: 0,
            }),
            TxType::FaucetClaim,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(!r[0].success, "zero-amount FaucetClaim must reject");
    }

    #[test]
    fn test_faucet_claim_rejects_insufficient_pool_balance() {
        let pool_addr = arc_types::transaction::faucet_pool_address();
        let validator = addr(11);
        let state = StateDB::with_genesis(&[(pool_addr, 500)]);
        seed_validator(&state, validator);

        let tx = make_channel_tx(
            validator,
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient: addr(46),
                amount: 1_000,
            }),
            TxType::FaucetClaim,
        );
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(!r[0].success, "underfunded pool must reject the claim");
        assert_eq!(state.get_account(&pool_addr).unwrap().balance, 500);
        assert!(
            state
                .get_account(&FaucetClaimBody::marker_address(&addr(46)))
                .is_none(),
            "a failed claim must not consume the recipient's exactly-once marker"
        );
        assert!(
            state.get_account(&addr(46)).is_none(),
            "a failed claim must not fabricate a recipient account"
        );
    }

    #[test]
    fn test_faucet_claim_is_exactly_once_across_validators() {
        let pool_addr = arc_types::transaction::faucet_pool_address();
        let validator_a = addr(111);
        let validator_b = addr(112);
        let recipient = addr(113);
        let state = StateDB::with_genesis(&[(pool_addr, 3 * FAUCET_CLAIM_MAX)]);
        seed_validator(&state, validator_a);
        seed_validator(&state, validator_b);

        let claim_a = make_channel_tx(
            validator_a,
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient,
                amount: FAUCET_CLAIM_MAX,
            }),
            TxType::FaucetClaim,
        );
        let claim_b = make_channel_tx(
            validator_b,
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient,
                amount: FAUCET_CLAIM_MAX,
            }),
            TxType::FaucetClaim,
        );

        let (_, receipts) = state
            .execute_block(&[claim_a, claim_b], addr(99))
            .expect("the block itself remains valid");
        assert!(receipts[0].success);
        assert!(!receipts[1].success, "a second validator cannot pay twice");
        assert_eq!(
            state.get_account(&recipient).unwrap().balance,
            FAUCET_CLAIM_MAX
        );
        assert_eq!(
            state.get_account(&pool_addr).unwrap().balance,
            2 * FAUCET_CLAIM_MAX
        );
        let marker = state
            .get_account(&FaucetClaimBody::marker_address(&recipient))
            .expect("successful claim writes a durable marker");
        assert_eq!(marker.nonce, 1);
        assert_eq!(marker.code_hash, recipient);
    }

    #[test]
    fn test_faucet_claim_overflow_is_atomic() {
        let pool_addr = arc_types::transaction::faucet_pool_address();
        let validator = addr(114);
        let recipient = addr(115);
        let state =
            StateDB::with_genesis(&[(pool_addr, 2 * FAUCET_CLAIM_MAX), (recipient, u64::MAX)]);
        seed_validator(&state, validator);
        let claim = make_channel_tx(
            validator,
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient,
                amount: FAUCET_CLAIM_MAX,
            }),
            TxType::FaucetClaim,
        );

        let (_, receipts) = state.execute_block(&[claim], addr(99)).unwrap();
        assert!(!receipts[0].success);
        assert_eq!(
            state.get_account(&pool_addr).unwrap().balance,
            2 * FAUCET_CLAIM_MAX,
            "overflow must not debit the faucet"
        );
        assert_eq!(state.get_account(&recipient).unwrap().balance, u64::MAX);
        assert!(
            state
                .get_account(&FaucetClaimBody::marker_address(&recipient))
                .is_none(),
            "overflow must not consume the recipient marker"
        );
    }

    #[test]
    fn test_faucet_claim_propagates_via_serialize_roundtrip() {
        use arc_crypto::KeyPair;
        let pool_addr = arc_types::transaction::faucet_pool_address();

        let kp = KeyPair::generate_ed25519();
        let validator = kp.address();

        let proposer_state = StateDB::with_genesis(&[(pool_addr, 100_000_000)]);
        seed_validator(&proposer_state, validator);

        let mut tx = Transaction::new_faucet_claim(validator, addr(50), 10_000, 0);
        tx.sign(&kp).expect("sign ok");
        let wire = bincode::serialize(&tx).expect("serialize tx");

        let peer_state = StateDB::with_genesis(&[(pool_addr, 100_000_000)]);
        seed_validator(&peer_state, validator);

        let peer_tx: Transaction = bincode::deserialize(&wire).expect("deserialize tx");
        assert!(
            !peer_tx.sig_verified,
            "wire-format tx must arrive with sig_verified=false"
        );
        assert!(
            !peer_tx.is_unsigned(),
            "wire-format tx must still carry the validator sig"
        );

        let (_, r) = peer_state.execute_block(&[peer_tx], addr(99)).unwrap();
        assert!(
            r[0].success,
            "peer must accept the validator-signed FaucetClaim and apply it"
        );

        let recv = peer_state.get_account(&addr(50)).unwrap();
        assert_eq!(recv.balance, 10_000, "peer credits recipient with 10_000");
        let pool = peer_state.get_account(&pool_addr).unwrap();
        assert_eq!(pool.balance, 99_990_000, "peer debits pool by 10_000");
    }

    fn build_tier1_request(
        from: Address,
        nonce: u64,
        request_id: [u8; 32],
        max_reward: u64,
        committee_size: u8,
        deadline_blocks: u64,
    ) -> arc_types::transaction::Transaction {
        use arc_crypto::Signature;
        use arc_types::transaction::{InferenceRequestBody, Transaction, TxBody, TxType};
        let input_blob = b"[INST] hi [/INST]".to_vec();
        let input_hash = hash_bytes(&input_blob);
        let body = TxBody::InferenceRequest(InferenceRequestBody {
            request_id,
            model_id: hash_bytes(b"arc-32L-test"),
            input_hash,
            input_blob,
            max_tokens: 32,
            tier: 1,
            max_reward,
            deadline_blocks,
            committee_size,
        });
        let mut tx = Transaction {
            tx_type: TxType::InferenceRequest,
            from,
            nonce,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: true,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    fn build_tier1_vote(
        from: Address,
        nonce: u64,
        request_id: [u8; 32],
        committee_seed: Hash256,
        output_hash: Hash256,
    ) -> arc_types::transaction::Transaction {
        use arc_crypto::Signature;
        use arc_types::transaction::{InferenceVoteBody, Transaction, TxBody, TxType};
        let body = TxBody::InferenceVote(InferenceVoteBody {
            request_id,
            output_hash,
            output_blob: None,
            vrf_proof: vec![0u8; 80],
            committee_seed,
        });
        let mut tx = Transaction {
            tx_type: TxType::InferenceVote,
            from,
            nonce,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: true,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    fn build_tier1_finalize(
        from: Address,
        nonce: u64,
        request_id: [u8; 32],
    ) -> arc_types::transaction::Transaction {
        use arc_crypto::Signature;
        use arc_types::transaction::{InferenceFinalizeBody, Transaction, TxBody, TxType};
        let body = TxBody::InferenceFinalize(InferenceFinalizeBody { request_id });
        let mut tx = Transaction {
            tx_type: TxType::InferenceFinalize,
            from,
            nonce,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: true,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    #[test]
    fn tier1_request_locks_escrow_and_records_metadata() {
        let requester = addr(1);
        let state = StateDB::with_genesis(&[(requester, 100)]);
        state
            .validators
            .insert(addr(99).0, StateDB::MIN_VALIDATOR_STAKE);

        let req_id = [42u8; 32];
        let tx = build_tier1_request(requester, 0, req_id, 10, 1, 20);
        let (_, receipts) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(receipts[0].success, "request must succeed");

        let r = state.get_account(&requester).unwrap();
        assert_eq!(r.balance, 90);
        assert_eq!(r.nonce, 1);

        let escrow_addr = hash_bytes(&[b"arc-infreq", req_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 10);
        assert_eq!(escrow.code_hash.0[0], TIER1_STATUS_OPEN);
        assert_eq!(escrow.code_hash.0[9], 1, "committee_size byte");
    }

    #[test]
    fn tier1_request_rejects_oversized_input() {
        let requester = addr(1);
        let state = StateDB::with_genesis(&[(requester, 1_000_000)]);
        state
            .validators
            .insert(addr(99).0, StateDB::MIN_VALIDATOR_STAKE);

        use arc_crypto::Signature;
        use arc_types::transaction::{
            InferenceRequestBody, TIER1_INPUT_BLOB_MAX, Transaction, TxBody, TxType,
        };
        let oversized = vec![0u8; TIER1_INPUT_BLOB_MAX + 1];
        let body = TxBody::InferenceRequest(InferenceRequestBody {
            request_id: [1u8; 32],
            model_id: hash_bytes(b"m"),
            input_hash: hash_bytes(&oversized),
            input_blob: oversized,
            max_tokens: 8,
            tier: 1,
            max_reward: 10,
            deadline_blocks: 20,
            committee_size: 1,
        });
        let mut tx = Transaction {
            tx_type: TxType::InferenceRequest,
            from: requester,
            nonce: 0,
            body,
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: true,
        };
        tx.hash = tx.compute_hash();

        let (_, receipts) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(
            !receipts[0].success,
            "oversized prompt must be rejected by state validator"
        );
    }

    #[test]
    fn tier1_full_consensus_payout() {
        let validator = addr(10);
        let state = StateDB::with_genesis(&[(validator, 1_000_000)]);
        state
            .validators
            .insert(validator.0, StateDB::MIN_VALIDATOR_STAKE);

        let req_id = [7u8; 32];
        let tx1 = build_tier1_request(validator, 0, req_id, 100, 1, 20);
        let (block1, r1) = state.execute_block(&[tx1], validator).unwrap();
        assert!(r1[0].success);

        let committee_seed = block1.hash;

        let output_hash = hash_bytes(b"the answer");
        let tx2 = build_tier1_vote(validator, 1, req_id, committee_seed, output_hash);
        let (_, r2) = state.execute_block(&[tx2], validator).unwrap();
        assert!(r2[0].success, "vote must succeed");

        let tx3 = build_tier1_finalize(validator, 2, req_id);
        let (_, r3) = state.execute_block(&[tx3], validator).unwrap();
        assert!(r3[0].success, "finalize must succeed");

        let escrow_addr = hash_bytes(&[b"arc-infreq", req_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 0);
        assert_eq!(escrow.code_hash.0[0], TIER1_STATUS_FINALIZED);

        let final_validator = state.get_account(&validator).unwrap();
        assert_eq!(final_validator.balance, 999_990);

        let treasury = state
            .get_account(&arc_types::transaction::faucet_pool_address())
            .unwrap();
        assert_eq!(treasury.balance, 10);
    }

    #[test]
    fn tier1_timeout_refunds_minus_anti_spam_fee() {
        let requester = addr(20);
        let state = StateDB::with_genesis(&[(requester, 1000)]);
        state
            .validators
            .insert(addr(99).0, StateDB::MIN_VALIDATOR_STAKE);

        let req_id = [99u8; 32];
        let max_reward = 50u64;
        let tx1 = build_tier1_request(requester, 0, req_id, max_reward, 1, 5);
        let (_, r1) = state.execute_block(&[tx1], addr(99)).unwrap();
        assert!(r1[0].success);

        for _ in 0..6 {
            state.execute_block(&[], addr(99)).unwrap();
        }

        let tx2 = build_tier1_finalize(addr(99), 0, req_id);
        let (_, r2) = state.execute_block(&[tx2], addr(99)).unwrap();
        assert!(r2[0].success, "finalize on timeout must succeed");

        let after = state.get_account(&requester).unwrap();
        assert_eq!(after.balance, 1000 - max_reward + (max_reward - 1));
    }

    #[test]
    fn tier1_vote_from_non_committee_rejected() {
        let alice = addr(30);
        let bob = addr(31);
        let state = StateDB::with_genesis(&[(alice, 1000), (bob, 1000)]);
        state
            .validators
            .insert(alice.0, StateDB::MIN_VALIDATOR_STAKE);

        let req_id = [55u8; 32];
        let tx1 = build_tier1_request(alice, 0, req_id, 10, 1, 20);
        let (block1, r1) = state.execute_block(&[tx1], alice).unwrap();
        assert!(r1[0].success);
        let committee_seed = block1.hash;

        let tx2 = build_tier1_vote(bob, 0, req_id, committee_seed, hash_bytes(b"x"));
        let (_, r2) = state.execute_block(&[tx2], alice).unwrap();
        assert!(
            !r2[0].success,
            "vote from non-committee member must be rejected"
        );
    }

    const REWARD: u64 = arc_types::economics::INFERENCE_ATTESTATION_REWARD;

    fn sum_all_balances(state: &StateDB) -> u128 {
        state
            .accounts
            .iter()
            .map(|e| e.value().balance as u128 + e.value().staked_balance as u128)
            .sum()
    }

    fn make_attestation(
        from: Address,
        nonce: u64,
        bond: u64,
        challenge_period: u64,
        tag: &[u8],
    ) -> Transaction {
        make_channel_tx(
            from,
            nonce,
            TxBody::InferenceAttestation(arc_types::transaction::InferenceAttestationBody {
                model_id: model_id(),
                input_hash: hash_bytes(tag),
                output_hash: hash_bytes(&[tag, b"-out"].concat()),
                challenge_period,
                bond,
                beneficiary: None,
            }),
            TxType::InferenceAttestation,
        )
    }

    fn make_signed_community_reward(
        validator: &arc_crypto::KeyPair,
        worker: &arc_crypto::KeyPair,
        reward_nonce: u64,
        tag: &[u8],
        expires_at_height: u64,
    ) -> Transaction {
        make_signed_community_reward_with_approvers(
            validator,
            worker,
            reward_nonce,
            tag,
            expires_at_height,
            &[validator],
        )
    }

    fn make_signed_community_reward_with_approvers(
        aggregator: &arc_crypto::KeyPair,
        worker: &arc_crypto::KeyPair,
        reward_nonce: u64,
        tag: &[u8],
        expires_at_height: u64,
        approvers: &[&arc_crypto::KeyPair],
    ) -> Transaction {
        let mut worker_attestation = make_attestation(worker.address(), 0, 0, 100, tag);
        worker_attestation.sign(worker).expect("worker signs");
        let TxBody::InferenceAttestation(attestation) = &worker_attestation.body else {
            unreachable!();
        };
        let assignment_epoch = hash_bytes(&[b"community-assignment-epoch-v1", tag].concat());
        let job_id = arc_types::transaction::CommunityInferenceRewardBody::derive_job_id(
            &aggregator.address(),
            &assignment_epoch,
            reward_nonce,
            &attestation.model_id,
            &attestation.input_hash,
            16,
        );
        let mut body = arc_types::transaction::CommunityInferenceRewardBody {
            chain_domain:
                arc_types::transaction::CommunityInferenceRewardBody::expected_chain_domain(),
            job_id,
            coordinator: aggregator.address(),
            assignment_epoch,
            job_nonce: reward_nonce,
            recovery_epoch: 0,
            validator_set_id: 0,
            transaction_domain: Hash256::ZERO,
            worker: worker.address(),
            model_id: attestation.model_id,
            input_hash: attestation.input_hash,
            output_hash: attestation.output_hash,
            max_tokens: 16,
            expires_at_height,
            worker_certificate: arc_types::transaction::WorkerInferenceCertificate {
                attestation_hash: worker_attestation.hash,
                nonce: worker_attestation.nonce,
                challenge_period: attestation.challenge_period,
                signature: worker_attestation.signature.clone(),
            },
            validator_approvals: Vec::new(),
        };
        let commitment = body.validator_approval_commitment();
        body.validator_approvals = approvers
            .iter()
            .map(|approver| {
                let signature = approver.sign(&commitment).expect("validator approves");
                arc_types::transaction::CommunityRewardValidatorApproval::from_ed25519_signature(
                    approver.address(),
                    signature,
                )
                .expect("reward approvals require Ed25519 validators")
            })
            .collect();
        let mut tx =
            Transaction::new_community_inference_reward(aggregator.address(), reward_nonce, body);
        tx.sign(aggregator).expect("aggregator signs");
        tx
    }

    fn make_recovery_signed_community_reward(
        state: &StateDB,
        aggregator: &arc_crypto::KeyPair,
        worker: &arc_crypto::KeyPair,
        approvers: &[arc_crypto::KeyPair],
        job_nonce: u64,
        tag: &[u8],
    ) -> Transaction {
        let mut worker_attestation = make_attestation(worker.address(), 0, 0, 100, tag);
        state
            .sign_transaction(&mut worker_attestation, worker)
            .unwrap();
        let TxBody::InferenceAttestation(attestation) = &worker_attestation.body else {
            unreachable!();
        };
        let context = state.recovery_context().expect("recovery context");
        let assignment_epoch = hash_bytes(&[b"v3-assignment-epoch", tag].concat());
        let job_id = arc_types::transaction::CommunityInferenceRewardBody::derive_job_id(
            &aggregator.address(),
            &assignment_epoch,
            job_nonce,
            &attestation.model_id,
            &attestation.input_hash,
            16,
        );
        let mut body = arc_types::transaction::CommunityInferenceRewardBody {
            chain_domain:
                arc_types::transaction::CommunityInferenceRewardBody::expected_chain_domain(),
            job_id,
            coordinator: aggregator.address(),
            assignment_epoch,
            job_nonce,
            recovery_epoch: context.recovery_epoch,
            validator_set_id: context.validator_set_id,
            transaction_domain: context.domain_hash(),
            worker: worker.address(),
            model_id: attestation.model_id,
            input_hash: attestation.input_hash,
            output_hash: attestation.output_hash,
            max_tokens: 16,
            expires_at_height: state.height() + 100,
            worker_certificate: arc_types::transaction::WorkerInferenceCertificate {
                attestation_hash: worker_attestation.hash,
                nonce: worker_attestation.nonce,
                challenge_period: attestation.challenge_period,
                signature: worker_attestation.signature.clone(),
            },
            validator_approvals: Vec::new(),
        };
        let commitment = body.validator_approval_commitment();
        body.validator_approvals = approvers
            .iter()
            .take(5)
            .map(|validator| {
                let signature = validator.sign(&commitment).unwrap();
                arc_types::transaction::CommunityRewardValidatorApproval::from_ed25519_signature(
                    validator.address(),
                    signature,
                )
                .unwrap()
            })
            .collect();
        let mut reward =
            Transaction::new_community_inference_reward(aggregator.address(), job_nonce, body);
        state.sign_transaction(&mut reward, aggregator).unwrap();
        reward
    }

    fn activated_reward_state(
        worker: Address,
        treasury_balance: u64,
        validators: &[(&arc_crypto::KeyPair, u64)],
    ) -> StateDB {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let state = StateDB::with_genesis(&[(pool, treasury_balance), (worker, 0)]);
        let validator_stakes: Vec<(Address, u64)> = validators
            .iter()
            .map(|(validator, stake)| (validator.address(), *stake))
            .collect();
        state.seed_genesis_validators(&validator_stakes);
        activate_community_rewards(&state);
        state
    }

    #[test]
    fn recovery_probe_marker_blocks_cross_coordinator_payment_and_outer_nonce_is_not_account_nonce()
    {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let first_validator = arc_crypto::KeyPair::generate_ed25519();
        let second_validator = arc_crypto::KeyPair::generate_ed25519();
        let first_worker = arc_crypto::KeyPair::generate_ed25519();
        let second_worker = arc_crypto::KeyPair::generate_ed25519();
        let state = StateDB::with_genesis(&[(pool, 3 * REWARD)]);
        seed_validator(&state, first_validator.address());
        activate_community_rewards(&state);

        let mut probe_bytes = [0u8; 32];
        probe_bytes[..arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX.len()]
            .copy_from_slice(&arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX);
        probe_bytes[arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX.len()..].fill(9);
        let probe_id = Hash256(probe_bytes);

        let bind_probe = |mut tx: Transaction,
                          validator: &arc_crypto::KeyPair,
                          outer_nonce: u64|
         -> Transaction {
            let TxBody::CommunityInferenceReward(body) = &mut tx.body else {
                unreachable!();
            };
            body.assignment_epoch = probe_id;
            body.job_id = arc_types::transaction::CommunityInferenceRewardBody::derive_job_id(
                &body.coordinator,
                &body.assignment_epoch,
                body.job_nonce,
                &body.model_id,
                &body.input_hash,
                body.max_tokens,
            );
            body.validator_approvals.clear();
            let commitment = body.validator_approval_commitment();
            body.validator_approvals.push(
                arc_types::transaction::CommunityRewardValidatorApproval::from_ed25519_signature(
                    validator.address(),
                    validator.sign(&commitment).unwrap(),
                )
                .unwrap(),
            );
            tx.nonce = outer_nonce;
            tx.sign(validator).unwrap();
            tx
        };

        let first = bind_probe(
            make_signed_community_reward(
                &first_validator,
                &first_worker,
                17,
                b"recovery-probe-first",
                100,
            ),
            &first_validator,
            u64::MAX - 1,
        );
        let coordinator_nonce_before = state
            .get_account(&first_validator.address())
            .map(|account| account.nonce)
            .unwrap_or(0);
        let (_, first_receipts) = state
            .execute_block(&[first], first_validator.address())
            .unwrap();
        assert!(first_receipts[0].success);
        assert_eq!(
            state
                .get_account(&first_validator.address())
                .map(|account| account.nonce)
                .unwrap_or(0),
            coordinator_nonce_before,
            "0x25 outer nonce is a deterministic envelope salt, not an account nonce"
        );
        seed_validator(&state, second_validator.address());

        let second = bind_probe(
            make_signed_community_reward(
                &second_validator,
                &second_worker,
                23,
                b"recovery-probe-second",
                100,
            ),
            &second_validator,
            1,
        );
        let replay_error = state.execute_tx(&second).unwrap_err();
        assert!(replay_error.to_string().contains("recovery probe"));
        assert!(replay_error.to_string().contains("already paid"));
        assert_eq!(
            state.get_account(&first_worker.address()).unwrap().balance,
            REWARD
        );
        assert_eq!(
            state
                .get_account(&second_worker.address())
                .map(|account| account.balance)
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn recovery_v3_reward_accepts_embedded_domain_certificate_while_raw_tx_is_denied() {
        let validators: Vec<_> = (0..6)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let treasury = arc_types::transaction::inference_reward_treasury_address();
        let mut genesis = vec![(treasury, 2 * REWARD)];
        genesis.extend(validators.iter().map(|validator| (validator.address(), 0)));
        let state = StateDB::with_genesis(&genesis);
        let validator_set: Vec<_> = validators
            .iter()
            .enumerate()
            .map(|(index, validator)| {
                (
                    validator.address(),
                    if index < 4 { 6_666_667 } else { 6_666_666 },
                )
            })
            .collect();
        state.seed_genesis_validators(&validator_set);
        state.staking_pool.store(40_000_000, Ordering::Release);
        state.set_community_rewards_v1_activation_height(Some(0));
        let context = crate::recovery::RecoveryContext::new(
            "0x415243",
            hash_bytes(b"v3-community-reward-genesis"),
            7,
            11,
        );
        *state.recovery_context.write() = Some(context.clone());

        let mut worker_attestation = make_attestation(worker.address(), 0, 0, 100, b"v3-job");
        state
            .sign_transaction(&mut worker_attestation, &worker)
            .unwrap();
        let before_accounts = state.accounts.len();
        let raw_error = state.execute_tx(&worker_attestation).unwrap_err();
        assert!(raw_error.to_string().contains("unavailable"));
        assert_eq!(state.accounts.len(), before_accounts);
        assert!(state.get_account(&worker.address()).is_none());

        let TxBody::InferenceAttestation(attestation) = &worker_attestation.body else {
            unreachable!();
        };
        let assignment_epoch = hash_bytes(b"v3-assignment-epoch");
        let job_nonce = 23;
        let job_id = arc_types::transaction::CommunityInferenceRewardBody::derive_job_id(
            &validators[0].address(),
            &assignment_epoch,
            job_nonce,
            &attestation.model_id,
            &attestation.input_hash,
            16,
        );
        let mut body = arc_types::transaction::CommunityInferenceRewardBody {
            chain_domain:
                arc_types::transaction::CommunityInferenceRewardBody::expected_chain_domain(),
            job_id,
            coordinator: validators[0].address(),
            assignment_epoch,
            job_nonce,
            recovery_epoch: context.recovery_epoch,
            validator_set_id: context.validator_set_id,
            transaction_domain: context.domain_hash(),
            worker: worker.address(),
            model_id: attestation.model_id,
            input_hash: attestation.input_hash,
            output_hash: attestation.output_hash,
            max_tokens: 16,
            expires_at_height: 100,
            worker_certificate: arc_types::transaction::WorkerInferenceCertificate {
                attestation_hash: worker_attestation.hash,
                nonce: worker_attestation.nonce,
                challenge_period: attestation.challenge_period,
                signature: worker_attestation.signature.clone(),
            },
            validator_approvals: Vec::new(),
        };
        let commitment = body.validator_approval_commitment();
        body.validator_approvals = validators
            .iter()
            .take(5)
            .map(|validator| {
                let signature = validator.sign(&commitment).unwrap();
                arc_types::transaction::CommunityRewardValidatorApproval::from_ed25519_signature(
                    validator.address(),
                    signature,
                )
                .unwrap()
            })
            .collect();
        let coordinator_nonce_before = state
            .get_account(&validators[0].address())
            .expect("genesis validator account")
            .nonce;
        let mut reward =
            Transaction::new_community_inference_reward(validators[0].address(), u64::MAX, body);
        state.sign_transaction(&mut reward, &validators[0]).unwrap();
        let TxBody::CommunityInferenceReward(reward_body) = &reward.body else {
            unreachable!();
        };
        let legacy_job_marker =
            arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                &reward_body.chain_domain,
                &reward_body.job_id,
            );
        let mut genuine_legacy_marker = Account::new(legacy_job_marker, 0);
        genuine_legacy_marker.nonce = 1;
        genuine_legacy_marker.code_hash = hash_bytes(b"historical-worker");
        genuine_legacy_marker.storage_root = hash_bytes(b"historical-output");
        state
            .accounts
            .insert(legacy_job_marker.0, genuine_legacy_marker);
        let legacy_replay = state
            .validate_v3_transaction_admission(&reward)
            .unwrap_err();
        assert!(
            legacy_replay
                .to_string()
                .contains("legacy marker semantics")
        );

        state
            .accounts
            .insert(legacy_job_marker.0, Account::new(legacy_job_marker, 1));
        state
            .validate_v3_transaction_admission(&reward)
            .expect("0x25 outer nonce is not account-nonce admission");

        let supply_before = sum_all_balances(&state);
        state.execute_tx(&reward).unwrap();
        let v3_job_marker = arc_types::transaction::CommunityInferenceRewardBody::v3_marker_address(
            &reward_body.chain_domain,
            &reward_body.job_id,
        );
        assert!(is_reserved_system_account(&v3_job_marker));
        assert!(state.get_account(&v3_job_marker).is_some());
        assert_eq!(
            state.get_account(&legacy_job_marker).unwrap().balance,
            1,
            "v3 must neither trust nor overwrite a pre-dusted legacy marker key"
        );
        assert_eq!(
            state.get_account(&worker.address()).unwrap().balance,
            REWARD
        );
        assert_eq!(state.get_account(&treasury).unwrap().balance, REWARD);
        assert_eq!(
            state.get_account(&validators[0].address()).unwrap().nonce,
            coordinator_nonce_before,
            "0x25 execution must not consume the coordinator account nonce"
        );
        assert_eq!(sum_all_balances(&state), supply_before);

        let status = state.community_reward_budget_status(
            worker.address(),
            validators[0].address(),
            state.height(),
        );
        assert_eq!(status.issued_this_block, 1);
        assert_eq!(status.remaining_this_block, 0);
        assert_eq!(status.issued_this_epoch, 1);
        assert_eq!(
            status.remaining_this_epoch,
            V3_COMMUNITY_REWARD_MAX_PER_EPOCH - 1
        );
        assert_eq!(status.worker_issued_this_epoch, 1);
        assert_eq!(
            status.worker_remaining_this_epoch,
            V3_COMMUNITY_REWARD_MAX_PER_WORKER_EPOCH - 1
        );
        assert_eq!(status.coordinator_issued_this_epoch, 1);
        assert_eq!(
            status.coordinator_remaining_this_epoch,
            V3_COMMUNITY_REWARD_MAX_PER_COORDINATOR_EPOCH - 1
        );
        assert_eq!(status.policy_hash, community_reward_issuance_policy_hash());

        let second = make_recovery_signed_community_reward(
            &state,
            &validators[0],
            &worker,
            &validators,
            24,
            b"v3-job-second",
        );
        let treasury_after_first = state.get_account(&treasury).unwrap().balance;
        let worker_after_first = state.get_account(&worker.address()).unwrap().balance;
        let error = state
            .validate_v3_transaction_admission_at(&second, state.height())
            .unwrap_err();
        assert!(
            error.to_string().contains("block issuance limit"),
            "{error}"
        );
        assert!(state.execute_tx(&second).is_err());
        assert_eq!(
            state.get_account(&treasury).unwrap().balance,
            treasury_after_first
        );
        assert_eq!(
            state.get_account(&worker.address()).unwrap().balance,
            worker_after_first
        );

        *state.height.write() = 1;
        let epoch = state.height() / V3_COMMUNITY_REWARD_EPOCH_BLOCKS;
        let worker_budget = community_reward_worker_budget_address(epoch, &worker.address());
        let mut worker_counter = state.get_account(&worker_budget).unwrap();
        worker_counter.nonce = V3_COMMUNITY_REWARD_MAX_PER_WORKER_EPOCH;
        state
            .accounts
            .insert(worker_budget.0, worker_counter.clone());
        let error = state
            .validate_v3_transaction_admission(&second)
            .unwrap_err();
        assert!(
            error.to_string().contains("worker epoch issuance limit"),
            "{error}"
        );

        worker_counter.nonce = 1;
        state.accounts.insert(worker_budget.0, worker_counter);
        let coordinator_budget =
            community_reward_coordinator_budget_address(epoch, &validators[0].address());
        let mut coordinator_counter = state.get_account(&coordinator_budget).unwrap();
        coordinator_counter.nonce = V3_COMMUNITY_REWARD_MAX_PER_COORDINATOR_EPOCH;
        state
            .accounts
            .insert(coordinator_budget.0, coordinator_counter.clone());
        let error = state
            .validate_v3_transaction_admission(&second)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("coordinator epoch issuance limit"),
            "{error}"
        );

        coordinator_counter.nonce = 1;
        state
            .accounts
            .insert(coordinator_budget.0, coordinator_counter);
        let epoch_budget = community_reward_epoch_budget_address(epoch);
        let mut epoch_counter = state.get_account(&epoch_budget).unwrap();
        epoch_counter.nonce = V3_COMMUNITY_REWARD_MAX_PER_EPOCH;
        state.accounts.insert(epoch_budget.0, epoch_counter);
        let error = state
            .validate_v3_transaction_admission(&second)
            .unwrap_err();
        assert!(
            error.to_string().contains("epoch issuance limit"),
            "{error}"
        );
    }

    #[test]
    fn recovery_v3_reward_budget_counters_are_rooted_and_wal_restartable() {
        let directory = persistent_test_dir("v3-reward-budget-restart");
        let genesis_hash = hash_bytes(b"v3-reward-budget-restart-genesis");
        let context = crate::recovery::RecoveryContext::new("0x415243", genesis_hash, 9, 12);
        let validators: Vec<_> = (0..6)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let treasury = arc_types::transaction::inference_reward_treasury_address();
        let mut prefunded = vec![(treasury, 4 * REWARD)];
        prefunded.extend(validators.iter().map(|validator| (validator.address(), 0)));
        let validator_set: Vec<_> = validators
            .iter()
            .enumerate()
            .map(|(index, validator)| {
                (
                    validator.address(),
                    if index < 4 { 6_666_667 } else { 6_666_666 },
                )
            })
            .collect();

        let state = StateDB::with_genesis_persistent(&prefunded, &directory, genesis_hash).unwrap();
        state.seed_genesis_validators(&validator_set);
        state.staking_pool.store(40_000_000, Ordering::Release);
        state.set_community_rewards_v1_activation_height(Some(0));
        *state.recovery_context.write() = Some(context.clone());

        let reward = make_recovery_signed_community_reward(
            &state,
            &validators[0],
            &worker,
            &validators,
            0,
            b"persistent-budget-job",
        );
        let (block, receipts) = state
            .execute_block_verified_at(
                std::slice::from_ref(&reward),
                validators[0].address(),
                1_787_857_623_010,
            )
            .unwrap();
        assert!(receipts[0].success);
        state.wal.sync().unwrap();
        let rooted_state = state.get_state_root();
        assert_eq!(block.header.state_root, rooted_state);
        let status =
            state.current_community_reward_budget_status(worker.address(), validators[0].address());
        assert_eq!(status.issued_this_block, 1);
        assert_eq!(status.issued_this_epoch, 1);
        assert_eq!(status.worker_issued_this_epoch, 1);
        assert_eq!(status.coordinator_issued_this_epoch, 1);
        let receipt_hash = receipts[0].tx_hash;
        let block_hash = block.hash;
        drop(state);

        let restarted =
            StateDB::with_genesis_persistent(&prefunded, &directory, genesis_hash).unwrap();
        restarted.set_community_rewards_v1_activation_height(Some(0));
        *restarted.recovery_context.write() = Some(context);
        assert_eq!(restarted.height(), 1);
        assert_eq!(restarted.get_state_root(), rooted_state);
        assert_eq!(restarted.get_block(1).unwrap().hash, block_hash);
        assert_eq!(
            restarted.get_receipt(&receipt_hash.0).unwrap().block_hash,
            block_hash
        );
        let restarted_status = restarted
            .current_community_reward_budget_status(worker.address(), validators[0].address());
        assert_eq!(restarted_status, status);
        assert_eq!(
            restarted.get_account(&worker.address()).unwrap().balance,
            REWARD
        );
        assert_eq!(
            restarted.get_account(&treasury).unwrap().balance,
            3 * REWARD
        );

        drop(restarted);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn reward_rejection(state: &StateDB, reward: &Transaction) -> String {
        state
            .execute_tx(reward)
            .expect_err("adversarial reward must be rejected")
            .to_string()
    }

    #[test]
    fn raw_attestation_locks_bond_without_treasury_reward() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let worker = addr(60);
        let treasury_start = 10 * REWARD;
        let worker_start = 5 * REWARD;
        let bond = REWARD / 2;
        let state = StateDB::with_genesis(&[(pool, treasury_start), (worker, worker_start)]);
        let before = sum_all_balances(&state);

        let tx = make_attestation(worker, 0, bond, 100, b"job-1");
        let att_hash = tx.hash;
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success);

        let w = state.get_account(&worker).unwrap();
        assert_eq!(w.balance, worker_start - bond);
        assert_eq!(w.nonce, 1);

        assert_eq!(state.get_account(&pool).unwrap().balance, treasury_start);

        let escrow_addr = hash_bytes(&[b"arc-inference", att_hash.as_ref()].concat());
        let esc = state.get_account(&escrow_addr).unwrap();
        assert_eq!(esc.balance, bond);
        assert_eq!(esc.code_hash, worker);
        assert_eq!(esc.storage_root.0[8], ATTEST_STATUS_OPEN);

        assert_eq!(sum_all_balances(&state), before, "supply conserved");
    }

    #[test]
    fn raw_bond_zero_attestation_creates_no_escrow_or_reward() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let worker = addr(61);
        let state = StateDB::with_genesis(&[(pool, 10 * REWARD), (worker, 0)]);
        let before = sum_all_balances(&state);

        let tx = make_attestation(worker, 0, 0, 100, b"job-community");
        let att_hash = tx.hash;
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success);

        assert_eq!(
            state.get_account(&worker).unwrap().balance,
            0,
            "an unattached self-signed attestation must never earn"
        );
        let escrow_addr = hash_bytes(&[b"arc-inference", att_hash.as_ref()].concat());
        assert!(
            state
                .get_account(&escrow_addr)
                .map(|e| e.balance)
                .unwrap_or(0)
                == 0
        );
        assert_eq!(state.pending_bond_release_count(), 0);
        assert_eq!(sum_all_balances(&state), before, "supply conserved");
    }

    #[test]
    fn authorized_stake_zero_community_reward_pays_exactly_once() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let faucet = arc_types::transaction::faucet_pool_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state =
            StateDB::with_genesis(&[(pool, 3 * REWARD), (faucet, 99), (worker.address(), 0)]);
        seed_validator(&state, validator.address());
        activate_community_rewards(&state);
        let before = sum_all_balances(&state);

        let reward = make_signed_community_reward(&validator, &worker, 7, b"paid-job", 100);
        let marker = match &reward.body {
            TxBody::CommunityInferenceReward(body) => {
                arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                    &body.chain_domain,
                    &body.job_id,
                )
            }
            _ => unreachable!(),
        };
        let (_, receipts) = state.execute_block(&[reward], validator.address()).unwrap();
        assert!(receipts[0].success);
        assert_eq!(
            state.get_account(&worker.address()).unwrap().balance,
            REWARD
        );
        assert_eq!(state.get_account(&pool).unwrap().balance, 2 * REWARD);
        assert_eq!(state.get_account(&faucet).unwrap().balance, 99);
        let paid = state.get_account(&marker).expect("replay marker");
        assert_eq!(paid.nonce, 1);
        assert_eq!(paid.code_hash, worker.address());
        assert_eq!(sum_all_balances(&state), before, "supply conserved");
    }

    #[test]
    fn community_reward_is_rejected_before_genesis_activation() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state = StateDB::with_genesis(&[(pool, 2 * REWARD), (worker.address(), 0)]);
        seed_validator(&state, validator.address());
        state.set_community_rewards_v1_activation_height(Some(10));

        let reward = make_signed_community_reward(&validator, &worker, 1, b"too-early", 100);
        let (_, receipts) = state.execute_block(&[reward], validator.address()).unwrap();
        assert!(!receipts[0].success);
        assert_eq!(state.get_account(&pool).unwrap().balance, 2 * REWARD);
        assert_eq!(state.get_account(&worker.address()).unwrap().balance, 0);
    }

    #[test]
    fn community_reward_accepts_strict_identity_and_stake_supermajority() {
        let validators: Vec<arc_crypto::KeyPair> = (0..4)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state = activated_reward_state(
            worker.address(),
            2 * REWARD,
            &validators
                .iter()
                .map(|validator| (validator, StateDB::MIN_VALIDATOR_STAKE))
                .collect::<Vec<_>>(),
        );
        let reward = make_signed_community_reward_with_approvers(
            &validators[0],
            &worker,
            1,
            b"threshold-success",
            100,
            &[&validators[0], &validators[1], &validators[2]],
        );

        state
            .execute_tx(&reward)
            .expect("3 of 4 equal-stake validators");
        assert_eq!(
            state.get_account(&worker.address()).unwrap().balance,
            REWARD
        );
    }

    #[test]
    fn community_reward_rejects_missing_approvals() {
        let validators: Vec<arc_crypto::KeyPair> = (0..4)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let stakes: Vec<_> = validators
            .iter()
            .map(|validator| (validator, StateDB::MIN_VALIDATOR_STAKE))
            .collect();
        let state = activated_reward_state(worker.address(), 2 * REWARD, &stakes);
        let reward = make_signed_community_reward_with_approvers(
            &validators[0],
            &worker,
            1,
            b"missing-approvals",
            100,
            &[],
        );

        assert!(
            reward_rejection(&state, &reward)
                .contains("insufficient validator approval identities")
        );
    }

    #[test]
    fn community_reward_rejects_duplicate_approval_signer() {
        let validators: Vec<arc_crypto::KeyPair> = (0..4)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let stakes: Vec<_> = validators
            .iter()
            .map(|validator| (validator, StateDB::MIN_VALIDATOR_STAKE))
            .collect();
        let state = activated_reward_state(worker.address(), 2 * REWARD, &stakes);
        let reward = make_signed_community_reward_with_approvers(
            &validators[0],
            &worker,
            1,
            b"duplicate-approval",
            100,
            &[&validators[0], &validators[1], &validators[1]],
        );

        assert!(reward_rejection(&state, &reward).contains("duplicate validator approval"));
    }

    #[test]
    fn community_reward_rejects_inactive_approval_signer() {
        let validators: Vec<arc_crypto::KeyPair> = (0..4)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let outsider = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let mut stakes: Vec<_> = validators
            .iter()
            .map(|validator| (validator, StateDB::MIN_VALIDATOR_STAKE))
            .collect();
        stakes.push((&outsider, StateDB::MIN_VALIDATOR_STAKE - 1));
        let state = activated_reward_state(worker.address(), 2 * REWARD, &stakes);
        let reward = make_signed_community_reward_with_approvers(
            &validators[0],
            &worker,
            1,
            b"inactive-approval",
            100,
            &[&validators[0], &validators[1], &outsider],
        );

        assert!(reward_rejection(&state, &reward).contains("is not an active validator"));
    }

    #[test]
    fn community_reward_rejects_insufficient_identity_count_even_with_quorum_stake() {
        let validators: Vec<arc_crypto::KeyPair> = (0..4)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let unit = StateDB::MIN_VALIDATOR_STAKE;
        let stakes = [
            (&validators[0], 10 * unit),
            (&validators[1], 10 * unit),
            (&validators[2], unit),
            (&validators[3], unit),
        ];
        let state = activated_reward_state(worker.address(), 2 * REWARD, &stakes);
        let reward = make_signed_community_reward_with_approvers(
            &validators[0],
            &worker,
            1,
            b"identity-shortfall",
            100,
            &[&validators[0], &validators[1]],
        );

        assert!(reward_rejection(&state, &reward).contains("have 2, need 3"));
    }

    #[test]
    fn community_reward_rejects_exactly_two_thirds_of_validator_identities() {
        let validators: Vec<arc_crypto::KeyPair> = (0..6)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let stakes: Vec<_> = validators
            .iter()
            .map(|validator| (validator, StateDB::MIN_VALIDATOR_STAKE))
            .collect();
        let state = activated_reward_state(worker.address(), 2 * REWARD, &stakes);
        let reward = make_signed_community_reward_with_approvers(
            &validators[0],
            &worker,
            1,
            b"exact-two-thirds-identities",
            100,
            &[
                &validators[0],
                &validators[1],
                &validators[2],
                &validators[3],
            ],
        );

        let error = reward_rejection(&state, &reward);
        assert!(error.contains("have 4, need 5"), "{error}");
    }

    #[test]
    fn community_reward_rejects_below_two_thirds_approved_stake() {
        let validators: Vec<arc_crypto::KeyPair> = (0..4)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let unit = StateDB::MIN_VALIDATOR_STAKE;
        let stakes = [
            (&validators[0], unit),
            (&validators[1], unit),
            (&validators[2], unit),
            (&validators[3], 10 * unit),
        ];
        let state = activated_reward_state(worker.address(), 2 * REWARD, &stakes);
        let reward = make_signed_community_reward_with_approvers(
            &validators[0],
            &worker,
            1,
            b"stake-shortfall",
            100,
            &[&validators[0], &validators[1], &validators[2]],
        );

        let error = reward_rejection(&state, &reward);
        assert!(error.contains("insufficient approved stake"), "{error}");
    }

    #[test]
    fn community_reward_rejects_exactly_two_thirds_approved_stake() {
        let validators: Vec<arc_crypto::KeyPair> = (0..4)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let unit = StateDB::MIN_VALIDATOR_STAKE;
        let stakes = [
            (&validators[0], unit),
            (&validators[1], unit),
            (&validators[2], 4 * unit),
            (&validators[3], 3 * unit),
        ];
        let state = activated_reward_state(worker.address(), 2 * REWARD, &stakes);
        let reward = make_signed_community_reward_with_approvers(
            &validators[0],
            &worker,
            1,
            b"exact-two-thirds-stake",
            100,
            &[&validators[0], &validators[1], &validators[2]],
        );

        let error = reward_rejection(&state, &reward);
        assert!(error.contains("insufficient approved stake"), "{error}");
        assert!(error.contains("need 600001"), "{error}");
    }

    #[test]
    fn community_reward_rejects_approval_after_semantic_field_mutation() {
        let validators: Vec<arc_crypto::KeyPair> = (0..4)
            .map(|_| arc_crypto::KeyPair::generate_ed25519())
            .collect();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let stakes: Vec<_> = validators
            .iter()
            .map(|validator| (validator, StateDB::MIN_VALIDATOR_STAKE))
            .collect();
        let state = activated_reward_state(worker.address(), 2 * REWARD, &stakes);
        let mut reward = make_signed_community_reward_with_approvers(
            &validators[0],
            &worker,
            1,
            b"tampered-approval",
            100,
            &[&validators[0], &validators[1], &validators[2]],
        );
        let TxBody::CommunityInferenceReward(body) = &mut reward.body else {
            unreachable!();
        };
        body.max_tokens += 1;
        body.job_id = arc_types::transaction::CommunityInferenceRewardBody::derive_job_id(
            &body.coordinator,
            &body.assignment_epoch,
            body.job_nonce,
            &body.model_id,
            &body.input_hash,
            body.max_tokens,
        );
        reward.sign(&validators[0]).unwrap();

        assert!(reward_rejection(&state, &reward).contains("invalid Ed25519 approval"));
    }

    #[test]
    fn community_reward_rejects_wrong_recovery_binding() {
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state = activated_reward_state(
            worker.address(),
            2 * REWARD,
            &[(&validator, StateDB::MIN_VALIDATOR_STAKE)],
        );
        let mut reward =
            make_signed_community_reward(&validator, &worker, 1, b"wrong-recovery", 100);
        let TxBody::CommunityInferenceReward(body) = &mut reward.body else {
            unreachable!();
        };
        body.recovery_epoch = 1;
        reward.sign(&validator).unwrap();

        let error = reward_rejection(&state, &reward);
        assert!(
            error.contains("recovery binding is non-zero on legacy/dev state"),
            "{error}"
        );
    }

    #[test]
    fn community_reward_rejects_more_than_64_approvals_before_verification() {
        use arc_types::transaction::MAX_COMMUNITY_REWARD_APPROVALS;

        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state = activated_reward_state(
            worker.address(),
            2 * REWARD,
            &[(&validator, StateDB::MIN_VALIDATOR_STAKE)],
        );
        let mut reward =
            make_signed_community_reward(&validator, &worker, 1, b"too-many-approvals", 100);
        let TxBody::CommunityInferenceReward(body) = &mut reward.body else {
            unreachable!();
        };
        let approval = body.validator_approvals[0].clone();
        body.validator_approvals = vec![approval; MAX_COMMUNITY_REWARD_APPROVALS + 1];
        reward.sign(&validator).unwrap();

        let error = reward_rejection(&state, &reward);
        assert!(error.contains("exceeds protocol maximum 64"), "{error}");
    }

    #[test]
    fn community_reward_replay_with_fresh_nonce_cannot_pay_twice() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state = StateDB::with_genesis(&[(pool, 3 * REWARD), (worker.address(), 0)]);
        seed_validator(&state, validator.address());
        activate_community_rewards(&state);

        let first = make_signed_community_reward(&validator, &worker, 1, b"same-job", 100);
        let second = make_signed_community_reward(&validator, &worker, 2, b"same-job", 100);
        assert!(
            state
                .execute_block(&[first], validator.address())
                .unwrap()
                .1[0]
                .success
        );
        let pool_after_first = state.get_account(&pool).unwrap().balance;
        let (_, replay_receipt) = state.execute_block(&[second], validator.address()).unwrap();
        assert!(!replay_receipt[0].success);
        assert_eq!(
            state.get_account(&worker.address()).unwrap().balance,
            REWARD
        );
        assert_eq!(state.get_account(&pool).unwrap().balance, pool_after_first);
    }

    #[test]
    fn community_reward_cannot_rewrap_one_certificate_under_a_fresh_job_id() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state = StateDB::with_genesis(&[(pool, 3 * REWARD), (worker.address(), 0)]);
        seed_validator(&state, validator.address());
        activate_community_rewards(&state);

        let first = make_signed_community_reward(&validator, &worker, 1, b"certificate", 100);
        let mut rewrapped = first.clone();
        rewrapped.nonce = 2;
        let TxBody::CommunityInferenceReward(body) = &mut rewrapped.body else {
            unreachable!();
        };
        body.job_id = hash_bytes(b"attacker-controlled-fresh-job-id");
        rewrapped.sign(&validator).unwrap();

        assert!(
            state
                .execute_block(&[first], validator.address())
                .unwrap()
                .1[0]
                .success
        );
        let treasury_after_first = state.get_account(&pool).unwrap().balance;
        let (_, receipts) = state
            .execute_block(&[rewrapped], validator.address())
            .unwrap();
        assert!(!receipts[0].success);
        assert_eq!(
            state.get_account(&pool).unwrap().balance,
            treasury_after_first
        );
        assert_eq!(
            state.get_account(&worker.address()).unwrap().balance,
            REWARD
        );
    }

    #[test]
    fn community_reward_rejects_unauthorized_or_mismatched_certificates() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let outsider = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let other_worker = arc_crypto::KeyPair::generate_ed25519();
        let state = StateDB::with_genesis(&[(pool, 5 * REWARD), (worker.address(), 0)]);
        seed_validator(&state, validator.address());
        activate_community_rewards(&state);

        let unauthorized =
            make_signed_community_reward(&outsider, &worker, 1, b"unauthorized", 100);
        assert!(!state.execute_block(&[unauthorized], addr(99)).unwrap().1[0].success);

        let mut mismatch = make_signed_community_reward(&validator, &worker, 2, b"mismatch", 100);
        if let TxBody::CommunityInferenceReward(body) = &mut mismatch.body {
            body.worker = other_worker.address();
        }
        mismatch.sign(&validator).unwrap();
        assert!(!state.execute_block(&[mismatch], addr(99)).unwrap().1[0].success);

        let mut wrong_domain =
            make_signed_community_reward(&validator, &worker, 3, b"wrong-domain", 100);
        if let TxBody::CommunityInferenceReward(body) = &mut wrong_domain.body {
            body.chain_domain = hash_bytes(b"another-chain");
        }
        wrong_domain.sign(&validator).unwrap();
        assert!(!state.execute_block(&[wrong_domain], addr(99)).unwrap().1[0].success);

        assert_eq!(state.get_account(&worker.address()).unwrap().balance, 0);
        assert_eq!(state.get_account(&pool).unwrap().balance, 5 * REWARD);
    }

    #[test]
    fn community_reward_is_all_or_nothing_when_treasury_is_low() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let partial = REWARD - 1;
        let state = StateDB::with_genesis(&[(pool, partial), (worker.address(), 0)]);
        seed_validator(&state, validator.address());
        activate_community_rewards(&state);

        let reward = make_signed_community_reward(&validator, &worker, 1, b"low-pool", 100);
        let (_, receipt) = state.execute_block(&[reward], validator.address()).unwrap();
        assert!(!receipt[0].success);
        assert_eq!(state.get_account(&pool).unwrap().balance, partial);
        assert_eq!(state.get_account(&worker.address()).unwrap().balance, 0);
    }

    #[test]
    fn failed_community_reward_does_not_create_an_absent_worker_account() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state = StateDB::with_genesis(&[(pool, REWARD - 1)]);
        seed_validator(&state, validator.address());
        activate_community_rewards(&state);
        assert!(state.get_account(&worker.address()).is_none());

        let reward = make_signed_community_reward(&validator, &worker, 1, b"absent-worker", 100);
        let (_, receipt) = state.execute_block(&[reward], validator.address()).unwrap();

        assert!(!receipt[0].success);
        assert_eq!(state.get_account(&pool).unwrap().balance, REWARD - 1);
        assert!(
            state.get_account(&worker.address()).is_none(),
            "a failed reward must not create state that was never WAL-persisted"
        );
    }

    #[test]
    fn community_reward_overflow_failure_does_not_debit_treasury() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state = StateDB::with_genesis(&[(pool, 2 * REWARD), (worker.address(), u64::MAX)]);
        seed_validator(&state, validator.address());
        activate_community_rewards(&state);

        let reward = make_signed_community_reward(&validator, &worker, 1, b"overflow", 100);
        let (_, receipt) = state.execute_block(&[reward], validator.address()).unwrap();
        assert!(!receipt[0].success);
        assert_eq!(state.get_account(&pool).unwrap().balance, 2 * REWARD);
        assert_eq!(
            state.get_account(&worker.address()).unwrap().balance,
            u64::MAX
        );
    }

    #[test]
    fn community_reward_rejects_expired_job() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker = arc_crypto::KeyPair::generate_ed25519();
        let state = StateDB::with_genesis(&[(pool, 2 * REWARD), (worker.address(), 0)]);
        seed_validator(&state, validator.address());
        activate_community_rewards(&state);

        let reward = make_signed_community_reward(&validator, &worker, 1, b"expired", 0);
        let (_, receipt) = state.execute_block(&[reward], validator.address()).unwrap();
        assert!(!receipt[0].success);
        assert_eq!(state.get_account(&pool).unwrap().balance, 2 * REWARD);
        assert_eq!(state.get_account(&worker.address()).unwrap().balance, 0);
    }

    #[test]
    fn community_reward_sequential_and_blockstm_state_match_at_treasury_tail() {
        let pool = arc_types::transaction::inference_reward_treasury_address();
        let validator = arc_crypto::KeyPair::generate_ed25519();
        let worker_a = arc_crypto::KeyPair::generate_ed25519();
        let worker_b = arc_crypto::KeyPair::generate_ed25519();
        let treasury = REWARD + REWARD / 2;
        let genesis = [
            (pool, treasury),
            (worker_a.address(), 0),
            (worker_b.address(), 0),
        ];
        let sequential = StateDB::with_genesis(&genesis);
        let parallel = StateDB::with_genesis(&genesis);
        seed_validator(&sequential, validator.address());
        seed_validator(&parallel, validator.address());
        activate_community_rewards(&sequential);
        activate_community_rewards(&parallel);

        let txs = vec![
            make_signed_community_reward(&validator, &worker_a, 1, b"ordered-a", 100),
            make_signed_community_reward(&validator, &worker_b, 2, b"ordered-b", 100),
        ];
        let (seq_block, seq_receipts) =
            sequential.execute_block(&txs, validator.address()).unwrap();
        let (stm_block, stm_receipts) = parallel
            .execute_block_stm(&txs, validator.address())
            .unwrap();

        assert_eq!(
            seq_receipts.iter().map(|r| r.success).collect::<Vec<_>>(),
            vec![true, false]
        );
        assert_eq!(
            stm_receipts.iter().map(|r| r.success).collect::<Vec<_>>(),
            vec![true, false]
        );
        assert_eq!(
            sequential.get_account(&worker_a.address()).unwrap().balance,
            parallel.get_account(&worker_a.address()).unwrap().balance
        );
        assert_eq!(
            sequential.get_account(&worker_b.address()).unwrap().balance,
            parallel.get_account(&worker_b.address()).unwrap().balance
        );
        assert_eq!(seq_block.header.state_root, stm_block.header.state_root);
    }

    #[test]
    fn attestation_unfunded_attester_still_insufficient_balance() {
        let pool = arc_types::transaction::faucet_pool_address();
        let worker = addr(62);
        let bond = REWARD;
        let state = StateDB::with_genesis(&[(pool, 10 * REWARD), (worker, 100)]);
        let before = sum_all_balances(&state);

        let tx = make_attestation(worker, 0, bond, 100, b"job-broke");
        let err = state.execute_tx_pub(&tx).unwrap_err();
        assert!(
            matches!(err, StateError::InsufficientBalance { have: 100, need } if need == bond),
            "error must be unchanged InsufficientBalance, got {:?}",
            err
        );

        assert_eq!(state.get_account(&pool).unwrap().balance, 10 * REWARD);
        let w = state.get_account(&worker).unwrap();
        assert_eq!(w.balance, 100);
        assert_eq!(w.nonce, 0);
        assert_eq!(sum_all_balances(&state), before, "supply conserved");
    }

    #[test]
    fn raw_attestation_never_drains_partial_treasury() {
        let pool = arc_types::transaction::faucet_pool_address();
        let worker = addr(63);
        let treasury_start = REWARD / 5;
        let state = StateDB::with_genesis(&[(pool, treasury_start), (worker, 0)]);
        let before = sum_all_balances(&state);

        let tx = make_attestation(worker, 0, 0, 100, b"job-drain");
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success);

        assert_eq!(state.get_account(&worker).unwrap().balance, 0);
        assert_eq!(state.get_account(&pool).unwrap().balance, treasury_start);
        assert_eq!(sum_all_balances(&state), before, "no tokens minted");
    }

    #[test]
    fn attestation_treasury_absent_skips_reward() {
        let worker = addr(64);
        let bond = REWARD / 4;
        let state = StateDB::with_genesis(&[(worker, REWARD)]);
        let before = sum_all_balances(&state);

        let tx = make_attestation(worker, 0, bond, 100, b"job-notreasury");
        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success);

        assert_eq!(state.get_account(&worker).unwrap().balance, REWARD - bond);
        assert!(
            state
                .get_account(&arc_types::transaction::faucet_pool_address())
                .is_none()
        );
        assert_eq!(sum_all_balances(&state), before, "supply conserved");
    }

    #[test]
    fn bond_released_after_challenge_window_conserved() {
        let pool = arc_types::transaction::faucet_pool_address();
        let worker = addr(65);
        let bond = REWARD / 2;
        let cp = 3u64;
        let state = StateDB::with_genesis(&[(pool, 10 * REWARD), (worker, 2 * REWARD)]);
        let before = sum_all_balances(&state);

        let tx = make_attestation(worker, 0, bond, cp, b"job-release");
        let att_hash = tx.hash;
        assert!(state.execute_block(&[tx], addr(99)).unwrap().1[0].success);

        let escrow_addr = hash_bytes(&[b"arc-inference", att_hash.as_ref()].concat());
        let release_h = state.get_account(&escrow_addr).unwrap().nonce;
        assert_eq!(release_h, 1 + cp, "attestation anchored at block 1");
        let worker_after_attest = state.get_account(&worker).unwrap().balance;
        assert_eq!(worker_after_attest, 2 * REWARD - bond);

        assert_eq!(state.sweep_matured_bond_releases(release_h - 1), 0);
        assert_eq!(state.get_account(&escrow_addr).unwrap().balance, bond);
        assert_eq!(
            state.get_account(&worker).unwrap().balance,
            worker_after_attest
        );

        assert_eq!(state.sweep_matured_bond_releases(release_h), 1);
        assert_eq!(state.get_account(&escrow_addr).unwrap().balance, 0);
        assert_eq!(
            state.get_account(&worker).unwrap().balance,
            worker_after_attest + bond
        );
        assert_eq!(state.sweep_matured_bond_releases(release_h), 0);
        assert_eq!(sum_all_balances(&state), before, "supply conserved");
    }

    #[test]
    fn challenged_bond_not_released_and_conserved() {
        let pool = arc_types::transaction::faucet_pool_address();
        let worker = addr(66);
        let challenger = addr(67);
        let bond = REWARD / 2;
        let ch_bond = REWARD;
        let cp = 3u64;
        let state = StateDB::with_genesis(&[
            (pool, 10 * REWARD),
            (worker, 2 * REWARD),
            (challenger, 3 * REWARD),
        ]);
        let before = sum_all_balances(&state);

        let att = make_attestation(worker, 0, bond, cp, b"job-challenged");
        let att_hash = att.hash;
        assert!(state.execute_block(&[att], addr(99)).unwrap().1[0].success);
        let escrow_addr = hash_bytes(&[b"arc-inference", att_hash.as_ref()].concat());
        let release_h = state.get_account(&escrow_addr).unwrap().nonce;

        let challenge = make_channel_tx(
            challenger,
            0,
            TxBody::InferenceChallenge(arc_types::transaction::InferenceChallengeBody {
                attestation_hash: att_hash,
                challenger_output_hash: hash_bytes(b"disagree"),
                challenger_bond: ch_bond,
            }),
            TxType::InferenceChallenge,
        );
        assert!(state.execute_block(&[challenge], addr(99)).unwrap().1[0].success);

        let esc = state.get_account(&escrow_addr).unwrap();
        assert_eq!(esc.balance, bond + ch_bond);
        assert_eq!(esc.storage_root.0[8], ATTEST_STATUS_CHALLENGED);

        assert_eq!(state.sweep_matured_bond_releases(release_h), 0);
        assert_eq!(state.sweep_matured_bond_releases(release_h + 100), 0);
        assert_eq!(
            state.get_account(&escrow_addr).unwrap().balance,
            bond + ch_bond,
            "challenged bond stays locked pending dispute resolution"
        );
        assert_eq!(sum_all_balances(&state), before, "supply conserved");
    }

    #[test]
    fn conservation_across_faucet_attestation_challenge_release() {
        let pool = arc_types::transaction::faucet_pool_address();
        let validator = addr(70);
        let worker_c = addr(71);
        let worker_b = addr(72);
        let worker_x = addr(73);
        let challenger = addr(74);
        let state = StateDB::with_genesis(&[
            (pool, 50 * REWARD),
            (validator, 0),
            (worker_c, 0),
            (worker_b, 5 * REWARD),
            (worker_x, 5 * REWARD),
            (challenger, 5 * REWARD),
        ]);
        seed_validator(&state, validator);
        let genesis_total = sum_all_balances(&state);

        macro_rules! conserved {
            () => {
                assert_eq!(
                    sum_all_balances(&state),
                    genesis_total,
                    "supply must be conserved at height {}",
                    state.height()
                );
            };
        }

        let faucet = make_channel_tx(
            validator,
            0,
            TxBody::FaucetClaim(FaucetClaimBody {
                recipient: worker_c,
                amount: 10_000,
            }),
            TxType::FaucetClaim,
        );
        assert!(state.execute_block(&[faucet], validator).unwrap().1[0].success);
        conserved!();

        let a_c = make_attestation(worker_c, 0, 0, 5, b"c");
        assert!(state.execute_block(&[a_c], validator).unwrap().1[0].success);
        conserved!();

        let a_b = make_attestation(worker_b, 0, REWARD, 4, b"b");
        let hb = a_b.hash;
        assert!(state.execute_block(&[a_b], validator).unwrap().1[0].success);
        conserved!();

        let a_x = make_attestation(worker_x, 0, REWARD, 50, b"x");
        let hx = a_x.hash;
        assert!(state.execute_block(&[a_x], validator).unwrap().1[0].success);
        conserved!();

        let ch = make_channel_tx(
            challenger,
            0,
            TxBody::InferenceChallenge(arc_types::transaction::InferenceChallengeBody {
                attestation_hash: hx,
                challenger_output_hash: hash_bytes(b"nope"),
                challenger_bond: 2 * REWARD,
            }),
            TxType::InferenceChallenge,
        );
        assert!(state.execute_block(&[ch], validator).unwrap().1[0].success);
        conserved!();

        let esc_b = hash_bytes(&[b"arc-inference", hb.as_ref()].concat());
        let release_b = state.get_account(&esc_b).unwrap().nonce;
        assert!(release_b > state.height(), "not matured yet");
        while state.height() < release_b {
            state.execute_block(&[], validator).unwrap();
            conserved!();
        }

        assert_eq!(state.get_account(&esc_b).unwrap().balance, 0);
        let esc_x = hash_bytes(&[b"arc-inference", hx.as_ref()].concat());
        assert_eq!(state.get_account(&esc_x).unwrap().balance, 3 * REWARD);
        assert_eq!(
            state.get_account(&esc_x).unwrap().storage_root.0[8],
            ATTEST_STATUS_CHALLENGED
        );
        conserved!();
    }

    #[test]
    fn rebuild_pending_bond_releases_recovers_open_escrows() {
        let pool = arc_types::transaction::faucet_pool_address();
        let worker = addr(75);
        let bond = REWARD / 2;
        let cp = 3u64;
        let state = StateDB::with_genesis(&[(pool, 10 * REWARD), (worker, 2 * REWARD)]);
        let tx = make_attestation(worker, 0, bond, cp, b"job-rebuild");
        let att_hash = tx.hash;
        assert!(state.execute_block(&[tx], addr(99)).unwrap().1[0].success);
        let escrow_addr = hash_bytes(&[b"arc-inference", att_hash.as_ref()].concat());
        let release_h = state.get_account(&escrow_addr).unwrap().nonce;

        state.pending_bond_releases.lock().clear();
        assert_eq!(state.pending_bond_release_count(), 0);
        assert_eq!(state.rebuild_pending_bond_releases(), 1);
        assert_eq!(state.pending_bond_release_count(), 1);

        assert_eq!(state.sweep_matured_bond_releases(release_h - 1), 0);
        assert_eq!(state.sweep_matured_bond_releases(release_h), 1);
        assert_eq!(state.get_account(&escrow_addr).unwrap().balance, 0);
    }
}
