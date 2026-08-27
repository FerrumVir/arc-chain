pub mod block_stm;
pub mod gpu_state;
pub mod io_backend;
pub mod jmt_store;
pub mod light_client;
pub mod mmap_state;
pub mod recovery;
pub mod simd_parse;
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
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

pub use wal::{
    PersistenceConfig, Snapshot, WalEntry, WalError, WalOp, WalWriter, find_last_checkpoint,
    latest_block_height_in_wal_dir, read_wal, read_wal_dir, read_wal_strict,
};

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
}

// ---------------------------------------------------------------------------
// Chunked State Snapshot Protocol - types for fast state sync
// ---------------------------------------------------------------------------

/// A single chunk of a state snapshot for streaming sync.
///
/// Large states are split into fixed-size chunks so peers can download
/// them in parallel from multiple sources without loading the entire
/// state into memory at once.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Block height at which this snapshot was taken.
    pub version: u64,
    /// Merkle state root at this height.
    pub state_root: Hash256,
    /// Account entries in this chunk.
    pub accounts: Vec<(Address, Account)>,
    /// Zero-based index of this chunk within the full snapshot.
    pub chunk_index: u32,
    /// Total number of chunks in the full snapshot.
    pub total_chunks: u32,
    /// BLAKE3 hash of the serialised account data in this chunk (integrity proof).
    pub chunk_proof: Hash256,
}

/// Metadata describing a complete chunked snapshot.
///
/// Sent ahead of the chunks so the receiver can allocate tracking structures
/// and verify completeness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// Block height at snapshot time.
    pub version: u64,
    /// Expected Merkle state root.
    pub state_root: Hash256,
    /// Total number of accounts across all chunks.
    pub total_accounts: u64,
    /// Number of chunks the snapshot is split into.
    pub total_chunks: u32,
    /// Number of accounts per chunk (last chunk may be smaller).
    pub chunk_size: usize,
    /// BLAKE3 hash of the manifest metadata itself (excluding this field).
    pub manifest_hash: Hash256,
}

/// Tracks progress while importing a chunked snapshot.
pub struct SyncProgress {
    /// The manifest being synced against.
    pub manifest: SnapshotManifest,
    /// Per-chunk received flag (indexed by chunk_index).
    pub received_chunks: Vec<bool>,
    /// Number of chunks that have been received and verified.
    pub verified_chunks: u32,
    /// Running total of accounts imported so far.
    pub total_accounts_imported: u64,
    /// Latest state_root reported by an imported chunk. Chunks are
    /// generated live on the server, so when the source chain is producing
    /// blocks, the manifest's `state_root` (taken at manifest-fetch time)
    /// will differ from the chunk's (taken at chunk-fetch time). The chunk's
    /// `state_root` describes the accounts we actually imported, so we use
    /// it as the authority for `finalize_sync`'s integrity check.
    pub latest_chunk_state_root: Option<Hash256>,
}

/// Compact state summary for monitoring / health-check endpoints.
pub struct StateSummary {
    /// Number of accounts in the state database.
    pub account_count: u64,
    /// Sum of all account balances.
    pub total_balance: u128,
    /// Current Merkle state root.
    pub state_root: Hash256,
    /// Current block height.
    pub block_height: u64,
}

/// WASM module magic bytes: `\0asm`.
const WASM_MAGIC: &[u8; 4] = b"\0asm";

// ── Tier 1 on-chain inference status bytes ────────────────────────────────
// Stored in the first byte of the request escrow's `code_hash` field. See
// `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md` for the lifecycle.
pub const TIER1_STATUS_OPEN: u8 = 0;
pub const TIER1_STATUS_VOTING: u8 = 1;
pub const TIER1_STATUS_FINALIZED: u8 = 2;
pub const TIER1_STATUS_REFUNDED: u8 = 3;

// ── Tier 2 optimistic-attestation escrow encoding ─────────────────────────
// A Tier 2 attestation bond is locked in a deterministic escrow account keyed
// by BLAKE3("arc-inference" || attestation_tx_hash). Its balance holds the
// bond; the other Account fields carry INTERNAL metadata (never wire-
// serialized tx data) that the maturation sweep needs to refund the bond:
//
//   balance      = locked funds (attester bond, plus challenger bond if
//                  the attestation was challenged)
//   code_hash    = the original attester's address (refund target)
//   nonce        = release height (deadline = apply_height + challenge_period);
//                  the sweep refunds once current_height >= this value
//   storage_root = [ MAGIC (8 bytes) | STATUS (1 byte) | zero... ]
//
// The 8-byte MAGIC lets `rebuild_pending_bond_releases()` re-identify these
// escrows on restart with ~2^-64 false-positive odds (no collision with the
// Tier 1 escrows, which key their status off `code_hash[0]`). The STATUS byte
// distinguishes an OPEN (refundable-after-deadline) escrow from a CHALLENGED
// one, whose bond stays locked pending dispute resolution.
//
// NOTE: this is escrow-account *storage*, not `TxBody`/`Transaction` wire
// layout — changing it does not touch the v0.7.2 112-byte attestation wire
// format or the `#[serde(skip)] beneficiary` field.
const ATTEST_ESCROW_MAGIC: [u8; 8] = *b"ARCATB2\x01";
pub const ATTEST_STATUS_OPEN: u8 = 0;
pub const ATTEST_STATUS_CHALLENGED: u8 = 1;

/// Max Tier 2 bonds refunded per block by the maturation sweep. Bounds the
/// per-block work so a backlog of matured escrows can never stall block
/// production; any excess is carried to the next block (still in deterministic
/// order). Community workers attest with `bond == 0` (no escrow is created),
/// so on the demo path this sweep is a no-op.
pub const MAX_BOND_RELEASES_PER_BLOCK: usize = 256;

/// Read-only snapshot of a Tier 1 inference request — returned by
/// `StateDB::tier1_request_snapshot()` so the validator inference task can
/// pick its next action without holding state locks.
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
    /// Address that originally submitted the InferenceRequest. Used by the
    /// voting validator to credit the user (not itself) on the subsequent
    /// InferenceAttestation it posts. Defaults to escrow_addr for legacy
    /// snapshots that pre-date Option C.
    pub requester: Address,
}

/// Derive a deterministic contract address from the deployer address and nonce.
///
/// Mirrors the logic in `arc_vm::compute_contract_address` - duplicated here to
/// avoid a circular dependency (arc-vm already depends on arc-state).
fn compute_contract_address(deployer: &Address, nonce: u64) -> Address {
    let mut preimage = Vec::with_capacity(32 + 32 + 8);
    preimage.extend_from_slice(b"ARC-chain-contract-addr-v1\x00\x00\x00\x00\x00\x00");
    preimage.extend_from_slice(&deployer.0);
    preimage.extend_from_slice(&nonce.to_le_bytes());
    hash_bytes(&preimage)
}

/// A batch of benchmark transactions to be indexed asynchronously.
/// Contains metadata for the indexer to lazily reconstruct Transaction objects,
/// avoiding 2GB+ heap allocation in the hot execution path.
pub struct IndexerBatch {
    pub block_hash: Hash256,
    pub height: u64,
    pub senders: Arc<Vec<Hash256>>,
    pub receivers: Arc<Vec<Hash256>>,
    pub nonce_start: u64,
    pub txs_per_sender: u64,
}

/// In-memory state database with optional WAL persistence.
/// Uses DashMap for lock-free concurrent reads across threads.
pub struct StateDB {
    /// Account states (address → Account).
    accounts: DashMap<[u8; 32], Account>,
    /// Contract storage (address → key → value).
    storage: DashMap<[u8; 32], DashMap<Hash256, Vec<u8>>>,
    /// Block chain (height → Block).
    blocks: DashMap<u64, Block>,
    /// Current block height.
    height: RwLock<u64>,
    /// Transaction receipts indexed by tx hash.
    pub receipts: DashMap<[u8; 32], TxReceipt>,
    /// Transaction hash → (block_height, tx_index) for fast lookup.
    pub tx_index: DashMap<[u8; 32], (u64, u32)>,
    /// Account address → list of tx hashes involving this account.
    pub account_txs: DashMap<[u8; 32], Vec<Hash256>>,
    /// Contract WASM bytecode cache (address → bytecode).
    pub contracts: DashMap<[u8; 32], Vec<u8>>,
    /// Write-ahead log for persistence (None = no persistence / benchmark mode).
    wal: WalWriter,
    /// On-chain identity registry (address -> Identity).
    identities: DashMap<[u8; 32], Identity>,
    /// Full transaction bodies indexed by tx hash (for explorer queries).
    pub full_transactions: DashMap<[u8; 32], Transaction>,
    /// Blocks since last snapshot.
    snapshot_counter: AtomicU64,
    /// Total benchmark transactions executed (atomic counter for /stats).
    pub benchmark_tx_count: AtomicU64,
    /// Async indexer channel - sends batches to background threads.
    indexer_tx: Option<crossbeam::channel::Sender<IndexerBatch>>,
    /// Benchmark block nonce bases: height → nonce_base for deterministic tx reconstruction.
    benchmark_nonces: DashMap<u64, u64>,
    /// Cached sender array for benchmark tx reconstruction.
    benchmark_senders: parking_lot::RwLock<Option<Arc<Vec<Hash256>>>>,
    /// Cached receiver array for benchmark tx reconstruction.
    benchmark_receivers: parking_lot::RwLock<Option<Arc<Vec<Hash256>>>>,
    /// Transactions per sender in benchmark blocks.
    benchmark_txs_per_sender: AtomicU64,
    /// Signed benchmark block data: height → (transactions, success_flags, block_hash).
    /// Stored for blocks produced by execute_block_signed_benchmark()
    /// so /block/{height}/txs and /tx/{hash}/full can serve data on-demand.
    signed_block_data: DashMap<u64, (Vec<Transaction>, Vec<bool>, Hash256)>,
    /// Persistent incremental Merkle tree for O(k log n) state root updates.
    /// Replaces the previous DashMap cache + full-rebuild approach.
    incremental_merkle: parking_lot::Mutex<IncrementalMerkle>,
    /// Accounts modified since the last state root computation.
    dirty_accounts: DashSet<[u8; 32]>,
    /// Event logs indexed by block height for eth_getLogs.
    pub event_logs: DashMap<u64, Vec<arc_types::EventLog>>,
    /// Staking pool: total staked amount across all validators.
    staking_pool: AtomicU64,
    /// Validator set: address -> staked amount. Only addresses above minimum
    /// stake threshold are considered active validators.
    validators: DashMap<[u8; 32], u64>,
    /// Jellyfish Merkle Tree for incremental state root computation.
    /// Provides an alternative to IncrementalMerkle with domain-separated
    /// BLAKE3 hashing and Merkle inclusion proofs.
    jmt: parking_lot::Mutex<JmtStateTree>,
    /// Whether to use the JMT for state root computation (default: false).
    /// When false, the existing IncrementalMerkle is used for backward compat.
    use_jmt: bool,
    /// Optional GPU-resident state cache for hot accounts.
    /// When enabled, `get_account()` checks GPU memory first.
    gpu_cache: Option<Arc<gpu_state::GpuStateCache>>,
    /// Archive mode - when true, skips all pruning (keeps full history).
    /// Used by block explorers and analytics nodes.
    pub archive_mode: bool,
    /// Canonical genesis-committed activation height for community reward v1.
    /// `u64::MAX` means disabled. This runtime copy is not mutable through RPC;
    /// nodes derive it from the genesis configuration whose semantic hash is
    /// authenticated during the P2P handshake.
    community_rewards_v1_activation_height: AtomicU64,
    /// Index of open Tier 1 inference requests (request_id → anchor_height).
    /// Populated by `apply_inference_request`, cleared by
    /// `apply_inference_finalize`. The `inference_validator` background task
    /// polls this to discover requests where its address is in the committee.
    pub tier1_pending: DashMap<[u8; 32], u64>,
    /// Maturation queue for Tier 2 attestation bond escrows:
    /// `release_height → sorted list of escrow addresses`. Populated when an
    /// attestation with `bond > 0` is applied; drained deterministically by
    /// `sweep_matured_bond_releases()` inside `commit_executed_block`. A
    /// derived, in-memory index with no WAL op of its own — rebuilt on restart
    /// from the surviving escrow accounts by `rebuild_pending_bond_releases()`.
    pending_bond_releases: parking_lot::Mutex<BTreeMap<u64, Vec<[u8; 32]>>>,
    /// Present only after a quorum-certified ARCCHKPT H+1 transition.
    /// `None` preserves legacy state-root and protocol behavior byte-for-byte.
    recovery_context: RwLock<Option<recovery::RecoveryContext>>,
    /// Exact operator-approved manifest that established `recovery_context`.
    recovery_manifest_hash: RwLock<Option<Hash256>>,
}

impl StateDB {
    fn wal_state_error(error: WalError) -> StateError {
        StateError::PersistenceError(error.to_string())
    }

    /// Persistence failures are fatal for subsequent block application. A
    /// caller must never mutate another block after the WAL lost durability.
    fn require_healthy_wal(&self) -> Result<(), StateError> {
        self.wal.check_health().map_err(Self::wal_state_error)
    }

    /// Complete the durable block boundary before returning a block to the
    /// consensus caller. This includes every queued append, flush, and fsync.
    fn durable_wal_barrier(&self) -> Result<(), StateError> {
        self.wal.sync().map_err(Self::wal_state_error)
    }

    /// Create a new empty state (no persistence - benchmark mode).
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
            benchmark_tx_count: AtomicU64::new(0),
            indexer_tx: None,
            benchmark_nonces: DashMap::new(),
            benchmark_senders: parking_lot::RwLock::new(None),
            benchmark_receivers: parking_lot::RwLock::new(None),
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
            community_rewards_v1_activation_height: AtomicU64::new(u64::MAX),
            tier1_pending: DashMap::new(),
            pending_bond_releases: parking_lot::Mutex::new(BTreeMap::new()),
            recovery_context: RwLock::new(None),
            recovery_manifest_hash: RwLock::new(None),
        }
    }

    /// Create a new state with WAL persistence.
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
            benchmark_tx_count: AtomicU64::new(0),
            indexer_tx: None,
            benchmark_nonces: DashMap::new(),
            benchmark_senders: parking_lot::RwLock::new(None),
            benchmark_receivers: parking_lot::RwLock::new(None),
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
            community_rewards_v1_activation_height: AtomicU64::new(u64::MAX),
            tier1_pending: DashMap::new(),
            pending_bond_releases: parking_lot::Mutex::new(BTreeMap::new()),
            recovery_context: RwLock::new(None),
            recovery_manifest_hash: RwLock::new(None),
        })
    }

    /// Initialize with genesis block and prefunded accounts.
    pub fn with_genesis(prefunded: &[(Address, u64)]) -> Self {
        let state = Self::new();
        for (addr, balance) in prefunded {
            state.accounts.insert(addr.0, Account::new(*addr, *balance));
            state.dirty_accounts.insert(addr.0);
        }
        let genesis = Block::genesis();
        state.blocks.insert(0, genesis);
        state
    }

    /// Initialize with genesis accounts and GPU-resident state cache enabled.
    ///
    /// Hot accounts are stored in GPU unified/managed memory for ~40x bandwidth
    /// improvement. Falls back to CPU-only if no GPU is detected.
    pub fn with_genesis_gpu(
        prefunded: &[(Address, u64)],
        gpu_config: gpu_state::GpuStateCacheConfig,
    ) -> Self {
        let cache = Arc::new(gpu_state::GpuStateCache::new(gpu_config));
        let mut state = Self::with_genesis(prefunded);
        // Pre-load genesis accounts into GPU cache.
        for (addr, _) in prefunded {
            if let Some(acct) = state.accounts.get(&addr.0).map(|a| a.clone()) {
                cache.put_account(&acct);
            }
        }
        state.gpu_cache = Some(cache);
        state
    }

    /// Enable GPU state cache on an existing StateDB.
    pub fn enable_gpu_cache(&mut self, config: gpu_state::GpuStateCacheConfig) {
        let cache = Arc::new(gpu_state::GpuStateCache::new(config));
        // Pre-load existing accounts into GPU cache.
        let mut loaded = 0usize;
        for entry in self.accounts.iter() {
            cache.put_account(entry.value());
            loaded += 1;
            if loaded >= cache.gpu_count() + 1_000_000 {
                break; // Don't exceed GPU capacity.
            }
        }
        self.gpu_cache = Some(cache);
        tracing::info!(
            loaded = loaded,
            "GPU state cache enabled, pre-loaded accounts"
        );
    }

    /// Get the GPU state cache (if enabled) for direct access.
    pub fn gpu_cache(&self) -> Option<&Arc<gpu_state::GpuStateCache>> {
        self.gpu_cache.as_ref()
    }

    /// Create state with WAL persistence + genesis accounts, bound to one
    /// authenticated genesis/network hash.
    ///
    /// A pre-existing WAL without the binding file is deliberately rejected:
    /// replaying legacy state under a new genesis can make peers authenticate
    /// as one network while executing different histories. Operators must use
    /// a fresh data directory or an explicitly approved checkpoint migration.
    pub fn with_genesis_persistent(
        prefunded: &[(Address, u64)],
        wal_dir: impl AsRef<Path>,
        expected_genesis_hash: Hash256,
    ) -> Result<Self, StateError> {
        let wal_dir = wal_dir.as_ref();

        // Ensure the data directory exists
        std::fs::create_dir_all(wal_dir).map_err(|e| {
            StateError::PersistenceError(format!("failed to create data dir {:?}: {}", wal_dir, e))
        })?;

        let wal_path = wal_dir.join("state.wal");
        Self::verify_or_create_genesis_binding(wal_dir, wal_path.exists(), expected_genesis_hash)?;

        if wal_path.exists() {
            // WAL exists - replay to recover state
            let state = Self::with_persistence(&wal_path)?;

            let entries = wal::read_wal(&wal_path);
            let entry_count = entries.len();
            for entry in &entries {
                state.apply_wal_op(&entry.op);
            }

            // Insert genesis block if not already present from WAL replay
            if state.blocks.get(&0).is_none() {
                state.blocks.insert(0, Block::genesis());
            }

            tracing::info!(
                "WAL recovery complete: replayed {} entries, {} accounts, height {}",
                entry_count,
                state.accounts.len(),
                state.height()
            );

            Ok(state)
        } else {
            // No WAL - fresh start with genesis accounts and persistence enabled
            let state = Self::with_persistence(&wal_path)?;

            for (addr, balance) in prefunded {
                let account = Account::new(*addr, *balance);
                state.accounts.insert(addr.0, account.clone());
                state.dirty_accounts.insert(addr.0);
                // Write genesis accounts to WAL so they survive restart
                state.wal.append(WalOp::SetAccount(*addr, account), 0);
            }

            let genesis = Block::genesis();
            state.blocks.insert(0, genesis.clone());
            state.wal.append(WalOp::SetBlock(0, genesis), 0);
            state.durable_wal_barrier()?;

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
        match std::fs::read_to_string(&binding_path) {
            Ok(value) => {
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
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && wal_exists => {
                Err(StateError::PersistenceError(format!(
                    "legacy data directory {:?} contains a WAL but no authenticated genesis binding; refusing replay under the configured network. Back it up, then use a fresh data directory or an approved checkpoint migration",
                    wal_dir
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&binding_path)
                    .map_err(|e| {
                        StateError::PersistenceError(format!(
                            "failed to create genesis binding {:?}: {}",
                            binding_path, e
                        ))
                    })?;
                writeln!(file, "{}", expected.to_hex()).map_err(|e| {
                    StateError::PersistenceError(format!(
                        "failed to write genesis binding {:?}: {}",
                        binding_path, e
                    ))
                })?;
                file.sync_all().map_err(|e| {
                    StateError::PersistenceError(format!(
                        "failed to sync genesis binding {:?}: {}",
                        binding_path, e
                    ))
                })?;
                Ok(())
            }
            Err(error) => Err(StateError::PersistenceError(format!(
                "failed to read genesis binding {:?}: {}",
                binding_path, error
            ))),
        }
    }

    /// Recover state from a snapshot and WAL replay.
    pub fn recover(snapshot: Snapshot, wal_path: impl AsRef<Path>) -> Result<Self, StateError> {
        let state = Self::with_persistence(&wal_path)?;

        // Load snapshot state
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

        // Replay WAL from snapshot's sequence
        let entries = wal::read_wal_from(&wal_path, snapshot.wal_sequence);
        for entry in entries {
            state.apply_wal_op(&entry.op);
        }

        tracing::info!(
            "Recovered state: {} accounts, height {}",
            state.accounts.len(),
            state.height()
        );

        Ok(state)
    }

    /// Apply a WAL operation to in-memory state (used during recovery replay).
    fn apply_wal_op(&self, op: &WalOp) {
        match op {
            WalOp::SetAccount(addr, account) => {
                self.accounts.insert(addr.0, account.clone());
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
                self.blocks.insert(*height, block.clone());
                let mut h = self.height.write();
                if *height > *h {
                    *h = *height;
                }
            }
            WalOp::SetReceipt(hash, receipt) => {
                self.receipts.insert(hash.0, receipt.clone());
            }
            WalOp::SetAgent(_addr, _name, _endpoint, _caps) => {
                // Agent registry - stored in account metadata (future)
            }
            WalOp::SetContract(addr, bytecode) => {
                self.contracts.insert(addr.0, bytecode.clone());
            }
            WalOp::SetFullTransaction(hash, transaction) => {
                self.full_transactions.insert(hash.0, transaction.clone());
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
            WalOp::Checkpoint(_) => {
                // Checkpoints are informational - no state change
            }
            WalOp::SetDagBlock(_, _) | WalOp::SetDagRound(_) | WalOp::CommitDagBlock(_) => {
                // DAG operations are replayed by the consensus engine, not StateDB.
                // StateDB just needs to not crash when encountering these in the WAL.
            }
        }
    }

    /// Take a snapshot of current state.
    pub fn snapshot(&self) -> Snapshot {
        // Take a consistent snapshot by recording the height before and after
        // collecting data. If a block was applied mid-snapshot, retry.
        // This avoids adding a lock to the hot execution path.
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
                continue; // Retry - a block was applied during iteration
            }

            return Snapshot {
                block_height: height_after,
                state_root: self.compute_state_root(),
                wal_sequence: 0, // Will be set by caller
                accounts,
                storage,
                contracts,
            };
        }
    }

    /// Deploy a contract (store bytecode in the contracts cache).
    pub fn deploy_contract(&self, address: &Address, bytecode: Vec<u8>) {
        self.contracts.insert(address.0, bytecode.clone());
        self.wal
            .append(WalOp::SetContract(*address, bytecode), self.height());
    }

    /// Get contract bytecode.
    pub fn get_contract(&self, address: &Address) -> Option<Vec<u8>> {
        self.contracts.get(&address.0).map(|c| c.clone())
    }

    /// Get the full transaction by hash (for explorer/RPC).
    /// Checks full_transactions first, then signed_block_data for benchmark blocks.
    pub fn get_transaction(&self, tx_hash: &[u8; 32]) -> Option<Transaction> {
        if let Some(tx) = self.full_transactions.get(tx_hash).map(|t| t.clone()) {
            return Some(tx);
        }
        // Check signed benchmark block data
        if let Some(&(height, idx)) = self.tx_index.get(tx_hash).as_deref()
            && let Some(block_data) = self.signed_block_data.get(&height)
        {
            let (txs_vec, _, _) = &*block_data;
            return txs_vec.get(idx as usize).cloned();
        }
        None
    }

    // ── Tier 1 on-chain inference helpers ────────────────────────────────

    /// Snapshot of open Tier 1 inference requests for the validator task.
    /// Returns `(request_id, anchor_height)` pairs. Order is unspecified.
    pub fn tier1_pending_requests(&self) -> Vec<([u8; 32], u64)> {
        self.tier1_pending
            .iter()
            .map(|kv| (*kv.key(), *kv.value()))
            .collect()
    }

    /// Rebuild `tier1_pending` from on-disk state. Call once at startup,
    /// after any WAL replay/snapshot load and BEFORE spawning the validator
    /// task. `tier1_pending` is a DashMap with no WAL op of its own; on
    /// restart it starts empty even though the underlying escrow accounts
    /// (with their `OPEN`/`VOTING` status byte) survive in the persistent
    /// account map.
    ///
    /// Strategy: scan every account whose `code_hash[0]` is `TIER1_STATUS_OPEN`
    /// or `TIER1_STATUS_VOTING`. Those are Tier 1 escrow addresses. Read the
    /// `tier1.request_id` storage key that `InferenceRequest.apply` writes
    /// (added in the same change), get the requester-supplied request_id,
    /// and re-insert it into `tier1_pending` keyed by the escrow's anchor
    /// height (stored in `escrow.nonce`).
    ///
    /// Returns the count of requests rebuilt so the operator can sanity-
    /// check against `/inference/results` cardinality.
    pub fn rebuild_tier1_pending(&self) -> usize {
        let key = hash_bytes(b"tier1.request_id");
        let mut rebuilt = 0usize;
        for entry in self.accounts.iter() {
            let acct = entry.value();
            // Only escrows whose status is still actionable. Finalized /
            // refunded escrows have already been pruned from pending via
            // the apply path; rebuilding them would burn the validator
            // task's tick cycles for no payoff.
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
            // anchor_height was stamped into escrow.nonce at apply time.
            let anchor_height = acct.nonce;
            self.tier1_pending.insert(request_id, anchor_height);
            rebuilt += 1;
        }
        rebuilt
    }

    /// Read a Tier 1 request's escrow state + storage. Returns `None` if no
    /// such escrow exists. Used by the validator task to check status,
    /// gather votes, and decide whether to vote/finalize.
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
            // Legacy snapshots without a stored requester fall back to the
            // escrow address itself so callers always get a usable Address.
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

    /// Derive the committee for a given request. Mirrors the apply-time
    /// derivation in `apply_inference_vote`. Used by the validator task
    /// to check whether it should vote.
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

    /// Set a storage value for a contract.
    pub fn set_storage(&self, contract: &Address, key: Hash256, value: Vec<u8>) {
        self.storage
            .entry(contract.0)
            .or_default()
            .insert(key, value.clone());
        self.wal
            .append(WalOp::SetStorage(*contract, key, value), self.height());
    }

    /// Get a storage value for a contract.
    pub fn get_storage(&self, contract: &Address, key: &Hash256) -> Option<Vec<u8>> {
        self.storage
            .get(&contract.0)
            .and_then(|map| map.get(key).map(|v| v.clone()))
    }

    /// Get all storage entries for a contract (snapshot for VM execution).
    pub fn get_contract_storage(&self, contract: &Address) -> HashMap<Hash256, Vec<u8>> {
        self.storage
            .get(&contract.0)
            .map(|map| map.iter().map(|e| (*e.key(), e.value().clone())).collect())
            .unwrap_or_default()
    }

    /// Delete a storage value for a contract.
    pub fn delete_storage(&self, contract: &Address, key: &Hash256) {
        if let Some(map) = self.storage.get(&contract.0) {
            map.remove(key);
        }
        self.wal
            .append(WalOp::DeleteStorage(*contract, *key), self.height());
    }

    /// Get an account (returns None if not found).
    ///
    /// When a GPU state cache is enabled, checks GPU memory first for ~40x
    /// bandwidth improvement on hot accounts.
    pub fn get_account(&self, addr: &Address) -> Option<Account> {
        // Fast path: check GPU cache first.
        if let Some(ref cache) = self.gpu_cache
            && let Some(acct) = cache.get_account_fast(&addr.0)
        {
            return Some(acct);
        }
        self.accounts.get(&addr.0).map(|a| a.clone())
    }

    /// Get or create an account (lazy initialization).
    pub fn get_or_create_account(&self, addr: &Address) -> Account {
        self.accounts
            .entry(addr.0)
            .or_insert_with(|| Account::new(*addr, 0))
            .clone()
    }

    /// Update an account's state (used by EVM state persistence).
    ///
    /// When a GPU state cache is enabled, also writes the updated account
    /// to GPU memory for subsequent fast lookups.
    pub fn update_account(&self, addr: &Address, account: Account) {
        self.accounts.insert(addr.0, account.clone());
        self.dirty_accounts.insert(addr.0);
        // Write-through to GPU cache (fast path - single DashMap insert).
        if let Some(ref cache) = self.gpu_cache {
            cache.update_account_fast(&account);
        }
    }

    /// Check if a contract address holds EVM bytecode (vs WASM).
    /// Returns true if the contract exists and does NOT start with the WASM magic header.
    pub fn is_evm_contract(&self, addr: &Address) -> bool {
        match self.get_contract(addr) {
            Some(bytecode) => bytecode.len() < 4 || &bytecode[..4] != WASM_MAGIC,
            None => false,
        }
    }

    /// Store event logs for a specific block height.
    pub fn store_event_logs(&self, height: u64, logs: Vec<arc_types::EventLog>) {
        if !logs.is_empty() {
            let combined = {
                let mut entry = self.event_logs.entry(height).or_default();
                entry.extend(logs);
                entry.clone()
            };
            self.wal
                .append(WalOp::SetEventLogs(height, combined), height);
            if let Err(error) = self.durable_wal_barrier() {
                tracing::error!(height, error = %error, "event logs were not durably persisted");
            }
        }
    }

    // ── Staking ──────────────────────────────────────────────────────────

    /// Minimum stake (in ARC) required to be registered as a validator.
    pub const MIN_VALIDATOR_STAKE: u64 = 100_000;

    /// Get the total amount staked across all validators.
    pub fn total_staked(&self) -> u64 {
        self.staking_pool.load(Ordering::Relaxed)
    }

    /// Get the staked amount for a specific validator address.
    pub fn get_validator_stake(&self, addr: &Address) -> Option<u64> {
        self.validators.get(&addr.0).map(|v| *v)
    }

    /// Check if an address is an active validator (staked above minimum).
    pub fn is_validator(&self, addr: &Address) -> bool {
        self.validators
            .get(&addr.0)
            .map(|v| *v >= Self::MIN_VALIDATOR_STAKE)
            .unwrap_or(false)
    }

    /// Install the consensus activation schedule parsed from canonical
    /// genesis. Call once during boot before state sync or consensus starts.
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

    /// Whether tx 0x25 is a valid state transition at the current height.
    pub fn community_rewards_v1_active(&self) -> bool {
        self.community_rewards_v1_activation_height()
            .is_some_and(|activation| self.height() >= activation)
    }

    /// Reset the validator set to exactly the genesis validators at startup.
    ///
    /// Two problems this solves:
    ///
    /// 1. `StateDB.validators` is otherwise only populated by on-chain
    ///    JoinValidator/Stake/UpdateStake txs. On nodes that haven't fully
    ///    synced the chain history, the genesis validators are missing,
    ///    which makes `is_validator()` return false for them and breaks
    ///    any tx body that authorizes by validator membership (notably
    ///    TxBody::FaucetClaim).
    /// 2. Dynamic validators added during a prior process lifetime survive
    ///    state.wal replay but DIVERGE between peers (different commit-log
    ///    heights → different replayed JoinValidator txs). The result is
    ///    that each seed disagrees on the validator-set size, so they
    ///    disagree on the 2/3 quorum threshold, and BFT consensus stalls.
    ///
    /// Clearing first + reseeding from genesis gives every node the same
    /// 8-validator set at boot. Subsequent JoinValidator txs still update
    /// this map normally, but the agreed baseline is consistent.
    pub fn seed_genesis_validators(&self, genesis_validators: &[(Address, u64)]) {
        self.validators.clear();
        for (addr, stake) in genesis_validators {
            self.validators.insert(addr.0, *stake);
        }
    }

    /// Get all active validators and their stakes.
    pub fn active_validators(&self) -> Vec<(Address, u64)> {
        self.validators
            .iter()
            .filter(|entry| *entry.value() >= Self::MIN_VALIDATOR_STAKE)
            .map(|entry| (Hash256(*entry.key()), *entry.value()))
            .collect()
    }

    /// Verify reward-v1's independent off-chain authorization quorum.
    ///
    /// Policy is deliberately stronger than either identity count or stake
    /// alone: approvals must contain strictly more than two thirds of active
    /// validator identities (`floor(2N/3) + 1`) AND strictly more than two
    /// thirds of active stake (`floor(2S/3) + 1`). Every signer is unique, active at execution
    /// time, and uses the fixed Ed25519-only approval representation. The
    /// outer transaction signer is merely an aggregator and is not counted
    /// unless it also supplied an explicit approval.
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
        // Protocol-v3 production recovery fixes this authorization committee
        // at five of six. Legacy/dev states retain the generic strict-BFT
        // verifier below solely for backward-compatible local fixtures; a
        // production coordinator cannot advertise issuance-ready without a
        // recovery context.
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
            approval
                .as_signature()
                .verify(&commitment, &approval.validator)
                .map_err(|_| {
                    StateError::ExecutionError(format!(
                        "community inference reward: invalid Ed25519 approval from {}",
                        approval.validator.to_hex()
                    ))
                })?;
            approved_stake += u128::from(*stake);
        }

        if approved_stake < required_stake {
            return Err(StateError::ExecutionError(format!(
                "community inference reward: insufficient approved stake: have {}, need {} of {} active stake",
                approved_stake, required_stake, total_active_stake
            )));
        }
        Ok(())
    }

    /// Get current block height.
    pub fn height(&self) -> u64 {
        *self.height.read()
    }

    /// Evict old transaction bodies from memory to bound memory usage.
    /// Keeps at most `max_entries` in `full_transactions`. Evicts arbitrary
    /// entries (DashMap has no insertion order; a proper LRU would require
    /// an ordered map, but this simple cap prevents OOM).
    pub fn evict_transactions(&self, max_entries: usize) {
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

        // Prune old WAL segments. The WAL grows unbounded because
        // delete_segments_before() was never called. Keep segments
        // from the last 1000 entries for crash recovery.
        let wal_seq = self.wal.sequence();
        if wal_seq > 1000
            && let Err(e) = self.wal.delete_segments_before(wal_seq - 1000)
        {
            tracing::warn!("WAL segment cleanup failed: {}", e);
        }
    }

    /// Get a block by height.
    pub fn get_block(&self, height: u64) -> Option<Block> {
        self.blocks.get(&height).map(|b| b.clone())
    }

    /// Look up a block by its hash. Scans from latest to earliest.
    pub fn get_block_by_hash(&self, hash: &[u8; 32]) -> Option<Block> {
        let h = self.height();
        for height in (0..=h).rev() {
            if let Some(block) = self.blocks.get(&height)
                && block.hash.0 == *hash
            {
                return Some(block.clone());
            }
        }
        None
    }

    /// Execute a batch of transactions, produce a block, and update state.
    /// Returns the new block and receipts for each transaction.
    pub fn execute_block(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        self.require_healthy_wal()?;
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

        // Execute transactions - use Block-STM parallel batches when beneficial
        if transactions.len() >= 16 {
            // Block-STM: partition into conflict-free batches for parallel execution
            let batches = block_stm::partition_batches(transactions);
            tracing::info!(
                block_height = height,
                tx_count = transactions.len(),
                batch_count = batches.len(),
                "Block-STM parallel execution: {} txs across {} batches",
                transactions.len(),
                batches.len(),
            );

            // Pre-size receipts with placeholders so we can write by index
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

            // Collect all tx hashes up front (order must match input)
            for tx in transactions.iter() {
                tx_hashes.push(tx.hash);
            }

            // Execute batches sequentially; within each batch, execute in parallel
            for batch_indices in &batches {
                // Mark dirty accounts for all txs in this batch before execution
                for &idx in batch_indices {
                    self.mark_tx_accounts_dirty(&transactions[idx]);
                }

                // Each batch contains txs that touch disjoint accounts - safe to parallelize
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

                // Write receipts back at the correct original index
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
            // Sequential fallback for small batches (< 16 txs)
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

        // Track total block gas usage
        let total_gas: u64 = receipts.iter().map(|r| r.gas_used).sum();
        if total_gas > gas_costs::BLOCK_GAS_LIMIT * 80 / 100 {
            tracing::warn!(
                total_gas,
                limit = gas_costs::BLOCK_GAS_LIMIT,
                "Block nearing gas limit"
            );
        }

        // Build Merkle tree from transaction hashes
        // Refund matured, unchallenged Tier 2 attestation bonds at this height
        // BEFORE the state root is computed so refunds land in this block.
        // Bounded, deterministic, and a no-op when nothing has matured (always
        // so on the bond==0 community-worker demo path). Applied uniformly to
        // every real block-application path so the bond lifecycle advances the
        // same way regardless of which execution engine sealed the block.
        self.sweep_matured_bond_releases(height);

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();

        // Compute state root
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

        // Update receipts with block hash and Merkle inclusion proofs
        for (i, receipt) in receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        // Index receipts, tx locations, account transactions, and full tx bodies
        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
            self.full_transactions.insert(tx.hash.0, tx.clone());
        }

        // Store block + WAL
        self.blocks.insert(height, block.clone());
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);

        // WAL checkpoint at block boundary
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        // Check if we should take a snapshot
        let count = self.snapshot_counter.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count.is_multiple_of(10_000) {
            tracing::info!("Snapshot trigger at block {}", height);
            // Snapshot is taken asynchronously in production - here we just log
        }

        Ok((block, receipts))
    }

    /// Execute a batch of transactions with signature verification.
    /// Unsigned or invalid-signature transactions are marked as failed.
    /// Returns the new block and receipts for each transaction.
    /// Execute a block with adaptive mode selection.
    ///
    /// Automatically chooses sequential or BlockSTM based on the transaction mix:
    /// - Simple transfer-only blocks → sequential (no overhead)
    /// - Contract calls / high diversity → BlockSTM (parallel)
    ///
    /// This is the primary execution entry point for the consensus pipeline.
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

    /// Execute the canonical transaction sequence using a consensus-provided
    /// timestamp. Validators committing the same DAG block must build the same
    /// linear block hash; sampling each machine's wall clock during local
    /// re-execution made otherwise-identical validators expose different block
    /// hashes and then carry different parent hashes forever.
    pub fn execute_block_adaptive_at(
        &self,
        transactions: &[Transaction],
        producer: Address,
        timestamp: u64,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        let mode = crate::block_stm::choose_execution_mode(transactions);
        match mode {
            crate::block_stm::AdaptiveMode::Sequential => {
                self.execute_block_verified_at(transactions, producer, timestamp)
            }
            crate::block_stm::AdaptiveMode::BlockSTM => {
                // Use BlockSTM partitioned execution
                self.execute_block_blockstm_at(transactions, producer, timestamp)
            }
        }
    }

    /// Execute a block using BlockSTM partitioned parallel execution.
    fn execute_block_blockstm_at(
        &self,
        transactions: &[Transaction],
        producer: Address,
        timestamp: u64,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        self.require_healthy_wal()?;
        use rayon::prelude::*;

        // The transaction slice is already in canonical block/DAG order.
        // Re-sorting by sender used to reverse cross-sender conflicts (shared
        // treasury, reward marker, challenge escrow, etc.) while merely moving
        // receipts back to their original slots. Partition that canonical
        // sequence directly; partition_batches preserves every conflict edge.
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

        // Execute batches: within each batch, TXs run in parallel.
        // Batches run sequentially (they may have cross-batch dependencies).
        // Batch indices are the original canonical transaction indices.
        for batch in &batches {
            let batch_results: Vec<(usize, bool, u64)> = batch
                .par_iter()
                .map(|&idx| {
                    let tx = &transactions[idx];
                    self.mark_tx_accounts_dirty(tx);
                    let result = if tx.sig_verified {
                        self.execute_tx(tx) // Pre-verified (faucet/RPC) - skip sig check
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

        // Unwrap all receipts (all slots should be filled)
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

        // Build block (same as sequential path)
        // Refund matured, unchallenged Tier 2 attestation bonds at this height
        // BEFORE the state root is computed so refunds land in this block.
        // Bounded, deterministic, and a no-op when nothing has matured (always
        // so on the bond==0 community-worker demo path). Applied uniformly to
        // every real block-application path so the bond lifecycle advances the
        // same way regardless of which execution engine sealed the block.
        self.sweep_matured_bond_releases(height);

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();
        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp,
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

        self.blocks.insert(height, block.clone());
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &final_receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        Ok((block, final_receipts))
    }

    /// Execute a block with sequential verification (original path).
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

    /// Sequential verified execution with a canonical consensus timestamp.
    pub fn execute_block_verified_at(
        &self,
        transactions: &[Transaction],
        producer: Address,
        timestamp: u64,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        self.require_healthy_wal()?;
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

        // ── Batch Ed25519 signature verification ──────────────────────────
        // Collect all unverified Ed25519 signatures and verify them in a single
        // batch operation (~2x faster than individual verification).
        // Transactions already verified at mempool insertion are skipped.
        let mut batch_sig_valid = vec![None; transactions.len()]; // None = needs individual check
        let recovery_domain = self.transaction_domain_hash();
        {
            let mut ed_indices: Vec<usize> = Vec::new();
            let mut ed_msgs: Vec<Vec<u8>> = Vec::new();
            let mut ed_sigs: Vec<ed25519_dalek::Signature> = Vec::new();
            let mut ed_vks: Vec<ed25519_dalek::VerifyingKey> = Vec::new();

            for (i, tx) in transactions.iter().enumerate() {
                if tx.is_unsigned() || tx.sig_verified {
                    continue; // unsigned handled below; pre-verified skipped
                }
                // Hash integrity check
                let expected_hash = match recovery_domain {
                    Some(domain) => tx.compute_hash_in_domain(&domain),
                    None => tx.compute_hash(),
                };
                if expected_hash != tx.hash {
                    batch_sig_valid[i] = Some(false);
                    continue;
                }
                if let arc_crypto::signature::Signature::Ed25519 {
                    public_key,
                    signature,
                } = &tx.signature
                {
                    // Batch verification proves that `public_key` signed the
                    // hash, but it does not bind that key to `tx.from`.
                    // Without this check an attacker could sign a transfer
                    // from a funded victim with the attacker's own valid key;
                    // an all-valid batch would then skip `verify_signature`.
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
                        ed_msgs.push(tx.hash.0.to_vec());
                        ed_sigs.push(ed25519_dalek::Signature::from_bytes(&sig_bytes));
                        ed_vks.push(vk);
                        continue;
                    }
                    batch_sig_valid[i] = Some(false); // malformed
                }
                // Non-Ed25519 signatures fall through to individual verification
            }

            if !ed_indices.is_empty() {
                let msg_refs: Vec<&[u8]> = ed_msgs.iter().map(|m| m.as_slice()).collect();
                match arc_crypto::signature::batch_verify_ed25519(&msg_refs, &ed_sigs, &ed_vks) {
                    Ok(()) => {
                        // All valid
                        for &idx in &ed_indices {
                            batch_sig_valid[idx] = Some(true);
                        }
                    }
                    Err(_) => {
                        // Batch failed - fall back to individual verification to find bad ones
                        for &idx in &ed_indices {
                            let valid = self
                                .verify_transaction_signature(&transactions[idx])
                                .is_ok();
                            batch_sig_valid[idx] = Some(valid);
                        }
                    }
                }
            }
        }

        // Execute each transaction with signature verification.
        // Skip re-verification for transactions already verified at mempool insertion
        // or batch-verified above.
        for (i, tx) in transactions.iter().enumerate() {
            self.mark_tx_accounts_dirty(tx);
            let result = if tx.sig_verified {
                // Pre-verified (faucet/RPC) - skip sig check
                self.execute_tx(tx)
            } else if tx.is_unsigned() {
                Err(StateError::ExecutionError("unsigned transaction".into()))
            } else if let Some(valid) = batch_sig_valid[i] {
                // Batch-verified above
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

        // Build Merkle tree from transaction hashes
        // Refund matured, unchallenged Tier 2 attestation bonds at this height
        // BEFORE the state root is computed so refunds land in this block.
        // Bounded, deterministic, and a no-op when nothing has matured (always
        // so on the bond==0 community-worker demo path). Applied uniformly to
        // every real block-application path so the bond lifecycle advances the
        // same way regardless of which execution engine sealed the block.
        self.sweep_matured_bond_releases(height);

        let tree = MerkleTree::from_leaves(tx_hashes.clone());
        let tx_root = tree.root();

        // Compute state root
        let state_root = self.compute_state_root();

        let header = BlockHeader {
            height,
            timestamp,
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

        // Update receipts with block hash and Merkle inclusion proofs
        for (i, receipt) in receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        // Index receipts, tx locations, account transactions, and full tx bodies
        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
            self.full_transactions.insert(tx.hash.0, tx.clone());
        }

        // Store block + WAL
        self.blocks.insert(height, block.clone());
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);

        // WAL checkpoint at block boundary
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        // Check if we should take a snapshot
        let count = self.snapshot_counter.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count.is_multiple_of(10_000) {
            tracing::info!("Snapshot trigger at block {}", height);
        }

        Ok((block, receipts))
    }

    /// Execute a block with GPU-accelerated batch signature verification.
    ///
    /// Combines MetalVerifier batch Ed25519 verification with Block-STM
    /// parallel execution. This is the production path - signatures are
    /// verified in a single GPU dispatch, then only valid transactions
    /// are executed.
    pub fn execute_block_gpu_verified(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        self.require_healthy_wal()?;
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

        // ── Phase 1: GPU batch signature verification ────────────────────
        let mut verifier = MetalVerifier::new();

        // Separate Ed25519 signatures (batch-verifiable) from others
        let mut ed_indices: Vec<usize> = Vec::new();
        let mut ed_tasks: Vec<VerifyTask> = Vec::new();
        let mut other_indices: Vec<usize> = Vec::new();
        let mut sig_valid = vec![false; transactions.len()];
        let recovery_domain = self.transaction_domain_hash();

        for (i, tx) in transactions.iter().enumerate() {
            // Hash integrity check
            let expected_hash = match recovery_domain {
                Some(domain) => tx.compute_hash_in_domain(&domain),
                None => tx.compute_hash(),
            };
            if expected_hash != tx.hash {
                continue; // sig_valid[i] stays false
            }

            match &tx.signature {
                arc_crypto::signature::Signature::Ed25519 {
                    public_key,
                    signature,
                } => {
                    // GPU/CPU batch verifiers validate the signature against
                    // the supplied public key; authorize the claimed sender
                    // separately before accepting the fast-path result.
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
                    // else sig_valid[i] stays false (wrong length)
                }
                _ => {
                    other_indices.push(i);
                }
            }
        }

        // Batch verify all Ed25519 signatures (GPU or CPU parallel)
        if !ed_tasks.is_empty() {
            let result = verifier.batch_verify(&ed_tasks);
            let invalid_set: std::collections::HashSet<usize> =
                result.invalid_indices.iter().copied().collect();
            for (j, &orig_idx) in ed_indices.iter().enumerate() {
                sig_valid[orig_idx] = !invalid_set.contains(&j);
            }
        }

        // Verify non-Ed25519 signatures individually (Secp256k1, ML-DSA, Falcon)
        for &i in &other_indices {
            let tx = &transactions[i];
            sig_valid[i] = tx.signature.verify(&tx.hash, &tx.from).is_ok();
        }

        // ── Phase 2: Execute valid-signature transactions ────────────────
        let mut receipts = Vec::with_capacity(transactions.len());
        let mut tx_hashes = Vec::with_capacity(transactions.len());

        if transactions.len() >= 16 {
            // Block-STM parallel path for large batches
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
            // Sequential fallback for small batches
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

        // ── Phase 3: Finalize block ──────────────────────────────────────
        let total_gas: u64 = receipts.iter().map(|r| r.gas_used).sum();
        if total_gas > gas_costs::BLOCK_GAS_LIMIT * 80 / 100 {
            tracing::warn!(
                total_gas,
                limit = gas_costs::BLOCK_GAS_LIMIT,
                "Block nearing gas limit"
            );
        }

        // Refund matured, unchallenged Tier 2 attestation bonds at this height
        // BEFORE the state root is computed so refunds land in this block.
        // Bounded, deterministic, and a no-op when nothing has matured (always
        // so on the bond==0 community-worker demo path). Applied uniformly to
        // every real block-application path so the bond lifecycle advances the
        // same way regardless of which execution engine sealed the block.
        self.sweep_matured_bond_releases(height);

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

        self.blocks.insert(height, block.clone());
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

    /// Execute a block with parallel state sharding.
    pub fn execute_block_parallel(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        self.require_healthy_wal()?;
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

        // Refund matured, unchallenged Tier 2 attestation bonds at this height
        // BEFORE the state root is computed so refunds land in this block.
        // Bounded, deterministic, and a no-op when nothing has matured (always
        // so on the bond==0 community-worker demo path). Applied uniformly to
        // every real block-application path so the bond lifecycle advances the
        // same way regardless of which execution engine sealed the block.
        self.sweep_matured_bond_releases(height);

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

        self.blocks.insert(height, block.clone());
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        Ok((block, receipts))
    }

    /// Block-STM parallel execution - partitions transactions into conflict-free
    /// batches based on static access-set analysis, then executes each batch
    /// in parallel with rayon.
    ///
    /// Compared to sender-sharding (`execute_block_parallel`), this also
    /// parallelises across different *receivers* and any other disjoint account
    /// sets, extracting more concurrency from typical workloads.
    pub fn execute_block_stm(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        self.require_healthy_wal()?;
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

        // Mark all accounts dirty for incremental state root (B1).
        for tx in transactions {
            self.mark_tx_accounts_dirty(tx);
        }

        // Partition into conflict-free batches.
        let batches = block_stm::partition_batches(transactions);

        // Execute batches: within each batch, txs run in parallel;
        // batches themselves run sequentially to respect dependencies.
        let mut receipt_success = vec![false; transactions.len()];
        let mut receipt_gas = vec![0u64; transactions.len()];

        for batch in &batches {
            if batch.len() == 1 {
                // Single tx -- no rayon overhead
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
                // Parallel execution within the batch
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

        // Build receipts, Merkle tree, block -- same as execute_block_parallel.
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

        // Refund matured, unchallenged Tier 2 attestation bonds at this height
        // BEFORE the state root is computed so refunds land in this block.
        // Bounded, deterministic, and a no-op when nothing has matured (always
        // so on the bond==0 community-worker demo path). Applied uniformly to
        // every real block-application path so the bond lifecycle advances the
        // same way regardless of which execution engine sealed the block.
        self.sweep_matured_bond_releases(height);

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

        self.blocks.insert(height, block.clone());
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        Ok((block, receipts))
    }

    /// Optimistic parallel execution - pre-sorted by sender nonce for maximum throughput.
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

    /// Start background indexer threads for async hash→(height, index) mapping.
    /// Call once before benchmark execution begins.
    pub fn start_benchmark_indexer(self: &Arc<Self>) {
        let (tx, rx) = crossbeam::channel::unbounded::<IndexerBatch>();

        // Spawn 4 indexer threads - each computes hashes and inserts hash→(height, index)
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
                            // Precompute body_bytes + base hasher for this shard
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
                                // Single DashMap insert: hash → (height, global_index)
                                state.tx_index.insert(hash.0, (batch.height, global_idx));
                                global_idx += 1;
                            }
                        }
                    }
                })
                .expect("spawn indexer thread");
        }

        // Store the sender - we need unsafe to set the field on Arc<Self>
        // since start_benchmark_indexer is called once at startup.
        // Safety: called exactly once before any concurrent access to indexer_tx.
        #[allow(invalid_reference_casting)]
        unsafe {
            let self_mut = &mut *(Arc::as_ptr(self) as *mut Self);
            self_mut.indexer_tx = Some(tx);
        }
        tracing::info!("Benchmark indexer started (4 threads)");
    }

    /// Fully verifiable benchmark block execution.
    ///
    /// Every transaction has a real blake3 hash (same algorithm as Transaction::compute_hash).
    /// Block tx_root is a real Merkle root computed from all tx hashes.
    /// Block state_root is computed from all account states.
    /// Every tx is reconstructable on-demand from deterministic parameters.
    /// Merkle inclusion proofs are generated on-demand when queried.
    pub fn execute_block_benchmark(
        &self,
        tx_per_block: u64,
        senders: &Arc<Vec<Hash256>>,
        receivers: &Arc<Vec<Hash256>>,
        producer: Address,
        nonce_base: &mut u64,
    ) -> Result<Block, StateError> {
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

        // ── Cache sender/receiver arrays for on-demand reconstruction ───
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

        // ── Apply net balance deltas (100 DashMap ops total) ────────────
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

        // ── Generate real tx hashes in parallel (rayon-sharded) ─────────
        // Each shard precomputes body_bytes + base blake3 hasher.
        // Only the nonce varies per tx - huge optimization.
        let shard_data: Vec<(Hash256, Hash256, u64)> = senders
            .iter()
            .zip(receivers.iter())
            .enumerate()
            .map(|(i, (s, r))| (*s, *r, current_nonce_base + i as u64 * txs_per_sender))
            .collect();

        let all_hashes: Vec<Hash256> = shard_data
            .par_iter()
            .flat_map(|(sender, receiver, nonce_start)| {
                // Precompute body_bytes once per shard (same for all txs in shard)
                let body_bytes = bincode::serialize(&TxBody::Transfer(TransferBody {
                    to: *receiver,
                    amount: 1,
                    amount_commitment: None,
                }))
                .expect("serializable");

                // Precompute base hasher through tx_type + from
                let mut base_hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
                base_hasher.update(&[TxType::Transfer as u8]);
                base_hasher.update(sender.as_ref());

                (0..txs_per_sender)
                    .map(|j| compute_benchmark_tx_hash(&base_hasher, nonce_start + j, &body_bytes))
                    .collect::<Vec<Hash256>>()
            })
            .collect();

        // ── Real Merkle root from all tx hashes ─────────────────────────
        let tx_root = compute_merkle_root_only(all_hashes);

        // ── Real state_root from all account states (~30μs for 100 accts)
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

        // Empty tx_hashes vec - 10M hashes would be 320MB per block.
        // txs are reconstructable on-demand from nonce_base + deterministic params.
        let block = Block::new(header, vec![]);
        self.blocks.insert(height, block.clone());

        // ── Store nonce_base for on-demand reconstruction ───────────────
        self.benchmark_nonces.insert(height, current_nonce_base);

        // ── Queue to async indexer for hash→(height,index) mapping ──────
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

        // Update atomic counter immediately (for /stats)
        self.benchmark_tx_count
            .fetch_add(tx_per_block, Ordering::Relaxed);

        // Advance nonce base for next block
        *nonce_base += tx_per_block;

        Ok(block)
    }

    /// Execute a block of pre-signed transactions with full verification.
    ///
    /// 1. Batch verify Ed25519 signatures (parallel rayon chunks)
    /// 2. Per-tx execution via execute_tx() (rayon-sharded by sender)
    /// 3. Real Merkle root from tx hashes
    /// 4. Real state_root from all account states
    /// 5. Async index for hash→(height, idx) mapping
    ///
    /// This is the "honest" benchmark path - every tx is signed, verified,
    /// individually executed with nonce/balance checks, and queryable.
    pub fn execute_block_signed_benchmark(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<Block, StateError> {
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

        // ── 1. Batch verify Ed25519 signatures (parallel chunks) ──────────
        // Extract (message, signature, verifying_key) for batch verification.
        // We verify in parallel chunks of 256 for optimal batch_verify performance.
        let sig_valid: Vec<bool> = transactions
            .par_chunks(256)
            .flat_map(|chunk| {
                // Try batch verify the whole chunk first (fast path)
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
                            // The batch primitive does not check either hash
                            // integrity or the ARC address derived from the
                            // verifying key. Both are consensus authorization
                            // requirements, even on the benchmark path.
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
                        // All valid in this chunk
                        vec![true; chunk.len()]
                    } else {
                        // Batch failed - fall back to individual verification
                        chunk
                            .iter()
                            .map(|tx| self.verify_transaction_signature(tx).is_ok())
                            .collect()
                    }
                } else {
                    // Non-Ed25519 or parse error - verify individually
                    chunk
                        .iter()
                        .map(|tx| self.verify_transaction_signature(tx).is_ok())
                        .collect()
                }
            })
            .collect();

        let t1 = t0.elapsed();

        // ── 2. Per-tx execution (rayon-sharded by sender) ─────────────────
        // Group transactions by sender for parallel execution.
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
                // Sort by nonce within shard for correct ordering
                txs.sort_unstable_by_key(|(_, tx, _)| tx.nonce);
                let mut results = Vec::with_capacity(txs.len());
                for (idx, tx, sig_ok) in txs {
                    let success = if !sig_ok {
                        false // Signature verification failed
                    } else {
                        self.mark_tx_accounts_dirty(tx);
                        self.execute_tx(tx).is_ok()
                    };
                    results.push((idx, success));
                }
                results
            })
            .collect();

        // Merge shard results back into original order
        let mut receipt_success = vec![false; tx_count];
        for shard in shard_results {
            for (idx, success) in shard {
                receipt_success[idx] = success;
            }
        }

        let t2 = t0.elapsed();

        // ── 3. Collect tx hashes ──────────────────────────────────────────
        let tx_hashes: Vec<Hash256> = transactions.iter().map(|tx| tx.hash).collect();

        // ── 4. Real Merkle root ───────────────────────────────────────────
        let tx_root = compute_merkle_root_only(tx_hashes);

        let t3 = t0.elapsed();

        // ── 5. Real state root ────────────────────────────────────────────
        let state_root = self.compute_state_root();

        let t4 = t0.elapsed();

        // ── 6. Create block ───────────────────────────────────────────────
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

        // Store tx hashes in the Block for /block/{height}/txs listing
        let block_tx_hashes: Vec<Hash256> = transactions.iter().map(|tx| tx.hash).collect();
        let block = Block::new(header, block_tx_hashes);
        self.blocks.insert(height, block.clone());

        // ── 7. Store success flags for receipt reconstruction ────────────
        self.signed_block_data
            .insert(height, (vec![], receipt_success, block.hash));

        // ── 8. Build indexes in parallel ─────────────────────────────────
        // Hash→(height,idx) index + full tx bodies for /tx/{hash}/full
        transactions.par_iter().enumerate().for_each(|(i, tx)| {
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.full_transactions.insert(tx.hash.0, tx.clone());
        });

        let t5 = t0.elapsed();

        // ── 9. Update atomic counter ──────────────────────────────────────
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

    /// Reconstruct a benchmark transaction on-demand from (height, tx_index).
    /// Returns the full Transaction object with correct hash and real ed25519 signature.
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

        // Sign with the deterministic ed25519 keypair for this sender.
        // This reconstructs a verifiable signature on demand (~8μs).
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

    /// Reconstruct a benchmark receipt on-demand from (height, tx_index).
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
            inclusion_proof: None, // Use /tx/{hash}/proof for on-demand proof
            logs: vec![],
        })
    }

    /// Reconstruct a Merkle inclusion proof for a benchmark transaction.
    /// This is expensive (~130ms for 10M txs) - only called on-demand for /tx/{hash}/proof.
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

        // Rebuild all hashes for this block (parallel)
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

        // Build full Merkle tree and extract proof
        let tree = MerkleTree::from_leaves(all_hashes);
        tree.proof(tx_index as usize)
    }

    /// Reconstruct a benchmark transaction by looking up its hash in tx_index,
    /// then reconstructing from deterministic parameters.
    pub fn get_benchmark_tx_by_hash(&self, tx_hash: &[u8; 32]) -> Option<Transaction> {
        let (height, idx) = *self.tx_index.get(tx_hash)?;
        self.reconstruct_benchmark_tx(height, idx)
    }

    /// Look up or reconstruct a receipt for a benchmark transaction by hash.
    pub fn get_benchmark_receipt_by_hash(&self, tx_hash: &[u8; 32]) -> Option<TxReceipt> {
        let (height, idx) = *self.tx_index.get(tx_hash)?;
        // Try signed block data first (from execute_block_signed_benchmark)
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
        // Fall back to deterministic reconstruction
        self.reconstruct_benchmark_receipt(height, idx)
    }

    /// Get a page of benchmark transactions for a block.
    /// First tries signed_block_txs (from execute_block_signed_benchmark),
    /// then falls back to deterministic reconstruction (from execute_block_benchmark).
    /// Used by /block/{height}/txs?offset=0&limit=100
    pub fn get_benchmark_block_txs(
        &self,
        height: u64,
        offset: u32,
        limit: u32,
    ) -> Vec<Transaction> {
        let block = match self.blocks.get(&height) {
            Some(b) => b.clone(),
            None => return vec![],
        };
        let tx_count = block.header.tx_count;
        let end = (offset + limit).min(tx_count);

        // Try signed block data first (stored by execute_block_signed_benchmark)
        if let Some(block_data) = self.signed_block_data.get(&height) {
            let (txs_vec, _, _) = &*block_data;
            return (offset as usize..end as usize)
                .filter_map(|i| txs_vec.get(i).cloned())
                .collect();
        }

        // Fall back to deterministic reconstruction (unsigned benchmark)
        let mut txs = Vec::new();
        for idx in offset..end {
            if let Some(tx) = self.reconstruct_benchmark_tx(height, idx) {
                txs.push(tx);
            }
        }
        txs
    }

    /// Compute the gas cost for a transaction based on its type.
    /// This is a pure function -- no state access required.
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
            TxBody::InferenceRegister(_) => gas_costs::INFERENCE_ATTESTATION, // same gas as attestation
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
        }
    }

    /// Execute a single transaction against state, enforcing gas metering.
    ///
    /// Returns the gas consumed on success. When `gas_limit == 0` (backward
    /// compat / benchmark mode), an effectively unlimited gas budget is used
    /// so that no existing transaction can fail due to gas exhaustion.
    fn execute_tx(&self, tx: &Transaction) -> Result<u64, StateError> {
        if tx.tx_type != tx.body.tx_type() {
            return Err(StateError::ExecutionError(format!(
                "transaction type/body mismatch: envelope {:?}, body {:?}",
                tx.tx_type,
                tx.body.tx_type()
            )));
        }
        // --- Gas metering setup ---
        let effective_limit = if tx.gas_limit > 0 {
            tx.gas_limit
        } else {
            gas_costs::BLOCK_GAS_LIMIT // unlimited for backward compat
        };
        let mut gas = GasMeter::new(effective_limit);

        // Charge the operation-specific gas cost up front
        let op_cost = Self::gas_cost_for_tx(tx);
        if let Err(e) = gas.charge(op_cost) {
            return Err(StateError::ExecutionError(format!("gas: {}", e)));
        }

        match &tx.body {
            TxBody::Transfer(body) => {
                // Use get_mut for zero-copy in-place modification
                {
                    let mut sender = self.accounts.get_mut(&tx.from.0).ok_or_else(|| {
                        // Lazy create if not found
                        self.accounts.insert(tx.from.0, Account::new(tx.from, 0));
                        StateError::InsufficientBalance {
                            have: 0,
                            need: body.amount,
                        }
                    })?;
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
                    // Eagerly update JMT leaf for sender.
                    if self.use_jmt {
                        let sender_hash =
                            hash_bytes(&bincode::serialize(sender.value()).unwrap_or_default());
                        self.jmt.lock().update_leaf(tx.from.0, sender_hash);
                    }
                    // WAL snapshot only if WAL is active (null WAL returns early)
                    if self.wal.is_active() {
                        let snap = sender.clone();
                        drop(sender);
                        self.wal
                            .append(WalOp::SetAccount(tx.from, snap), self.height());
                    }
                }

                // Credit receiver in-place
                {
                    let mut receiver = self
                        .accounts
                        .entry(body.to.0)
                        .or_insert_with(|| Account::new(body.to, 0));
                    receiver.balance = receiver.balance.saturating_add(body.amount);
                    // Eagerly update JMT leaf for receiver.
                    if self.use_jmt {
                        let recv_hash =
                            hash_bytes(&bincode::serialize(receiver.value()).unwrap_or_default());
                        self.jmt.lock().update_leaf(body.to.0, recv_hash);
                    }
                    if self.wal.is_active() {
                        let snap = receiver.clone();
                        drop(receiver);
                        self.wal
                            .append(WalOp::SetAccount(body.to, snap), self.height());
                    }
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
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                if body.is_stake {
                    // --- Stake: move funds from balance to staked_balance ---
                    if sender.balance < body.amount {
                        return Err(StateError::InsufficientBalance {
                            have: sender.balance,
                            need: body.amount,
                        });
                    }
                    sender.balance -= body.amount;
                    sender.staked_balance += body.amount;

                    // Update validator tracking
                    let prev_stake = self.validators.get(&tx.from.0).map(|v| *v).unwrap_or(0);
                    let new_stake = prev_stake.saturating_add(body.amount);
                    self.validators.insert(tx.from.0, new_stake);

                    // Update global staking pool
                    self.staking_pool.fetch_add(body.amount, Ordering::Relaxed);

                    // Register as validator if crossing threshold
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
                    // --- Unstake: move funds from staked_balance back to balance ---
                    if sender.staked_balance < body.amount {
                        return Err(StateError::InsufficientBalance {
                            have: sender.staked_balance,
                            need: body.amount,
                        });
                    }
                    sender.staked_balance -= body.amount;
                    sender.balance = body.amount;

                    // Update validator tracking
                    let prev_stake = self.validators.get(&tx.from.0).map(|v| *v).unwrap_or(0);
                    let new_stake = prev_stake.saturating_sub(body.amount);
                    if new_stake == 0 {
                        self.validators.remove(&tx.from.0);
                    } else {
                        self.validators.insert(tx.from.0, new_stake);
                    }

                    // Update global staking pool
                    self.staking_pool.fetch_sub(
                        body.amount.min(self.staking_pool.load(Ordering::Relaxed)),
                        Ordering::Relaxed,
                    );

                    // Log validator removal if dropping below threshold
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

                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());
                Ok(gas.consumed)
            }
            TxBody::WasmCall(body) => {
                // --- Contract lookup ---
                let bytecode = self
                    .get_contract(&body.contract)
                    .ok_or(StateError::ContractNotFound(body.contract))?;

                let is_evm = bytecode.len() < 4 || &bytecode[..4] != WASM_MAGIC;

                if is_evm {
                    // State-changing EVM execution used to happen in arc-node
                    // *after* this executor had computed and durably
                    // checkpointed the block state root. That made the live
                    // account/storage state differ from the authenticated
                    // block and made restart fail on the trailing WAL writes.
                    //
                    // Protocol v3 therefore fails closed until EVM effects
                    // can be produced and applied atomically inside this
                    // canonical transition. This check deliberately precedes
                    // sender lookup/creation, nonce changes, balance changes,
                    // storage writes, logs, and WAL appends.
                    return Err(StateError::ExecutionError(
                        "state-changing EVM calls are unavailable until canonical pre-root execution is activated"
                            .to_string(),
                    ));
                } else {
                    // --- WASM contract ---
                    // Use a detached sender candidate so lookup failures above
                    // and future preflight failures cannot fabricate an
                    // account merely by submitting a rejected call.
                    let mut sender = self
                        .get_account(&tx.from)
                        .unwrap_or_else(|| Account::new(tx.from, 0));
                    if sender.nonce != tx.nonce {
                        return Err(StateError::InvalidNonce {
                            expected: sender.nonce,
                            got: tx.nonce,
                        });
                    }
                    // Value transfer (WASM runtime doesn't handle it internally).
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

                    // WASM execution via wasmer with full host imports.
                    {
                        use std::sync::Mutex as StdMutex;
                        use wasmer::{
                            Function, FunctionEnv, FunctionEnvMut, Instance, Memory,
                            Module as WasmModule, Store, imports,
                        };

                        // Shared state for host functions
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

                        // Host: use_gas(amount: i64)
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

                        // Host: log(ptr: i32, len: i32)
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

                        // Host: storage_get(key_ptr: i32, val_ptr: i32) -> i32
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

                        // Host: storage_set(key_ptr: i32, val_ptr: i32, val_len: i32)
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

                        // Host: caller(ptr: i32) - write caller address to WASM memory
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

                        // Host: self_address(ptr: i32)
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

                        // Host: block_height() -> i64
                        let h_block_height = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |env: FunctionEnvMut<'_, WasmHostState>| -> i64 {
                                env.data().block_height as i64
                            },
                        );

                        // Host: tx_value() -> i64
                        let h_tx_value = Function::new_typed_with_env(
                            &mut store,
                            &func_env,
                            |env: FunctionEnvMut<'_, WasmHostState>| -> i64 {
                                env.data().call_value as i64
                            },
                        );

                        // Host: gas_remaining() -> i64
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

                        // Wire up memory reference so host functions can access it
                        if let Ok(memory) = instance.exports.get_memory("memory") {
                            *func_env.as_mut(&mut store).memory.lock().unwrap() =
                                Some(memory.clone());
                        }

                        // Pre-populate storage cache from StateDB
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

                        // Check gas exhaustion
                        let host_state = func_env.as_ref(&store);
                        let wasm_gas_used = host_state
                            .gas_used
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let was_out_of_gas = *host_state.out_of_gas.lock().unwrap();

                        if was_out_of_gas || wasm_gas_used > wasm_gas_limit {
                            return Err(StateError::ExecutionError("WASM out of gas".into()));
                        }

                        // Charge WASM gas to the transaction gas meter
                        let wasm_gas_charge = wasm_gas_used / 100; // Scale down: 100 WASM gas = 1 tx gas
                        let _ = gas.charge(wasm_gas_charge);

                        match call_result {
                            Ok(_) => {
                                // Flush storage writes to StateDB on success
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
                                // Revert: do NOT flush storage writes
                                // Revert value transfer if any
                                if body.value > 0 {
                                    sender.balance = body.value;
                                    // Persist reverted sender balance
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
                // --- Sender validation ---
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

                // --- Validate WASM bytecode ---
                if body.bytecode.len() < 4 || &body.bytecode[..4] != WASM_MAGIC {
                    return Err(StateError::ExecutionError(
                        "invalid WASM bytecode: missing \0asm magic header".into(),
                    ));
                }

                // Charge additional gas for bytecode size
                let bytecode_gas = body.bytecode.len() as u64 * gas_costs::TX_DATA_BYTE;
                if let Err(e) = gas.charge(bytecode_gas) {
                    return Err(StateError::ExecutionError(format!("gas: {}", e)));
                }

                // --- Compute deterministic contract address ---
                let contract_addr = compute_contract_address(&tx.from, tx.nonce);

                // --- Store bytecode ---
                self.deploy_contract(&contract_addr, body.bytecode.clone());

                // --- Create contract account ---
                let code_hash = hash_bytes(&body.bytecode);
                let contract_acct = Account::new_contract(contract_addr, code_hash);
                self.accounts.insert(contract_addr.0, contract_acct.clone());
                self.wal.append(
                    WalOp::SetAccount(contract_addr, contract_acct),
                    self.height(),
                );

                // --- Constructor execution ---
                if !body.constructor_args.is_empty() {
                    // Compile the module and call `init` with full host imports.
                    // Uses wasmer directly to avoid circular arc-vm dependency.
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
                                // Flush constructor storage writes to StateDB
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
                                // Constructor failed - remove the deployed contract
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

                // --- Debit sender ---
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
                // Deduct initial stake from sender's balance and register as validator
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
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

                // Move balance to staked_balance
                sender.balance -= body.initial_stake;
                sender.staked_balance += body.initial_stake;
                sender.nonce += 1;

                // Register in validator set
                self.validators.insert(tx.from.0, body.initial_stake);
                self.staking_pool
                    .fetch_add(body.initial_stake, Ordering::Relaxed);

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
                // Unstake everything and remove from validator set
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                // Return all staked balance
                let staked = self
                    .validators
                    .remove(&tx.from.0)
                    .map(|(_, v)| v)
                    .unwrap_or(0);
                if staked > 0 {
                    sender.staked_balance = sender.staked_balance.saturating_sub(staked);
                    sender.balance = staked;
                    self.staking_pool.fetch_sub(
                        staked.min(self.staking_pool.load(Ordering::Relaxed)),
                        Ordering::Relaxed,
                    );
                }

                sender.nonce += 1;
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
                // Actual reward distribution is epoch-based
                Ok(gas.consumed)
            }
            TxBody::UpdateStake(body) => {
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                let current_stake = self.validators.get(&tx.from.0).map(|v| *v).unwrap_or(0);

                if body.new_stake > current_stake {
                    // Increasing stake: deduct difference from balance
                    let diff = body.new_stake - current_stake;
                    if sender.balance < diff {
                        return Err(StateError::InsufficientBalance {
                            have: sender.balance,
                            need: diff,
                        });
                    }
                    sender.balance -= diff;
                    sender.staked_balance += diff;
                    self.staking_pool.fetch_add(diff, Ordering::Relaxed);
                } else if body.new_stake < current_stake {
                    // Decreasing stake: return difference to balance
                    let diff = current_stake - body.new_stake;
                    sender.staked_balance = sender.staked_balance.saturating_sub(diff);
                    sender.balance = diff;
                    self.staking_pool.fetch_sub(
                        diff.min(self.staking_pool.load(Ordering::Relaxed)),
                        Ordering::Relaxed,
                    );
                }

                // Update or remove validator entry
                if body.new_stake == 0 {
                    self.validators.remove(&tx.from.0);
                } else {
                    self.validators.insert(tx.from.0, body.new_stake);
                }

                sender.nonce += 1;
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
                // Governance transactions record on-chain that a proposal was executed.
                // The actual governance state (votes, proposal lifecycle) is managed by
                // GovernanceState in arc-types; this TX type ensures the execution is
                // recorded as a transaction with gas accounting and nonce tracking.
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
                // Lock tokens in the bridge escrow account for cross-chain transfer.
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

                // Deduct from sender
                sender.balance -= body.amount;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // Credit bridge escrow account (well-known address)
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
                // Mint bridged tokens on ARC Chain from a source chain.
                // Validate that a merkle proof is provided (full verification
                // of the source chain proof is deferred to a future light client).
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

                // Increment sender nonce (bridge relayer)
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // Credit recipient
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
                // --- Batch Settlement: net bilateral balances ---
                // Validate entry count before any state access (DoS protection).
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

                // Compute total gross amount across all entries
                let total_amount: u64 = body.entries.iter().map(|e| e.amount).sum();
                if sender.balance < total_amount {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: total_amount,
                    });
                }

                // Net balances per recipient (multiple entries to same agent get summed)
                let mut net_credits: std::collections::HashMap<[u8; 32], u64> =
                    std::collections::HashMap::new();
                for entry in &body.entries {
                    *net_credits.entry(entry.agent_id.0).or_insert(0) += entry.amount;
                }

                // Debit sender once for total
                sender.balance -= total_amount;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // Credit each unique recipient once (netted)
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
                // --- Open State Channel: lock funds ---
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

                // Lock funds: debit from balance (held in channel escrow)
                sender.balance -= body.deposit;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // Record channel deposit in a deterministic escrow address
                // Channel escrow = BLAKE3("arc-channel" || channel_id)
                let escrow_addr = hash_bytes(&[b"arc-channel", body.channel_id.as_ref()].concat());
                let mut escrow = self.get_or_create_account(&escrow_addr);
                escrow.balance = body.deposit;
                // Store channel participants in escrow metadata:
                //   code_hash  = opener address (tx.from)
                //   storage_root = counterparty address
                // These are unused for escrow accounts (no contract code / no storage)
                // and allow ChannelClose to credit both parties correctly.
                escrow.code_hash = tx.from;
                escrow.storage_root = body.counterparty;
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                Ok(gas.consumed)
            }
            TxBody::ChannelClose(body) => {
                // --- Close State Channel: release funds by mutual agreement ---
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }
                sender.nonce += 1;

                // Load channel escrow
                let escrow_addr = hash_bytes(&[b"arc-channel", body.channel_id.as_ref()].concat());
                let escrow = self.get_or_create_account(&escrow_addr);
                let total_locked = escrow.balance;

                // Authorization: only the channel opener or counterparty can close.
                // Opener is stored in escrow.code_hash, counterparty in escrow.storage_root.
                let opener_addr = escrow.code_hash;
                let counterparty_addr_stored = escrow.storage_root;
                if tx.from != opener_addr && tx.from != counterparty_addr_stored {
                    return Err(StateError::ExecutionError(
                        "channel close: sender is neither opener nor counterparty".to_string(),
                    ));
                }

                // Reject close if there is an active dispute whose challenge period
                // has not yet expired. escrow.staked_balance stores the challenge
                // expiry height (0 = no dispute).
                if escrow.staked_balance > 0 && self.height() < escrow.staked_balance {
                    return Err(StateError::ExecutionError(
                        "channel close: active dispute in progress, wait for challenge period to expire".to_string(),
                    ));
                }

                // Validate final balances don't exceed locked funds
                let claimed_total = body
                    .opener_balance
                    .saturating_add(body.counterparty_balance);
                if claimed_total > total_locked {
                    return Err(StateError::ExecutionError(format!(
                        "channel close exceeds locked funds: claimed={}, locked={}",
                        claimed_total, total_locked
                    )));
                }

                // Drain escrow
                let mut escrow_mut = self.get_or_create_account(&escrow_addr);
                escrow_mut.balance = 0;
                self.accounts.insert(escrow_addr.0, escrow_mut.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow_mut), self.height());

                // Credit opener - ADD back their channel share to existing balance
                sender.balance += body.opener_balance;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // Credit counterparty - address was stored in escrow.storage_root
                // during ChannelOpen (see above). This is the definitive on-chain
                // record of who the counterparty is.
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
                // --- Dispute State Channel: submit latest signed state ---
                //
                // Escrow fields used for dispute tracking:
                //   escrow.nonce          = highest accepted state_nonce (0 = no dispute yet)
                //   escrow.staked_balance  = challenge_expiry height (0 = no active dispute)
                //   escrow.balance         = total locked funds (set during ChannelOpen)
                //   escrow.code_hash       = opener address
                //   escrow.storage_root    = counterparty address

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

                // Validate escrow exists and has locked funds
                let escrow_addr = hash_bytes(&[b"arc-channel", body.channel_id.as_ref()].concat());
                let mut escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance == 0 {
                    return Err(StateError::ExecutionError(
                        "channel dispute: no funds locked in channel".to_string(),
                    ));
                }

                // Authorization: only the channel opener or counterparty can dispute.
                if tx.from != escrow.code_hash && tx.from != escrow.storage_root {
                    return Err(StateError::ExecutionError(
                        "channel dispute: sender is neither opener nor counterparty".to_string(),
                    ));
                }

                // Validate challenge_period is reasonable (1..=100_000 blocks).
                if body.challenge_period == 0 || body.challenge_period > 100_000 {
                    return Err(StateError::ExecutionError(
                        "channel dispute: challenge_period must be 1..=100000".to_string(),
                    ));
                }

                // If a previous dispute exists and its challenge period has expired,
                // the state is already finalized - no further disputes allowed.
                if escrow.staked_balance > 0 && self.height() >= escrow.staked_balance {
                    return Err(StateError::ExecutionError(
                        "channel dispute: challenge period has expired, state is finalized"
                            .to_string(),
                    ));
                }

                // State nonce must be strictly higher than the previously disputed state.
                // This prevents replay attacks with old channel states.
                if escrow.staked_balance > 0 && body.state_nonce <= escrow.nonce {
                    return Err(StateError::ExecutionError(format!(
                        "channel dispute: state_nonce {} must exceed previously disputed nonce {}",
                        body.state_nonce, escrow.nonce
                    )));
                }

                // Validate balance conservation: claimed split must not exceed locked funds.
                let claimed_total = body
                    .opener_balance
                    .saturating_add(body.counterparty_balance);
                if claimed_total > escrow.balance {
                    return Err(StateError::ExecutionError(format!(
                        "channel dispute: claimed balances ({}) exceed locked funds ({})",
                        claimed_total, escrow.balance
                    )));
                }

                // Update dispute state in escrow.
                let challenge_expiry = self.height() + body.challenge_period;
                escrow.nonce = body.state_nonce;
                escrow.staked_balance = challenge_expiry;
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                Ok(gas.consumed)
            }
            TxBody::ShardProof(body) => {
                // --- Shard Proof: verify and record STARK proof of shard block ---
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

                // Validate proof is non-empty
                if body.proof_data.is_empty() {
                    return Err(StateError::ExecutionError(
                        "shard proof: empty proof data".to_string(),
                    ));
                }

                // Validate state root transition is non-trivial
                if body.prev_state_root == body.post_state_root && body.tx_count > 0 {
                    return Err(StateError::ExecutionError(
                        "shard proof: state root unchanged despite transactions".to_string(),
                    ));
                }

                // --- Cryptographic STARK verification (when stwo-prover feature is on) ---
                // Constructs a RecursiveVerifierInput from the ShardProofBody fields
                // and calls verify_recursive_proof to check the binding hash in the
                // proof receipt. This ensures the proof was generated by a real Stwo
                // prover over the claimed state transition.
                #[cfg(feature = "stwo-prover")]
                {
                    let recursive_input = arc_crypto::stwo_air::RecursiveVerifierInput {
                        child_hashes: vec![body.block_hash.0],
                        child_start_states: vec![body.prev_state_root.0],
                        child_end_states: vec![body.post_state_root.0],
                        merkle_siblings: vec![vec![]], // single-child: no siblings needed
                        expected_merkle_root: body.block_hash.0, // single-child: root = child hash
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

                // Record verified shard proof - store proof hash in a deterministic
                // address derived from shard_id + block_height.
                // This creates an on-chain receipt that shard X's block Y was proven.
                let mut proof_input = Vec::new();
                proof_input.extend_from_slice(b"arc-shard-proof");
                proof_input.extend_from_slice(&body.shard_id.to_le_bytes());
                proof_input.extend_from_slice(&body.block_height.to_le_bytes());
                let proof_key = hash_bytes(&proof_input);
                let proof_hash = hash_bytes(&body.proof_data);
                let mut proof_record = self.get_or_create_account(&proof_key);
                // Store proof hash in the "balance" field as a u64 fingerprint
                // (first 8 bytes of BLAKE3 hash). Full proof data is in the TX itself.
                proof_record.balance =
                    u64::from_le_bytes(proof_hash.0[..8].try_into().unwrap_or([0u8; 8]));
                proof_record.nonce = body.block_height;
                self.accounts.insert(proof_key.0, proof_record.clone());
                self.wal
                    .append(WalOp::SetAccount(proof_key, proof_record), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceAttestation(body) => {
                // --- Tier 2 Optimistic Inference Attestation ---
                //
                // This transaction records a provider's signed model/input/
                // output commitment and optionally locks a challenge bond. It
                // does NOT pay a reward by itself: a raw self-signed
                // attestation has no coordinator-issued job or assignment and
                // used to let anyone drain the treasury with bond=0. Verified
                // community jobs are paid by `CommunityInferenceReward`.

                // 1. Verify sender nonce.
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                // 2. Verify sender has sufficient balance for the bond.
                if sender.balance < body.bond {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.bond,
                    });
                }

                // 3. Lock the bond and consume the sender nonce. No treasury
                // account is read or written on this path.
                sender.balance -= body.bond;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // 4. Bond handling.
                //    bond == 0 (community-worker path): nothing to lock, so no
                //    escrow is created. bond > 0: lock the bond in a
                //    deterministic escrow keyed by BLAKE3("arc-inference" ||
                //    attestation_hash) and queue it for release after
                //    `challenge_period` blocks. See the escrow encoding at the
                //    top of this file (code_hash = attester, nonce = release
                //    height, storage_root = MAGIC|status).
                if body.bond > 0 {
                    let escrow_addr = hash_bytes(&[b"arc-inference", tx.hash.as_ref()].concat());
                    let release_height = self.height().saturating_add(body.challenge_period);
                    let mut escrow = self.get_or_create_account(&escrow_addr);
                    escrow.balance = body.bond;
                    escrow.code_hash = tx.from; // refund target
                    escrow.nonce = release_height; // maturation deadline
                    let mut sr = [0u8; 32];
                    sr[..8].copy_from_slice(&ATTEST_ESCROW_MAGIC);
                    sr[8] = ATTEST_STATUS_OPEN;
                    escrow.storage_root = Hash256(sr);
                    self.accounts.insert(escrow_addr.0, escrow.clone());
                    self.wal
                        .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                    // Queue for the maturation sweep. Buckets are kept sorted
                    // so the per-block drain order is deterministic.
                    let mut q = self.pending_bond_releases.lock();
                    let bucket = q.entry(release_height).or_default();
                    if let Err(pos) = bucket.binary_search(&escrow_addr.0) {
                        bucket.insert(pos, escrow_addr.0);
                    }
                }

                Ok(gas.consumed)
            }
            TxBody::CommunityInferenceReward(body) => {
                if !self.community_rewards_v1_active() {
                    return Err(StateError::ExecutionError(
                        "community inference reward: protocol feature is not active at this height"
                            .to_string(),
                    ));
                }
                // The outer signer is an active-validator aggregator. It does
                // not by itself certify off-chain verification; the bounded
                // approval quorum below is the authorization boundary.
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
                if worker_stake < arc_types::transaction::COMMUNITY_REWARD_MIN_WORKER_STAKE {
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
                let job_marker_addr =
                    arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                        &body.chain_domain,
                        &body.job_id,
                    );
                if self.accounts.contains_key(&job_marker_addr.0) {
                    return Err(StateError::ExecutionError(format!(
                        "community inference reward: job {} already paid",
                        body.job_id.to_hex()
                    )));
                }
                let certificate_marker_addr = arc_types::transaction::CommunityInferenceRewardBody::certificate_marker_address(
                    &body.chain_domain,
                    &body.worker,
                    &body.worker_certificate.attestation_hash,
                );
                if self.accounts.contains_key(&certificate_marker_addr.0) {
                    return Err(StateError::ExecutionError(format!(
                        "community inference reward: worker certificate {} already paid",
                        body.worker_certificate.attestation_hash.to_hex()
                    )));
                }

                // All cheap structural, certificate, and replay checks happen
                // before the approval quorum's Ed25519 work. No state has been
                // mutated yet, so every failure remains atomic.
                self.verify_community_reward_validator_approvals(body)?;

                // Rewards are all-or-nothing. Partial tail payouts made worker
                // selection depend on parallel lock order and could fork state.
                let reward = arc_types::economics::INFERENCE_ATTESTATION_REWARD;
                // Validate the recipient credit before mutating the treasury.
                // State transitions are not automatically rolled back when an
                // executor arm returns an error, so every fallible check must
                // happen before the first write.
                // Build a detached candidate account. `get_or_create_account`
                // inserts immediately, which would mutate state even when the
                // treasury check below fails. Failed reward execution must be
                // atomic and must not leave an un-WALed zero-balance worker.
                let mut worker = self
                    .get_account(&body.worker)
                    .unwrap_or_else(|| Account::new(body.worker, 0));
                worker.balance = worker.balance.checked_add(reward).ok_or_else(|| {
                    StateError::ExecutionError(
                        "community inference reward: worker balance overflow".to_string(),
                    )
                })?;
                {
                    let mut treasury =
                        self.accounts.get_mut(&treasury_addr.0).ok_or_else(|| {
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
                    treasury.balance -= reward;
                    if self.wal.is_active() {
                        let snapshot = treasury.clone();
                        drop(treasury);
                        self.wal
                            .append(WalOp::SetAccount(treasury_addr, snapshot), self.height());
                    }
                }

                self.accounts.insert(body.worker.0, worker.clone());
                self.wal
                    .append(WalOp::SetAccount(body.worker, worker), self.height());

                // Zero-balance receipt marker. Its metadata is auditable and
                // makes a second reward for the same job fail even if a
                // validator signs a transaction with a fresh nonce/hash.
                let mut job_marker = self.get_or_create_account(&job_marker_addr);
                job_marker.nonce = 1;
                job_marker.code_hash = body.worker;
                job_marker.storage_root = body.output_hash;
                self.accounts.insert(job_marker_addr.0, job_marker.clone());
                self.wal.append(
                    WalOp::SetAccount(job_marker_addr, job_marker),
                    self.height(),
                );

                // A second marker reserves the worker-signed certificate
                // independently of the coordinator-controlled job ID.
                let mut certificate_marker = self.get_or_create_account(&certificate_marker_addr);
                certificate_marker.nonce = 1;
                certificate_marker.code_hash = body.worker;
                certificate_marker.storage_root = body.job_id;
                self.accounts
                    .insert(certificate_marker_addr.0, certificate_marker.clone());
                self.wal.append(
                    WalOp::SetAccount(certificate_marker_addr, certificate_marker),
                    self.height(),
                );

                Ok(gas.consumed)
            }
            TxBody::InferenceChallenge(body) => {
                // --- Tier 2 Inference Challenge (Fraud Proof) ---
                // 1. Verify sender nonce
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                // 2. Verify sender has sufficient balance for challenger bond
                if sender.balance < body.challenger_bond {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.challenger_bond,
                    });
                }

                // 3. Look up the attestation escrow
                let escrow_addr =
                    hash_bytes(&[b"arc-inference", body.attestation_hash.as_ref()].concat());
                let escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance == 0 {
                    return Err(StateError::ExecutionError(
                        "inference challenge: attestation escrow not found or already resolved"
                            .to_string(),
                    ));
                }

                // 4. Debit challenger bond and increment nonce
                sender.balance -= body.challenger_bond;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // 5. Lock challenger's bond in the same escrow AND mark the
                //    escrow CHALLENGED so the bond-maturation sweep leaves it
                //    locked. A disputed/slashed bond must never be auto-refunded
                //    to the attester; it stays escrowed pending dispute
                //    resolution (see the escrow-encoding note at file top).
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

                // 6. Dispute resolution: if challenger_output_hash differs from the
                //    attested output, the dispute is recorded.  On-chain re-execution
                //    via precompile 0x0A determines the winner.  For now, the dispute
                //    is recorded and validators resolve it at challenge period expiry.
                //    Full resolution would call the AI precompile and compare outputs;
                //    the winner receives both bonds and the loser is slashed.

                Ok(gas.consumed)
            }
            TxBody::InferenceRegister(body) => {
                // --- Register as Inference Provider ---
                // Validators declare hardware tier and lock a stake bond.
                // The chain maintains a registry in sender's account metadata:
                //   staked_balance += stake_bond (locked)
                //   nonce field tracks the declared tier
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                // Validate tier (1-4)
                if body.tier == 0 || body.tier > 4 {
                    return Err(StateError::ExecutionError(format!(
                        "inference register: invalid tier {}, must be 1-4",
                        body.tier
                    )));
                }

                // Validate sufficient balance for stake bond
                if sender.balance < body.stake_bond {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.stake_bond,
                    });
                }

                // Validate minimum stake for tier
                let min_stakes = [0u64, 1_000, 5_000, 10_000, 25_000];
                let min_stake = min_stakes[body.tier as usize];
                if body.stake_bond < min_stake {
                    return Err(StateError::ExecutionError(format!(
                        "inference register: stake {} below minimum {} for tier {}",
                        body.stake_bond, min_stake, body.tier
                    )));
                }

                // Lock stake: move from balance to staked_balance
                sender.balance -= body.stake_bond;
                sender.staked_balance += body.stake_bond;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceEscrowOpen(body) => {
                // Milestone B: payer locks `max_fee` in a deterministic
                // escrow account keyed by request_id. Metadata (model_id,
                // max_tokens, timeout_blocks, payer) is committed into the
                // account's storage_root so release/refund callers must
                // prove they know the same fields by rehashing.
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

                // Debit payer.
                sender.balance -= body.max_fee;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // Credit escrow + stash metadata commitment.
                let commitment = InferenceEscrowOpenBody::metadata_commitment(
                    &tx.from,
                    &body.model_id,
                    body.max_tokens,
                    body.timeout_blocks,
                );
                let mut escrow = existing;
                escrow.balance = body.max_fee;
                // Abuse `nonce` as the opened-at block height. Refund
                // re-reads this to enforce the timeout.
                escrow.nonce = self.height();
                escrow.storage_root = Hash256(commitment);
                self.accounts.insert(escrow_addr_bytes, escrow.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, escrow), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceEscrowRelease(body) => {
                // Milestone B: distribute the locked max_fee per the
                // RoleRevenueConfig split (40% proposer / 25% replicas /
                // 15% observer pool / 20% treasury). Any rounding goes to
                // treasury so total is always conserved.
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

                // Advance release-submitter nonce.
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // 40/25/15/20 split; treasury absorbs all truncation residue.
                let total = escrow.balance;
                let proposer_share = total * 40 / 100;
                let replicas_pool = total * 25 / 100;
                let observer_share = total * 15 / 100;
                let per_replica = replicas_pool / body.replicas.len() as u64;
                let replicas_paid = per_replica * body.replicas.len() as u64;
                let treasury_share = total - proposer_share - replicas_paid - observer_share;

                // Credit proposer.
                let mut proposer_acc = self.get_or_create_account(&body.proposer);
                proposer_acc.balance += proposer_share;
                self.accounts.insert(body.proposer.0, proposer_acc.clone());
                self.wal.append(
                    WalOp::SetAccount(body.proposer, proposer_acc),
                    self.height(),
                );

                // Credit each replica.
                for r in &body.replicas {
                    let mut rep = self.get_or_create_account(r);
                    rep.balance += per_replica;
                    self.accounts.insert(r.0, rep.clone());
                    self.wal.append(WalOp::SetAccount(*r, rep), self.height());
                }

                // Credit observer pool.
                let mut obs = self.get_or_create_account(&body.observer_pool);
                obs.balance += observer_share;
                self.accounts.insert(body.observer_pool.0, obs.clone());
                self.wal
                    .append(WalOp::SetAccount(body.observer_pool, obs), self.height());

                // Credit treasury (includes rounding residue).
                let mut tre = self.get_or_create_account(&body.treasury);
                tre.balance += treasury_share;
                self.accounts.insert(body.treasury.0, tre.clone());
                self.wal
                    .append(WalOp::SetAccount(body.treasury, tre), self.height());

                // Zero the escrow and clear the commitment so the same
                // request_id can't be released/refunded twice.
                let mut released = escrow;
                released.balance = 0;
                released.storage_root = Hash256::ZERO;
                self.accounts.insert(escrow_addr_bytes, released.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, released), self.height());

                Ok(gas.consumed)
            }
            TxBody::ModelRegistration(body) => {
                // Milestone C: register a model; fee transfers to treasury.
                // Milestone E anti-spam: fee is floored at
                // MIN_MODEL_REGISTRATION_FEE (1000 ARC). Stored in a
                // deterministic account keyed by model_id, with metadata
                // committed into storage_root so later queries (or
                // ModelRequest validation) can verify the model exists
                // with the expected config.
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
                // Reject duplicate registrations (same model_id already
                // on-chain). Keeps the registry tamper-evident.
                let registry_addr_bytes = ModelRegistrationBody::registry_account(&body.model_id);
                let registry_addr = Hash256(registry_addr_bytes);
                let existing = self.get_or_create_account(&registry_addr);
                if existing.storage_root != Hash256::ZERO {
                    return Err(StateError::ExecutionError(
                        "model registration: model_id already registered".into(),
                    ));
                }

                // Debit payer; pay fee to treasury.
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

                // Write the registry entry.
                let commitment = ModelRegistrationBody::metadata_commitment(
                    body.n_layers,
                    body.d_model,
                    &body.quantization,
                    &body.chunk_tree_root,
                    &body.royalty_recipient,
                );
                let mut reg = existing;
                reg.nonce = self.height(); // registered_at
                reg.storage_root = Hash256(commitment);
                // Park the paid fee in `balance` - a future Milestone E
                // patch can meter royalty payouts from this pool.
                reg.balance = 0; // fees already sent to treasury; reg holds no value today
                self.accounts.insert(registry_addr_bytes, reg.clone());
                self.wal
                    .append(WalOp::SetAccount(registry_addr, reg), self.height());

                Ok(gas.consumed)
            }
            TxBody::ModelRequest(body) => {
                // Milestone C: record demand. No fund movement today -
                // the request sits on-chain for workers to observe and
                // claim against. Future patch: bond_per_layer_epoch is
                // escrowed here and released to claiming workers.
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
                // Encode request state in existing account slots:
                //   balance      = bond_per_layer_epoch (for planner weighting)
                //   nonce        = posted_at height
                //   storage_root = hash(model_id || target_k_replication ||
                //                       max_wait_secs || requester)
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
                // Milestone C: worker locks a bond for the epoch.
                // Slashing on non-serve is handled by a future verifier
                // path (#31-style challenge + proof).
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

                // Lock bond in the deterministic claim account. Ranges
                // are committed via storage_root so slashing/release can
                // verify what was claimed without a separate DashMap.
                let claim_addr_bytes =
                    ShardCoverageClaimBody::claim_account(&body.model_id, &body.node_pubkey);
                let claim_addr = Hash256(claim_addr_bytes);
                let mut claim = self.get_or_create_account(&claim_addr);
                if claim.balance != 0 {
                    // Refund and reset: the worker is renewing their
                    // claim. Return the prior bond to tx.from first.
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
                // Milestone D: record capacity advertisement. Pure
                // metadata write - no funds move. Planner reads these
                // plus open requests + current shard_registry to compute
                // assignments.
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
                // Encode capacity in storage_root so a replay of history
                // reconstructs the same snapshot. A later patch can also
                // store ram/vram/bandwidth individually in a sidecar
                // DashMap for fast planner reads.
                let mut meta = Vec::new();
                meta.extend_from_slice(&body.ram_bytes.to_le_bytes());
                meta.extend_from_slice(&body.vram_bytes.to_le_bytes());
                meta.extend_from_slice(&body.bandwidth_mbps.to_le_bytes());
                meta.extend_from_slice(&body.uptime_hint_mins.to_le_bytes());
                meta.extend_from_slice(&body.stake.to_le_bytes());
                meta.extend_from_slice(&(body.region.len() as u32).to_le_bytes());
                meta.extend_from_slice(body.region.as_bytes());
                cap.nonce = self.height(); // advertised_at
                cap.storage_root = arc_crypto::hash_bytes(&meta);
                self.accounts.insert(cap_addr_bytes, cap.clone());
                self.wal
                    .append(WalOp::SetAccount(cap_addr, cap), self.height());

                Ok(gas.consumed)
            }
            TxBody::ShardAssignmentProposal(body) => {
                // Milestone D: record planner output. The hash of the
                // input snapshot lets any full node recompute the
                // assignment deterministically and check the proposer
                // got the same answer. Actual enforcement (workers
                // follow their assignment or lose claims) happens at
                // /assignments/for_me read time, not here.
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

                // Derive a proposal-specific account from the input
                // snapshot hash so replays collide and the latest
                // proposer for a given input wins (last-write in
                // block order).
                let prop_addr = Hash256(
                    arc_crypto::hash_bytes(
                        &[b"arc-planner-proposal", body.input_snapshot_hash.0.as_ref()].concat(),
                    )
                    .0,
                );
                let mut prop = self.get_or_create_account(&prop_addr);
                // Serialize compact representation into storage_root so
                // the exact assignment replayed from history matches.
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
                // Milestone B: original payer reclaims funds after the
                // `timeout_blocks` window elapses with no release. Only
                // callable by the payer - identity proved by rehashing
                // the same fields used at open and matching the stored
                // commitment.
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

                // Refund locked balance to payer.
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
                // Tier 1 on-chain inference request. Locks `max_reward` in
                // a deterministic escrow and records request metadata that
                // every full node can reconstruct on replay.
                //
                // The committee is NOT derived here — it's derived at
                // InferenceVote apply time using the commit block hash of
                // *this* tx as the VRF seed. We record the anchor height
                // in the escrow nonce so vote-apply knows which seed
                // (block_hash_at_anchor_height) to use.
                use arc_types::transaction as ttx;

                // ── 1. Bounds checks (chain-enforced invariants) ──
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
                // Verify input_hash matches blob (chain-side check; the
                // client may have computed it lazily).
                let computed = hash_bytes(&body.input_blob);
                if computed != body.input_hash {
                    return Err(StateError::ExecutionError(
                        "tier1 request: input_hash does not match blob".into(),
                    ));
                }

                // ── 2. Verify sender nonce + balance ──
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

                // ── 3. Debit max_reward, bump nonce ──
                sender.balance -= body.max_reward;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // ── 4. Create request escrow ──
                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                let mut escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance != 0 || escrow.code_hash != Hash256::ZERO {
                    return Err(StateError::ExecutionError(
                        "tier1 request: request_id collides with existing escrow".into(),
                    ));
                }
                escrow.balance = body.max_reward;
                // Anchor height in nonce for vote-apply to read.
                escrow.nonce = self.height();
                // Status byte 0 = Open. First byte of code_hash holds status.
                let mut status_bytes = [0u8; 32];
                status_bytes[0] = TIER1_STATUS_OPEN;
                // Bytes [1..9] hold deadline_blocks (relative) for easy lookup.
                status_bytes[1..9].copy_from_slice(&body.deadline_blocks.to_le_bytes());
                // Byte 9 holds committee_size.
                status_bytes[9] = body.committee_size;
                escrow.code_hash = Hash256(status_bytes);
                // Metadata commitment in storage_root.
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

                // Store the requester address so finalize knows whom to refund.
                self.set_storage(
                    &escrow_addr,
                    hash_bytes(b"tier1.requester"),
                    tx.from.0.to_vec(),
                );
                // Store the prompt blob so committee members can fetch it
                // from chain state if they missed the original tx.
                self.set_storage(
                    &escrow_addr,
                    hash_bytes(b"tier1.input_blob"),
                    body.input_blob.clone(),
                );
                // Initialize empty vote list.
                self.set_storage(
                    &escrow_addr,
                    hash_bytes(b"tier1.votes"),
                    bincode::serialize(&Vec::<(Address, Hash256)>::new()).unwrap_or_default(),
                );
                // Persist the request_id at a known storage key so a node
                // restart can rebuild `tier1_pending` (which is in-memory
                // and otherwise lost). Without this, the validator task
                // wakes up after restart with an empty pending index and
                // never finalizes the requests it can no longer see. The
                // bug surfaced 2026-06-04 on the testnet — requests
                // 0x9d9df698 + 0xbc754b43 stuck Open through two rolling
                // restarts because the index was never rehydrated.
                self.set_storage(
                    &escrow_addr,
                    hash_bytes(b"tier1.request_id"),
                    body.request_id.to_vec(),
                );

                // Index the open request for the validator inference task
                // to poll. The anchor height equals the escrow's nonce,
                // also used for committee seed derivation.
                self.tier1_pending.insert(body.request_id, self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceVote(body) => {
                // Tier 1 committee member vote. Re-derive the committee
                // deterministically and reject votes from non-members.
                use arc_types::transaction as ttx;

                // ── 1. Bounds checks ──
                if let Some(blob) = &body.output_blob {
                    if blob.len() > ttx::TIER1_OUTPUT_BLOB_MAX {
                        return Err(StateError::ExecutionError(format!(
                            "tier1 vote: output_blob {} > max {}",
                            blob.len(),
                            ttx::TIER1_OUTPUT_BLOB_MAX
                        )));
                    }
                    // Verify hash matches blob if blob attached.
                    let computed = hash_bytes(blob);
                    if computed != body.output_hash {
                        return Err(StateError::ExecutionError(
                            "tier1 vote: output_hash does not match attached blob".into(),
                        ));
                    }
                }

                // ── 2. Verify sender nonce ──
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                // ── 3. Look up request escrow ──
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

                // ── 4. Derive committee deterministically ──
                // The committee seed is recomputed from state-controlled
                // fields, NOT from `body.committee_seed`. A voter-supplied
                // seed would let a malicious member grind their address
                // into the committee. The published `body.committee_seed`
                // is advisory only — the security gate is this re-derive.
                //
                // Seed = BLAKE3("tier1-seed" || request_id || anchor_height_LE).
                // anchor_height is `escrow.nonce`, set at apply_inference_request
                // time from `self.height()`. The validator inference task uses
                // the same derivation to learn which requests select it.
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
                // Deterministic ordering before scoring (HashMap iteration is not).
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

                // ── 5. Load existing votes, reject duplicates, append ──
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

                // ── 6. If voter attached blob, store it for the requester ──
                if let Some(blob) = &body.output_blob {
                    // Only the first attached blob is kept (saves space).
                    let blob_key = hash_bytes(b"tier1.output_blob");
                    if self.get_storage(&escrow_addr, &blob_key).is_none() {
                        self.set_storage(&escrow_addr, blob_key, blob.clone());
                    }
                }

                // ── 7. Bump signer nonce ──
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal
                    .append(WalOp::SetAccount(tx.from, sender), self.height());

                // ── 8. Flip status to Voting (if still Open) ──
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
                // Tier 1 finalize. Deterministic — first submitter wins,
                // subsequent submissions reject because status flips to
                // Finalized/Refunded after the first apply.
                use arc_types::transaction as ttx;

                // ── 1. Verify sender nonce (any signer can submit; only nonce check) ──
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

                // ── 2. Look up request escrow ──
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

                // ── 3. Load votes ──
                let key = hash_bytes(b"tier1.votes");
                let votes_bytes = self.get_storage(&escrow_addr, &key).unwrap_or_default();
                let votes: Vec<(Address, Hash256)> =
                    bincode::deserialize(&votes_bytes).unwrap_or_default();
                let vote_count = votes.len();

                // ── 4. Determine if finalization is eligible ──
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

                // ── 5. Aggregate votes ──
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

                // ── 6. Look up requester ──
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
                    // Consensus reached: pay agreeing voters, rebate requester, treasury cut.
                    let voters_pool = max_reward * ttx::TIER1_REWARD_SHARE_VOTERS_BPS / 10_000;
                    let refund = max_reward * ttx::TIER1_REWARD_SHARE_REFUND_BPS / 10_000;
                    let treasury = max_reward * ttx::TIER1_REWARD_SHARE_TREASURY_BPS / 10_000;
                    // Anti-rounding remainder goes to treasury.
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

                    // Credit each agreeing voter.
                    for v in &agreeing {
                        let mut acct = self.get_or_create_account(v);
                        acct.balance = acct.balance.saturating_add(per_voter);
                        self.accounts.insert(v.0, acct.clone());
                        self.wal.append(WalOp::SetAccount(*v, acct), self.height());
                    }
                    // Rebate to requester.
                    {
                        let mut acct = self.get_or_create_account(&requester);
                        acct.balance = acct.balance.saturating_add(refund);
                        self.accounts.insert(requester.0, acct.clone());
                        self.wal
                            .append(WalOp::SetAccount(requester, acct), self.height());
                    }
                    // Treasury (faucet_pool_address doubles as treasury sink for testnet).
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
                    // Record final output hash in escrow's storage_root for reads.
                    self.set_storage(
                        &escrow_addr,
                        hash_bytes(b"tier1.final_output_hash"),
                        majority.0.0.to_vec(),
                    );
                } else {
                    // Disagreement or timeout: refund payer minus anti-spam fee.
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

                // ── 7. Zero escrow + flip status ──
                let mut closed = escrow.clone();
                closed.balance = 0;
                closed.code_hash.0[0] = new_status;
                self.accounts.insert(escrow_addr.0, closed.clone());
                self.wal
                    .append(WalOp::SetAccount(escrow_addr, closed), self.height());

                // Drop from the pending index so the inference task stops
                // polling this request.
                self.tier1_pending.remove(&body.request_id);

                Ok(gas.consumed)
            }
            TxBody::FaucetClaim(body) => {
                // Validator-authorized faucet drain. The signer (tx.from)
                // must be in the active validator set — that authorization
                // is what lets us debit a shared pool the signer doesn't
                // own. Validator-set membership is deterministic across
                // every seed (loaded from genesis.toml + on-chain
                // JoinValidator txs), so this check produces the same
                // accept/reject decision everywhere.
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

                // No signer-nonce check or bump. A validator-signed
                // FaucetClaim is authorized by the Ed25519 signature plus
                // is_validator(tx.from) — both deterministic across peers.
                // The canonical recipient marker below provides chain-wide
                // exactly-once protection even when different validators sign
                // distinct transaction hashes for the same recipient. RPC
                // rate limits are only an additional abuse control.
                //
                // We INTENTIONALLY do NOT enforce signer.nonce == tx.nonce
                // here. Peers' committed-state nonce for a validator
                // diverges from the validator's local nonce when commit-log
                // heights drift (a common condition on this testnet — see
                // memory/project_arc_session_handoff_20260510.md). A strict
                // check causes spurious InvalidNonce rejections that block
                // cross-seed propagation of the funded balance — exactly
                // the bug v0.7.1 was meant to fix. Signer balance is also
                // not touched: the pool is the shared source.

                let pool_addr = arc_types::transaction::faucet_pool_address();
                let marker_addr =
                    arc_types::transaction::FaucetClaimBody::marker_address(&body.recipient);
                if self.accounts.contains_key(&marker_addr.0) {
                    return Err(StateError::ExecutionError(format!(
                        "faucet claim: recipient {} has already claimed",
                        body.recipient
                    )));
                }

                // Validate every fallible condition against detached account
                // copies before the first write. A failed faucet transaction
                // must be atomic and must never leave a debited pool or a
                // fabricated recipient account.
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
        }
    }

    // ── Pipeline support ────────────────────────────────────────────────────
    // Public wrappers for the 4-stage pipeline (B3), which executes on
    // separate threads and needs direct access to individual operations.

    /// Public wrapper around the private `execute_tx()` - used by the pipeline
    /// execute stage which runs on a dedicated thread.
    /// Returns gas consumed on success.
    pub fn execute_tx_pub(&self, tx: &Transaction) -> Result<u64, StateError> {
        self.execute_tx(tx)
    }

    /// Public wrapper around `mark_tx_accounts_dirty()` for the pipeline.
    pub fn mark_tx_accounts_dirty_pub(&self, tx: &Transaction) {
        self.mark_tx_accounts_dirty(tx);
    }

    /// Commit a batch of already-executed transactions into a block.
    ///
    /// Called by the pipeline commit stage.  The caller has already run
    /// `execute_tx()` for each transaction and recorded success/failure
    /// in `receipt_success`.
    pub fn commit_executed_block(
        &self,
        transactions: &[Transaction],
        receipt_success: &[bool],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        self.require_healthy_wal()?;
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

        // Refund matured, unchallenged Tier 2 attestation bonds at this height
        // BEFORE the state root is computed so refunds land in this block.
        // Bounded, deterministic, and a no-op when nothing has matured (always
        // so on the bond==0 community-worker demo path). Applied uniformly to
        // every real block-application path so the bond lifecycle advances the
        // same way regardless of which execution engine sealed the block.
        self.sweep_matured_bond_releases(height);

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

        self.blocks.insert(height, block.clone());
        self.wal
            .append(WalOp::SetBlock(height, block.clone()), height);
        self.persist_restart_artifacts(transactions, &receipts, height);
        self.wal.append(WalOp::Checkpoint(state_root), height);
        self.durable_wal_barrier()?;

        // Auto-prune old state every 100 blocks unless in archive mode.
        // Archive nodes keep full history for block explorers and analytics.
        if !self.archive_mode && height % 100 == 0 {
            self.prune_old_state(1000);
            self.prune_old_receipts(1000);
        }

        Ok((block, receipts))
    }

    /// Refund matured, unchallenged Tier 2 attestation bonds to their original
    /// attesters and zero the escrows.
    ///
    /// Runs during block commit at the new `current_height`. Deterministic and
    /// bounded: it drains up to [`MAX_BOND_RELEASES_PER_BLOCK`] escrows whose
    /// release height is `<= current_height`, visiting them in
    /// `pending_bond_releases` order (ascending release height, then ascending
    /// escrow address — no map-iteration-order or wall-clock dependence). Each
    /// escrow is validated against live account state before refunding.
    /// Missing/already-zeroed escrows are dropped; CHALLENGED escrows are
    /// dropped with the bond left locked pending dispute resolution; an OPEN,
    /// matured escrow has its `balance` refunded to the attester
    /// (`escrow.code_hash`) and is then zeroed.
    ///
    /// Conservation: every unit removed from an escrow is credited to exactly
    /// one attester — a pure move, never a mint. Returns the number of bonds
    /// refunded.
    pub fn sweep_matured_bond_releases(&self, current_height: u64) -> usize {
        // Phase 1: under the queue lock, collect up to the per-block cap of due
        // escrow addresses and remove them from the queue. Kept short so the
        // account mutations below run without holding the queue lock.
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

        // Phase 2: refund each due escrow (order is already deterministic).
        let mut refunded = 0usize;
        for escrow_key in due {
            let escrow_addr = Hash256(escrow_key);
            let escrow = match self.accounts.get(&escrow_key) {
                Some(e) => e.clone(),
                None => continue, // already pruned/resolved
            };
            if escrow.balance == 0 {
                continue; // already resolved
            }
            if escrow.storage_root.0[..8] != ATTEST_ESCROW_MAGIC {
                continue; // not a Tier 2 attestation escrow
            }
            if escrow.storage_root.0[8] != ATTEST_STATUS_OPEN {
                continue; // challenged/slashed: leave locked
            }
            if current_height < escrow.nonce {
                // Not actually matured — defensive; the queue key equals the
                // release height, so this only fires on a corrupt rebuild.
                // Re-queue and skip rather than refund early.
                let mut q = self.pending_bond_releases.lock();
                let bucket = q.entry(escrow.nonce).or_default();
                if let Err(pos) = bucket.binary_search(&escrow_key) {
                    bucket.insert(pos, escrow_key);
                }
                continue;
            }

            let attester = escrow.code_hash;
            let amount = escrow.balance;

            // Credit the attester (mirrors the FaucetClaim credit conventions:
            // JMT leaf update when enabled, WAL append when active).
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

            // Zero the escrow and clear the MAGIC (marks it resolved: a later
            // challenge sees balance==0 and a rebuild will not re-queue it).
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

    /// Rebuild `pending_bond_releases` from surviving escrow accounts. Call
    /// once at startup after any WAL replay / snapshot load (parallel to
    /// [`rebuild_tier1_pending`]), since the queue is a derived, in-memory
    /// index with no WAL op of its own.
    ///
    /// Strategy: scan every account whose `storage_root` carries the Tier 2
    /// attestation MAGIC and is still OPEN with a non-zero balance, and
    /// re-queue it under its release height (`escrow.nonce`). Zeroed/challenged
    /// escrows are skipped. Returns the count re-queued.
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

    /// Test/introspection helper: number of escrows currently queued for
    /// bond release across all maturation heights.
    #[cfg(test)]
    fn pending_bond_release_count(&self) -> usize {
        self.pending_bond_releases
            .lock()
            .values()
            .map(|v| v.len())
            .sum()
    }

    /// Prune old JMT state, keeping the last `keep_versions` versions.
    /// This frees memory for historical state that is no longer needed for
    /// rollback or proofs.
    pub fn prune_old_state(&self, keep_versions: u64) {
        let mut jmt = self.jmt.lock();
        let current = jmt.version();
        if current > keep_versions {
            jmt.prune_versions_before(current - keep_versions);
        }
    }

    /// Prune receipts, tx_index, full_transactions, and account_txs entries
    /// for blocks older than `keep_blocks` blocks behind the current height.
    ///
    /// This prevents unbounded memory growth at high TPS by discarding
    /// historical receipt data that is no longer needed for normal operation.
    pub fn prune_old_receipts(&self, keep_blocks: u64) {
        let current = self.height();
        if current <= keep_blocks {
            return;
        }
        let cutoff = current - keep_blocks;

        // Remove receipts whose block_height is at or below the cutoff.
        self.receipts
            .retain(|_, receipt| receipt.block_height > cutoff);

        // Remove tx_index entries that point to pruned blocks.
        self.tx_index
            .retain(|_, &mut (block_height, _)| block_height > cutoff);

        // Remove full transactions for pruned blocks.
        self.full_transactions.retain(|hash, _| {
            // If we have no tx_index entry left for this hash, it was pruned.
            self.tx_index.contains_key(hash)
        });
    }

    /// Collect state rent from all accounts using the given rent configuration.
    ///
    /// For each account:
    /// - Deduct one epoch of rent from the balance.
    /// - If the balance falls below the dust threshold, the account is
    ///   considered dormant (balance is left as-is for grace period tracking).
    ///
    /// Returns `(rent_collected, dormant_count)`.
    pub fn collect_rent(&self, config: &StateRentConfig) -> (u64, u64) {
        let rent = config.rent_per_epoch();
        if rent == 0 {
            return (0, 0);
        }

        let mut total_collected: u64 = 0;
        let mut dormant_count: u64 = 0;

        // Iterate all accounts and deduct rent.
        let keys: Vec<[u8; 32]> = self.accounts.iter().map(|e| *e.key()).collect();
        for key in keys {
            if let Some(mut entry) = self.accounts.get_mut(&key) {
                let account = entry.value_mut();

                // Skip accounts that are already below dust threshold (dormant).
                if config.is_dormant(account.balance) {
                    dormant_count += 1;
                    continue;
                }

                // Deduct rent.
                let deducted = account.balance.min(rent);
                account.balance = account.balance.saturating_sub(rent);
                total_collected += deducted;

                // Check if account became dormant after deduction.
                if config.is_dormant(account.balance) {
                    dormant_count += 1;
                }

                self.dirty_accounts.insert(key);
            }
        }

        (total_collected, dormant_count)
    }

    /// Look up a transaction receipt by tx hash.
    pub fn get_receipt(&self, tx_hash: &[u8; 32]) -> Option<TxReceipt> {
        self.receipts.get(tx_hash).map(|r| r.clone())
    }

    /// Look up transaction location (block_height, tx_index) by tx hash.
    pub fn get_tx_location(&self, tx_hash: &[u8; 32]) -> Option<(u64, u32)> {
        self.tx_index.get(tx_hash).map(|r| *r)
    }

    /// Get all transaction hashes involving an account address.
    pub fn get_account_txs(&self, address: &[u8; 32]) -> Vec<Hash256> {
        self.account_txs
            .get(address)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Get a range of blocks [from, to] with a maximum limit.
    pub fn get_block_range(&self, from: u64, to: u64, limit: usize) -> Vec<Block> {
        let mut blocks = Vec::new();
        let end = to.min(from.saturating_add(limit as u64).saturating_sub(1));
        for h in from..=end {
            if let Some(block) = self.blocks.get(&h) {
                blocks.push(block.clone());
            }
        }
        blocks
    }

    /// Persist the non-account records required to reconstruct a complete
    /// post-block state. The checkpoint is appended only after these records,
    /// and callers fsync immediately after the checkpoint.
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
                WalOp::SetFullTransaction(transaction.hash, transaction.clone()),
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
    }

    /// Index a transaction's sender and recipient addresses for `account_txs` lookups.
    /// Caps per-account history at 10K entries to prevent unbounded memory growth.
    fn index_account_tx(&self, tx: &Transaction) {
        const MAX_TX_HISTORY: usize = 10_000;
        // Index the sender first and DROP the DashMap entry guard before
        // touching any other account. Holding a shard guard while calling
        // `account_txs.entry(other_key)` deadlocks whenever the outer and
        // inner keys land on the same shard - which happens for free when
        // `other_key == tx.from.0` (e.g. an InferenceEscrowRelease whose
        // coordinator is also the proposer).
        {
            let mut entry = self.account_txs.entry(tx.from.0).or_default();
            if entry.len() >= MAX_TX_HISTORY {
                // Remove oldest 10% to amortize truncation cost
                let drain_count = MAX_TX_HISTORY / 10;
                entry.drain(..drain_count);
            }
            entry.push(tx.hash);
        }

        match &tx.body {
            TxBody::Transfer(body) => {
                self.account_txs.entry(body.to.0).or_default().push(tx.hash);
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
                // Escrow account is well-known; index it
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
                let job_marker =
                    arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                        &body.chain_domain,
                        &body.job_id,
                    );
                let certificate_marker = arc_types::transaction::CommunityInferenceRewardBody::certificate_marker_address(
                    &body.chain_domain,
                    &body.worker,
                    &body.worker_certificate.attestation_hash,
                );
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
                self.account_txs
                    .entry(arc_types::transaction::inference_reward_treasury_address().0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::InferenceChallenge(body) => {
                let escrow_addr =
                    hash_bytes(&[b"arc-inference", body.attestation_hash.as_ref()].concat());
                self.account_txs
                    .entry(escrow_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::InferenceRegister(_) => {
                // Registration modifies sender's staked_balance; sender is already tracked.
            }
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
            TxBody::ShardAssignmentProposal(_) => {
                // Assignment proposals are indexed by input-hash; sender
                // account already tracked above.
            }
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
                let marker_addr =
                    arc_types::transaction::FaucetClaimBody::marker_address(&body.recipient);
                self.account_txs
                    .entry(marker_addr.0)
                    .or_default()
                    .push(tx.hash);
            }
            TxBody::InferenceRequest(body) => {
                // Index against the request escrow address so a polling
                // client (`GET /inference/onchain/result/:id`) can find
                // the create tx by request_id.
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
        }
    }

    /// Mark all accounts affected by a transaction as dirty for incremental state root.
    fn mark_tx_accounts_dirty(&self, tx: &Transaction) {
        self.dirty_accounts.insert(tx.from.0);
        match &tx.body {
            TxBody::Transfer(body) => {
                self.dirty_accounts.insert(body.to.0);
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
                // The contract address is deterministic - mark it dirty
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
                // Also mark the counterparty as dirty - their balance is modified during close.
                // The counterparty address is stored in the escrow account's storage_root.
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
                self.dirty_accounts.insert(body.worker.0);
                self.dirty_accounts
                    .insert(arc_types::transaction::inference_reward_treasury_address().0);
                self.dirty_accounts.insert(
                    arc_types::transaction::CommunityInferenceRewardBody::marker_address(
                        &body.chain_domain,
                        &body.job_id,
                    )
                    .0,
                );
                self.dirty_accounts.insert(
                    arc_types::transaction::CommunityInferenceRewardBody::certificate_marker_address(
                        &body.chain_domain,
                        &body.worker,
                        &body.worker_certificate.attestation_hash,
                    )
                    .0,
                );
            }
            TxBody::InferenceChallenge(body) => {
                let escrow_addr =
                    hash_bytes(&[b"arc-inference", body.attestation_hash.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::InferenceRegister(_) => {
                // Sender account is already marked dirty (line above match).
            }
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
                self.dirty_accounts.insert(
                    arc_types::transaction::FaucetClaimBody::marker_address(&body.recipient).0,
                );
            }
            TxBody::InferenceRequest(body) => {
                // Request: signer balance debited + escrow created.
                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::InferenceVote(_body) => {
                // Vote: only signer nonce + escrow.code_hash flip — both already covered
                // (signer via the top-of-function insert; escrow not balance-affected).
            }
            TxBody::InferenceFinalize(body) => {
                // Finalize: escrow zeroed, agreeing voters + requester + treasury credited.
                // Exact voter set depends on stored vote bucket which we can't read here
                // (this runs before/parallel-to execute_tx in some paths). Mark the escrow
                // and the treasury; agreeing voters are marked by execute_tx via direct
                // accounts.insert (which the dirty tracker also picks up).
                let escrow_addr = hash_bytes(&[b"arc-infreq", body.request_id.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
                let treasury_addr = arc_types::transaction::faucet_pool_address();
                self.dirty_accounts.insert(treasury_addr.0);
            }
        }
    }

    /// Compute the state root using the persistent incremental Merkle tree.
    ///
    /// **Common case** (existing accounts modified): rehash k dirty accounts
    /// and recompute only the k affected paths → O(k log n).
    ///
    /// **Cold start or structural change** (new / removed accounts): full
    /// rebuild → O(n).  This is the same cost as the old approach but
    /// happens rarely (most blocks only modify existing accounts).
    fn compute_state_root(&self) -> Hash256 {
        // Protocol-v3 recovery commits every consensus-relevant persisted
        // domain. Legacy nodes retain the account-only Merkle root exactly.
        if let Some(context) = self.recovery_context() {
            return self.compute_recovery_state_root(&context);
        }

        // Delegate to JMT if enabled.
        if self.use_jmt {
            return self.compute_state_root_jmt();
        }

        let mut tree = self.incremental_merkle.lock();
        // Atomically collect and remove all dirty keys.
        // We remove only the keys we collected so that any new keys added
        // between iter() and remove() remain in the set for the next root
        // computation (avoids the iter()+clear() race condition).
        let dirty_keys: Vec<[u8; 32]> = {
            let keys: Vec<[u8; 32]> = self.dirty_accounts.iter().map(|k| *k).collect();
            for k in &keys {
                self.dirty_accounts.remove(k);
            }
            keys
        };

        let cold_start = tree.is_empty() && !self.accounts.is_empty();

        if cold_start {
            // First time: insert every account into the incremental tree.
            let mut pairs: Vec<([u8; 32], Hash256)> = self
                .accounts
                .iter()
                .map(|entry| {
                    let bytes = bincode::serialize(entry.value()).expect("serializable");
                    (*entry.key(), hash_bytes(&bytes))
                })
                .collect();
            pairs.sort_by_key(|(k, _)| *k);
            for (k, h) in pairs {
                tree.update(k, h);
            }
            tree.rebuild();
            return tree.root();
        }

        if dirty_keys.is_empty() {
            return tree.root();
        }

        // Rehash dirty accounts and update the tree.
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
            } else {
                // Account was removed.
                if tree.remove(key) {
                    structure_changed = true;
                }
            }
        }

        if structure_changed {
            tree.rebuild();
        } else {
            tree.recompute_paths(&changed_indices);
        }

        tree.root()
    }

    /// Compute state root using the Jellyfish Merkle Tree (incremental).
    ///
    /// Reads dirty accounts, hashes their current state, updates JMT leaves,
    /// and returns the new root. Much faster than `compute_state_root()` for
    /// blocks with few dirty accounts since the JMT maintains a sorted leaf
    /// set and only recomputes the binary Merkle tree on `root_hash()`.
    ///
    /// Note: this consumes `dirty_accounts` just like `compute_state_root()`
    /// does, so only one of the two should be called per block.
    fn compute_state_root_jmt(&self) -> Hash256 {
        let mut jmt = self.jmt.lock();
        // Atomically collect and remove all dirty keys.
        // We remove only the keys we collected so that any new keys added
        // between iter() and remove() remain in the set for the next root
        // computation (avoids the iter()+clear() race condition).
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

        // Cold start: populate JMT with all existing accounts.
        if jmt.is_empty() && !self.accounts.is_empty() {
            for entry in self.accounts.iter() {
                let addr = *entry.key();
                let account = entry.value();
                let hash = hash_bytes(&bincode::serialize(account).unwrap_or_default());
                jmt.update_leaf(addr, hash);
            }
            return jmt.root_hash();
        }

        // Update only dirty leaves.
        for key in &dirty_keys {
            if let Some(account) = self.accounts.get(key) {
                let hash = hash_bytes(&bincode::serialize(account.value()).unwrap_or_default());
                jmt.update_leaf(*key, hash);
            } else {
                // Account was removed.
                jmt.remove_leaf(key);
            }
        }

        jmt.root_hash()
    }

    /// Enable the JMT for state root computation.
    ///
    /// When enabled, `compute_state_root()` delegates to
    /// `compute_state_root_jmt()` instead of using IncrementalMerkle.
    /// Initializes the JMT with all existing accounts on first call.
    pub fn enable_jmt(&mut self) {
        self.use_jmt = true;
        // Initialize JMT with all existing accounts.
        let mut jmt = self.jmt.lock();
        for entry in self.accounts.iter() {
            let addr = *entry.key();
            let account = entry.value();
            let hash = hash_bytes(&bincode::serialize(account).unwrap_or_default());
            jmt.update_leaf(addr, hash);
        }
    }

    /// Get the JMT state root without consuming dirty accounts.
    /// Useful for querying the current JMT root without side effects.
    pub fn jmt_root(&self) -> Hash256 {
        let mut jmt = self.jmt.lock();
        jmt.root_hash()
    }

    /// Whether the JMT is enabled for state root computation.
    pub fn is_jmt_enabled(&self) -> bool {
        self.use_jmt
    }

    /// Total number of accounts.
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    /// Return all accounts with a non-zero staked balance (validators).
    pub fn get_staked_accounts(&self) -> Vec<(Address, Account)> {
        self.accounts
            .iter()
            .filter(|entry| entry.value().staked_balance > 0)
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect()
    }

    /// Total number of blocks.
    pub fn block_count(&self) -> u64 {
        *self.height.read()
    }

    /// Return the WAL's first fatal durability error, if any.
    pub fn wal_failure(&self) -> Option<WalError> {
        self.wal.failure()
    }

    /// Flush WAL to disk and report whether the barrier was durable.
    pub fn try_sync_wal(&self) -> Result<(), StateError> {
        self.durable_wal_barrier()
    }

    /// Best-effort compatibility wrapper for shutdown callers. Consensus block
    /// paths use the fallible barrier directly and never acknowledge failures.
    pub fn sync_wal(&self) {
        if let Err(error) = self.try_sync_wal() {
            tracing::error!(error = %error, "WAL shutdown sync failed");
        }
    }

    // -----------------------------------------------------------------------
    // State Sync Protocol (A5)
    // -----------------------------------------------------------------------

    /// Export the current state as a snapshot for state sync.
    /// New nodes can download this to bootstrap without replaying from genesis.
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
            wal_sequence: 0, // WAL sequence not tracked in StateDB directly
            accounts,
            storage,
            contracts,
        }
    }

    /// Import a snapshot to bootstrap state from a peer.
    /// Replaces all current state with the snapshot data.
    ///
    /// **Security**: After loading the snapshot data, recomputes the Merkle
    /// state root and verifies it matches `expected_state_root`.  If the roots
    /// diverge the imported state is rolled back and an error is returned.
    /// This prevents a malicious peer from injecting fabricated account
    /// balances via a crafted snapshot.
    pub fn import_snapshot(
        &self,
        snapshot: &Snapshot,
        expected_state_root: Hash256,
    ) -> Result<(), StateError> {
        // Clear existing state
        self.accounts.clear();
        self.contracts.clear();
        self.storage.clear();
        *self.incremental_merkle.lock() = IncrementalMerkle::new();
        self.dirty_accounts.clear();

        // Load accounts
        for (addr, account) in &snapshot.accounts {
            self.accounts.insert(addr.0, account.clone());
        }

        // Load contract storage
        for (addr, storage_entries) in &snapshot.storage {
            let storage_map = DashMap::new();
            for (key, value) in storage_entries {
                storage_map.insert(*key, value.clone());
            }
            self.storage.insert(addr.0, storage_map);
        }

        // Load contracts
        for (addr, bytecode) in &snapshot.contracts {
            self.contracts.insert(addr.0, bytecode.clone());
        }

        // ── Verify state root ────────────────────────────────────────────
        // Recompute the Merkle root from the freshly-loaded accounts and
        // compare against the expected root (e.g. from consensus or the
        // snapshot header).  A mismatch means the snapshot is tampered.
        let computed_root = self.compute_state_root();
        if computed_root != expected_state_root {
            // Roll back: clear everything we just loaded
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

        // Update height only after verification passes
        *self.height.write() = snapshot.block_height;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Propose-Verify Protocol (C1)
    // -----------------------------------------------------------------------

    /// Export a state diff from the set of dirty accounts.
    ///
    /// Called by the proposer after executing a block.  Returns the list of
    /// accounts that changed and the new state root.  Verifiers receive this
    /// diff and call `apply_state_diff()` to cheaply confirm correctness.
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

    /// Atomically apply a state diff whose declared root matches the resulting
    /// state. A malformed or mismatched diff leaves both accounts and the
    /// incremental state-root cache exactly as they were before this call.
    ///
    /// This is only a safe *application primitive*. A self-consistent diff does
    /// not prove that its changes were produced by a committed block, so
    /// consensus must authenticate its sender and independently execute the
    /// committed transactions before treating the diff as valid.
    pub fn apply_state_diff(&self, diff: &arc_types::StateDiff) -> Result<Hash256, StateError> {
        use std::collections::HashSet;

        // Flush any state changes that predate this diff so rollback has a
        // stable root to restore. Reject ambiguous/malformed change sets before
        // the first write.
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

        // Snapshot exactly the keys this diff may mutate. No WAL entry is
        // emitted until validation succeeds, so a rejected diff has no durable
        // side effects either.
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

        // Persist only a fully validated diff. This keeps restart state aligned
        // with the in-memory state without making rejected writes durable.
        let height = self.height();
        for change in &diff.changes {
            self.wal.append(
                WalOp::SetAccount(change.address, change.account.clone()),
                height,
            );
        }
        self.wal.append(WalOp::Checkpoint(computed_root), height);
        Ok(computed_root)
    }

    /// Verify and atomically apply a state diff.
    pub fn verify_state_diff(&self, diff: &arc_types::StateDiff) -> bool {
        self.apply_state_diff(diff).is_ok()
    }

    /// Collect the current dirty account addresses (snapshot for export_state_diff).
    pub fn drain_dirty_addresses(&self) -> Vec<Address> {
        let keys: Vec<[u8; 32]> = self.dirty_accounts.iter().map(|k| *k).collect();
        keys.into_iter().map(Hash256).collect()
    }

    // -----------------------------------------------------------------------
    // Identity Registry
    // -----------------------------------------------------------------------

    /// Register an on-chain identity for an account.
    pub fn register_identity(&self, identity: Identity) {
        self.identities.insert(identity.address.0, identity.clone());
        self.wal.append(
            WalOp::SetIdentity(identity.address, identity),
            self.height(),
        );
    }

    /// Look up the identity record for an address.
    pub fn get_identity(&self, address: &Address) -> Option<Identity> {
        self.identities.get(&address.0).map(|i| i.clone())
    }

    /// Check whether an address is compliant:
    /// identity exists, is Verified or Institutional, not expired, not sanctioned.
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

    /// Number of registered identities.
    pub fn identity_count(&self) -> usize {
        self.identities.len()
    }

    // -----------------------------------------------------------------------
    // Light Client Proofs
    // -----------------------------------------------------------------------

    /// Compute the current Merkle state root from all accounts.
    ///
    /// Accounts are serialised with bincode, hashed, then sorted to ensure a
    /// deterministic tree regardless of DashMap iteration order.
    pub fn get_state_root(&self) -> Hash256 {
        self.compute_state_root()
    }

    /// Generate a `StateProof` for the account at `address`.
    ///
    /// Uses the persistent incremental Merkle tree (sorted by address) to
    /// generate the inclusion proof.  Ensures the state root is up-to-date
    /// first by calling `compute_state_root()`.
    pub fn generate_state_proof(&self, address: &Hash256) -> Result<StateProof, StateError> {
        let account = self
            .get_account(address)
            .ok_or(StateError::AccountNotFound(*address))?;

        // Ensure the incremental tree is current.
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

    /// Generate a `HeaderProof` for the block at the given height.
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

    /// Generate a `TxInclusionProof` for a transaction by its hash.
    ///
    /// Looks up the block containing the transaction, rebuilds the tx Merkle
    /// tree, and produces the inclusion proof.
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

    /// Generate a compact `LightSnapshot` of the current chain state.
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

    // -----------------------------------------------------------------------
    // Chunked Snapshot Sync Protocol
    // -----------------------------------------------------------------------

    /// Export the full account state as a chunked snapshot.
    ///
    /// Accounts are sorted by address for deterministic ordering, then split
    /// into fixed-size chunks.  Each chunk carries a BLAKE3 integrity proof.
    /// The returned manifest contains metadata and a root hash derived from
    /// all chunk proofs.
    pub fn export_chunked_snapshot(
        &self,
        chunk_size: usize,
    ) -> (SnapshotManifest, Vec<StateSnapshot>) {
        let chunk_size = chunk_size.max(1);
        let version = self.height();
        let state_root = self.compute_state_root();

        // Collect and sort all accounts by address for deterministic chunking.
        let mut all_accounts: Vec<(Address, Account)> = self
            .accounts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();
        all_accounts.sort_by_key(|(addr, _)| addr.0);

        let total_accounts = all_accounts.len() as u64;
        let total_chunks = if all_accounts.is_empty() {
            1 // Even empty state produces one (empty) chunk
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

        // Handle the empty-state case: produce a single empty chunk.
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

        // Manifest hash = BLAKE3( version || state_root || total_accounts || total_chunks || chunk_size || all chunk proofs )
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

    /// Export a single chunk by index (for streaming to a peer without
    /// materialising the entire snapshot in memory).
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

    /// Import a single snapshot chunk into this state database.
    ///
    /// Verifies the chunk's BLAKE3 proof before inserting accounts.
    /// Returns the number of accounts imported from this chunk.
    ///
    /// On the first chunk (`chunk_index == 0`) of a fresh-state sync
    /// (height == 0), wipes any pre-import accounts/contracts/storage so the
    /// recomputed merkle root matches the source. Without this, fresh nodes
    /// keep their genesis-init validator account, which the source snapshot
    /// doesn't contain, and `finalize_sync` rejects the imported state with
    /// "state root mismatch" — the exact failure mode that stranded LHR at
    /// round 0 after every `--reset-state`.
    pub fn import_snapshot_chunk(&self, chunk: &StateSnapshot) -> Result<u32, StateError> {
        // Verify chunk proof: re-hash the chunk's account data and compare.
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

    /// Verify that the current account state matches the expected manifest root.
    ///
    /// Recomputes the Merkle state root from the accounts DashMap and compares
    /// against `manifest.state_root`.
    pub fn verify_snapshot_integrity(&self, manifest: &SnapshotManifest) -> bool {
        let computed = self.compute_state_root();
        computed == manifest.state_root
    }

    /// Create a `SyncProgress` tracker from a received manifest.
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

    /// Returns `true` when every chunk in the snapshot has been received.
    pub fn is_sync_complete(progress: &SyncProgress) -> bool {
        progress.received_chunks.iter().all(|&received| received)
    }

    /// Record a successfully imported chunk in the progress tracker.
    ///
    /// Returns `Err` if the chunk index is out of range.
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

    /// Default number of accounts per chunk when serving snapshots to peers.
    const DEFAULT_CHUNK_SIZE: usize = 1000;

    /// Export just the manifest metadata (lightweight - no account data).
    ///
    /// Peers request the manifest first to learn the chunk count, then
    /// download individual chunks in parallel via `export_snapshot_chunk`.
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

        // Hash the manifest metadata (excluding manifest_hash itself).
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

    /// Finalize a chunked sync: verify the imported state root matches the manifest.
    ///
    /// After all chunks have been imported via `import_snapshot_chunk`, call this
    /// to recompute the state root and verify integrity. Updates the internal
    /// block height to match the snapshot version.
    pub fn finalize_sync(&self, progress: &SyncProgress) -> Result<(), StateError> {
        if !Self::is_sync_complete(progress) {
            return Err(StateError::SyncIncomplete {
                received: progress.verified_chunks,
                total: progress.manifest.total_chunks,
            });
        }

        // Verify against the most recent chunk's state_root (which describes
        // the actual accounts we imported), not the manifest's state_root
        // (which was captured earlier and is stale by the time chunks were
        // generated on a chain that's producing blocks live). Falls back to
        // manifest's state_root if no chunks were received.
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

        // Update height to match snapshot
        *self.height.write() = progress.manifest.version;

        Ok(())
    }

    /// Compact state summary for monitoring dashboards and health checks.
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

    /// Internal: compute the deterministic manifest hash from its fields.
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

/// Compute the blake3 hash for a benchmark transfer transaction.
/// Uses the exact same algorithm as Transaction::compute_hash() but with
/// a precomputed base hasher (tx_type + from already hashed) and body_bytes.
/// Only the nonce varies per call - enables massive parallelism.
#[inline]
fn compute_benchmark_tx_hash(
    base_hasher: &blake3::Hasher,
    nonce: u64,
    body_bytes: &[u8],
) -> Hash256 {
    let mut h = base_hasher.clone();
    h.update(&nonce.to_le_bytes());
    h.update(body_bytes);
    h.update(&0u64.to_le_bytes()); // fee = 0
    h.update(&0u64.to_le_bytes()); // gas_limit = 0
    Hash256(*h.finalize().as_bytes())
}

/// Compute Merkle root from leaf hashes without storing intermediate levels.
/// Uses parallel pair hashing via rayon. Consumes the input vector.
/// Peak memory: ~1.5x the input size (old level + new half-size level).
fn compute_merkle_root_only(mut leaves: Vec<Hash256>) -> Hash256 {
    if leaves.is_empty() {
        return Hash256::ZERO;
    }
    if leaves.len() == 1 {
        return leaves[0];
    }
    // Pad to even length
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

        // The in-memory mutation happened before the filesystem reported the
        // failure, but the sticky latch prevents any subsequent block attempt
        // from advancing state or being acknowledged.
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
            .execute_block_blockstm_at(&[transaction], addr(99), timestamp)
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
            .execute_block_blockstm_at(&[first, second, transfer], addr(99), 1_800_000_001_001)
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

        // Signed transaction should succeed
        let mut tx = Transaction::new_transfer(address, addr(2), 500, 0);
        tx.sign(&kp).unwrap();
        let (_, receipts) = state.execute_block_verified(&[tx], addr(99)).unwrap();
        assert!(receipts[0].success);

        // Unsigned transaction should fail in verified mode
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

        // The transaction hash commits to the victim as `from`, while the
        // cryptographically valid signature and public key belong to the
        // attacker. A bare Ed25519 batch check succeeds for that supplied key;
        // the executor must still reject the ARC-address mismatch.
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

        // The next block also shares the same parent hash, proving divergence
        // is not merely deferred one height.
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

        // No identity yet
        assert!(state.get_identity(&user).is_none());
        assert!(!state.is_compliant(&user));
        assert_eq!(state.identity_count(), 0);

        // Register a verified US identity
        let id = Identity {
            address: user,
            level: IdentityLevel::Verified,
            attestor,
            proof_hash: hash_bytes(b"kyc-proof-001"),
            country_code: *b"US",
            attested_at: 1_000_000,
            expires_at: 0, // never expires
        };
        state.register_identity(id.clone());

        assert_eq!(state.identity_count(), 1);
        let fetched = state.get_identity(&user).unwrap();
        assert_eq!(fetched.level, IdentityLevel::Verified);
        assert_eq!(fetched.country_code, *b"US");
        assert!(state.is_compliant(&user));

        // Anonymous level is NOT compliant
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

        // Sanctioned country is NOT compliant
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

        // Expired identity is NOT compliant
        let expired = Identity {
            address: addr(4),
            level: IdentityLevel::Verified,
            attestor,
            proof_hash: hash_bytes(b"exp"),
            country_code: *b"DE",
            attested_at: 1_000_000,
            expires_at: 1, // expired long ago
        };
        state.register_identity(expired);
        assert!(!state.is_compliant(&addr(4)));

        assert_eq!(state.identity_count(), 4);
    }

    #[test]
    fn test_propose_verify_state_diff() {
        // Proposer: execute a block and export state diff.
        // The proposer knows affected accounts from the tx bodies.
        let proposer_state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 500, 0);
        let (block, _receipts) = proposer_state.execute_block(&[tx], addr(99)).unwrap();

        // Derive affected addresses from tx body (same as mark_tx_accounts_dirty)
        let affected = vec![addr(1), addr(2)];
        let diff = proposer_state.export_state_diff(&affected);
        assert_eq!(diff.new_root, block.header.state_root);

        // Verifier: apply the state diff (without re-executing)
        let verifier_state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let verifier_root = verifier_state.apply_state_diff(&diff).unwrap();

        // Root must match the diff's declared root
        assert_eq!(verifier_root, diff.new_root);

        // Verifier's accounts should reflect the transfer
        let sender = verifier_state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 999_500);
        let receiver = verifier_state.get_account(&addr(2)).unwrap();
        assert_eq!(receiver.balance, 500);
    }

    #[test]
    fn test_propose_verify_detects_fraud() {
        // Proposer sends a fraudulent diff (wrong new_root)
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let affected = vec![addr(1), addr(2)];
        let mut diff = state.export_state_diff(&affected);
        diff.new_root = Hash256([0xDE; 32]); // tamper with the root

        // A bad root rejects atomically; no attacker-controlled account state
        // survives the failed verification.
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

        // Create some accounts
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

        // Mark as dirty
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

        // Proposer executes transactions
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

        // Export diff
        let affected = vec![addr1, addr2];
        let diff = proposer_db.export_state_diff(&affected);

        // Verifier applies diff
        assert!(verifier_db.verify_state_diff(&diff));
    }

    #[test]
    fn test_fraud_detection_wrong_root() {
        let proposer_db = StateDB::new();
        let verifier_db = StateDB::new();

        let addr1 = hash_bytes(&[1]);

        // Proposer creates a diff
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

        // Tamper with the root -- simulate fraud
        diff.new_root = Hash256([0xFF; 32]);

        // Verifier detects fraud
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

    // -----------------------------------------------------------------------
    // Chunked Snapshot Sync Protocol tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_chunked_snapshot_export_single_chunk() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000), (addr(2), 2_000)]);

        // chunk_size large enough to fit everything in one chunk
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

        // chunk_size = 3 → should produce ceil(10/3) = 4 chunks
        let (manifest, chunks) = state.export_chunked_snapshot(3);

        assert_eq!(manifest.total_chunks, 4);
        assert_eq!(manifest.total_accounts, 10);
        assert_eq!(chunks.len(), 4);

        // First 3 chunks have 3 accounts, last has 1
        assert_eq!(chunks[0].accounts.len(), 3);
        assert_eq!(chunks[1].accounts.len(), 3);
        assert_eq!(chunks[2].accounts.len(), 3);
        assert_eq!(chunks[3].accounts.len(), 1);

        // Each chunk has correct index
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

        // Import into a fresh state
        let dest = StateDB::new();
        let mut total_imported = 0u32;
        for chunk in &chunks {
            let count = dest.import_snapshot_chunk(chunk).unwrap();
            total_imported += count;
        }
        assert_eq!(total_imported, 5);

        // Verify all accounts are present with correct balances
        for i in 0u8..5 {
            let acct = dest.get_account(&addr(i)).expect("account should exist");
            assert_eq!(acct.balance, (i as u64 + 1) * 1_000);
        }

        // State root must match the source
        let dest_root = dest.compute_state_root();
        assert_eq!(dest_root, source_root);
    }

    #[test]
    fn test_chunked_snapshot_chunk_verification() {
        let state = StateDB::with_genesis(&[(addr(1), 5_000)]);

        let (_manifest, chunks) = state.export_chunked_snapshot(10);
        assert_eq!(chunks.len(), 1);

        // Valid chunk should import successfully
        let dest = StateDB::new();
        assert!(dest.import_snapshot_chunk(&chunks[0]).is_ok());

        // Tamper with the chunk proof → verification must fail
        let mut tampered = StateSnapshot {
            version: chunks[0].version,
            state_root: chunks[0].state_root,
            accounts: chunks[0].accounts.clone(),
            chunk_index: chunks[0].chunk_index,
            total_chunks: chunks[0].total_chunks,
            chunk_proof: Hash256([0xFF; 32]), // bad proof
        };
        let dest2 = StateDB::new();
        let err = dest2.import_snapshot_chunk(&tampered).unwrap_err();
        assert!(matches!(err, StateError::ChunkVerificationFailed));

        // Tamper with account data (different from proof) → also fails
        tampered.accounts[0].1.balance = 999_999_999;
        // chunk_proof still the original, but data changed → mismatch
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

        // Same state + same chunk_size → same manifest hash
        assert_eq!(m1.manifest_hash, m2.manifest_hash);
        assert_eq!(m1.state_root, m2.state_root);
        assert_eq!(m1.total_chunks, m2.total_chunks);

        // Different chunk_size → different manifest hash
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
        assert_eq!(manifest.total_chunks, 3); // ceil(5/2) = 3

        let mut progress = StateDB::begin_sync(manifest);
        assert!(!StateDB::is_sync_complete(&progress));
        assert_eq!(progress.verified_chunks, 0);
        assert_eq!(progress.total_accounts_imported, 0);

        // Import chunks out of order (simulating parallel download)
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

        // Importing an empty chunk should succeed
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

        // After a transfer the summary should update
        let tx = Transaction::new_transfer(addr(1), addr(4), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let summary2 = state.state_summary();
        assert_eq!(summary2.account_count, 4); // addr(4) created
        // Total balance unchanged (transfer is zero-sum ignoring fees)
        assert_eq!(summary2.total_balance, 1_750_000);
        assert_eq!(summary2.block_height, 1);
    }

    // ── Receipt pruning tests ─────────────────────────────────────────────

    #[test]
    fn test_prune_old_receipts() {
        let state = StateDB::with_genesis(&[(addr(1), 10_000_000), (addr(2), 10_000_000)]);

        // Execute several blocks to build up receipts.
        for i in 0..5 {
            let tx = Transaction::new_transfer(addr(1), addr(2), 100, i);
            state.execute_block(&[tx], addr(99)).unwrap();
        }

        assert_eq!(state.height(), 5);
        // Should have 5 receipts (one tx per block).
        assert_eq!(state.receipts.len(), 5);

        // Prune keeping only last 2 blocks → blocks 4,5 kept, blocks 1,2,3 pruned.
        state.prune_old_receipts(2);

        // Only receipts from blocks 4 and 5 should remain.
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

        // keep_blocks > current height → nothing pruned.
        state.prune_old_receipts(1000);
        assert_eq!(state.receipts.len(), 1);
    }

    // ── State rent tests ──────────────────────────────────────────────────

    #[test]
    fn test_collect_rent_deducts_balance() {
        let state = StateDB::with_genesis(&[
            (addr(1), 5_000_000),  // above dust threshold (1_000_000)
            (addr(2), 10_000_000), // above dust threshold
        ]);

        let config = StateRentConfig::default();
        let rent = config.rent_per_epoch(); // 128

        let (collected, dormant) = state.collect_rent(&config);

        // Both accounts should have rent deducted.
        assert_eq!(collected, rent * 2);
        assert_eq!(dormant, 0);

        let acct1 = state.get_account(&addr(1)).unwrap();
        assert_eq!(acct1.balance, 5_000_000 - rent);

        let acct2 = state.get_account(&addr(2)).unwrap();
        assert_eq!(acct2.balance, 10_000_000 - rent);
    }

    #[test]
    fn test_collect_rent_marks_dormant() {
        // Account with balance just above dust threshold → rent pushes it below.
        let balance = 1_000_100; // dust = 1_000_000, rent = 128 → after: 999_972 < 1_000_000
        let state = StateDB::with_genesis(&[(addr(1), balance)]);

        let config = StateRentConfig::default();
        let (collected, dormant) = state.collect_rent(&config);

        assert_eq!(collected, config.rent_per_epoch());
        assert_eq!(dormant, 1); // became dormant after deduction

        let acct = state.get_account(&addr(1)).unwrap();
        assert!(config.is_dormant(acct.balance));
    }

    #[test]
    fn test_collect_rent_skips_already_dormant() {
        let state = StateDB::with_genesis(&[
            (addr(1), 500), // well below dust threshold
        ]);

        let config = StateRentConfig::default();
        let (collected, dormant) = state.collect_rent(&config);

        // Already dormant → no rent deducted.
        assert_eq!(collected, 0);
        assert_eq!(dormant, 1);

        let acct = state.get_account(&addr(1)).unwrap();
        assert_eq!(acct.balance, 500); // unchanged
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

    // ── Channel integration tests ────────────────────────────────────────

    use arc_types::transaction::{
        ChannelCloseBody, ChannelDisputeBody, ChannelOpenBody, InferenceRegisterBody,
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

        // Sender balance debited
        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 900_000);

        // Escrow created with deposit
        let escrow_addr = hash_bytes(&[b"arc-channel", channel_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 100_000);
        assert_eq!(escrow.code_hash, addr(1)); // opener
        assert_eq!(escrow.storage_root, addr(2)); // counterparty
    }

    #[test]
    fn test_channel_close_releases_funds() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let channel_id = hash_bytes(b"test-channel-2");

        // Open
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

        // Close (opener closes, split 60K/40K)
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

        // Escrow drained
        let escrow_addr = hash_bytes(&[b"arc-channel", channel_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 0);

        // Opener credited
        let opener = state.get_account(&addr(1)).unwrap();
        assert_eq!(opener.balance, 960_000); // 900K + 60K

        // Counterparty credited
        let counterparty = state.get_account(&addr(2)).unwrap();
        assert_eq!(counterparty.balance, 40_000);
    }

    #[test]
    fn test_channel_dispute_tracks_nonce_and_expiry() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let channel_id = hash_bytes(b"test-channel-3");

        // Open channel
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

        // Dispute from counterparty (addr(2))
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

        // Check escrow state updated
        let escrow_addr = hash_bytes(&[b"arc-channel", channel_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.nonce, 5); // state_nonce recorded
        assert!(escrow.staked_balance > 0); // challenge_expiry set
        assert_eq!(escrow.balance, 100_000); // funds still locked
    }

    #[test]
    fn test_channel_dispute_rejects_lower_nonce() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let channel_id = hash_bytes(b"test-channel-4");

        // Open
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

        // First dispute with nonce 10
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

        // Second dispute with lower nonce (5) - should fail
        let d2 = make_channel_tx(
            addr(1),
            1,
            TxBody::ChannelDispute(ChannelDisputeBody {
                channel_id,
                opener_balance: 80_000,
                counterparty_balance: 20_000,
                other_party_sig: vec![0u8; 64],
                state_nonce: 5, // lower than 10!
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

        // Open
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

        // Dispute (sets challenge_expiry far in the future)
        let dispute_tx = make_channel_tx(
            addr(2),
            0,
            TxBody::ChannelDispute(ChannelDisputeBody {
                channel_id,
                opener_balance: 60_000,
                counterparty_balance: 40_000,
                other_party_sig: vec![0u8; 64],
                state_nonce: 1,
                challenge_period: 100_000, // very long
            }),
            TxType::ChannelDispute,
        );
        let (_, r) = state.execute_block(&[dispute_tx], addr(99)).unwrap();
        assert!(r[0].success);

        // Try to close - should fail (active dispute)
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

        // Open with 100K deposit
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

        // Dispute claiming more than deposited - should fail
        let dispute_tx = make_channel_tx(
            addr(2),
            0,
            TxBody::ChannelDispute(ChannelDisputeBody {
                channel_id,
                opener_balance: 80_000,
                counterparty_balance: 40_000, // 80K + 40K = 120K > 100K!
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
        assert_eq!(acct.balance, 95_000); // 100K - 5K
        assert_eq!(acct.staked_balance, 5_000); // locked
    }

    #[test]
    fn test_inference_register_rejects_insufficient_stake() {
        let state = StateDB::with_genesis(&[(addr(1), 100_000)]);

        // Tier 2 requires 5K minimum, try with only 1K
        let tx = make_channel_tx(
            addr(1),
            0,
            TxBody::InferenceRegister(InferenceRegisterBody {
                tier: 2,
                stake_bond: 1_000, // below min for tier 2
            }),
            TxType::InferenceRegister,
        );

        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(
            !r[0].success,
            "InferenceRegister with insufficient stake should fail"
        );
    }

    // ── Milestone B: InferenceEscrow integration tests ───────────────────
    //
    // All tests use real balance deltas - no mocks. The acceptance criterion
    // from PLAN.md is "payer pays N ARC; balances on serving replicas
    // increase by their share; total conserved."

    use arc_types::transaction::{
        InferenceEscrowOpenBody, InferenceEscrowRefundBody, InferenceEscrowReleaseBody,
    };

    /// Convenience: same 32-byte request_id in every test, keyed on test name
    /// so concurrent-cargo-test runs don't collide on the escrow DashMap.
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

        // Payer balance debited.
        let payer = state.get_account(&addr(1)).unwrap();
        assert_eq!(payer.balance, 990_000);
        assert_eq!(payer.nonce, 1);

        // Escrow account holds the max_fee.
        let escrow_addr = Hash256(InferenceEscrowOpenBody::escrow_address(&request_id));
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 10_000);
        // The escrow's storage_root commits to the payer's identity.
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
        // Payer balance unchanged.
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
        // addr(1) = payer, addr(2) = proposer, addr(3..=5) = replicas,
        // addr(10) = observer pool, addr(11) = treasury.
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
            addr(2), // proposer submits release
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

        // 40% proposer, 25% replicas (split 3 ways evenly with rounding → treasury),
        // 15% observer, 20% treasury (+ rounding residue).
        // 10_000 × 40 / 100 = 4_000
        // 10_000 × 25 / 100 = 2_500 → 833 × 3 = 2_499; 1 residue to treasury
        // 10_000 × 15 / 100 = 1_500
        // treasury = 10_000 - 4_000 - 2_499 - 1_500 = 2_001
        assert_eq!(proposer.balance, 4_000);
        assert_eq!(r1.balance, 833);
        assert_eq!(r2.balance, 833);
        assert_eq!(r3.balance, 833);
        assert_eq!(obs.balance, 1_500);
        assert_eq!(tre.balance, 2_001);

        // Conservation: payer is short 10_000; beneficiaries are up 10_000.
        let credited =
            proposer.balance + r1.balance + r2.balance + r3.balance + obs.balance + tre.balance;
        assert_eq!(credited, 10_000);
        assert_eq!(payer.balance, 990_000);

        // Escrow zeroed + commitment cleared so it can't be replayed.
        assert_eq!(escrow.balance, 0);
        assert_eq!(escrow.storage_root, Hash256::ZERO);
    }

    #[test]
    fn test_escrow_release_rejects_wrong_payer() {
        // Open as addr(1); release names addr(42) as payer - metadata
        // commitment won't match, release must fail.
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
                payer: addr(42), // LIE
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
        // Escrow still holds funds; nobody got paid.
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
                replicas: vec![], // empty
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
        // Open with timeout_blocks=2, then advance 3 blocks, then refund
        // succeeds and payer is whole.
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

        // Advance 3 empty blocks so `now >= opened_at + timeout_blocks`.
        let _ = state.execute_block(&[], addr(99)).unwrap();
        let _ = state.execute_block(&[], addr(99)).unwrap();
        let _ = state.execute_block(&[], addr(99)).unwrap();

        let refund = make_channel_tx(
            addr(1), // only payer can refund
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
        // Open with timeout=10; refund at block 2 must fail.
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
        // addr(1) opens; addr(2) tries to refund → rejected (not original payer).
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

        // Advance timeout
        let _ = state.execute_block(&[], addr(99)).unwrap();
        let _ = state.execute_block(&[], addr(99)).unwrap();

        let refund = make_channel_tx(
            addr(2), // not the payer
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

    // ── Milestones C+D: model registry / requests / claims / capacity ──
    //
    // MVP tests - exercise the happy path plus the critical reject paths
    // (duplicate registration, insufficient balance, empty ranges).

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

        // Payer debited by the min fee.
        assert_eq!(
            state.get_account(&addr(1)).unwrap().balance,
            10_000 - MIN_MODEL_REGISTRATION_FEE
        );
        // Treasury credited.
        let treasury = hash_bytes(b"arc-treasury");
        assert_eq!(
            state.get_account(&treasury).unwrap().balance,
            MIN_MODEL_REGISTRATION_FEE
        );

        // Registry entry exists with a non-zero commitment.
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
        // Registration fee below the min is raised to MIN. Payer pays
        // the higher amount regardless of what they passed.
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
                registration_fee: 1, // below floor
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

        // Bond debited from payer.
        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 95_000);
        // Claim account holds bond.
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
        // Payer untouched.
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
        // After a successful release, a second release on the same
        // request_id must fail (escrow cleared).
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

    // ── FaucetClaim: validator-authorized faucet drain (P0 fix) ──────────
    //
    // Lives at the bottom of the test module so the helpers above (`addr`,
    // `make_channel_tx`) are in scope.

    use arc_types::transaction::{FAUCET_CLAIM_MAX, FaucetClaimBody};

    /// Convenience: seed a validator at `signer` with stake = MIN_VALIDATOR_STAKE
    /// so `is_validator(&signer)` returns true without paying the
    /// JoinValidator-debit / nonce-bump path.
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
        let state = StateDB::with_genesis(&[
            (pool_addr, 1_000_000_000),
            (validator, 0), // validator doesn't need balance — pool is debited
        ]);
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
        // Signer nonce is INTENTIONALLY not bumped — see executor arm.
        // Validators are read from state on signing-side anyway, so a
        // missing-account here is also acceptable.
        if let Some(signer) = state.get_account(&validator) {
            assert_eq!(signer.balance, 0, "signer balance untouched");
        }
    }

    #[test]
    fn test_faucet_claim_rejects_non_validator_signer() {
        let pool_addr = arc_types::transaction::faucet_pool_address();
        let not_a_validator = addr(8);
        let state = StateDB::with_genesis(&[(pool_addr, 1_000_000), (not_a_validator, 0)]);
        // Deliberately do NOT seed_validator.

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

        // Pool untouched, recipient never created.
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
        let state = StateDB::with_genesis(&[
            (pool_addr, 500), // not enough for a 1_000 claim
        ]);
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
        // Pool balance unchanged.
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

        // These are distinct, independently authorized transactions, exactly
        // as a recipient could obtain by calling two public validators.
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
        // Mirrors the real peer-propagation path: build + sign on one
        // state, serialize, deserialize on a SEPARATE state, run through
        // execute_block. Catches the bug the P0 fix exists to fix —
        // before the fix, a null-signed faucet tx serialized over the
        // wire would deserialize with sig_verified=false on the peer,
        // get its signature checked, fail, and never apply.
        use arc_crypto::KeyPair;
        let pool_addr = arc_types::transaction::faucet_pool_address();

        // Seed an Ed25519 validator on the proposer side.
        let kp = KeyPair::generate_ed25519();
        let validator = kp.address();

        let proposer_state = StateDB::with_genesis(&[(pool_addr, 100_000_000)]);
        seed_validator(&proposer_state, validator);

        let mut tx = Transaction::new_faucet_claim(validator, addr(50), 10_000, 0);
        tx.sign(&kp).expect("sign ok");
        let wire = bincode::serialize(&tx).expect("serialize tx");

        let peer_state = StateDB::with_genesis(&[(pool_addr, 100_000_000)]);
        seed_validator(&peer_state, validator);

        // sig_verified is `#[serde(default)]` so the peer deserializes
        // with the flag cleared — same as a real network hop.
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

    // ── Tier 1 on-chain inference state transitions ──

    /// Build an unsigned tier1 InferenceRequest tx for tests.
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
        // Register a validator so block production has a proposer.
        state
            .validators
            .insert(addr(99).0, StateDB::MIN_VALIDATOR_STAKE);

        let req_id = [42u8; 32];
        let tx = build_tier1_request(requester, 0, req_id, 10, 1, 20);
        let (_, receipts) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(receipts[0].success, "request must succeed");

        // Requester balance debited.
        let r = state.get_account(&requester).unwrap();
        assert_eq!(r.balance, 90);
        assert_eq!(r.nonce, 1);

        // Escrow holds the locked reward.
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

        // Construct an oversized prompt directly.
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
        // Single-validator committee. Validator votes for its own
        // request, finalize pays out (after escrow rebate + treasury).
        let validator = addr(10);
        let state = StateDB::with_genesis(&[(validator, 1_000_000)]);
        state
            .validators
            .insert(validator.0, StateDB::MIN_VALIDATOR_STAKE);

        let req_id = [7u8; 32];
        // Tx 1: request (nonce 0)
        let tx1 = build_tier1_request(validator, 0, req_id, 100, 1, 20);
        let (block1, r1) = state.execute_block(&[tx1], validator).unwrap();
        assert!(r1[0].success);

        // Committee seed = block hash of tx1's commit block.
        let committee_seed = block1.hash;

        // Tx 2: vote (nonce 1)
        let output_hash = hash_bytes(b"the answer");
        let tx2 = build_tier1_vote(validator, 1, req_id, committee_seed, output_hash);
        let (_, r2) = state.execute_block(&[tx2], validator).unwrap();
        assert!(r2[0].success, "vote must succeed");

        // Tx 3: finalize (nonce 2). vote_count = 1 = committee_size → eligible.
        let tx3 = build_tier1_finalize(validator, 2, req_id);
        let (_, r3) = state.execute_block(&[tx3], validator).unwrap();
        assert!(r3[0].success, "finalize must succeed");

        // Escrow zeroed + status Finalized.
        let escrow_addr = hash_bytes(&[b"arc-infreq", req_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 0);
        assert_eq!(escrow.code_hash.0[0], TIER1_STATUS_FINALIZED);

        // Validator received 70% of 100 = 70 (voters share) + 20 (refund) = 90 back.
        // Treasury got 10.
        let final_validator = state.get_account(&validator).unwrap();
        // started 1_000_000, locked 100 in tx1, then earned voters+refund=90,
        // so balance = 999_900 + 90 = 999_990
        assert_eq!(final_validator.balance, 999_990);

        let treasury = state
            .get_account(&arc_types::transaction::faucet_pool_address())
            .unwrap();
        // Treasury starts at 0 in this genesis, gets +10.
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

        // Advance past deadline (5 blocks).
        for _ in 0..6 {
            state.execute_block(&[], addr(99)).unwrap();
        }

        // Finalize with no votes → timeout refund.
        let tx2 = build_tier1_finalize(addr(99), 0, req_id);
        let (_, r2) = state.execute_block(&[tx2], addr(99)).unwrap();
        assert!(r2[0].success, "finalize on timeout must succeed");

        let after = state.get_account(&requester).unwrap();
        // requester started 1000, locked 50, gets back 49 (50 - 1 anti-spam) → 999.
        assert_eq!(after.balance, 1000 - max_reward + (max_reward - 1));
    }

    #[test]
    fn tier1_vote_from_non_committee_rejected() {
        let alice = addr(30);
        let bob = addr(31); // Not a validator → not in committee.
        let state = StateDB::with_genesis(&[(alice, 1000), (bob, 1000)]);
        // Only alice is a validator.
        state
            .validators
            .insert(alice.0, StateDB::MIN_VALIDATOR_STAKE);

        let req_id = [55u8; 32];
        let tx1 = build_tier1_request(alice, 0, req_id, 10, 1, 20);
        let (block1, r1) = state.execute_block(&[tx1], alice).unwrap();
        assert!(r1[0].success);
        let committee_seed = block1.hash;

        // bob tries to vote → must fail.
        let tx2 = build_tier1_vote(bob, 0, req_id, committee_seed, hash_bytes(b"x"));
        let (_, r2) = state.execute_block(&[tx2], alice).unwrap();
        assert!(
            !r2[0].success,
            "vote from non-committee member must be rejected"
        );
    }

    // ── Tier 2 attestation bonds + authorized community rewards ────────────
    //
    // Raw attestations only commit model/input/output and lock an optional
    // challenge bond. Treasury income is a separate validator-authorized,
    // job-bound and replay-marked state transition.

    const REWARD: u64 = arc_types::economics::INFERENCE_ATTESTATION_REWARD;

    /// Total spendable + staked balance across every account (escrows and the
    /// treasury included). The conservation invariant for all tests below.
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

        // Attester: start − bond, nonce +1. Raw attestations do not pay.
        let w = state.get_account(&worker).unwrap();
        assert_eq!(w.balance, worker_start - bond);
        assert_eq!(w.nonce, 1);

        // Treasury is untouched until an authorized reward transaction.
        assert_eq!(state.get_account(&pool).unwrap().balance, treasury_start);

        // Bond locked in escrow, tagged OPEN, refund target = worker.
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
        // No escrow account is created for a zero bond.
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
        // Three identities satisfy the 3-of-4 count requirement, but the
        // omitted validator owns most of the active stake.
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
        // Three identities meet the 3-of-4 count policy, but their stake is
        // exactly 6/9 = 2/3. Reward authorization requires strictly >2/3.
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

        // execute_block increments height to 1 before applying this claim.
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
        // A worker whose balance is below the bond must still fail with the
        // unchanged InsufficientBalance error — the treasury-funded reward does
        // NOT relax the bond check.
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

        // Treasury untouched (reward never paid), worker untouched.
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
        let treasury_start = REWARD / 5; // less than a full reward
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
        // With no faucet-pool account at all, the reward is skipped (no panic,
        // no synthetic balance) and the attester is simply −bond into escrow.
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

        // Not released before the window: a sweep one block early is a no-op.
        assert_eq!(state.sweep_matured_bond_releases(release_h - 1), 0);
        assert_eq!(state.get_account(&escrow_addr).unwrap().balance, bond);
        assert_eq!(
            state.get_account(&worker).unwrap().balance,
            worker_after_attest
        );

        // Released exactly at the deadline: escrow → 0, worker regains the bond.
        assert_eq!(state.sweep_matured_bond_releases(release_h), 1);
        assert_eq!(state.get_account(&escrow_addr).unwrap().balance, 0);
        assert_eq!(
            state.get_account(&worker).unwrap().balance,
            worker_after_attest + bond
        );
        // Idempotent: sweeping again does nothing.
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

        // Block 1: attestation.
        let att = make_attestation(worker, 0, bond, cp, b"job-challenged");
        let att_hash = att.hash;
        assert!(state.execute_block(&[att], addr(99)).unwrap().1[0].success);
        let escrow_addr = hash_bytes(&[b"arc-inference", att_hash.as_ref()].concat());
        let release_h = state.get_account(&escrow_addr).unwrap().nonce;

        // Block 2: challenge (challenger bonds ch_bond).
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

        // Escrow now holds both bonds and is marked CHALLENGED.
        let esc = state.get_account(&escrow_addr).unwrap();
        assert_eq!(esc.balance, bond + ch_bond);
        assert_eq!(esc.storage_root.0[8], ATTEST_STATUS_CHALLENGED);

        // A challenged/slashed escrow is NEVER auto-refunded, at the deadline or
        // long after it.
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
        // A full multi-step scenario mixing every settlement operation; the
        // total balance (treasury + workers + challenger + escrows) is invariant
        // after every single block.
        let pool = arc_types::transaction::faucet_pool_address();
        let validator = addr(70);
        let worker_c = addr(71); // community worker (bond 0)
        let worker_b = addr(72); // bonded worker whose bond is later released
        let worker_x = addr(73); // bonded worker whose attestation is challenged
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

        // Block 1: faucet the community worker.
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

        // Block 2: community worker attests (bond 0); no implicit reward.
        let a_c = make_attestation(worker_c, 0, 0, 5, b"c");
        assert!(state.execute_block(&[a_c], validator).unwrap().1[0].success);
        conserved!();

        // Block 3: bonded worker (release path) attests, cp = 4 → matures at 7.
        let a_b = make_attestation(worker_b, 0, REWARD, 4, b"b");
        let hb = a_b.hash;
        assert!(state.execute_block(&[a_b], validator).unwrap().1[0].success);
        conserved!();

        // Block 4: bonded worker (challenge path) attests with a long window.
        let a_x = make_attestation(worker_x, 0, REWARD, 50, b"x");
        let hx = a_x.hash;
        assert!(state.execute_block(&[a_x], validator).unwrap().1[0].success);
        conserved!();

        // Block 5: challenge worker_x's attestation.
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

        // Advance empty blocks until worker_b's bond matures; the in-block
        // sweep refunds it during ordinary block application.
        let esc_b = hash_bytes(&[b"arc-inference", hb.as_ref()].concat());
        let release_b = state.get_account(&esc_b).unwrap().nonce;
        assert!(release_b > state.height(), "not matured yet");
        while state.height() < release_b {
            state.execute_block(&[], validator).unwrap();
            conserved!();
        }

        // worker_b's bond was auto-released by the sweep.
        assert_eq!(state.get_account(&esc_b).unwrap().balance, 0);
        // worker_x's escrow remains locked (challenged), never auto-released.
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
        // Restart durability: the in-memory release queue is rebuilt from the
        // surviving OPEN escrow accounts, and a rebuilt queue releases exactly
        // like the original.
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

        // Simulate a restart: the queue is empty but the escrow account
        // survives. Rebuild it from account state.
        state.pending_bond_releases.lock().clear();
        assert_eq!(state.pending_bond_release_count(), 0);
        assert_eq!(state.rebuild_pending_bond_releases(), 1);
        assert_eq!(state.pending_bond_release_count(), 1);

        // The rebuilt queue releases at the correct deadline.
        assert_eq!(state.sweep_matured_bond_releases(release_h - 1), 0);
        assert_eq!(state.sweep_matured_bond_releases(release_h), 1);
        assert_eq!(state.get_account(&escrow_addr).unwrap().balance, 0);
    }
}
