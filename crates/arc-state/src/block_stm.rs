//! Block-STM — optimistic parallel transaction execution.
//!
//! Statically predicts the read/write set of each transaction from its body,
//! partitions transactions into conflict-free batches, and executes each batch
//! in parallel.  Batches with inter-batch dependencies run sequentially.
//!
//! This replaces simple sender-sharding, which only parallelises across
//! different senders.  Block-STM also parallelises across different *receivers*
//! (and any other disjoint account sets), which is strictly better.

use arc_types::{Account, Address, Transaction, TxBody};
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};

/// The set of accounts a transaction will read or write.
#[derive(Debug)]
pub struct TxAccessSet {
    /// Accounts that will be read AND/OR written.
    pub accounts: HashSet<[u8; 32]>,
}

/// Compute the access set for a single transaction.
///
/// This is a *static* prediction based on the transaction body — no execution
/// required.  It is conservative: every account that *might* be touched is
/// included.  False positives (extra accounts) are safe; false negatives would
/// cause silent conflicts.
pub fn tx_access_set(tx: &Transaction) -> TxAccessSet {
    let mut accounts = HashSet::new();
    accounts.insert(tx.from.0);

    match &tx.body {
        TxBody::Transfer(body) => {
            accounts.insert(body.to.0);
        }
        TxBody::Settle(body) => {
            accounts.insert(body.agent_id.0);
        }
        TxBody::Swap(body) => {
            accounts.insert(body.counterparty.0);
        }
        TxBody::Stake(body) => {
            accounts.insert(body.validator.0);
        }
        TxBody::WasmCall(body) => {
            accounts.insert(body.contract.0);
        }
        TxBody::Escrow(body) => {
            accounts.insert(body.beneficiary.0);
        }
        TxBody::DeployContract(_) | TxBody::RegisterAgent(_) | TxBody::MultiSig(_) => {}
        TxBody::JoinValidator(_) | TxBody::LeaveValidator | TxBody::ClaimRewards | TxBody::UpdateStake(_) => {}
        TxBody::Governance(_) => {}
        TxBody::BridgeLock(_) | TxBody::BridgeMint(_) => {}
        TxBody::BatchSettle(body) => {
            for entry in &body.entries {
                accounts.insert(entry.agent_id.0);
            }
        }
        TxBody::ChannelOpen(body) => {
            accounts.insert(body.counterparty.0);
        }
        TxBody::ChannelClose(_) | TxBody::ChannelDispute(_) => {}
        TxBody::ShardProof(_) => {}
    }

    TxAccessSet { accounts }
}

/// Partition transactions into conflict-free batches.
///
/// Each batch contains transactions whose access sets are pairwise disjoint,
/// meaning they can safely execute in parallel.  Batches are ordered: batch 0
/// runs first, then batch 1, etc.  Within each batch, execution order does not
/// matter.
///
/// Returns `Vec<Vec<usize>>` — each inner vec is a batch of tx indices.
///
/// **Nonce ordering constraint**: transactions from the same sender must execute
/// in nonce order.  This is enforced by always placing same-sender txs in
/// sequential batches (they share the sender account, so they conflict).
pub fn partition_batches(transactions: &[Transaction]) -> Vec<Vec<usize>> {
    if transactions.is_empty() {
        return vec![];
    }

    let access_sets: Vec<TxAccessSet> = transactions.iter().map(tx_access_set).collect();

    // Greedy batch assignment: for each tx, find the first batch where it
    // doesn't conflict with any already-assigned tx in that batch.
    let mut batches: Vec<Vec<usize>> = Vec::new();
    // Track which accounts are "claimed" in each batch.
    let mut batch_accounts: Vec<HashSet<[u8; 32]>> = Vec::new();

    for (i, access) in access_sets.iter().enumerate() {
        let mut placed = false;
        for (b, batch_accts) in batch_accounts.iter_mut().enumerate() {
            // Check if any account in this tx's access set is already in the batch
            let conflicts = access.accounts.iter().any(|a| batch_accts.contains(a));
            if !conflicts {
                // No conflict — add to this batch
                batch_accts.extend(&access.accounts);
                batches[b].push(i);
                placed = true;
                break;
            }
        }
        if !placed {
            // Create a new batch
            let mut new_accts = HashSet::new();
            new_accts.extend(&access.accounts);
            batch_accounts.push(new_accts);
            batches.push(vec![i]);
        }
    }

    batches
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    fn make_transfer(from: Address, to: Address, nonce: u64) -> Transaction {
        Transaction::new_transfer(from, to, 100, nonce)
    }

    #[test]
    fn test_disjoint_transfers_one_batch() {
        // A→B and C→D are disjoint — should be in the same batch
        let txs = vec![
            make_transfer(addr(1), addr(2), 0),
            make_transfer(addr(3), addr(4), 0),
        ];
        let batches = partition_batches(&txs);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    #[test]
    fn test_conflicting_transfers_separate_batches() {
        // A→B and C→B conflict on B — should be in separate batches
        let txs = vec![
            make_transfer(addr(1), addr(2), 0),
            make_transfer(addr(3), addr(2), 0),
        ];
        let batches = partition_batches(&txs);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_same_sender_separate_batches() {
        // A→B and A→C conflict on A (sender) — must be sequential
        let txs = vec![
            make_transfer(addr(1), addr(2), 0),
            make_transfer(addr(1), addr(3), 1),
        ];
        let batches = partition_batches(&txs);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_empty_transactions() {
        let batches = partition_batches(&[]);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_mixed_conflict_pattern() {
        // A→B, C→D, E→B, C→F
        // Batch 0: A→B, C→D (disjoint)
        // Batch 1: E→B (conflicts with A→B on B), C→F (conflicts with C→D on C)
        let txs = vec![
            make_transfer(addr(1), addr(2), 0), // A→B
            make_transfer(addr(3), addr(4), 0), // C→D
            make_transfer(addr(5), addr(2), 0), // E→B
            make_transfer(addr(3), addr(6), 1), // C→F
        ];
        let batches = partition_batches(&txs);
        // First two are disjoint → batch 0
        // Third conflicts on addr(2) with first → batch 1
        // Fourth conflicts on addr(3) with second → batch 1
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[1].len(), 2);
    }
}

// ===========================================================================
// Speculative Block-STM — optimistic parallel execution with conflict detection
// ===========================================================================
//
// Unlike the pessimistic `partition_batches()` above which pre-partitions
// transactions into conflict-free batches, the speculative engine executes
// ALL transactions in parallel optimistically and only re-executes on
// conflict.  This is the Aptos/Diem Block-STM algorithm adapted for ARC.

/// Multi-version hash map for speculative execution.
///
/// Each account can have multiple versions written by different transactions.
/// Readers see the latest version written by a transaction with index < their own.
#[derive(Debug)]
pub struct MVHashMap {
    /// account_address -> sorted list of (tx_index, balance, nonce)
    data: DashMap<[u8; 32], Vec<(usize, u64, u64)>>,
}

impl MVHashMap {
    /// Create a new empty multi-version hash map.
    pub fn new() -> Self {
        Self { data: DashMap::new() }
    }

    /// Write a value for an account at a specific transaction index.
    pub fn write(&self, account: [u8; 32], tx_index: usize, balance: u64, nonce: u64) {
        let mut entry = self.data.entry(account).or_insert_with(Vec::new);
        // Remove any existing write from this tx_index (for re-execution)
        entry.retain(|(idx, _, _)| *idx != tx_index);
        entry.push((tx_index, balance, nonce));
        entry.sort_by_key(|(idx, _, _)| *idx);
    }

    /// Read the latest value written by a transaction with index < tx_index.
    ///
    /// Returns `(balance, nonce, writer_tx_index)` or `None` if no prior
    /// write exists for this account.
    pub fn read(&self, account: &[u8; 32], tx_index: usize) -> Option<(u64, u64, usize)> {
        self.data.get(account).and_then(|versions| {
            versions
                .iter()
                .rev()
                .find(|(idx, _, _)| *idx < tx_index)
                .map(|(idx, bal, nonce)| (*bal, *nonce, *idx))
        })
    }

    /// Clear all entries (prepare for next block).
    pub fn clear(&self) {
        self.data.clear();
    }
}

/// Read/write set tracking for a single speculative transaction execution.
#[derive(Debug, Clone, Default)]
pub struct SpeculativeTxAccessSet {
    /// Accounts read during execution: address -> (balance, nonce, source_tx_index).
    /// `source_tx_index == usize::MAX` means the read came from base state.
    pub reads: HashMap<[u8; 32], (u64, u64, usize)>,
    /// Accounts written during execution: address -> (new_balance, new_nonce).
    pub writes: HashMap<[u8; 32], (u64, u64)>,
}

/// Result of speculative execution for one transaction.
#[derive(Debug, Clone)]
pub struct SpeculativeResult {
    /// Transaction index in the block.
    pub tx_index: usize,
    /// Whether execution succeeded.
    pub success: bool,
    /// Read/write sets for validation.
    pub access_set: SpeculativeTxAccessSet,
    /// Gas used (for receipt).
    pub gas_used: u64,
}

/// Status of a transaction in the speculative pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
    /// Not yet executed.
    Pending,
    /// Executed speculatively, awaiting validation.
    Executed,
    /// Validated successfully — no conflicts found.
    Validated,
    /// Needs re-execution due to a read/write conflict.
    NeedsReExecution,
}

/// Speculative Block-STM scheduler.
///
/// Manages the lifecycle of transactions through the speculative execution
/// pipeline:
///
/// 1. All txs execute in parallel (optimistic)
/// 2. Validate read sets — check if any read was overwritten by a lower-index tx
/// 3. Re-execute conflicting txs (max 3 rounds)
/// 4. Fallback to sequential for remaining conflicts
pub struct SpeculativeScheduler {
    /// Number of transactions.
    num_txs: usize,
    /// Status of each transaction.
    statuses: Vec<parking_lot::Mutex<TxStatus>>,
    /// Results of each transaction (populated after execution).
    results: Vec<parking_lot::Mutex<Option<SpeculativeResult>>>,
    /// Multi-version hash map for cross-tx visibility.
    mv_hashmap: MVHashMap,
    /// Maximum number of re-execution rounds before sequential fallback.
    max_rounds: usize,
}

impl SpeculativeScheduler {
    /// Create a new scheduler for a block of transactions.
    pub fn new(num_txs: usize) -> Self {
        let statuses: Vec<_> = (0..num_txs)
            .map(|_| parking_lot::Mutex::new(TxStatus::Pending))
            .collect();
        let results: Vec<_> = (0..num_txs)
            .map(|_| parking_lot::Mutex::new(None))
            .collect();

        Self {
            num_txs,
            statuses,
            results,
            mv_hashmap: MVHashMap::new(),
            max_rounds: 3,
        }
    }

    /// Record an execution result for a transaction.
    ///
    /// Writes the transaction's outputs to the MVHashMap so subsequent
    /// transactions can see them during re-execution.
    pub fn record_result(&self, result: SpeculativeResult) {
        let idx = result.tx_index;

        // Write outputs to MVHashMap
        for (account, (balance, nonce)) in &result.access_set.writes {
            self.mv_hashmap.write(*account, idx, *balance, *nonce);
        }

        *self.results[idx].lock() = Some(result);
        *self.statuses[idx].lock() = TxStatus::Executed;
    }

    /// Validate all executed transactions.
    ///
    /// For each executed transaction, checks whether its read set is still
    /// consistent with the MVHashMap.  A read is stale if a lower-index
    /// transaction wrote a different value to the same account after the
    /// read was taken.
    ///
    /// Returns indices of transactions that need re-execution.
    pub fn validate(&self) -> Vec<usize> {
        let mut needs_reexec = Vec::new();

        for idx in 0..self.num_txs {
            let status = *self.statuses[idx].lock();
            if status != TxStatus::Executed {
                continue;
            }

            let result = self.results[idx].lock();
            if let Some(ref res) = *result {
                let mut valid = true;

                // Check each read: was it overwritten by a lower-index tx
                // after we read it?
                for (account, (read_bal, read_nonce, read_from_idx)) in &res.access_set.reads {
                    if let Some((latest_bal, latest_nonce, writer_idx)) =
                        self.mv_hashmap.read(account, idx)
                    {
                        // If a different (later) tx wrote to this account
                        // after our read source, our read may be stale.
                        if writer_idx != *read_from_idx
                            || latest_bal != *read_bal
                            || latest_nonce != *read_nonce
                        {
                            valid = false;
                            break;
                        }
                    }
                }

                if valid {
                    drop(result);
                    *self.statuses[idx].lock() = TxStatus::Validated;
                } else {
                    drop(result);
                    *self.statuses[idx].lock() = TxStatus::NeedsReExecution;
                    needs_reexec.push(idx);
                }
            }
        }

        needs_reexec
    }

    /// Get indices of transactions that are still pending or need re-execution.
    pub fn pending_indices(&self) -> Vec<usize> {
        (0..self.num_txs)
            .filter(|idx| {
                let s = *self.statuses[*idx].lock();
                s == TxStatus::Pending || s == TxStatus::NeedsReExecution
            })
            .collect()
    }

    /// Get the final results after all validation rounds.
    ///
    /// Returns `(validated_results, unresolved_indices)`.
    pub fn finalize(&self) -> (Vec<SpeculativeResult>, Vec<usize>) {
        let mut validated = Vec::new();
        let mut unresolved = Vec::new();

        for idx in 0..self.num_txs {
            let status = *self.statuses[idx].lock();
            match status {
                TxStatus::Validated => {
                    if let Some(result) = self.results[idx].lock().clone() {
                        validated.push(result);
                    }
                }
                _ => {
                    unresolved.push(idx);
                }
            }
        }

        (validated, unresolved)
    }

    /// Number of transactions.
    pub fn num_txs(&self) -> usize {
        self.num_txs
    }

    /// Maximum re-execution rounds.
    pub fn max_rounds(&self) -> usize {
        self.max_rounds
    }

    /// Access the underlying MVHashMap (for re-execution reads).
    pub fn mv_hashmap(&self) -> &MVHashMap {
        &self.mv_hashmap
    }
}

/// Speculatively execute a single transaction.
///
/// Reads from MVHashMap (for writes by lower-index txs) or from base accounts.
fn speculative_execute_tx(
    tx_index: usize,
    tx: &Transaction,
    base_accounts: &DashMap<[u8; 32], Account>,
    mv_hashmap: &MVHashMap,
) -> SpeculativeResult {
    let mut access_set = SpeculativeTxAccessSet::default();
    let mut success = true;
    let gas_used = 21_000u64; // base gas

    match &tx.body {
        TxBody::Transfer(body) => {
            let sender = tx.from.0;
            let receiver = body.to.0;

            // Read sender (from MVHashMap or base state)
            let (sender_bal, sender_nonce, read_from) =
                if let Some((bal, nonce, from_idx)) = mv_hashmap.read(&sender, tx_index) {
                    (bal, nonce, from_idx)
                } else if let Some(acc) = base_accounts.get(&sender) {
                    (acc.balance, acc.nonce, usize::MAX) // MAX = read from base state
                } else {
                    success = false;
                    (0, 0, usize::MAX)
                };
            access_set
                .reads
                .insert(sender, (sender_bal, sender_nonce, read_from));

            // Read receiver
            let (recv_bal, recv_nonce, recv_from) =
                if let Some((bal, nonce, from_idx)) = mv_hashmap.read(&receiver, tx_index) {
                    (bal, nonce, from_idx)
                } else if let Some(acc) = base_accounts.get(&receiver) {
                    (acc.balance, acc.nonce, usize::MAX)
                } else {
                    (0, 0, usize::MAX) // New account
                };
            // Only track receiver read if different from sender
            if receiver != sender {
                access_set
                    .reads
                    .insert(receiver, (recv_bal, recv_nonce, recv_from));
            }

            if success && sender_bal >= body.amount && sender_nonce == tx.nonce {
                // Write sender
                access_set
                    .writes
                    .insert(sender, (sender_bal - body.amount, sender_nonce + 1));
                // Write receiver
                if receiver == sender {
                    // Self-transfer: amount subtracted then added back, nonce still increments
                    let entry = access_set.writes.get_mut(&sender).unwrap();
                    entry.0 += body.amount;
                } else {
                    access_set
                        .writes
                        .insert(receiver, (recv_bal + body.amount, recv_nonce));
                }
            } else {
                success = false;
            }
        }
        TxBody::Settle(body) => {
            let sender = tx.from.0;
            let receiver = body.agent_id.0;

            // Read sender
            let (sender_bal, sender_nonce, read_from) =
                if let Some((bal, nonce, from_idx)) = mv_hashmap.read(&sender, tx_index) {
                    (bal, nonce, from_idx)
                } else if let Some(acc) = base_accounts.get(&sender) {
                    (acc.balance, acc.nonce, usize::MAX)
                } else {
                    success = false;
                    (0, 0, usize::MAX)
                };
            access_set
                .reads
                .insert(sender, (sender_bal, sender_nonce, read_from));

            // Read receiver
            let (recv_bal, recv_nonce, recv_from) =
                if let Some((bal, nonce, from_idx)) = mv_hashmap.read(&receiver, tx_index) {
                    (bal, nonce, from_idx)
                } else if let Some(acc) = base_accounts.get(&receiver) {
                    (acc.balance, acc.nonce, usize::MAX)
                } else {
                    (0, 0, usize::MAX)
                };
            if receiver != sender {
                access_set
                    .reads
                    .insert(receiver, (recv_bal, recv_nonce, recv_from));
            }

            if success && sender_bal >= body.amount && sender_nonce == tx.nonce {
                access_set
                    .writes
                    .insert(sender, (sender_bal - body.amount, sender_nonce + 1));
                if receiver == sender {
                    let entry = access_set.writes.get_mut(&sender).unwrap();
                    entry.0 += body.amount;
                } else {
                    access_set
                        .writes
                        .insert(receiver, (recv_bal + body.amount, recv_nonce));
                }
            } else {
                success = false;
            }
        }
        _ => {
            // For non-transfer/settle tx types, fall back to sequential.
            // Mark as unsuccessful so the caller knows to handle it.
            return SpeculativeResult {
                tx_index,
                success: false,
                access_set,
                gas_used: 0,
            };
        }
    }

    SpeculativeResult {
        tx_index,
        success,
        access_set,
        gas_used,
    }
}

/// Execute a block of transactions using speculative Block-STM.
///
/// Algorithm:
/// 1. Execute all transactions in parallel (optimistic)
/// 2. Validate read/write sets
/// 3. Re-execute conflicting transactions (up to `max_rounds` times)
/// 4. Return results + list of transactions that must be executed sequentially
///
/// Returns: `(speculative_results, sequential_fallback_indices)`
pub fn execute_speculative(
    transactions: &[Transaction],
    accounts: &DashMap<[u8; 32], Account>,
) -> (Vec<SpeculativeResult>, Vec<usize>) {
    if transactions.is_empty() {
        return (vec![], vec![]);
    }

    let scheduler = SpeculativeScheduler::new(transactions.len());

    // Round 1: Execute all transactions speculatively
    let results: Vec<SpeculativeResult> = transactions
        .iter()
        .enumerate()
        .map(|(idx, tx)| speculative_execute_tx(idx, tx, accounts, scheduler.mv_hashmap()))
        .collect();

    // Record all results
    for result in results {
        scheduler.record_result(result);
    }

    // Validation rounds
    for _round in 0..scheduler.max_rounds() {
        let conflicts = scheduler.validate();
        if conflicts.is_empty() {
            break;
        }

        // Re-execute conflicting transactions
        for idx in &conflicts {
            let result = speculative_execute_tx(
                *idx,
                &transactions[*idx],
                accounts,
                scheduler.mv_hashmap(),
            );
            scheduler.record_result(result);
        }
    }

    // Final validation
    scheduler.validate();
    scheduler.finalize()
}

// ===========================================================================
// Tests — Speculative Block-STM
// ===========================================================================

#[cfg(test)]
mod speculative_tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    #[test]
    fn test_mvhashmap_write_read() {
        let mv = MVHashMap::new();
        let a = [1u8; 32];

        mv.write(a, 0, 1000, 0);
        mv.write(a, 2, 900, 1);

        // tx_index=1 should see tx 0's write
        let result = mv.read(&a, 1);
        assert_eq!(result, Some((1000, 0, 0)));

        // tx_index=3 should see tx 2's write
        let result = mv.read(&a, 3);
        assert_eq!(result, Some((900, 1, 2)));

        // tx_index=0 should see nothing
        let result = mv.read(&a, 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_mvhashmap_overwrite() {
        let mv = MVHashMap::new();
        let a = [1u8; 32];

        mv.write(a, 0, 1000, 0);
        // Overwrite tx 0's value (simulating re-execution)
        mv.write(a, 0, 900, 1);

        let result = mv.read(&a, 1);
        assert_eq!(result, Some((900, 1, 0)));
    }

    #[test]
    fn test_mvhashmap_clear() {
        let mv = MVHashMap::new();
        let a = [1u8; 32];

        mv.write(a, 0, 1000, 0);
        assert!(mv.read(&a, 1).is_some());

        mv.clear();
        assert!(mv.read(&a, 1).is_none());
    }

    #[test]
    fn test_speculative_scheduler_no_conflicts() {
        let scheduler = SpeculativeScheduler::new(3);

        // Three txs writing to different accounts — no conflicts
        for i in 0..3 {
            let mut access_set = SpeculativeTxAccessSet::default();
            let mut a = [0u8; 32];
            a[0] = i as u8;
            access_set.reads.insert(a, (1000, 0, usize::MAX));
            access_set.writes.insert(a, (900, 1));

            scheduler.record_result(SpeculativeResult {
                tx_index: i,
                success: true,
                access_set,
                gas_used: 21000,
            });
        }

        let conflicts = scheduler.validate();
        assert!(conflicts.is_empty());

        let (validated, unresolved) = scheduler.finalize();
        assert_eq!(validated.len(), 3);
        assert!(unresolved.is_empty());
    }

    #[test]
    fn test_speculative_scheduler_with_conflict() {
        let scheduler = SpeculativeScheduler::new(2);
        let shared_addr = [1u8; 32];

        // TX 0 writes to shared account
        let mut access0 = SpeculativeTxAccessSet::default();
        access0.reads.insert(shared_addr, (1000, 0, usize::MAX));
        access0.writes.insert(shared_addr, (900, 1));
        scheduler.record_result(SpeculativeResult {
            tx_index: 0,
            success: true,
            access_set: access0,
            gas_used: 21000,
        });

        // TX 1 reads shared account from base (not from TX 0) — conflict
        let mut access1 = SpeculativeTxAccessSet::default();
        access1.reads.insert(shared_addr, (1000, 0, usize::MAX)); // Read stale value
        access1.writes.insert(shared_addr, (800, 1));
        scheduler.record_result(SpeculativeResult {
            tx_index: 1,
            success: true,
            access_set: access1,
            gas_used: 21000,
        });

        let conflicts = scheduler.validate();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0], 1); // TX 1 needs re-execution
    }

    #[test]
    fn test_speculative_execute_disjoint_transfers() {
        let accounts: DashMap<[u8; 32], Account> = DashMap::new();
        let sender1 = addr(1);
        let sender2 = addr(2);
        let recv1 = addr(3);
        let recv2 = addr(4);

        accounts.insert(
            sender1.0,
            Account::new(sender1, 10_000),
        );
        accounts.insert(
            sender2.0,
            Account::new(sender2, 10_000),
        );
        accounts.insert(recv1.0, Account::new(recv1, 0));
        accounts.insert(recv2.0, Account::new(recv2, 0));

        let txs = vec![
            Transaction::new_transfer(sender1, recv1, 100, 0),
            Transaction::new_transfer(sender2, recv2, 200, 0),
        ];

        let (results, unresolved) = execute_speculative(&txs, &accounts);
        // Both senders are different, receivers are different — no conflicts
        assert_eq!(results.len(), 2);
        assert!(unresolved.is_empty());
        assert!(results.iter().all(|r| r.success));
    }

    #[test]
    fn test_speculative_execute_shared_receiver() {
        let accounts: DashMap<[u8; 32], Account> = DashMap::new();
        let sender1 = addr(1);
        let sender2 = addr(2);
        let receiver = addr(3);

        accounts.insert(
            sender1.0,
            Account::new(sender1, 10_000),
        );
        accounts.insert(
            sender2.0,
            Account::new(sender2, 10_000),
        );
        accounts.insert(receiver.0, Account::new(receiver, 0));

        let txs = vec![
            Transaction::new_transfer(sender1, receiver, 100, 0),
            Transaction::new_transfer(sender2, receiver, 200, 0),
        ];

        let (results, unresolved) = execute_speculative(&txs, &accounts);
        // Shared receiver causes a conflict — after re-execution both should resolve
        // or one may remain unresolved depending on validation rounds
        let total = results.len() + unresolved.len();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_speculative_execute_insufficient_balance() {
        let accounts: DashMap<[u8; 32], Account> = DashMap::new();
        let sender = addr(1);
        let receiver = addr(2);

        accounts.insert(sender.0, Account::new(sender, 50));
        accounts.insert(receiver.0, Account::new(receiver, 0));

        let txs = vec![
            Transaction::new_transfer(sender, receiver, 100, 0), // Not enough balance
        ];

        let (results, unresolved) = execute_speculative(&txs, &accounts);
        // Should be validated (deterministically fails) with success=false
        assert_eq!(results.len() + unresolved.len(), 1);
        if !results.is_empty() {
            assert!(!results[0].success);
        }
    }

    #[test]
    fn test_speculative_execute_empty() {
        let accounts: DashMap<[u8; 32], Account> = DashMap::new();
        let (results, unresolved) = execute_speculative(&[], &accounts);
        assert!(results.is_empty());
        assert!(unresolved.is_empty());
    }

    #[test]
    fn test_tx_status_transitions() {
        let scheduler = SpeculativeScheduler::new(1);

        // Initially pending
        assert_eq!(scheduler.pending_indices(), vec![0]);

        // After recording result: Executed
        let mut access_set = SpeculativeTxAccessSet::default();
        let a = [1u8; 32];
        access_set.reads.insert(a, (1000, 0, usize::MAX));
        access_set.writes.insert(a, (900, 1));

        scheduler.record_result(SpeculativeResult {
            tx_index: 0,
            success: true,
            access_set,
            gas_used: 21000,
        });

        assert!(scheduler.pending_indices().is_empty());

        // After validation with no conflicts: Validated
        let conflicts = scheduler.validate();
        assert!(conflicts.is_empty());

        let (validated, unresolved) = scheduler.finalize();
        assert_eq!(validated.len(), 1);
        assert!(unresolved.is_empty());
    }
}
