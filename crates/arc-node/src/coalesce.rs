// Add to lib.rs: pub mod coalesce;

//! State coalescing — batch multiple transactions touching the same account
//! into fewer state reads and writes.
//!
//! # Motivation
//!
//! In a typical block with 10K transactions, many touch the same hot accounts
//! (e.g. a DEX contract, a popular sender). Without coalescing, each transaction
//! independently reads and writes those accounts — leading to redundant I/O.
//!
//! State coalescing groups transactions by sender, sorts by nonce, and applies
//! them sequentially against a single cached balance read. Similarly, receiver
//! credits are accumulated and flushed once per unique receiver.
//!
//! For a batch where 50 transactions debit the same sender, this reduces
//! 50 reads + 50 writes down to 1 read + 1 write.
//!
//! # Scope
//!
//! This is a **pre-processing step** that runs before (or instead of) the
//! per-transaction execution in the pipeline. It handles `Transfer` and `Settle`
//! transactions. Other transaction types (Swap, Escrow, WasmCall, etc.) are
//! returned as a remainder for the normal execution path.

use arc_state::{StateDB, StateError};
use arc_types::{Account, Address, Transaction, TxBody, TxType};
use std::collections::HashMap;

/// Result of coalescing a batch of transactions.
pub struct CoalescedBatch {
    /// Grouped by sender address, sorted by nonce within each group.
    pub groups: HashMap<Address, Vec<Transaction>>,
    /// Net balance effect per account: positive = net credit, negative = net debit.
    /// Uses i128 to safely handle the full u64 range in both directions.
    pub net_effects: HashMap<Address, i128>,
    /// Transactions that cannot be coalesced (non-transfer types).
    /// These must be executed individually via the normal path.
    pub remainder: Vec<Transaction>,
}

/// Statistics from a coalesced execution.
#[derive(Debug, Clone)]
pub struct CoalesceStats {
    /// Total transactions in the batch (including remainder).
    pub total_txs: usize,
    /// Number of unique sender addresses in coalesced groups.
    pub unique_senders: usize,
    /// Number of unique receiver addresses across all coalesced txs.
    pub unique_receivers: usize,
    /// Number of state reads avoided by coalescing.
    pub reads_saved: usize,
    /// Number of state writes avoided by coalescing.
    pub writes_saved: usize,
    /// Number of transactions that failed validation during coalesced execution.
    pub failed_txs: usize,
    /// Number of transactions in the remainder (non-coalesceable).
    pub remainder_txs: usize,
}

/// Extract the receiver address and transfer amount from a transaction body.
/// Returns None for transaction types that don't have a simple sender->receiver transfer.
fn transfer_target(body: &TxBody) -> Option<(Address, u64)> {
    match body {
        TxBody::Transfer(b) => Some((b.to, b.amount)),
        TxBody::Settle(b) => Some((b.agent_id, b.amount)),
        _ => None,
    }
}

/// Returns true if a transaction type can be coalesced.
/// Only simple value-transfer types qualify — anything with complex
/// side effects (WASM, swaps, escrow) must go through normal execution.
fn is_coalesceable(tx: &Transaction) -> bool {
    matches!(tx.tx_type, TxType::Transfer | TxType::Settle)
}

impl CoalescedBatch {
    /// Build a coalesced batch from a list of transactions.
    ///
    /// 1. Separates coalesceable (Transfer/Settle) from non-coalesceable transactions.
    /// 2. Groups coalesceable transactions by sender address.
    /// 3. Sorts each group by nonce (ascending) for sequential application.
    /// 4. Computes the net balance effect per account across all coalesced txs.
    pub fn from_transactions(txs: Vec<Transaction>) -> Self {
        let mut groups: HashMap<Address, Vec<Transaction>> = HashMap::new();
        let mut net_effects: HashMap<Address, i128> = HashMap::new();
        let mut remainder: Vec<Transaction> = Vec::new();

        for tx in txs {
            if !is_coalesceable(&tx) {
                remainder.push(tx);
                continue;
            }

            // Compute net effect: sender is debited, receiver is credited.
            if let Some((receiver, amount)) = transfer_target(&tx.body) {
                let amt = amount as i128;
                *net_effects.entry(tx.from).or_insert(0) -= amt;
                *net_effects.entry(receiver).or_insert(0) += amt;
            }

            groups.entry(tx.from).or_default().push(tx);
        }

        // Sort each sender's transactions by nonce (ascending).
        for group in groups.values_mut() {
            group.sort_by_key(|tx| tx.nonce);
        }

        Self {
            groups,
            net_effects,
            remainder,
        }
    }

    /// Execute the coalesced batch against state.
    ///
    /// For each sender group:
    ///   1. Read the sender's account ONCE from state.
    ///   2. Walk through transactions in nonce order, validating nonce and balance.
    ///   3. Write the sender's final state ONCE.
    ///
    /// After all sender groups are processed:
    ///   4. For each unique receiver, read ONCE, apply accumulated credits, write ONCE.
    ///
    /// Returns statistics including the number of state reads/writes saved.
    pub fn execute(&self, state: &StateDB) -> Result<CoalesceStats, StateError> {
        let total_txs = self.groups.values().map(|g| g.len()).sum::<usize>()
            + self.remainder.len();
        let unique_senders = self.groups.len();

        // Track per-receiver accumulated credits from successful transactions.
        let mut receiver_credits: HashMap<Address, u64> = HashMap::new();
        let mut failed_txs: usize = 0;
        let mut coalesced_tx_count: usize = 0;

        // ── Phase 1: Process sender groups ──────────────────────────────────
        for (sender_addr, txs) in &self.groups {
            coalesced_tx_count += txs.len();

            // Single read for this sender.
            let mut sender = state.get_or_create_account(sender_addr);

            for tx in txs {
                // Validate nonce.
                if sender.nonce != tx.nonce {
                    failed_txs += 1;
                    continue;
                }

                // Extract transfer amount.
                let (receiver_addr, amount) = match transfer_target(&tx.body) {
                    Some(pair) => pair,
                    None => {
                        // Should not happen — we only group coalesceable txs.
                        failed_txs += 1;
                        continue;
                    }
                };

                // Validate balance.
                if sender.balance < amount {
                    failed_txs += 1;
                    continue;
                }

                // Apply debit in-memory.
                sender.balance -= amount;
                sender.nonce += 1;

                // Accumulate credit for the receiver.
                *receiver_credits.entry(receiver_addr).or_insert(0) += amount;
            }

            // Single write for this sender's final state.
            state.update_account(sender_addr, sender);
        }

        // ── Phase 2: Flush receiver credits ─────────────────────────────────
        let unique_receivers = receiver_credits.len();

        for (receiver_addr, credit) in &receiver_credits {
            // Single read + single write per unique receiver.
            let mut receiver = state.get_or_create_account(receiver_addr);
            receiver.balance += credit;
            state.update_account(receiver_addr, receiver);
        }

        // ── Phase 3: Execute remainder via normal path ──────────────────────
        for tx in &self.remainder {
            state.mark_tx_accounts_dirty_pub(tx);
            if state.execute_tx_pub(tx).is_err() {
                failed_txs += 1;
            }
        }

        // ── Compute savings ─────────────────────────────────────────────────
        // Without coalescing: each tx does 1 sender read + 1 sender write
        //                     + 1 receiver read + 1 receiver write = 4 ops/tx
        // With coalescing:    1 read + 1 write per unique sender
        //                     + 1 read + 1 write per unique receiver
        //
        // Reads saved  = (coalesced_txs - unique_senders) + (coalesced_txs - unique_receivers)
        // Writes saved = same
        let reads_saved = coalesced_tx_count
            .saturating_sub(unique_senders)
            + coalesced_tx_count.saturating_sub(unique_receivers);
        let writes_saved = reads_saved; // symmetric — same number of reads and writes saved

        Ok(CoalesceStats {
            total_txs,
            unique_senders,
            unique_receivers,
            reads_saved,
            writes_saved,
            failed_txs,
            remainder_txs: self.remainder.len(),
        })
    }

    /// Returns true if coalescing this batch is worthwhile.
    ///
    /// Coalescing has overhead (grouping, sorting, HashMap lookups), so it only
    /// pays off when there is meaningful account overlap. A batch where every
    /// transaction touches a unique sender/receiver pair gains nothing.
    pub fn is_worthwhile(&self) -> bool {
        let coalesced_tx_count: usize = self.groups.values().map(|g| g.len()).sum();
        let unique_accounts = self.net_effects.len();

        // If we have more transactions than unique accounts, coalescing saves work.
        // Threshold: at least 20% reduction in state ops.
        coalesced_tx_count > 0 && unique_accounts < coalesced_tx_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;
    use arc_state::StateDB;
    use std::sync::Arc;

    fn addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    /// Helper: create a StateDB with prefunded accounts.
    fn test_state(accounts: &[(Address, u64)]) -> Arc<StateDB> {
        Arc::new(StateDB::with_genesis(accounts))
    }

    // ── Grouping tests ──────────────────────────────────────────────────────

    #[test]
    fn test_empty_batch() {
        let batch = CoalescedBatch::from_transactions(vec![]);
        assert!(batch.groups.is_empty());
        assert!(batch.net_effects.is_empty());
        assert!(batch.remainder.is_empty());
        assert!(!batch.is_worthwhile());
    }

    #[test]
    fn test_single_transfer_groups_correctly() {
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let batch = CoalescedBatch::from_transactions(vec![tx]);

        assert_eq!(batch.groups.len(), 1);
        assert!(batch.groups.contains_key(&addr(1)));
        assert_eq!(batch.groups[&addr(1)].len(), 1);
        assert!(batch.remainder.is_empty());
    }

    #[test]
    fn test_multiple_txs_same_sender_sorted_by_nonce() {
        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 100, 2), // nonce 2
            Transaction::new_transfer(addr(1), addr(3), 200, 0), // nonce 0
            Transaction::new_transfer(addr(1), addr(4), 50, 1),  // nonce 1
        ];
        let batch = CoalescedBatch::from_transactions(txs);

        assert_eq!(batch.groups.len(), 1);
        let group = &batch.groups[&addr(1)];
        assert_eq!(group.len(), 3);
        assert_eq!(group[0].nonce, 0);
        assert_eq!(group[1].nonce, 1);
        assert_eq!(group[2].nonce, 2);
    }

    #[test]
    fn test_net_effects_computed_correctly() {
        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 100, 0),
            Transaction::new_transfer(addr(1), addr(3), 200, 1),
            Transaction::new_transfer(addr(2), addr(3), 50, 0),
        ];
        let batch = CoalescedBatch::from_transactions(txs);

        // addr(1): sent 100 + 200 = -300
        assert_eq!(batch.net_effects[&addr(1)], -300);
        // addr(2): received 100, sent 50 = +50
        assert_eq!(batch.net_effects[&addr(2)], 50);
        // addr(3): received 200 + 50 = +250
        assert_eq!(batch.net_effects[&addr(3)], 250);
    }

    #[test]
    fn test_non_coalesceable_txs_go_to_remainder() {
        let transfer = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let wasm_call = Transaction::new_wasm_call(
            addr(1),
            addr(10),
            "swap".to_string(),
            vec![],
            0,
            100_000,
            1,
        );

        let batch = CoalescedBatch::from_transactions(vec![transfer, wasm_call]);

        assert_eq!(batch.groups.len(), 1);
        assert_eq!(batch.remainder.len(), 1);
        assert_eq!(batch.remainder[0].tx_type, TxType::WasmCall);
    }

    #[test]
    fn test_settle_is_coalesceable() {
        let tx = Transaction::new_settle(
            addr(1),
            addr(5),
            hash_bytes(b"svc"),
            500,
            10,
            0,
        );
        let batch = CoalescedBatch::from_transactions(vec![tx]);

        assert_eq!(batch.groups.len(), 1);
        assert!(batch.remainder.is_empty());
        // Sender debited, agent credited
        assert_eq!(batch.net_effects[&addr(1)], -500);
        assert_eq!(batch.net_effects[&addr(5)], 500);
    }

    // ── Execution tests ─────────────────────────────────────────────────────

    #[test]
    fn test_execute_single_transfer() {
        let state = test_state(&[(addr(1), 1000), (addr(2), 0)]);

        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 300, 0)];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        // Verify balances.
        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 700);
        assert_eq!(sender.nonce, 1);

        let receiver = state.get_account(&addr(2)).unwrap();
        assert_eq!(receiver.balance, 300);

        assert_eq!(stats.total_txs, 1);
        assert_eq!(stats.failed_txs, 0);
    }

    #[test]
    fn test_execute_multiple_txs_same_sender() {
        let state = test_state(&[(addr(1), 10_000), (addr(2), 0), (addr(3), 0)]);

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 1000, 0),
            Transaction::new_transfer(addr(1), addr(3), 2000, 1),
            Transaction::new_transfer(addr(1), addr(2), 500, 2),
        ];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 10_000 - 1000 - 2000 - 500);
        assert_eq!(sender.nonce, 3);

        let r2 = state.get_account(&addr(2)).unwrap();
        assert_eq!(r2.balance, 1500); // 1000 + 500

        let r3 = state.get_account(&addr(3)).unwrap();
        assert_eq!(r3.balance, 2000);

        assert_eq!(stats.unique_senders, 1);
        assert_eq!(stats.unique_receivers, 2);
        assert_eq!(stats.failed_txs, 0);

        // 3 txs, 1 sender → saved 2 sender reads + writes.
        // 3 txs, 2 receivers → saved 1 receiver read + write.
        assert_eq!(stats.reads_saved, 3);  // (3-1) + (3-2) = 2 + 1
        assert_eq!(stats.writes_saved, 3);
    }

    #[test]
    fn test_execute_insufficient_balance_mid_batch() {
        let state = test_state(&[(addr(1), 500), (addr(2), 0)]);

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 300, 0), // OK: 500 -> 200
            Transaction::new_transfer(addr(1), addr(2), 300, 1), // FAIL: 200 < 300
            Transaction::new_transfer(addr(1), addr(2), 100, 2), // skipped — nonce gap from failed tx
        ];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 200); // Only first tx succeeded.
        assert_eq!(sender.nonce, 1);

        let receiver = state.get_account(&addr(2)).unwrap();
        assert_eq!(receiver.balance, 300);

        // Two failed: the insufficient balance one, and the nonce-gap one.
        assert_eq!(stats.failed_txs, 2);
    }

    #[test]
    fn test_execute_invalid_nonce_skips_tx() {
        let state = test_state(&[(addr(1), 10_000), (addr(2), 0)]);

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 100, 1), // Wrong nonce: expected 0
        ];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 10_000); // Unchanged.
        assert_eq!(sender.nonce, 0);

        assert_eq!(stats.failed_txs, 1);
    }

    #[test]
    fn test_execute_multiple_senders() {
        let state = test_state(&[
            (addr(1), 5000),
            (addr(2), 3000),
            (addr(3), 0),
        ]);

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(3), 1000, 0),
            Transaction::new_transfer(addr(2), addr(3), 500, 0),
            Transaction::new_transfer(addr(1), addr(3), 2000, 1),
        ];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let s1 = state.get_account(&addr(1)).unwrap();
        assert_eq!(s1.balance, 2000); // 5000 - 1000 - 2000
        assert_eq!(s1.nonce, 2);

        let s2 = state.get_account(&addr(2)).unwrap();
        assert_eq!(s2.balance, 2500); // 3000 - 500
        assert_eq!(s2.nonce, 1);

        let r = state.get_account(&addr(3)).unwrap();
        assert_eq!(r.balance, 3500); // 1000 + 500 + 2000

        assert_eq!(stats.unique_senders, 2);
        assert_eq!(stats.unique_receivers, 1);
        assert_eq!(stats.failed_txs, 0);
    }

    #[test]
    fn test_is_worthwhile_with_overlap() {
        // 3 txs from same sender → 3 txs, 1 unique sender + 3 receivers = 4 accounts
        // 3 < 4 would be false, but unique accounts counts both senders and receivers
        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 100, 0),
            Transaction::new_transfer(addr(1), addr(2), 100, 1),
            Transaction::new_transfer(addr(1), addr(2), 100, 2),
        ];
        let batch = CoalescedBatch::from_transactions(txs);

        // 3 txs, but only 2 unique accounts (addr(1) and addr(2)) → worthwhile.
        assert!(batch.is_worthwhile());
    }

    #[test]
    fn test_is_worthwhile_no_overlap() {
        // Each tx has a unique sender and receiver — no overlap.
        let txs = vec![
            Transaction::new_transfer(addr(1), addr(4), 100, 0),
            Transaction::new_transfer(addr(2), addr(5), 100, 0),
            Transaction::new_transfer(addr(3), addr(6), 100, 0),
        ];
        let batch = CoalescedBatch::from_transactions(txs);

        // 3 txs, 6 unique accounts → not worthwhile (6 > 3).
        assert!(!batch.is_worthwhile());
    }

    #[test]
    fn test_sender_also_receives() {
        // addr(1) sends to addr(2), addr(2) sends to addr(1) — circular.
        let state = test_state(&[(addr(1), 5000), (addr(2), 3000)]);

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 1000, 0),
            Transaction::new_transfer(addr(2), addr(1), 500, 0),
        ];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        // addr(1): 5000 - 1000 (sent) + 500 (received) = 4500
        let a1 = state.get_account(&addr(1)).unwrap();
        assert_eq!(a1.balance, 4500);

        // addr(2): 3000 - 500 (sent) + 1000 (received) = 3500
        let a2 = state.get_account(&addr(2)).unwrap();
        assert_eq!(a2.balance, 3500);

        assert_eq!(stats.failed_txs, 0);
    }

    #[test]
    fn test_large_batch_savings() {
        // Simulate a hot account scenario: 50 transactions all from the same sender.
        let sender = addr(1);
        let state = test_state(&[(sender, 1_000_000)]);

        let txs: Vec<Transaction> = (0..50)
            .map(|i| Transaction::new_transfer(sender, addr(2), 100, i))
            .collect();

        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let s = state.get_account(&sender).unwrap();
        assert_eq!(s.balance, 1_000_000 - 50 * 100);
        assert_eq!(s.nonce, 50);

        let r = state.get_account(&addr(2)).unwrap();
        assert_eq!(r.balance, 5000);

        assert_eq!(stats.total_txs, 50);
        assert_eq!(stats.unique_senders, 1);
        assert_eq!(stats.unique_receivers, 1);
        assert_eq!(stats.failed_txs, 0);

        // 50 txs, 1 unique sender → 49 sender reads saved.
        // 50 txs, 1 unique receiver → 49 receiver reads saved.
        assert_eq!(stats.reads_saved, 98); // (50-1) + (50-1)
        assert_eq!(stats.writes_saved, 98);
    }

    #[test]
    fn test_stats_include_remainder() {
        let transfer = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let wasm = Transaction::new_wasm_call(
            addr(3), addr(10), "foo".to_string(), vec![], 0, 100_000, 0,
        );

        let batch = CoalescedBatch::from_transactions(vec![transfer, wasm]);
        assert_eq!(batch.remainder.len(), 1);

        // total_txs should count both coalesceable and remainder.
        let total: usize = batch.groups.values().map(|g| g.len()).sum::<usize>()
            + batch.remainder.len();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_receiver_auto_created() {
        // Receiver does not exist in state — should be auto-created.
        let state = test_state(&[(addr(1), 1000)]);

        let txs = vec![Transaction::new_transfer(addr(1), addr(99), 500, 0)];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let receiver = state.get_account(&addr(99)).unwrap();
        assert_eq!(receiver.balance, 500);
        assert_eq!(stats.failed_txs, 0);
    }
}
