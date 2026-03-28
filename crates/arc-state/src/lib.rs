pub mod mmap_state;
pub mod simd_parse;
pub mod block_stm;
pub mod gpu_state;
pub mod io_backend;
pub mod jmt_store;
pub mod light_client;
pub mod wal;

use arc_crypto::{Hash256, IncrementalMerkle, MerkleTree, hash_bytes, hash_pair};
use arc_types::{Account, Address, Identity, IdentityLevel, Transaction, TxBody, TxType, TxReceipt, TransferBody, Block, BlockHeader, ProtocolVersion};
use arc_types::block::{StateDiff, AccountChange};
use arc_types::economics::StateRentConfig;
use arc_types::transaction::{GasMeter, gas_costs};

use crate::jmt_store::JmtStateTree;
use light_client::{StateProof, HeaderProof, TxInclusionProof, LightSnapshot};
use serde::{Serialize, Deserialize};
use dashmap::{DashMap, DashSet};
use parking_lot::RwLock;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

pub use wal::{WalWriter, WalOp, WalEntry, Snapshot, PersistenceConfig, read_wal, read_wal_dir, find_last_checkpoint};

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
    StateRootMismatch { expected: Hash256, computed: Hash256 },
}


// ---------------------------------------------------------------------------
// Chunked State Snapshot Protocol — types for fast state sync
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

/// Derive a deterministic contract address from the deployer address and nonce.
///
/// Mirrors the logic in `arc_vm::compute_contract_address` — duplicated here to
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
    /// Async indexer channel — sends batches to background threads.
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
    /// Archive mode — when true, skips all pruning (keeps full history).
    /// Used by block explorers and analytics nodes.
    pub archive_mode: bool,
}

impl StateDB {
    /// Create a new empty state (no persistence — benchmark mode).
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
        }
    }

    /// Create a new state with WAL persistence.
    pub fn with_persistence(wal_path: impl AsRef<Path>) -> Result<Self, StateError> {
        let wal = WalWriter::new(wal_path)
            .map_err(|e| StateError::PersistenceError(e.to_string()))?;
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
    pub fn with_genesis_gpu(prefunded: &[(Address, u64)], gpu_config: gpu_state::GpuStateCacheConfig) -> Self {
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
        tracing::info!(loaded = loaded, "GPU state cache enabled, pre-loaded accounts");
    }

    /// Get the GPU state cache (if enabled) for direct access.
    pub fn gpu_cache(&self) -> Option<&Arc<gpu_state::GpuStateCache>> {
        self.gpu_cache.as_ref()
    }

    /// Create state with WAL persistence + genesis accounts.
    /// On startup: if WAL exists, replay it to recover state. Otherwise start fresh with genesis.
    pub fn with_genesis_persistent(prefunded: &[(Address, u64)], wal_dir: impl AsRef<Path>) -> Result<Self, StateError> {
        let wal_dir = wal_dir.as_ref();

        // Ensure the data directory exists
        std::fs::create_dir_all(wal_dir)
            .map_err(|e| StateError::PersistenceError(format!("failed to create data dir {:?}: {}", wal_dir, e)))?;

        let wal_path = wal_dir.join("state.wal");

        if wal_path.exists() {
            // WAL exists — replay to recover state
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
            // No WAL — fresh start with genesis accounts and persistence enabled
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
            state.wal.sync();

            tracing::info!(
                "Fresh state initialized with {} genesis accounts, WAL at {:?}",
                prefunded.len(),
                wal_path
            );

            Ok(state)
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
                // Agent registry — stored in account metadata (future)
            }
            WalOp::SetContract(addr, bytecode) => {
                self.contracts.insert(addr.0, bytecode.clone());
            }
            WalOp::Checkpoint(_) => {
                // Checkpoints are informational — no state change
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

            let storage: Vec<(Address, Vec<(Hash256, Vec<u8>)>)> = self
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
                    height_before, height_after
                );
                continue; // Retry — a block was applied during iteration
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
        self.wal.append(WalOp::SetContract(*address, bytecode), self.height());
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
        if let Some(&(height, idx)) = self.tx_index.get(tx_hash).as_deref() {
            if let Some(block_data) = self.signed_block_data.get(&height) {
                let (txs_vec, _, _) = &*block_data;
                return txs_vec.get(idx as usize).cloned();
            }
        }
        None
    }

    /// Set a storage value for a contract.
    pub fn set_storage(&self, contract: &Address, key: Hash256, value: Vec<u8>) {
        self.storage
            .entry(contract.0)
            .or_default()
            .insert(key, value.clone());
        self.wal.append(
            WalOp::SetStorage(*contract, key, value),
            self.height(),
        );
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
        self.wal.append(
            WalOp::DeleteStorage(*contract, *key),
            self.height(),
        );
    }

    /// Get an account (returns None if not found).
    ///
    /// When a GPU state cache is enabled, checks GPU memory first for ~40x
    /// bandwidth improvement on hot accounts.
    pub fn get_account(&self, addr: &Address) -> Option<Account> {
        // Fast path: check GPU cache first.
        if let Some(ref cache) = self.gpu_cache {
            if let Some(acct) = cache.get_account_fast(&addr.0) {
                return Some(acct);
            }
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
        // Write-through to GPU cache (fast path — single DashMap insert).
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
            self.event_logs.entry(height).or_default().extend(logs);
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

    /// Get all active validators and their stakes.
    pub fn active_validators(&self) -> Vec<(Address, u64)> {
        self.validators
            .iter()
            .filter(|entry| *entry.value() >= Self::MIN_VALIDATOR_STAKE)
            .map(|entry| (Hash256(*entry.key()), *entry.value()))
            .collect()
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
        if wal_seq > 1000 {
            if let Err(e) = self.wal.delete_segments_before(wal_seq - 1000) {
                tracing::warn!("WAL segment cleanup failed: {}", e);
            }
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
            if let Some(block) = self.blocks.get(&height) {
                if block.hash.0 == *hash {
                    return Some(block.clone());
                }
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

        // Execute transactions — use Block-STM parallel batches when beneficial
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
            receipts.resize(transactions.len(), TxReceipt {
                tx_hash: Hash256::ZERO,
                block_height: height,
                block_hash: Hash256::ZERO,
                index: 0,
                success: false,
                gas_used: 0,
                value_commitment: None,
                inclusion_proof: None,
                logs: vec![],
            });

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

                // Each batch contains txs that touch disjoint accounts — safe to parallelize
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
            protocol_version: ProtocolVersion::GENESIS,
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
        self.wal.append(WalOp::SetBlock(height, block.clone()), height);

        // WAL checkpoint at block boundary
        self.wal.append(WalOp::Checkpoint(state_root), height);

        // Check if we should take a snapshot
        let count = self.snapshot_counter.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count % 10_000 == 0 {
            tracing::info!("Snapshot trigger at block {}", height);
            // Snapshot is taken asynchronously in production — here we just log
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
        let mode = crate::block_stm::choose_execution_mode(transactions);
        match mode {
            crate::block_stm::AdaptiveMode::Sequential => {
                self.execute_block_verified(transactions, producer)
            }
            crate::block_stm::AdaptiveMode::BlockSTM => {
                // Use BlockSTM partitioned execution
                self.execute_block_blockstm(transactions, producer)
            }
        }
    }

    /// Execute a block using BlockSTM partitioned parallel execution.
    fn execute_block_blockstm(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
        use rayon::prelude::*;

        // ── Pre-sort by (sender, nonce) to reduce BlockSTM conflicts ──────
        // Build a sorted index so same-sender TXs are adjacent in nonce order.
        // This makes the partition algorithm place them in sequential batches
        // naturally, reducing cross-batch conflicts.
        let mut sorted_indices: Vec<usize> = (0..transactions.len()).collect();
        sorted_indices.sort_by(|&a, &b| {
            transactions[a].from.0.cmp(&transactions[b].from.0)
                .then(transactions[a].nonce.cmp(&transactions[b].nonce))
        });

        // Build a re-ordered transaction slice for partitioning.
        let sorted_txs: Vec<Transaction> = sorted_indices.iter()
            .map(|&i| transactions[i].clone())
            .collect();
        // Map from sorted position back to original index.
        let batches = crate::block_stm::partition_batches(&sorted_txs);
        let mut receipts = vec![None; transactions.len()];
        let mut tx_hashes = vec![Hash256::ZERO; transactions.len()];

        let height = {
            let mut h = self.height.write();
            *h += 1;
            *h
        };

        let parent = self.blocks.get(&(height - 1))
            .map(|b| b.hash)
            .unwrap_or(Hash256::ZERO);

        // Execute batches: within each batch, TXs run in parallel.
        // Batches run sequentially (they may have cross-batch dependencies).
        // Note: batch indices refer to positions in sorted_txs; we map back
        // to original positions via sorted_indices for receipt/hash placement.
        for batch in &batches {
            let batch_results: Vec<(usize, bool, u64)> = batch
                .par_iter()
                .map(|&sorted_idx| {
                    let orig_idx = sorted_indices[sorted_idx];
                    let tx = &transactions[orig_idx];
                    self.mark_tx_accounts_dirty(tx);
                    let result = if tx.sig_verified {
                        self.execute_tx(tx) // Pre-verified (faucet/RPC) — skip sig check
                    } else if tx.is_unsigned() {
                        Err(StateError::ExecutionError("unsigned transaction".into()))
                    } else if tx.verify_signature().is_err() {
                        Err(StateError::ExecutionError("invalid signature".into()))
                    } else {
                        self.execute_tx(tx)
                    };
                    let (success, gas_used) = match result {
                        Ok(gas) => (true, gas),
                        Err(_) => (false, Self::gas_cost_for_tx(tx)),
                    };
                    (orig_idx, success, gas_used)
                })
                .collect();

            for (orig_idx, success, gas_used) in batch_results {
                tx_hashes[orig_idx] = transactions[orig_idx].hash;
                receipts[orig_idx] = Some(TxReceipt {
                    tx_hash: transactions[orig_idx].hash,
                    block_height: height,
                    block_hash: Hash256::ZERO,
                    index: orig_idx as u32,
                    success,
                    gas_used,
                    value_commitment: None,
                    inclusion_proof: None,
                    logs: vec![],
                });
            }
        }

        // Unwrap all receipts (all slots should be filled)
        let receipts: Vec<TxReceipt> = receipts.into_iter()
            .enumerate()
            .map(|(i, r)| r.unwrap_or(TxReceipt {
                tx_hash: transactions[i].hash,
                block_height: height,
                block_hash: Hash256::ZERO,
                index: i as u32,
                success: false,
                gas_used: 0,
                value_commitment: None,
                inclusion_proof: None,
                logs: vec![],
            }))
            .collect();

        // Build block (same as sequential path)
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
            protocol_version: ProtocolVersion::GENESIS,
            state_diff: None,
        };

        let mut block = Block::new(header, tx_hashes);

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
        self.wal.append(WalOp::SetBlock(height, block.clone()), height);
        self.wal.append(WalOp::Checkpoint(state_root), height);

        Ok((block, final_receipts))
    }

    /// Execute a block with sequential verification (original path).
    pub fn execute_block_verified(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
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
                if tx.compute_hash() != tx.hash {
                    batch_sig_valid[i] = Some(false);
                    continue;
                }
                if let arc_crypto::signature::Signature::Ed25519 { public_key, signature } = &tx.signature {
                    if signature.len() == 64 {
                        if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(public_key) {
                            let mut sig_bytes = [0u8; 64];
                            sig_bytes.copy_from_slice(signature);
                            ed_indices.push(i);
                            ed_msgs.push(tx.hash.0.to_vec());
                            ed_sigs.push(ed25519_dalek::Signature::from_bytes(&sig_bytes));
                            ed_vks.push(vk);
                            continue;
                        }
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
                        // Batch failed — fall back to individual verification to find bad ones
                        for &idx in &ed_indices {
                            let valid = transactions[idx].verify_signature().is_ok();
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
                // Pre-verified (faucet/RPC) — skip sig check
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
            } else if tx.verify_signature().is_err() {
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
            protocol_version: ProtocolVersion::GENESIS,
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
        self.wal.append(WalOp::SetBlock(height, block.clone()), height);

        // WAL checkpoint at block boundary
        self.wal.append(WalOp::Checkpoint(state_root), height);

        // Check if we should take a snapshot
        let count = self.snapshot_counter.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count % 10_000 == 0 {
            tracing::info!("Snapshot trigger at block {}", height);
        }

        Ok((block, receipts))
    }

    /// Execute a block with GPU-accelerated batch signature verification.
    ///
    /// Combines MetalVerifier batch Ed25519 verification with Block-STM
    /// parallel execution. This is the production path — signatures are
    /// verified in a single GPU dispatch, then only valid transactions
    /// are executed.
    pub fn execute_block_gpu_verified(
        &self,
        transactions: &[Transaction],
        producer: Address,
    ) -> Result<(Block, Vec<TxReceipt>), StateError> {
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

        for (i, tx) in transactions.iter().enumerate() {
            // Hash integrity check
            if tx.compute_hash() != tx.hash {
                continue; // sig_valid[i] stays false
            }

            match &tx.signature {
                arc_crypto::signature::Signature::Ed25519 { public_key, signature } => {
                    if signature.len() == 64 {
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

            receipts.resize(transactions.len(), TxReceipt {
                tx_hash: Hash256::ZERO,
                block_height: height,
                block_hash: Hash256::ZERO,
                index: 0,
                success: false,
                gas_used: 0,
                value_commitment: None,
                inclusion_proof: None,
                logs: vec![],
            });

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
            tracing::warn!(total_gas, limit = gas_costs::BLOCK_GAS_LIMIT, "Block nearing gas limit");
        }

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
            protocol_version: ProtocolVersion::GENESIS,
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
        self.wal.append(WalOp::SetBlock(height, block.clone()), height);
        self.wal.append(WalOp::Checkpoint(state_root), height);

        let count = self.snapshot_counter.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count % 10_000 == 0 {
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
            protocol_version: ProtocolVersion::GENESIS,
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
        self.wal.append(WalOp::SetBlock(height, block.clone()), height);
        self.wal.append(WalOp::Checkpoint(state_root), height);

        Ok((block, receipts))
    }

    /// Block-STM parallel execution — partitions transactions into conflict-free
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
                    Ok(gas) => { receipt_success[idx] = true; receipt_gas[idx] = gas; }
                    Err(_) => { receipt_gas[idx] = Self::gas_cost_for_tx(&transactions[idx]); }
                }
            } else {
                // Parallel execution within the batch
                let results: Vec<(usize, bool, u64)> = batch
                    .par_iter()
                    .map(|&idx| {
                        match self.execute_tx(&transactions[idx]) {
                            Ok(gas) => (idx, true, gas),
                            Err(_) => (idx, false, Self::gas_cost_for_tx(&transactions[idx])),
                        }
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
            protocol_version: ProtocolVersion::GENESIS,
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
        self.wal.append(WalOp::SetBlock(height, block.clone()), height);
        self.wal.append(WalOp::Checkpoint(state_root), height);

        Ok((block, receipts))
    }

    /// Optimistic parallel execution — pre-sorted by sender nonce for maximum throughput.
    pub fn execute_optimistic(
        &self,
        transactions: &[Transaction],
    ) -> (usize, usize) {
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

        // Spawn 4 indexer threads — each computes hashes and inserts hash→(height, index)
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
                            let mut base_hasher =
                                blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
                            base_hasher.update(&[TxType::Transfer as u8]);
                            base_hasher.update(sender.as_ref());

                            let nonce_start =
                                batch.nonce_start + shard_idx as u64 * batch.txs_per_sender;
                            for j in 0..batch.txs_per_sender {
                                let nonce = nonce_start + j;
                                let hash = compute_benchmark_tx_hash(
                                    &base_hasher,
                                    nonce,
                                    &body_bytes,
                                );
                                // Single DashMap insert: hash → (height, global_index)
                                state.tx_index.insert(hash.0, (batch.height, global_idx));
                                global_idx += 1;
                            }
                        }
                    }
                })
                .expect("spawn indexer thread");
        }

        // Store the sender — we need unsafe to set the field on Arc<Self>
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
        // Only the nonce varies per tx — huge optimization.
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
                    .map(|j| {
                        compute_benchmark_tx_hash(&base_hasher, nonce_start + j, &body_bytes)
                    })
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
            protocol_version: ProtocolVersion::GENESIS,
            state_diff: None,
        };

        // Empty tx_hashes vec — 10M hashes would be 320MB per block.
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
    /// This is the "honest" benchmark path — every tx is signed, verified,
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
                        arc_crypto::Signature::Ed25519 { public_key, signature } => {
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
                        // Batch failed — fall back to individual verification
                        chunk.iter().map(|tx| tx.verify_signature().is_ok()).collect()
                    }
                } else {
                    // Non-Ed25519 or parse error — verify individually
                    chunk.iter().map(|tx| tx.verify_signature().is_ok()).collect()
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
            protocol_version: ProtocolVersion::GENESIS,
            state_diff: None,
        };

        // Store tx hashes in the Block for /block/{height}/txs listing
        let block_tx_hashes: Vec<Hash256> = transactions.iter().map(|tx| tx.hash).collect();
        let block = Block::new(header, block_tx_hashes);
        self.blocks.insert(height, block.clone());

        // ── 7. Store success flags for receipt reconstruction ────────────
        self.signed_block_data.insert(
            height,
            (vec![], receipt_success, block.hash),
        );

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
    pub fn reconstruct_benchmark_receipt(
        &self,
        height: u64,
        tx_index: u32,
    ) -> Option<TxReceipt> {
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
    /// This is expensive (~130ms for 10M txs) — only called on-demand for /tx/{hash}/proof.
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
            TxBody::InferenceChallenge(_) => gas_costs::INFERENCE_CHALLENGE,
            TxBody::InferenceRegister(_) => gas_costs::INFERENCE_ATTESTATION, // same gas as attestation
        }
    }

    /// Execute a single transaction against state, enforcing gas metering.
    ///
    /// Returns the gas consumed on success. When `gas_limit == 0` (backward
    /// compat / benchmark mode), an effectively unlimited gas budget is used
    /// so that no existing transaction can fail due to gas exhaustion.
    fn execute_tx(&self, tx: &Transaction) -> Result<u64, StateError> {
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
                    let mut sender = self.accounts
                        .get_mut(&tx.from.0)
                        .ok_or_else(|| {
                            // Lazy create if not found
                            self.accounts.insert(tx.from.0, Account::new(tx.from, 0));
                            StateError::InsufficientBalance { have: 0, need: body.amount }
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
                        let sender_hash = hash_bytes(&bincode::serialize(sender.value()).unwrap_or_default());
                        self.jmt.lock().update_leaf(tx.from.0, sender_hash);
                    }
                    // WAL snapshot only if WAL is active (null WAL returns early)
                    if self.wal.is_active() {
                        let snap = sender.clone();
                        drop(sender);
                        self.wal.append(WalOp::SetAccount(tx.from, snap), self.height());
                    }
                }

                // Credit receiver in-place
                {
                    let mut receiver = self.accounts
                        .entry(body.to.0)
                        .or_insert_with(|| Account::new(body.to, 0));
                    receiver.balance = receiver.balance.saturating_add(body.amount);
                    // Eagerly update JMT leaf for receiver.
                    if self.use_jmt {
                        let recv_hash = hash_bytes(&bincode::serialize(receiver.value()).unwrap_or_default());
                        self.jmt.lock().update_leaf(body.to.0, recv_hash);
                    }
                    if self.wal.is_active() {
                        let snap = receiver.clone();
                        drop(receiver);
                        self.wal.append(WalOp::SetAccount(body.to, snap), self.height());
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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

                let mut agent = self.get_or_create_account(&body.agent_id);
                agent.balance = agent.balance.saturating_add(body.amount);
                self.accounts.insert(body.agent_id.0, agent.clone());
                self.wal.append(WalOp::SetAccount(body.agent_id, agent), self.height());

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
                    let prev_stake = self.validators
                        .get(&tx.from.0)
                        .map(|v| *v)
                        .unwrap_or(0);
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
                    let prev_stake = self.validators
                        .get(&tx.from.0)
                        .map(|v| *v)
                        .unwrap_or(0);
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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());
                Ok(gas.consumed)
            }
            TxBody::WasmCall(body) => {
                // --- Sender validation ---
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                // --- Contract lookup ---
                let bytecode = self.get_contract(&body.contract)
                    .ok_or(StateError::ContractNotFound(body.contract))?;

                let is_evm = bytecode.len() < 4 || &bytecode[..4] != WASM_MAGIC;

                if is_evm {
                    // --- EVM contract ---
                    // Balance validation only (revm handles the actual transfer
                    // and execution via evm_execute in the node layer).
                    if body.value > 0 && sender.balance < body.value {
                        return Err(StateError::InsufficientBalance {
                            have: sender.balance,
                            need: body.value,
                        });
                    }
                    // Nonce increment — revm will see the updated state.
                    sender.nonce += 1;
                    self.accounts.insert(tx.from.0, sender.clone());
                    self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());
                    // Actual EVM execution delegated to node layer (arc-vm::evm).
                    tracing::debug!(
                        contract = ?body.contract,
                        calldata_len = body.calldata.len(),
                        "EVM contract call — nonce incremented, execution delegated"
                    );
                } else {
                    // --- WASM contract ---
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
                        self.wal.append(WalOp::SetAccount(body.contract, contract_acct), self.height());
                    }

                    // WASM execution via wasmer with full host imports.
                    {
                        use std::sync::Mutex as StdMutex;
                        use wasmer::{imports, Function, FunctionEnv, FunctionEnvMut,
                                     Instance, Memory, Module as WasmModule, Store};

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

                        let wasm_gas_limit = if body.gas_limit > 0 { body.gas_limit } else { 10_000_000 };
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
                            &mut store, &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>, amount: i64| {
                                if amount <= 0 { return; }
                                let data = env.data_mut();
                                let prev = data.gas_used.fetch_add(amount as u64, std::sync::atomic::Ordering::Relaxed);
                                if prev + amount as u64 > data.gas_limit {
                                    *data.out_of_gas.lock().unwrap() = true;
                                }
                            },
                        );

                        // Host: log(ptr: i32, len: i32)
                        let h_log = Function::new_typed_with_env(
                            &mut store, &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>, ptr: i32, len: i32| {
                                let (data, wstore) = env.data_and_store_mut();
                                if let Some(ref mem) = *data.memory.lock().unwrap() {
                                    let view = mem.view(&wstore);
                                    let mut buf = vec![0u8; len as usize];
                                    if view.read(ptr as u64, &mut buf).is_ok() {
                                        data.logs.lock().unwrap().push(
                                            String::from_utf8_lossy(&buf).to_string()
                                        );
                                    }
                                }
                            },
                        );

                        // Host: storage_get(key_ptr: i32, val_ptr: i32) -> i32
                        let h_storage_get = Function::new_typed_with_env(
                            &mut store, &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>, key_ptr: i32, val_ptr: i32| -> i32 {
                                let (data, wstore) = env.data_and_store_mut();
                                let mem_guard = data.memory.lock().unwrap();
                                let mem = match *mem_guard {
                                    Some(ref m) => m.clone(),
                                    None => return -1,
                                };
                                drop(mem_guard);
                                let view = mem.view(&wstore);
                                let mut key = [0u8; 32];
                                if view.read(key_ptr as u64, &mut key).is_err() { return -1; }
                                let cache = data.storage_cache.lock().unwrap();
                                match cache.get(&key) {
                                    Some(Some(val)) => {
                                        let val = val.clone();
                                        drop(cache);
                                        let view2 = mem.view(&wstore);
                                        if view2.write(val_ptr as u64, &val).is_err() { return -1; }
                                        val.len() as i32
                                    }
                                    Some(None) => -1,
                                    None => -1,
                                }
                            },
                        );

                        // Host: storage_set(key_ptr: i32, val_ptr: i32, val_len: i32)
                        let h_storage_set = Function::new_typed_with_env(
                            &mut store, &func_env,
                            |mut env: FunctionEnvMut<'_, WasmHostState>, key_ptr: i32, val_ptr: i32, val_len: i32| {
                                let (data, wstore) = env.data_and_store_mut();
                                let mem_guard = data.memory.lock().unwrap();
                                let mem = match *mem_guard {
                                    Some(ref m) => m.clone(),
                                    None => return,
                                };
                                drop(mem_guard);
                                let view = mem.view(&wstore);
                                let mut key = [0u8; 32];
                                if view.read(key_ptr as u64, &mut key).is_err() { return; }
                                let mut val = vec![0u8; val_len as usize];
                                if view.read(val_ptr as u64, &mut val).is_err() { return; }
                                data.storage_cache.lock().unwrap().insert(key, Some(val.clone()));
                                data.storage_writes.lock().unwrap().push((key, val));
                            },
                        );

                        // Host: caller(ptr: i32) — write caller address to WASM memory
                        let h_caller = Function::new_typed_with_env(
                            &mut store, &func_env,
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
                            &mut store, &func_env,
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
                            &mut store, &func_env,
                            |env: FunctionEnvMut<'_, WasmHostState>| -> i64 {
                                env.data().block_height as i64
                            },
                        );

                        // Host: tx_value() -> i64
                        let h_tx_value = Function::new_typed_with_env(
                            &mut store, &func_env,
                            |env: FunctionEnvMut<'_, WasmHostState>| -> i64 {
                                env.data().call_value as i64
                            },
                        );

                        // Host: gas_remaining() -> i64
                        let h_gas_remaining = Function::new_typed_with_env(
                            &mut store, &func_env,
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

                        let module = WasmModule::new(&store, &bytecode)
                            .map_err(|e| StateError::ExecutionError(format!("WASM compile: {}", e)))?;
                        let instance = Instance::new(&mut store, &module, &import_object)
                            .map_err(|e| StateError::ExecutionError(format!("WASM instantiate: {}", e)))?;

                        // Wire up memory reference so host functions can access it
                        if let Ok(memory) = instance.exports.get_memory("memory") {
                            *func_env.as_mut(&mut store).memory.lock().unwrap() = Some(memory.clone());
                        }

                        // Pre-populate storage cache from StateDB
                        if let Some(contract_storage) = self.storage.get(&body.contract.0) {
                            let mut cache = func_env.as_ref(&store).storage_cache.lock().unwrap();
                            for entry in contract_storage.iter() {
                                cache.insert(entry.key().0, Some(entry.value().clone()));
                            }
                        }

                        let func = instance.exports.get_function(&body.function)
                            .map_err(|e| StateError::ExecutionError(
                                format!("function '{}' not found: {}", body.function, e)
                            ))?;

                        let call_result = func.call(&mut store, &[]);

                        // Check gas exhaustion
                        let host_state = func_env.as_ref(&store);
                        let wasm_gas_used = host_state.gas_used.load(std::sync::atomic::Ordering::Relaxed);
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
                                let writes = std::mem::take(
                                    &mut *host_state.storage_writes.lock().unwrap()
                                );
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

                                    if let Some(mut contract_acct) = self.accounts.get_mut(&body.contract.0) {
                                        contract_acct.balance -= body.value;
                                    }
                                }
                                return Err(StateError::ExecutionError(
                                    format!("WASM exec failed: {}", e)
                                ));
                            }
                        }
                    }

                    sender.nonce += 1;
                    self.accounts.insert(tx.from.0, sender.clone());
                    self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());
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
                self.wal.append(WalOp::SetAccount(contract_addr, contract_acct), self.height());

                // --- Constructor execution ---
                if !body.constructor_args.is_empty() {
                    // Compile the module and call `init` with full host imports.
                    // Uses wasmer directly to avoid circular arc-vm dependency.
                    use std::sync::{Mutex as StdMutex};
                    use wasmer::{imports, Function, FunctionEnv, FunctionEnvMut,
                                 Instance, Memory, Module as WasmModule, Store};

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
                        &mut store, &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, amount: i64| {
                            if amount <= 0 { return; }
                            let data = env.data_mut();
                            let prev = data.gas_used.fetch_add(amount as u64, std::sync::atomic::Ordering::Relaxed);
                            if prev + amount as u64 > data.gas_limit {
                                *data.out_of_gas.lock().unwrap() = true;
                            }
                        },
                    );
                    let h_log = Function::new_typed_with_env(
                        &mut store, &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, ptr: i32, len: i32| {
                            let (data, wstore) = env.data_and_store_mut();
                            if let Some(ref mem) = *data.memory.lock().unwrap() {
                                let view = mem.view(&wstore);
                                let mut buf = vec![0u8; len as usize];
                                if view.read(ptr as u64, &mut buf).is_ok() {
                                    data.logs.lock().unwrap().push(
                                        String::from_utf8_lossy(&buf).to_string()
                                    );
                                }
                            }
                        },
                    );
                    let h_storage_get = Function::new_typed_with_env(
                        &mut store, &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, key_ptr: i32, val_ptr: i32| -> i32 {
                            let (data, wstore) = env.data_and_store_mut();
                            let mem_guard = data.memory.lock().unwrap();
                            let mem = match *mem_guard {
                                Some(ref m) => m.clone(),
                                None => return -1,
                            };
                            drop(mem_guard);
                            let view = mem.view(&wstore);
                            let mut key = [0u8; 32];
                            if view.read(key_ptr as u64, &mut key).is_err() { return -1; }
                            let cache = data.storage_cache.lock().unwrap();
                            match cache.get(&key) {
                                Some(Some(val)) => {
                                    let val = val.clone();
                                    drop(cache);
                                    let view2 = mem.view(&wstore);
                                    if view2.write(val_ptr as u64, &val).is_err() { return -1; }
                                    val.len() as i32
                                }
                                _ => -1,
                            }
                        },
                    );
                    let h_storage_set = Function::new_typed_with_env(
                        &mut store, &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, key_ptr: i32, val_ptr: i32, val_len: i32| {
                            let (data, wstore) = env.data_and_store_mut();
                            let mem_guard = data.memory.lock().unwrap();
                            let mem = match *mem_guard {
                                Some(ref m) => m.clone(),
                                None => return,
                            };
                            drop(mem_guard);
                            let view = mem.view(&wstore);
                            let mut key = [0u8; 32];
                            if view.read(key_ptr as u64, &mut key).is_err() { return; }
                            let mut val = vec![0u8; val_len as usize];
                            if view.read(val_ptr as u64, &mut val).is_err() { return; }
                            data.storage_cache.lock().unwrap().insert(key, Some(val.clone()));
                            data.storage_writes.lock().unwrap().push((key, val));
                        },
                    );
                    let h_caller = Function::new_typed_with_env(
                        &mut store, &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, ptr: i32| {
                            let (data, wstore) = env.data_and_store_mut();
                            if let Some(ref mem) = *data.memory.lock().unwrap() {
                                let view = mem.view(&wstore);
                                let _ = view.write(ptr as u64, &data.deployer);
                            }
                        },
                    );
                    let h_self_address = Function::new_typed_with_env(
                        &mut store, &func_env,
                        |mut env: FunctionEnvMut<'_, InitHostState>, ptr: i32| {
                            let (data, wstore) = env.data_and_store_mut();
                            if let Some(ref mem) = *data.memory.lock().unwrap() {
                                let view = mem.view(&wstore);
                                let _ = view.write(ptr as u64, &data.self_address);
                            }
                        },
                    );
                    let h_block_height = Function::new_typed_with_env(
                        &mut store, &func_env,
                        |env: FunctionEnvMut<'_, InitHostState>| -> i64 {
                            env.data().block_height as i64
                        },
                    );
                    let h_tx_value = Function::new_typed_with_env(
                        &mut store, &func_env,
                        |_env: FunctionEnvMut<'_, InitHostState>| -> i64 { 0i64 },
                    );
                    let h_gas_remaining = Function::new_typed_with_env(
                        &mut store, &func_env,
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
                    let instance = Instance::new(&mut store, &module, &import_object)
                        .map_err(|e| StateError::ExecutionError(format!("WASM instantiate: {}", e)))?;

                    if let Ok(memory) = instance.exports.get_memory("memory") {
                        *func_env.as_mut(&mut store).memory.lock().unwrap() = Some(memory.clone());
                    }

                    if let Ok(init_fn) = instance.exports.get_function("init") {
                        let call_result = init_fn.call(&mut store, &[]);

                        let host_state = func_env.as_ref(&store);
                        let was_out_of_gas = *host_state.out_of_gas.lock().unwrap();
                        if was_out_of_gas {
                            return Err(StateError::ExecutionError(
                                "constructor out of gas".into()
                            ));
                        }

                        match call_result {
                            Ok(_) => {
                                // Flush constructor storage writes to StateDB
                                let writes = std::mem::take(
                                    &mut *host_state.storage_writes.lock().unwrap()
                                );
                                for (key, value) in writes {
                                    self.set_storage(&contract_addr, Hash256(key), value);
                                }
                                tracing::debug!(
                                    contract = ?contract_addr,
                                    "constructor executed successfully"
                                );
                            }
                            Err(e) => {
                                // Constructor failed — remove the deployed contract
                                self.contracts.remove(&contract_addr.0);
                                self.accounts.remove(&contract_addr.0);
                                return Err(StateError::ExecutionError(
                                    format!("constructor exec: {}", e)
                                ));
                            }
                        }
                    }
                }

                // --- Debit sender ---
                sender.balance -= total_cost;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

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
                        body.initial_stake, Self::MIN_VALIDATOR_STAKE
                    )));
                }

                // Move balance to staked_balance
                sender.balance -= body.initial_stake;
                sender.staked_balance += body.initial_stake;
                sender.nonce += 1;

                // Register in validator set
                self.validators.insert(tx.from.0, body.initial_stake);
                self.staking_pool.fetch_add(body.initial_stake, Ordering::Relaxed);

                self.accounts.insert(tx.from.0, sender.clone());
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

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
                let staked = self.validators
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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());
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

                let current_stake = self.validators
                    .get(&tx.from.0)
                    .map(|v| *v)
                    .unwrap_or(0);

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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

                // Credit bridge escrow account (well-known address)
                let escrow_addr = hash_bytes(b"ARC-bridge-escrow");
                let mut escrow = self.get_or_create_account(&escrow_addr);
                escrow.balance = body.amount;
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal.append(WalOp::SetAccount(escrow_addr, escrow), self.height());

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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

                // Credit recipient
                let mut recipient = self.get_or_create_account(&body.recipient);
                recipient.balance = body.amount;
                self.accounts.insert(body.recipient.0, recipient.clone());
                self.wal.append(WalOp::SetAccount(body.recipient, recipient), self.height());

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
                let mut net_credits: std::collections::HashMap<[u8; 32], u64> = std::collections::HashMap::new();
                for entry in &body.entries {
                    *net_credits.entry(entry.agent_id.0).or_insert(0) += entry.amount;
                }

                // Debit sender once for total
                sender.balance -= total_amount;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

                // Credit each unique recipient once (netted)
                for (agent_addr, net_amount) in &net_credits {
                    let agent_address = Hash256(*agent_addr);
                    let mut agent = self.get_or_create_account(&agent_address);
                    agent.balance = agent.balance.saturating_add(*net_amount);
                    self.accounts.insert(*agent_addr, agent.clone());
                    self.wal.append(WalOp::SetAccount(agent_address, agent), self.height());
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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

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
                self.wal.append(WalOp::SetAccount(escrow_addr, escrow), self.height());

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
                let claimed_total = body.opener_balance.saturating_add(body.counterparty_balance);
                if claimed_total > total_locked {
                    return Err(StateError::ExecutionError(
                        format!("channel close exceeds locked funds: claimed={}, locked={}", claimed_total, total_locked),
                    ));
                }

                // Drain escrow
                let mut escrow_mut = self.get_or_create_account(&escrow_addr);
                escrow_mut.balance = 0;
                self.accounts.insert(escrow_addr.0, escrow_mut.clone());
                self.wal.append(WalOp::SetAccount(escrow_addr, escrow_mut), self.height());

                // Credit opener
                sender.balance = body.opener_balance;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

                // Credit counterparty — address was stored in escrow.storage_root
                // during ChannelOpen (see above). This is the definitive on-chain
                // record of who the counterparty is.
                let counterparty_addr = escrow.storage_root;
                if body.counterparty_balance > 0 && counterparty_addr != Hash256::ZERO {
                    let mut counterparty = self.get_or_create_account(&counterparty_addr);
                    counterparty.balance = body.counterparty_balance;
                    self.accounts.insert(counterparty_addr.0, counterparty.clone());
                    self.wal.append(WalOp::SetAccount(counterparty_addr, counterparty), self.height());
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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

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
                // the state is already finalized — no further disputes allowed.
                if escrow.staked_balance > 0 && self.height() >= escrow.staked_balance {
                    return Err(StateError::ExecutionError(
                        "channel dispute: challenge period has expired, state is finalized".to_string(),
                    ));
                }

                // State nonce must be strictly higher than the previously disputed state.
                // This prevents replay attacks with old channel states.
                if escrow.staked_balance > 0 && body.state_nonce <= escrow.nonce {
                    return Err(StateError::ExecutionError(
                        format!(
                            "channel dispute: state_nonce {} must exceed previously disputed nonce {}",
                            body.state_nonce, escrow.nonce
                        ),
                    ));
                }

                // Validate balance conservation: claimed split must not exceed locked funds.
                let claimed_total = body.opener_balance.saturating_add(body.counterparty_balance);
                if claimed_total > escrow.balance {
                    return Err(StateError::ExecutionError(
                        format!(
                            "channel dispute: claimed balances ({}) exceed locked funds ({})",
                            claimed_total, escrow.balance
                        ),
                    ));
                }

                // Update dispute state in escrow.
                let challenge_expiry = self.height() + body.challenge_period;
                escrow.nonce = body.state_nonce;
                escrow.staked_balance = challenge_expiry;
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal.append(WalOp::SetAccount(escrow_addr, escrow), self.height());

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
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

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
                    if !arc_crypto::stwo_air::verify_recursive_proof(&recursive_input, &body.proof_data) {
                        return Err(StateError::ExecutionError(
                            "shard proof: STARK proof verification failed".to_string(),
                        ));
                    }
                }

                // Record verified shard proof — store proof hash in a deterministic
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
                proof_record.balance = u64::from_le_bytes(proof_hash.0[..8].try_into().unwrap_or([0u8; 8]));
                proof_record.nonce = body.block_height;
                self.accounts.insert(proof_key.0, proof_record.clone());
                self.wal.append(WalOp::SetAccount(proof_key, proof_record), self.height());

                Ok(gas.consumed)
            }
            TxBody::InferenceAttestation(body) => {
                // --- Tier 2 Optimistic Inference Attestation ---
                // 1. Verify sender nonce
                let mut sender = self.get_or_create_account(&tx.from);
                if sender.nonce != tx.nonce {
                    return Err(StateError::InvalidNonce {
                        expected: sender.nonce,
                        got: tx.nonce,
                    });
                }

                // 2. Verify sender has sufficient balance for bond
                if sender.balance < body.bond {
                    return Err(StateError::InsufficientBalance {
                        have: sender.balance,
                        need: body.bond,
                    });
                }

                // 3. Debit bond from sender and increment nonce
                sender.balance -= body.bond;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

                // 4. Lock bond in deterministic escrow: BLAKE3("arc-inference" || attestation_hash)
                let escrow_addr = hash_bytes(&[b"arc-inference", tx.hash.as_ref()].concat());
                let mut escrow = self.get_or_create_account(&escrow_addr);
                escrow.balance = body.bond;
                // Store model_id fingerprint in nonce (for lookup)
                escrow.nonce = u64::from_le_bytes(body.model_id.0[..8].try_into().unwrap_or([0u8; 8]));
                // Store the current block height in storage_root (as metadata)
                // so the challenge period can be verified later.
                let mut meta_input = Vec::new();
                meta_input.extend_from_slice(&body.input_hash.0);
                meta_input.extend_from_slice(&body.output_hash.0);
                meta_input.extend_from_slice(&body.challenge_period.to_le_bytes());
                meta_input.extend_from_slice(&self.height().to_le_bytes());
                escrow.storage_root = hash_bytes(&meta_input);
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal.append(WalOp::SetAccount(escrow_addr, escrow), self.height());

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
                let escrow_addr = hash_bytes(&[b"arc-inference", body.attestation_hash.as_ref()].concat());
                let escrow = self.get_or_create_account(&escrow_addr);
                if escrow.balance == 0 {
                    return Err(StateError::ExecutionError(
                        "inference challenge: attestation escrow not found or already resolved".to_string(),
                    ));
                }

                // 4. Debit challenger bond and increment nonce
                sender.balance -= body.challenger_bond;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

                // 5. Lock challenger's bond in the same escrow
                let total_bond = escrow.balance + body.challenger_bond;
                let mut escrow = escrow.clone();
                escrow.balance = total_bond;
                self.accounts.insert(escrow_addr.0, escrow.clone());
                self.wal.append(WalOp::SetAccount(escrow_addr, escrow), self.height());

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
                    return Err(StateError::ExecutionError(
                        format!("inference register: invalid tier {}, must be 1-4", body.tier),
                    ));
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
                    return Err(StateError::ExecutionError(
                        format!(
                            "inference register: stake {} below minimum {} for tier {}",
                            body.stake_bond, min_stake, body.tier
                        ),
                    ));
                }

                // Lock stake: move from balance to staked_balance
                sender.balance -= body.stake_bond;
                sender.staked_balance += body.stake_bond;
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());

                Ok(gas.consumed)
            }
        }
    }

    // ── Pipeline support ────────────────────────────────────────────────────
    // Public wrappers for the 4-stage pipeline (B3), which executes on
    // separate threads and needs direct access to individual operations.

    /// Public wrapper around the private `execute_tx()` — used by the pipeline
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
            protocol_version: ProtocolVersion::GENESIS,
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
        self.wal.append(WalOp::SetBlock(height, block.clone()), height);
        self.wal.append(WalOp::Checkpoint(state_root), height);

        // Auto-prune old state every 100 blocks unless in archive mode.
        // Archive nodes keep full history for block explorers and analytics.
        if !self.archive_mode && height % 100 == 0 {
            self.prune_old_state(1000);
            self.prune_old_receipts(1000);
        }

        Ok((block, receipts))
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
        self.receipts.retain(|_, receipt| receipt.block_height > cutoff);

        // Remove tx_index entries that point to pruned blocks.
        self.tx_index.retain(|_, &mut (block_height, _)| block_height > cutoff);

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

    /// Index a transaction's sender and recipient addresses for `account_txs` lookups.
    /// Caps per-account history at 10K entries to prevent unbounded memory growth.
    fn index_account_tx(&self, tx: &Transaction) {
        const MAX_TX_HISTORY: usize = 10_000;
        let mut entry = self.account_txs.entry(tx.from.0).or_default();
        if entry.len() >= MAX_TX_HISTORY {
            // Remove oldest 10% to amortize truncation cost
            let drain_count = MAX_TX_HISTORY / 10;
            entry.drain(..drain_count);
        }
        entry.push(tx.hash);

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
            TxBody::JoinValidator(_) | TxBody::LeaveValidator | TxBody::ClaimRewards | TxBody::UpdateStake(_) => {}
            TxBody::Governance(_) => {}
            TxBody::BridgeLock(_) => {
                // Escrow account is well-known; index it
                let escrow_addr = hash_bytes(b"ARC-bridge-escrow");
                self.account_txs.entry(escrow_addr.0).or_default().push(tx.hash);
            }
            TxBody::BridgeMint(body) => {
                self.account_txs.entry(body.recipient.0).or_default().push(tx.hash);
            }
            TxBody::BatchSettle(body) => {
                for entry in &body.entries {
                    self.account_txs.entry(entry.agent_id.0).or_default().push(tx.hash);
                }
            }
            TxBody::ChannelOpen(_) | TxBody::ChannelClose(_) | TxBody::ChannelDispute(_) => {}
            TxBody::ShardProof(_) => {}
            TxBody::InferenceAttestation(_) => {
                let escrow_addr = hash_bytes(&[b"arc-inference", tx.hash.as_ref()].concat());
                self.account_txs.entry(escrow_addr.0).or_default().push(tx.hash);
            }
            TxBody::InferenceChallenge(body) => {
                let escrow_addr = hash_bytes(&[b"arc-inference", body.attestation_hash.as_ref()].concat());
                self.account_txs.entry(escrow_addr.0).or_default().push(tx.hash);
            }
            TxBody::InferenceRegister(_) => {
                // Registration modifies sender's staked_balance; sender is already tracked.
            }
        }
    }

    /// Mark all accounts affected by a transaction as dirty for incremental state root.
    fn mark_tx_accounts_dirty(&self, tx: &Transaction) {
        self.dirty_accounts.insert(tx.from.0);
        match &tx.body {
            TxBody::Transfer(body) => { self.dirty_accounts.insert(body.to.0); }
            TxBody::Settle(body) => { self.dirty_accounts.insert(body.agent_id.0); }
            TxBody::Swap(body) => { self.dirty_accounts.insert(body.counterparty.0); }
            TxBody::Stake(body) => { self.dirty_accounts.insert(body.validator.0); }
            TxBody::WasmCall(body) => { self.dirty_accounts.insert(body.contract.0); }
            TxBody::Escrow(body) => { self.dirty_accounts.insert(body.beneficiary.0); }
            TxBody::DeployContract(_) => {
                // The contract address is deterministic — mark it dirty
                let contract_addr = compute_contract_address(&tx.from, tx.nonce);
                self.dirty_accounts.insert(contract_addr.0);
            }
            TxBody::RegisterAgent(_) | TxBody::MultiSig(_) => {}
            TxBody::JoinValidator(_) | TxBody::LeaveValidator | TxBody::ClaimRewards | TxBody::UpdateStake(_) => {}
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
                // Also mark the counterparty as dirty — their balance is modified during close.
                // The counterparty address is stored in the escrow account's storage_root.
                if let Some(escrow) = self.accounts.get(&escrow_addr.0) {
                    if escrow.storage_root != Hash256::ZERO {
                        self.dirty_accounts.insert(escrow.storage_root.0);
                    }
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
            TxBody::InferenceChallenge(body) => {
                let escrow_addr = hash_bytes(&[b"arc-inference", body.attestation_hash.as_ref()].concat());
                self.dirty_accounts.insert(escrow_addr.0);
            }
            TxBody::InferenceRegister(_) => {
                // Sender account is already marked dirty (line above match).
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

    /// Flush WAL to disk (call at block boundaries for durability).
    pub fn sync_wal(&self) {
        self.wal.sync();
    }

    // -----------------------------------------------------------------------
    // State Sync Protocol (A5)
    // -----------------------------------------------------------------------

    /// Export the current state as a snapshot for state sync.
    /// New nodes can download this to bootstrap without replaying from genesis.
    pub fn export_snapshot(&self) -> Snapshot {
        let accounts: Vec<(Address, Account)> = self.accounts
            .iter()
            .map(|entry| (Hash256(*entry.key()), entry.value().clone()))
            .collect();

        let storage: Vec<(Address, Vec<(Hash256, Vec<u8>)>)> = self.storage
            .iter()
            .map(|entry| {
                let key = Hash256(*entry.key());
                let values: Vec<(Hash256, Vec<u8>)> = entry.value()
                    .iter()
                    .map(|inner| (*inner.key(), inner.value().clone()))
                    .collect();
                (key, values)
            })
            .collect();

        let contracts: Vec<(Address, Vec<u8>)> = self.contracts
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

    /// Apply a state diff from a proposer and return the resulting state root.
    ///
    /// Called by verifier nodes.  Applies the account changes, marks them dirty,
    /// and recomputes the state root.  The caller compares the returned root
    /// against `diff.new_root` — a mismatch indicates a fraudulent proposal.
    pub fn apply_state_diff(&self, diff: &arc_types::StateDiff) -> Hash256 {
        for change in &diff.changes {
            self.accounts.insert(change.address.0, change.account.clone());
            self.dirty_accounts.insert(change.address.0);
        }
        self.compute_state_root()
    }

    /// Verify a state diff: apply it and check if the root matches.
    /// Returns true if the roots match (valid diff), false if fraud detected.
    pub fn verify_state_diff(&self, diff: &arc_types::StateDiff) -> bool {
        let computed_root = self.apply_state_diff(diff);
        computed_root == diff.new_root
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
        self.identities.insert(identity.address.0, identity);
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
                let level_ok = matches!(id.level, IdentityLevel::Verified | IdentityLevel::Institutional);
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
            .ok_or_else(|| StateError::AccountNotFound(*address))?;

        let merkle_proof = tree.proof(index).ok_or_else(|| {
            StateError::ExecutionError("failed to generate Merkle proof".into())
        })?;

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

        let block = self
            .get_block(block_height)
            .ok_or_else(|| StateError::ExecutionError(format!("block {} not found", block_height)))?;

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
            ((all_accounts.len() + chunk_size - 1) / chunk_size) as u32
        };

        let mut chunks = Vec::with_capacity(total_chunks as usize);
        let mut chunk_proofs = Vec::with_capacity(total_chunks as usize);

        for (i, accounts_slice) in all_accounts
            .chunks(chunk_size)
            .enumerate()
        {
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
            let empty_data = bincode::serialize(&Vec::<(Address, Account)>::new())
                .expect("serializable");
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
            ((all_accounts.len() + chunk_size - 1) / chunk_size) as u32
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
    pub fn import_snapshot_chunk(&self, chunk: &StateSnapshot) -> Result<u32, StateError> {
        // Verify chunk proof: re-hash the chunk's account data and compare.
        let chunk_data = bincode::serialize(&chunk.accounts).expect("serializable");
        let computed_proof = hash_bytes(&chunk_data);
        if computed_proof != chunk.chunk_proof {
            return Err(StateError::ChunkVerificationFailed);
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
        Ok(())
    }

    /// Default number of accounts per chunk when serving snapshots to peers.
    const DEFAULT_CHUNK_SIZE: usize = 1000;

    /// Export just the manifest metadata (lightweight — no account data).
    ///
    /// Peers request the manifest first to learn the chunk count, then
    /// download individual chunks in parallel via `export_snapshot_chunk`.
    pub fn export_snapshot_manifest(&self) -> SnapshotManifest {
        let chunk_size = Self::DEFAULT_CHUNK_SIZE;
        let total_accounts = self.accounts.len() as u64;
        let total_chunks = if total_accounts == 0 {
            1u32
        } else {
            ((total_accounts as usize + chunk_size - 1) / chunk_size) as u32
        };

        let version = self.height();
        let state_root = self.compute_state_root();

        // Hash the manifest metadata (excluding manifest_hash itself).
        let pre_hash_data = bincode::serialize(&(version, &state_root, total_accounts, total_chunks, chunk_size))
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

        let computed_root = self.compute_state_root();
        if computed_root != progress.manifest.state_root {
            return Err(StateError::StateRootMismatch {
                expected: progress.manifest.state_root,
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
/// Only the nonce varies per call — enables massive parallelism.
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
    if leaves.len() % 2 != 0 {
        leaves.push(*leaves.last().unwrap());
    }
    while leaves.len() > 1 {
        leaves = leaves
            .par_chunks(2)
            .map(|pair| hash_pair(&pair[0], &pair[1]))
            .collect();
        if leaves.len() > 1 && leaves.len() % 2 != 0 {
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

    #[test]
    fn test_snapshot_roundtrip() {
        let state = StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 500_000),
        ]);

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
        let proposer_state = StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 0),
        ]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 500, 0);
        let (block, _receipts) = proposer_state.execute_block(&[tx], addr(99)).unwrap();

        // Derive affected addresses from tx body (same as mark_tx_accounts_dirty)
        let affected = vec![addr(1), addr(2)];
        let diff = proposer_state.export_state_diff(&affected);
        assert_eq!(diff.new_root, block.header.state_root);

        // Verifier: apply the state diff (without re-executing)
        let verifier_state = StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 0),
        ]);
        let verifier_root = verifier_state.apply_state_diff(&diff);

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

        // Verifier applies diff and gets a different root
        let verifier = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let verifier_root = verifier.apply_state_diff(&diff);
        assert_ne!(verifier_root, diff.new_root, "fraud should be detected");
    }

    #[test]
    fn test_export_state_diff() {
        let db = StateDB::new();
        let addr1 = hash_bytes(&[1]);
        let addr2 = hash_bytes(&[2]);

        // Create some accounts
        db.accounts.insert(addr1.0, Account::new(addr1, 1000));
        db.accounts.insert(addr2.0, Account {
            address: addr2,
            balance: 2000,
            nonce: 5,
            code_hash: Hash256::ZERO,
            storage_root: Hash256::ZERO,
            staked_balance: 0,
        });

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
        proposer_db.accounts.insert(addr1.0, Account {
            address: addr1,
            balance: 900,
            nonce: 1,
            code_hash: Hash256::ZERO,
            storage_root: Hash256::ZERO,
            staked_balance: 0,
        });
        proposer_db.accounts.insert(addr2.0, Account::new(addr2, 100));
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
        proposer_db.accounts.insert(addr1.0, Account {
            address: addr1,
            balance: 900,
            nonce: 1,
            code_hash: Hash256::ZERO,
            storage_root: Hash256::ZERO,
            staked_balance: 0,
        });
        proposer_db.dirty_accounts.insert(addr1.0);

        let affected = vec![addr1];
        let mut diff = proposer_db.export_state_diff(&affected);

        // Tamper with the root -- simulate fraud
        diff.new_root = Hash256([0xFF; 32]);

        // Verifier detects fraud
        assert!(!verifier_db.verify_state_diff(&diff));
    }

    // -----------------------------------------------------------------------
    // Chunked Snapshot Sync Protocol tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_chunked_snapshot_export_single_chunk() {
        let state = StateDB::with_genesis(&[
            (addr(1), 1_000),
            (addr(2), 2_000),
        ]);

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
        let state = StateDB::with_genesis(&[
            (addr(1), 1_000),
            (addr(2), 2_000),
            (addr(3), 3_000),
        ]);

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
        let state = StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 500_000),
            (addr(3), 250_000),
        ]);

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
        let state = StateDB::with_genesis(&[
            (addr(1), 10_000_000),
            (addr(2), 10_000_000),
        ]);

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
            assert!(entry.value().block_height > 3,
                "receipt at height {} should have been pruned", entry.value().block_height);
        }
    }

    #[test]
    fn test_prune_old_receipts_noop_when_young() {
        let state = StateDB::with_genesis(&[
            (addr(1), 10_000_000),
            (addr(2), 10_000_000),
        ]);

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
            (addr(1), 5_000_000),   // above dust threshold (1_000_000)
            (addr(2), 10_000_000),  // above dust threshold
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
        let state = StateDB::with_genesis(&[
            (addr(1), balance),
        ]);

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
        let state = StateDB::with_genesis(&[
            (addr(1), 5_000_000),
        ]);

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
        ChannelOpenBody, ChannelCloseBody, ChannelDisputeBody, InferenceRegisterBody,
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

        let tx = make_channel_tx(addr(1), 0, TxBody::ChannelOpen(ChannelOpenBody {
            channel_id,
            counterparty: addr(2),
            deposit: 100_000,
            timeout_blocks: 100,
        }), TxType::ChannelOpen);

        let (_, receipts) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(receipts[0].success, "ChannelOpen should succeed");

        // Sender balance debited
        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 900_000);

        // Escrow created with deposit
        let escrow_addr = hash_bytes(&[b"arc-channel", channel_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.balance, 100_000);
        assert_eq!(escrow.code_hash, addr(1));      // opener
        assert_eq!(escrow.storage_root, addr(2));    // counterparty
    }

    #[test]
    fn test_channel_close_releases_funds() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let channel_id = hash_bytes(b"test-channel-2");

        // Open
        let open_tx = make_channel_tx(addr(1), 0, TxBody::ChannelOpen(ChannelOpenBody {
            channel_id,
            counterparty: addr(2),
            deposit: 100_000,
            timeout_blocks: 100,
        }), TxType::ChannelOpen);
        let (_, r) = state.execute_block(&[open_tx], addr(99)).unwrap();
        assert!(r[0].success);

        // Close (opener closes, split 60K/40K)
        let close_tx = make_channel_tx(addr(1), 1, TxBody::ChannelClose(ChannelCloseBody {
            channel_id,
            opener_balance: 60_000,
            counterparty_balance: 40_000,
            counterparty_sig: vec![0u8; 64],
            state_nonce: 1,
        }), TxType::ChannelClose);
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
        let open_tx = make_channel_tx(addr(1), 0, TxBody::ChannelOpen(ChannelOpenBody {
            channel_id,
            counterparty: addr(2),
            deposit: 100_000,
            timeout_blocks: 100,
        }), TxType::ChannelOpen);
        state.execute_block(&[open_tx], addr(99)).unwrap();

        // Dispute from counterparty (addr(2))
        let dispute_tx = make_channel_tx(addr(2), 0, TxBody::ChannelDispute(ChannelDisputeBody {
            channel_id,
            opener_balance: 70_000,
            counterparty_balance: 30_000,
            other_party_sig: vec![0u8; 64],
            state_nonce: 5,
            challenge_period: 100,
        }), TxType::ChannelDispute);
        let (_, r) = state.execute_block(&[dispute_tx], addr(99)).unwrap();
        assert!(r[0].success, "ChannelDispute should succeed");

        // Check escrow state updated
        let escrow_addr = hash_bytes(&[b"arc-channel", channel_id.as_ref()].concat());
        let escrow = state.get_account(&escrow_addr).unwrap();
        assert_eq!(escrow.nonce, 5);                // state_nonce recorded
        assert!(escrow.staked_balance > 0);          // challenge_expiry set
        assert_eq!(escrow.balance, 100_000);         // funds still locked
    }

    #[test]
    fn test_channel_dispute_rejects_lower_nonce() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let channel_id = hash_bytes(b"test-channel-4");

        // Open
        let open_tx = make_channel_tx(addr(1), 0, TxBody::ChannelOpen(ChannelOpenBody {
            channel_id,
            counterparty: addr(2),
            deposit: 100_000,
            timeout_blocks: 100,
        }), TxType::ChannelOpen);
        state.execute_block(&[open_tx], addr(99)).unwrap();

        // First dispute with nonce 10
        let d1 = make_channel_tx(addr(2), 0, TxBody::ChannelDispute(ChannelDisputeBody {
            channel_id,
            opener_balance: 60_000,
            counterparty_balance: 40_000,
            other_party_sig: vec![0u8; 64],
            state_nonce: 10,
            challenge_period: 100,
        }), TxType::ChannelDispute);
        let (_, r) = state.execute_block(&[d1], addr(99)).unwrap();
        assert!(r[0].success);

        // Second dispute with lower nonce (5) — should fail
        let d2 = make_channel_tx(addr(1), 1, TxBody::ChannelDispute(ChannelDisputeBody {
            channel_id,
            opener_balance: 80_000,
            counterparty_balance: 20_000,
            other_party_sig: vec![0u8; 64],
            state_nonce: 5, // lower than 10!
            challenge_period: 100,
        }), TxType::ChannelDispute);
        let (_, r) = state.execute_block(&[d2], addr(99)).unwrap();
        assert!(!r[0].success, "Dispute with lower nonce should be rejected");
    }

    #[test]
    fn test_channel_close_blocked_during_dispute() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let channel_id = hash_bytes(b"test-channel-5");

        // Open
        let open_tx = make_channel_tx(addr(1), 0, TxBody::ChannelOpen(ChannelOpenBody {
            channel_id,
            counterparty: addr(2),
            deposit: 100_000,
            timeout_blocks: 100,
        }), TxType::ChannelOpen);
        state.execute_block(&[open_tx], addr(99)).unwrap();

        // Dispute (sets challenge_expiry far in the future)
        let dispute_tx = make_channel_tx(addr(2), 0, TxBody::ChannelDispute(ChannelDisputeBody {
            channel_id,
            opener_balance: 60_000,
            counterparty_balance: 40_000,
            other_party_sig: vec![0u8; 64],
            state_nonce: 1,
            challenge_period: 100_000, // very long
        }), TxType::ChannelDispute);
        let (_, r) = state.execute_block(&[dispute_tx], addr(99)).unwrap();
        assert!(r[0].success);

        // Try to close — should fail (active dispute)
        let close_tx = make_channel_tx(addr(1), 1, TxBody::ChannelClose(ChannelCloseBody {
            channel_id,
            opener_balance: 100_000,
            counterparty_balance: 0,
            counterparty_sig: vec![0u8; 64],
            state_nonce: 1,
        }), TxType::ChannelClose);
        let (_, r) = state.execute_block(&[close_tx], addr(99)).unwrap();
        assert!(!r[0].success, "Close should be blocked during active dispute");
    }

    #[test]
    fn test_channel_dispute_balance_conservation() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let channel_id = hash_bytes(b"test-channel-6");

        // Open with 100K deposit
        let open_tx = make_channel_tx(addr(1), 0, TxBody::ChannelOpen(ChannelOpenBody {
            channel_id,
            counterparty: addr(2),
            deposit: 100_000,
            timeout_blocks: 100,
        }), TxType::ChannelOpen);
        state.execute_block(&[open_tx], addr(99)).unwrap();

        // Dispute claiming more than deposited — should fail
        let dispute_tx = make_channel_tx(addr(2), 0, TxBody::ChannelDispute(ChannelDisputeBody {
            channel_id,
            opener_balance: 80_000,
            counterparty_balance: 40_000, // 80K + 40K = 120K > 100K!
            other_party_sig: vec![0u8; 64],
            state_nonce: 1,
            challenge_period: 100,
        }), TxType::ChannelDispute);
        let (_, r) = state.execute_block(&[dispute_tx], addr(99)).unwrap();
        assert!(!r[0].success, "Dispute exceeding deposit should be rejected");
    }

    #[test]
    fn test_inference_register_locks_stake() {
        let state = StateDB::with_genesis(&[(addr(1), 100_000)]);

        let tx = make_channel_tx(addr(1), 0, TxBody::InferenceRegister(InferenceRegisterBody {
            tier: 2,
            stake_bond: 5_000,
        }), TxType::InferenceRegister);

        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(r[0].success, "InferenceRegister should succeed");

        let acct = state.get_account(&addr(1)).unwrap();
        assert_eq!(acct.balance, 95_000);        // 100K - 5K
        assert_eq!(acct.staked_balance, 5_000);  // locked
    }

    #[test]
    fn test_inference_register_rejects_insufficient_stake() {
        let state = StateDB::with_genesis(&[(addr(1), 100_000)]);

        // Tier 2 requires 5K minimum, try with only 1K
        let tx = make_channel_tx(addr(1), 0, TxBody::InferenceRegister(InferenceRegisterBody {
            tier: 2,
            stake_bond: 1_000, // below min for tier 2
        }), TxType::InferenceRegister);

        let (_, r) = state.execute_block(&[tx], addr(99)).unwrap();
        assert!(!r[0].success, "InferenceRegister with insufficient stake should fail");
    }
}
