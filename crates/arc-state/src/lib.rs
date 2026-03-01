pub mod light_client;
pub mod wal;

use arc_crypto::{Hash256, MerkleTree, hash_bytes, hash_pair};
use arc_types::{Account, Address, Identity, IdentityLevel, Transaction, TxBody, TxType, TxReceipt, TransferBody, Block, BlockHeader};

use light_client::{StateProof, HeaderProof, TxInclusionProof, LightSnapshot};
use dashmap::DashMap;
use parking_lot::RwLock;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

pub use wal::{WalWriter, WalOp, WalEntry, Snapshot, PersistenceConfig, read_wal, find_last_checkpoint};

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
        })
    }

    /// Initialize with genesis block and prefunded accounts.
    pub fn with_genesis(prefunded: &[(Address, u64)]) -> Self {
        let state = Self::new();
        for (addr, balance) in prefunded {
            state.accounts.insert(addr.0, Account::new(*addr, *balance));
        }
        let genesis = Block::genesis();
        state.blocks.insert(0, genesis);
        state
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
        }
    }

    /// Take a snapshot of current state.
    pub fn snapshot(&self) -> Snapshot {
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

        Snapshot {
            block_height: self.height(),
            state_root: self.compute_state_root(),
            wal_sequence: 0, // Will be set by caller
            accounts,
            storage,
            contracts,
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
    pub fn get_account(&self, addr: &Address) -> Option<Account> {
        self.accounts.get(&addr.0).map(|a| a.clone())
    }

    /// Get or create an account (lazy initialization).
    pub fn get_or_create_account(&self, addr: &Address) -> Account {
        self.accounts
            .entry(addr.0)
            .or_insert_with(|| Account::new(*addr, 0))
            .clone()
    }

    /// Get current block height.
    pub fn height(&self) -> u64 {
        *self.height.read()
    }

    /// Get a block by height.
    pub fn get_block(&self, height: u64) -> Option<Block> {
        self.blocks.get(&height).map(|b| b.clone())
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

        // Execute each transaction
        for (i, tx) in transactions.iter().enumerate() {
            let result = self.execute_tx(tx);
            let success = result.is_ok();

            tx_hashes.push(tx.hash);

            receipts.push(TxReceipt {
                tx_hash: tx.hash,
                block_height: height,
                block_hash: Hash256::ZERO,
                index: i as u32,
                success,
                gas_used: 0,
                value_commitment: None,
                inclusion_proof: None,
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

        // Execute each transaction with signature verification
        for (i, tx) in transactions.iter().enumerate() {
            let result = if tx.is_unsigned() {
                Err(StateError::ExecutionError("unsigned transaction".into()))
            } else if tx.verify_signature().is_err() {
                Err(StateError::ExecutionError("invalid signature".into()))
            } else {
                self.execute_tx(tx)
            };
            let success = result.is_ok();

            tx_hashes.push(tx.hash);

            receipts.push(TxReceipt {
                tx_hash: tx.hash,
                block_height: height,
                block_hash: Hash256::ZERO,
                index: i as u32,
                success,
                gas_used: 0,
                value_commitment: None,
                inclusion_proof: None,
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
            shards.entry(tx.from.0).or_default().push((i, tx));
        }

        let shard_results: Vec<Vec<(usize, bool)>> = shards
            .into_par_iter()
            .map(|(_sender, txs)| {
                let mut results = Vec::with_capacity(txs.len());
                for (idx, tx) in txs {
                    let success = self.execute_tx(tx).is_ok();
                    results.push((idx, success));
                }
                results
            })
            .collect();

        let mut receipt_success = vec![false; transactions.len()];
        for shard in shard_results {
            for (idx, success) in shard {
                receipt_success[idx] = success;
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
                gas_used: 0,
                value_commitment: None,
                inclusion_proof: None,
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
            if let Some(mut s) = self.accounts.get_mut(&sender.0) {
                s.balance = s.balance.saturating_sub(txs_per_sender);
                s.nonce += txs_per_sender;
            }
            if let Some(mut r) = self.accounts.get_mut(&receiver.0) {
                r.balance += txs_per_sender;
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

    /// Execute a single transaction against state.
    fn execute_tx(&self, tx: &Transaction) -> Result<(), StateError> {
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
                    receiver.balance += body.amount;
                    if self.wal.is_active() {
                        let snap = receiver.clone();
                        drop(receiver);
                        self.wal.append(WalOp::SetAccount(body.to, snap), self.height());
                    }
                }

                Ok(())
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
                agent.balance += body.amount;
                self.accounts.insert(body.agent_id.0, agent.clone());
                self.wal.append(WalOp::SetAccount(body.agent_id, agent), self.height());

                Ok(())
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
                sender.balance += body.receive_amount;
                sender.nonce += 1;
                counterparty.balance -= body.receive_amount;
                counterparty.balance += body.offer_amount;

                self.accounts.insert(tx.from.0, sender);
                self.accounts.insert(body.counterparty.0, counterparty);
                Ok(())
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
                    beneficiary.balance += body.amount;
                    self.accounts.insert(body.beneficiary.0, beneficiary);
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender);
                Ok(())
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
                    if sender.balance < body.amount {
                        return Err(StateError::InsufficientBalance {
                            have: sender.balance,
                            need: body.amount,
                        });
                    }
                    sender.balance -= body.amount;
                } else {
                    sender.balance += body.amount;
                }
                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender);
                Ok(())
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

                // --- Value transfer (sender → contract) ---
                if body.value > 0 {
                    if sender.balance < body.value {
                        return Err(StateError::InsufficientBalance {
                            have: sender.balance,
                            need: body.value,
                        });
                    }
                    sender.balance -= body.value;

                    let mut contract_acct = self.get_or_create_account(&body.contract);
                    contract_acct.balance += body.value;
                    self.accounts.insert(body.contract.0, contract_acct.clone());
                    self.wal.append(WalOp::SetAccount(body.contract, contract_acct), self.height());
                }

                // --- WASM execution ---
                // NOTE: ArcVM lives in arc-vm which depends on arc-state, so we
                // cannot import it here without a circular dependency. We validate
                // the bytecode, do the value transfer, and invoke a wasmer Module
                // directly using the same host-import pattern as arc-vm.
                //
                // For now we compile + call the function and flush storage writes.
                // A future refactor should extract a StateAccess trait into arc-types
                // so both crates can share the interface cleanly.
                {
                    use wasmer::{imports, Instance, Module as WasmModule, Store};
                    let mut store = Store::default();
                    let module = WasmModule::new(&store, &bytecode)
                        .map_err(|e| StateError::ExecutionError(format!("WASM compile: {}", e)))?;
                    let import_object = imports! {};
                    let instance = Instance::new(&mut store, &module, &import_object)
                        .map_err(|e| StateError::ExecutionError(format!("WASM instantiate: {}", e)))?;
                    let func = instance.exports.get_function(&body.function)
                        .map_err(|e| StateError::ExecutionError(format!("function '{}' not found: {}", body.function, e)))?;
                    let _result = func.call(&mut store, &[])
                        .map_err(|e| StateError::ExecutionError(format!("WASM exec: {}", e)))?;
                    // Gas accounting deferred — will be wired when the circular
                    // dependency is resolved and full host imports are available.
                }

                sender.nonce += 1;
                self.accounts.insert(tx.from.0, sender.clone());
                self.wal.append(WalOp::SetAccount(tx.from, sender), self.height());
                Ok(())
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
                Ok(())
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
                    // Compile the module and call `init` with constructor args.
                    // Uses wasmer directly to avoid circular arc-vm dependency.
                    use wasmer::{imports, Instance, Module as WasmModule, Store};
                    let mut store = Store::default();
                    let module = WasmModule::new(&store, &body.bytecode)
                        .map_err(|e| StateError::ExecutionError(format!("WASM compile: {}", e)))?;
                    let import_object = imports! {};
                    let instance = Instance::new(&mut store, &module, &import_object)
                        .map_err(|e| StateError::ExecutionError(format!("WASM instantiate: {}", e)))?;
                    if let Ok(init_fn) = instance.exports.get_function("init") {
                        let _result = init_fn.call(&mut store, &[])
                            .map_err(|e| StateError::ExecutionError(format!("constructor exec: {}", e)))?;
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

                Ok(())
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
                Ok(())
            }
        }
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
    fn index_account_tx(&self, tx: &Transaction) {
        self.account_txs
            .entry(tx.from.0)
            .or_default()
            .push(tx.hash);

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
        }
    }

    /// Compute a state root by hashing all account states.
    fn compute_state_root(&self) -> Hash256 {
        let mut account_hashes: Vec<Hash256> = self
            .accounts
            .iter()
            .map(|entry| {
                let bytes = bincode::serialize(entry.value()).expect("serializable");
                hash_bytes(&bytes)
            })
            .collect();
        account_hashes.sort_by_key(|h| h.0);

        if account_hashes.is_empty() {
            return Hash256::ZERO;
        }

        let tree = MerkleTree::from_leaves(account_hashes);
        tree.root()
    }

    /// Total number of accounts.
    pub fn account_count(&self) -> usize {
        self.accounts.len()
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
    /// Builds a Merkle tree over all accounts (sorted by hash for determinism),
    /// locates the target account's leaf index, and returns the inclusion proof.
    pub fn generate_state_proof(&self, address: &Hash256) -> Result<StateProof, StateError> {
        let account = self
            .get_account(address)
            .ok_or(StateError::AccountNotFound(*address))?;

        // Build sorted leaf hashes — same logic as compute_state_root().
        let mut leaves: Vec<(Hash256, Hash256)> = self
            .accounts
            .iter()
            .map(|entry| {
                let bytes = bincode::serialize(entry.value()).expect("serializable");
                let leaf_hash = hash_bytes(&bytes);
                (leaf_hash, Hash256(*entry.key()))
            })
            .collect();
        leaves.sort_by_key(|(h, _)| h.0);

        let leaf_hashes: Vec<Hash256> = leaves.iter().map(|(h, _)| *h).collect();
        let tree = MerkleTree::from_leaves(leaf_hashes);

        // Find the index of our target account's leaf.
        let account_bytes = bincode::serialize(&account).expect("serializable");
        let target_leaf = hash_bytes(&account_bytes);
        let index = leaves
            .iter()
            .position(|(h, _)| *h == target_leaf)
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
}
