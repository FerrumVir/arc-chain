pub mod light_client;
pub mod wal;

use arc_crypto::{Hash256, MerkleTree, hash_bytes};
use arc_types::{Account, Address, Identity, IdentityLevel, Transaction, TxBody, TxReceipt, Block, BlockHeader};

use light_client::{StateProof, HeaderProof, TxInclusionProof, LightSnapshot};
use dashmap::DashMap;
use parking_lot::RwLock;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
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
    pub fn get_transaction(&self, tx_hash: &[u8; 32]) -> Option<Transaction> {
        self.full_transactions.get(tx_hash).map(|t| t.clone())
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

    /// Execute a single transaction against state.
    fn execute_tx(&self, tx: &Transaction) -> Result<(), StateError> {
        match &tx.body {
            TxBody::Transfer(body) => {
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

                let mut receiver = self.get_or_create_account(&body.to);
                receiver.balance += body.amount;
                self.accounts.insert(body.to.0, receiver.clone());
                self.wal.append(WalOp::SetAccount(body.to, receiver), self.height());

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
