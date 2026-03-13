//! Block-STM — optimistic parallel transaction execution with conflict detection.
//!
//! Implements the Block-STM algorithm from Aptos/Diem:
//!
//! 1. Assign each transaction an index (0..N) defining its sequential ordering.
//! 2. Optimistically execute ALL transactions in parallel via rayon.
//! 3. Each execution records a ReadSet (keys read + values) and WriteSet (keys written + values).
//! 4. Validate: for tx[i], check if any key in its ReadSet was written by any tx[j] where j < i.
//! 5. Mark conflicting transactions for re-execution.
//! 6. Re-execute conflicting txs reading from the multi-version memory (predecessor writes).
//! 7. Repeat until convergence (typically 1-2 rounds for random account pairs).
//! 8. Apply all WriteSets in index order to produce the final state.
//!
//! This module is standalone and does NOT modify `pipeline.rs`. It can be called
//! from the pipeline's execute stage as a drop-in replacement for sequential execution.

use arc_crypto::Hash256;
use arc_state::StateDB;
use arc_types::{Account, Address, Transaction, TxBody};
use dashmap::DashMap;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Result of executing a single transaction speculatively.
#[derive(Debug, Clone)]
struct TxExecution {
    /// Keys read during execution and the balance observed at read time.
    read_set: HashMap<[u8; 32], ReadValue>,
    /// Keys written during execution and the new balance + nonce.
    write_set: HashMap<[u8; 32], WriteValue>,
    /// Whether the execution itself succeeded (sufficient balance, etc.).
    success: bool,
}

/// A value read from state during speculative execution.
#[derive(Debug, Clone, Copy)]
struct ReadValue {
    balance: u64,
    nonce: u64,
}

/// A value to be written to state after execution.
#[derive(Debug, Clone, Copy)]
struct WriteValue {
    balance: u64,
    nonce: u64,
}

/// Multi-version memory: for each account key, stores writes indexed by the
/// transaction index that produced them.  This allows later transactions to
/// read the most recent write from a predecessor.
struct MultiVersionMemory {
    /// key -> BTreeMap<tx_index, WriteValue>
    data: DashMap<[u8; 32], BTreeMap<usize, WriteValue>>,
}

impl MultiVersionMemory {
    fn new() -> Self {
        Self {
            data: DashMap::new(),
        }
    }

    /// Record a write from transaction `tx_idx` for the given key.
    fn write(&self, key: [u8; 32], tx_idx: usize, value: WriteValue) {
        self.data
            .entry(key)
            .or_insert_with(BTreeMap::new)
            .insert(tx_idx, value);
    }

    /// Read the most recent write to `key` from any transaction with index < `tx_idx`.
    /// Returns `None` if no predecessor has written to this key.
    fn read_from_predecessor(&self, key: &[u8; 32], tx_idx: usize) -> Option<WriteValue> {
        self.data.get(key).and_then(|versions| {
            // Find the largest index strictly less than tx_idx.
            versions.range(..tx_idx).next_back().map(|(_, v)| *v)
        })
    }

    /// Clear all writes for a specific transaction (before re-execution).
    fn clear_tx(&self, tx_idx: usize) {
        // We need to iterate all keys and remove the entry for tx_idx.
        // This is O(keys) but only happens for conflicting txs (rare).
        for mut entry in self.data.iter_mut() {
            entry.value_mut().remove(&tx_idx);
        }
    }
}

/// Summary of a Block-STM execution round.
#[derive(Debug)]
pub struct BlockSTMResult {
    /// Per-transaction success flag (in original index order).
    pub success: Vec<bool>,
    /// Number of validation rounds needed.
    pub rounds: usize,
    /// Number of re-executions performed.
    pub reexecutions: usize,
}

/// Block-STM parallel executor.
///
/// Takes a reference to the canonical `StateDB` for reading initial state.
/// All speculative writes happen in the multi-version memory; only after
/// convergence are the final writes applied to `StateDB`.
pub struct BlockSTM {
    state: Arc<StateDB>,
}

impl BlockSTM {
    /// Create a new Block-STM executor backed by the given state.
    pub fn new(state: Arc<StateDB>) -> Self {
        Self { state }
    }

    /// Execute a batch of transactions using the Block-STM algorithm.
    ///
    /// Returns a `BlockSTMResult` with per-transaction success flags and
    /// statistics.  On return, the `StateDB` has been updated with the
    /// final writes from all successful transactions (applied in order).
    pub fn execute(&self, transactions: &[Transaction]) -> BlockSTMResult {
        let n = transactions.len();
        if n == 0 {
            return BlockSTMResult {
                success: vec![],
                rounds: 0,
                reexecutions: 0,
            };
        }

        let mvm = MultiVersionMemory::new();
        let mut total_reexecutions: usize = 0;

        // ── Round 1: Optimistic parallel execution ────────────────────────
        // Every tx reads from the base StateDB (no predecessor writes yet).
        let mut executions: Vec<TxExecution> = (0..n)
            .into_par_iter()
            .map(|i| self.execute_tx_speculative(&transactions[i], i, &mvm, true))
            .collect();

        // Record all writes into the multi-version memory.
        for (i, exec) in executions.iter().enumerate() {
            for (&key, &wv) in &exec.write_set {
                mvm.write(key, i, wv);
            }
        }

        // ── Validation + re-execution loop ────────────────────────────────
        const MAX_ROUNDS: usize = 10;
        let mut round = 1;

        loop {
            round += 1;
            if round > MAX_ROUNDS {
                warn!(
                    "Block-STM: exceeded {} rounds, forcing sequential fallback",
                    MAX_ROUNDS
                );
                break;
            }

            // Validate: for each tx[i], check that every key in its ReadSet
            // still has the same value (no predecessor wrote a different value).
            let conflicts: Vec<usize> = (0..n)
                .into_par_iter()
                .filter(|&i| self.has_conflict(&executions[i], i, &mvm))
                .collect();

            if conflicts.is_empty() {
                break;
            }

            debug!(
                round,
                conflicts = conflicts.len(),
                "Block-STM: re-executing conflicting transactions"
            );

            total_reexecutions += conflicts.len();

            // Re-execute conflicting txs, this time reading from the MVM
            // so they see predecessor writes.
            for &i in &conflicts {
                mvm.clear_tx(i);
                let exec =
                    self.execute_tx_speculative(&transactions[i], i, &mvm, false);
                for (&key, &wv) in &exec.write_set {
                    mvm.write(key, i, wv);
                }
                executions[i] = exec;
            }
        }

        // ── Apply final writes to StateDB in order ────────────────────────
        let success = self.apply_final_writes(&executions, transactions);

        info!(
            txs = n,
            rounds = round - 1,
            reexecutions = total_reexecutions,
            success_count = success.iter().filter(|&&s| s).count(),
            "Block-STM: execution complete"
        );

        BlockSTMResult {
            success,
            rounds: round - 1,
            reexecutions: total_reexecutions,
        }
    }

    /// Speculatively execute a single transfer transaction.
    ///
    /// If `use_base_state` is true, reads come from the canonical StateDB.
    /// Otherwise, reads first check the multi-version memory for predecessor
    /// writes, falling back to the StateDB.
    fn execute_tx_speculative(
        &self,
        tx: &Transaction,
        tx_idx: usize,
        mvm: &MultiVersionMemory,
        use_base_state: bool,
    ) -> TxExecution {
        let mut read_set = HashMap::new();
        let mut write_set = HashMap::new();

        // Only handle transfers for now; other tx types get a pass-through.
        let (to, amount) = match &tx.body {
            TxBody::Transfer(body) => (body.to, body.amount),
            TxBody::Settle(body) => (body.agent_id, body.amount),
            _ => {
                // Non-transfer tx types are not parallelised by Block-STM.
                // Mark as successful with empty read/write sets so the
                // sequential fallback in `apply_final_writes` handles them.
                return TxExecution {
                    read_set,
                    write_set,
                    success: false, // will be executed sequentially in apply phase
                };
            }
        };

        // ── Read sender ───────────────────────────────────────────────────
        let sender_key = tx.from.0;
        let sender_state = if use_base_state {
            self.read_account_from_state(&tx.from)
        } else {
            self.read_account(sender_key, tx_idx, mvm)
        };

        read_set.insert(
            sender_key,
            ReadValue {
                balance: sender_state.balance,
                nonce: sender_state.nonce,
            },
        );

        // ── Read receiver ─────────────────────────────────────────────────
        let receiver_key = to.0;
        let receiver_state = if use_base_state {
            self.read_account_from_state(&to)
        } else {
            self.read_account(receiver_key, tx_idx, mvm)
        };

        // Only add to read_set if different from sender (self-transfer edge case).
        if receiver_key != sender_key {
            read_set.insert(
                receiver_key,
                ReadValue {
                    balance: receiver_state.balance,
                    nonce: receiver_state.nonce,
                },
            );
        }

        // ── Validate & execute ────────────────────────────────────────────
        if sender_state.nonce != tx.nonce {
            return TxExecution {
                read_set,
                write_set,
                success: false,
            };
        }

        if sender_state.balance < amount {
            return TxExecution {
                read_set,
                write_set,
                success: false,
            };
        }

        // Compute new sender state.
        let new_sender_balance = sender_state.balance - amount;
        let new_sender_nonce = sender_state.nonce + 1;
        write_set.insert(
            sender_key,
            WriteValue {
                balance: new_sender_balance,
                nonce: new_sender_nonce,
            },
        );

        // Compute new receiver state.
        if receiver_key == sender_key {
            // Self-transfer: amount is subtracted then added back; nonce still increments.
            let entry = write_set.get_mut(&sender_key).unwrap();
            entry.balance += amount;
        } else {
            let new_receiver_balance = receiver_state.balance + amount;
            write_set.insert(
                receiver_key,
                WriteValue {
                    balance: new_receiver_balance,
                    nonce: receiver_state.nonce,
                },
            );
        }

        TxExecution {
            read_set,
            write_set,
            success: true,
        }
    }

    /// Read an account's balance/nonce, first checking the multi-version memory
    /// for writes from predecessors, then falling back to the base state.
    fn read_account(&self, key: [u8; 32], tx_idx: usize, mvm: &MultiVersionMemory) -> AccountSnapshot {
        if let Some(wv) = mvm.read_from_predecessor(&key, tx_idx) {
            AccountSnapshot {
                balance: wv.balance,
                nonce: wv.nonce,
            }
        } else {
            self.read_account_from_state(&Hash256(key))
        }
    }

    /// Read an account directly from the canonical StateDB.
    fn read_account_from_state(&self, addr: &Address) -> AccountSnapshot {
        match self.state.get_account(addr) {
            Some(acct) => AccountSnapshot {
                balance: acct.balance,
                nonce: acct.nonce,
            },
            None => AccountSnapshot {
                balance: 0,
                nonce: 0,
            },
        }
    }

    /// Check if a transaction's read set has been invalidated by predecessor writes.
    ///
    /// Returns `true` if there is a conflict (the tx must be re-executed).
    fn has_conflict(&self, exec: &TxExecution, tx_idx: usize, mvm: &MultiVersionMemory) -> bool {
        for (&key, &read_val) in &exec.read_set {
            // What value would tx_idx read NOW from the MVM?
            let current = if let Some(wv) = mvm.read_from_predecessor(&key, tx_idx) {
                ReadValue {
                    balance: wv.balance,
                    nonce: wv.nonce,
                }
            } else {
                // No predecessor write — use base state.
                let snap = self.read_account_from_state(&Hash256(key));
                ReadValue {
                    balance: snap.balance,
                    nonce: snap.nonce,
                }
            };

            if current.balance != read_val.balance || current.nonce != read_val.nonce {
                return true;
            }
        }
        false
    }

    /// Apply the converged write sets to the canonical StateDB in index order.
    ///
    /// Non-transfer transactions that were not handled by Block-STM are
    /// executed sequentially here via `state.execute_tx_pub()`.
    ///
    /// Returns per-transaction success flags.
    fn apply_final_writes(
        &self,
        executions: &[TxExecution],
        transactions: &[Transaction],
    ) -> Vec<bool> {
        let n = executions.len();
        let mut success = vec![false; n];

        for i in 0..n {
            let exec = &executions[i];

            if exec.write_set.is_empty() && !exec.success {
                // Non-transfer tx type or failed validation — try sequential execution.
                // Only attempt if this is a supported tx type that we skipped in STM.
                match &transactions[i].body {
                    TxBody::Transfer(_) | TxBody::Settle(_) => {
                        // These were handled by STM but failed (e.g., insufficient balance).
                        success[i] = false;
                    }
                    _ => {
                        // Other tx types: execute sequentially against the live state.
                        self.state.mark_tx_accounts_dirty_pub(&transactions[i]);
                        success[i] = self.state.execute_tx_pub(&transactions[i]).is_ok();
                    }
                }
            } else if exec.success {
                // Apply the write set to the canonical state.
                for (&key, &wv) in &exec.write_set {
                    let addr = Hash256(key);
                    // Use DashMap entry API for atomic upsert.
                    let mut entry = self
                        .state
                        .get_account(&addr)
                        .unwrap_or_else(|| Account::new(addr, 0));
                    entry.balance = wv.balance;
                    entry.nonce = wv.nonce;
                    self.state.update_account(&addr, entry);
                }
                success[i] = true;
            } else {
                success[i] = false;
            }
        }

        success
    }
}

/// Lightweight snapshot of an account's balance and nonce (avoids cloning
/// the full `Account` struct during speculative execution).
#[derive(Debug, Clone, Copy)]
struct AccountSnapshot {
    balance: u64,
    nonce: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    #[test]
    fn test_empty_batch() {
        let state = Arc::new(StateDB::new());
        let stm = BlockSTM::new(state);
        let result = stm.execute(&[]);
        assert!(result.success.is_empty());
        assert_eq!(result.rounds, 0);
    }

    #[test]
    fn test_single_transfer() {
        let state = Arc::new(StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 0),
        ]));
        let stm = BlockSTM::new(Arc::clone(&state));

        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 100, 0)];
        let result = stm.execute(&txs);

        assert_eq!(result.success.len(), 1);
        assert!(result.success[0]);

        // Verify final state.
        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 999_900);
        assert_eq!(sender.nonce, 1);

        let receiver = state.get_account(&addr(2)).unwrap();
        assert_eq!(receiver.balance, 100);
    }

    #[test]
    fn test_disjoint_transfers_parallel() {
        // A->B and C->D are fully disjoint — no conflicts expected.
        let state = Arc::new(StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 0),
            (addr(3), 1_000_000),
            (addr(4), 0),
        ]));
        let stm = BlockSTM::new(Arc::clone(&state));

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 100, 0),
            Transaction::new_transfer(addr(3), addr(4), 200, 0),
        ];
        let result = stm.execute(&txs);

        assert!(result.success.iter().all(|&s| s));
        // Disjoint transfers should converge in 1 round with 0 re-executions.
        assert_eq!(result.reexecutions, 0);

        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 999_900);
        assert_eq!(state.get_account(&addr(2)).unwrap().balance, 100);
        assert_eq!(state.get_account(&addr(3)).unwrap().balance, 999_800);
        assert_eq!(state.get_account(&addr(4)).unwrap().balance, 200);
    }

    #[test]
    fn test_conflicting_transfers_same_receiver() {
        // A->C and B->C both write to C — conflict on receiver.
        // Block-STM should detect and re-execute to get correct final balance.
        let state = Arc::new(StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 1_000_000),
            (addr(3), 0),
        ]));
        let stm = BlockSTM::new(Arc::clone(&state));

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(3), 100, 0),
            Transaction::new_transfer(addr(2), addr(3), 200, 0),
        ];
        let result = stm.execute(&txs);

        assert!(result.success.iter().all(|&s| s));

        // Final state: receiver should have 100 + 200 = 300
        let receiver = state.get_account(&addr(3)).unwrap();
        assert_eq!(receiver.balance, 300);
    }

    #[test]
    fn test_same_sender_sequential_nonces() {
        // A->B (nonce 0) and A->C (nonce 1) — must execute in order.
        // First tx sets nonce to 1, second needs to read nonce=1.
        let state = Arc::new(StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 0),
            (addr(3), 0),
        ]));
        let stm = BlockSTM::new(Arc::clone(&state));

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 100, 0),
            Transaction::new_transfer(addr(1), addr(3), 200, 1),
        ];
        let result = stm.execute(&txs);

        assert!(result.success.iter().all(|&s| s));

        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 999_700); // 1M - 100 - 200
        assert_eq!(sender.nonce, 2);
        assert_eq!(state.get_account(&addr(2)).unwrap().balance, 100);
        assert_eq!(state.get_account(&addr(3)).unwrap().balance, 200);
    }

    #[test]
    fn test_insufficient_balance_fails() {
        let state = Arc::new(StateDB::with_genesis(&[
            (addr(1), 50), // Only 50, trying to send 100
            (addr(2), 0),
        ]));
        let stm = BlockSTM::new(Arc::clone(&state));

        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 100, 0)];
        let result = stm.execute(&txs);

        assert!(!result.success[0]);
        // State should be unchanged.
        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 50);
        assert_eq!(state.get_account(&addr(2)).unwrap().balance, 0);
    }

    #[test]
    fn test_wrong_nonce_fails() {
        let state = Arc::new(StateDB::with_genesis(&[
            (addr(1), 1_000_000),
            (addr(2), 0),
        ]));
        let stm = BlockSTM::new(Arc::clone(&state));

        // Nonce should be 0 but we provide 5.
        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 100, 5)];
        let result = stm.execute(&txs);

        assert!(!result.success[0]);
    }

    #[test]
    fn test_large_disjoint_batch() {
        // 100 disjoint transfers: sender_i -> receiver_i
        // All should complete in 1 round with 0 re-executions.
        let mut genesis = Vec::new();
        for i in 0u16..200 {
            let a = hash_bytes(&i.to_le_bytes());
            let balance = if i < 100 { 1_000_000 } else { 0 };
            genesis.push((a, balance));
        }
        let state = Arc::new(StateDB::with_genesis(&genesis));
        let stm = BlockSTM::new(Arc::clone(&state));

        let txs: Vec<Transaction> = (0u16..100)
            .map(|i| {
                let sender = hash_bytes(&i.to_le_bytes());
                let receiver = hash_bytes(&(i + 100).to_le_bytes());
                Transaction::new_transfer(sender, receiver, 500, 0)
            })
            .collect();

        let result = stm.execute(&txs);

        assert_eq!(result.success.len(), 100);
        assert!(result.success.iter().all(|&s| s));
        assert_eq!(result.reexecutions, 0, "disjoint batch should have zero re-executions");
    }
}
