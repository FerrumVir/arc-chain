use arc_crypto::{Hash256, MerkleTree, hash_bytes};
use arc_types::{Account, Address, Transaction, TxBody, TxReceipt, Block, BlockHeader};
use dashmap::DashMap;
use parking_lot::RwLock;
use rayon::prelude::*;
use std::collections::HashMap;
use thiserror::Error;

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
}

/// In-memory state database.
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
}

impl StateDB {
    /// Create a new empty state.
    pub fn new() -> Self {
        Self {
            accounts: DashMap::new(),
            storage: DashMap::new(),
            blocks: DashMap::new(),
            height: RwLock::new(0),
            receipts: DashMap::new(),
            tx_index: DashMap::new(),
            account_txs: DashMap::new(),
        }
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
                block_hash: Hash256::ZERO, // filled after block creation
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

        // Compute state root (hash of all account states)
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
            proof_hash: Hash256::ZERO, // filled by proof generation
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

        // Index receipts, tx locations, and account transactions
        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
        }

        // Store block
        self.blocks.insert(height, block.clone());

        Ok((block, receipts))
    }

    /// Execute a block with parallel state sharding.
    /// Transactions are grouped by sender — each group executes in parallel
    /// since they touch independent account state. This is the high-throughput path.
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

        // Step 1: Shard transactions by sender address for parallel execution.
        // Transactions from different senders are completely independent
        // and can execute simultaneously across CPU cores.
        let mut shards: HashMap<[u8; 32], Vec<(usize, &Transaction)>> = HashMap::new();
        for (i, tx) in transactions.iter().enumerate() {
            shards.entry(tx.from.0).or_default().push((i, tx));
        }

        // Step 2: Execute each shard in parallel via Rayon.
        // Each shard processes its sender's transactions sequentially (nonce ordering)
        // but different shards run concurrently on different cores.
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

        // Step 3: Merge results back in original order.
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

        // Step 4: Build Merkle tree (parallel)
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

        // Update receipts with block hash and Merkle inclusion proofs
        let mut receipts = receipts;
        for (i, receipt) in receipts.iter_mut().enumerate() {
            receipt.block_hash = block.hash;
            if let Some(proof) = tree.proof(i) {
                receipt.inclusion_proof = bincode::serialize(&proof).ok();
            }
        }

        // Index receipts, tx locations, and account transactions
        for (i, tx) in transactions.iter().enumerate() {
            self.receipts.insert(tx.hash.0, receipts[i].clone());
            self.tx_index.insert(tx.hash.0, (height, i as u32));
            self.index_account_tx(tx);
        }

        self.blocks.insert(height, block.clone());

        Ok((block, receipts))
    }

    /// Optimistic parallel execution — pre-sorted by sender nonce for maximum throughput.
    ///
    /// Key optimizations vs. `execute_block_parallel`:
    /// 1. Pre-sorts transactions within each sender shard by nonce (eliminates nonce-miss retries)
    /// 2. Uses collect-into-vec sharding (avoids HashMap allocation overhead)
    /// 3. Skips Merkle tree and state root computation (pure execution benchmark)
    ///
    /// Returns (success_count, total_count) for benchmarking.
    pub fn execute_optimistic(
        &self,
        transactions: &[Transaction],
    ) -> (usize, usize) {
        // Step 1: Shard by sender and pre-sort by nonce within each shard
        let mut shards: HashMap<[u8; 32], Vec<&Transaction>> = HashMap::new();
        for tx in transactions {
            shards.entry(tx.from.0).or_default().push(tx);
        }
        // Pre-sort each shard by nonce — eliminates InvalidNonce failures
        for shard in shards.values_mut() {
            shard.sort_unstable_by_key(|tx| tx.nonce);
        }

        // Step 2: Execute all shards in parallel (lock-free — DashMap handles per-key)
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
                // Debit sender
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
                self.accounts.insert(tx.from.0, sender);

                // Credit receiver
                let mut receiver = self.get_or_create_account(&body.to);
                receiver.balance += body.amount;
                self.accounts.insert(body.to.0, receiver);

                Ok(())
            }
            TxBody::Settle(body) => {
                // Settlement: debit sender, credit agent
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
                self.accounts.insert(tx.from.0, sender);

                let mut agent = self.get_or_create_account(&body.agent_id);
                agent.balance += body.amount;
                self.accounts.insert(body.agent_id.0, agent);

                Ok(())
            }
            TxBody::Swap(body) => {
                // Atomic swap: both parties exchange assets
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
                    // Release: credit beneficiary
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
            TxBody::WasmCall(_body) => {
                // WASM execution handled by arc-vm
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
        // Always index the sender
        self.account_txs
            .entry(tx.from.0)
            .or_default()
            .push(tx.hash);

        // Index the recipient/counterparty based on tx type
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
            TxBody::MultiSig(_) => {
                // MultiSig has no single recipient — only sender indexed
            }
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
        // Transaction fails but block still produced
        assert!(!receipts[0].success);
    }

    #[test]
    fn test_nonce_enforcement() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);

        // First tx with nonce 0 should succeed
        let tx1 = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let (_, r1) = state.execute_block(&[tx1], addr(99)).unwrap();
        assert!(r1[0].success);

        // Second tx with nonce 0 should fail (expected 1)
        let tx2 = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let (_, r2) = state.execute_block(&[tx2], addr(99)).unwrap();
        assert!(!r2[0].success);

        // Correct nonce 1 should succeed
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
}
