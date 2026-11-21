use arc_state::{StateDB, StateError};
use arc_types::{Address, Transaction, TxBody, TxType};
use std::collections::HashMap;

pub struct CoalescedBatch {
    pub groups: HashMap<Address, Vec<Transaction>>,
    pub net_effects: HashMap<Address, i128>,
    pub remainder: Vec<Transaction>,
}

#[derive(Debug, Clone)]
pub struct CoalesceStats {
    pub total_txs: usize,
    pub unique_senders: usize,
    pub unique_receivers: usize,
    pub reads_saved: usize,
    pub writes_saved: usize,
    pub failed_txs: usize,
    pub remainder_txs: usize,
}

fn transfer_target(body: &TxBody) -> Option<(Address, u64)> {
    match body {
        TxBody::Transfer(b) => Some((b.to, b.amount)),
        TxBody::Settle(b) => Some((b.agent_id, b.amount)),
        _ => None,
    }
}

fn is_coalesceable(tx: &Transaction) -> bool {
    matches!(tx.tx_type, TxType::Transfer | TxType::Settle)
}

impl CoalescedBatch {
    pub fn from_transactions(txs: Vec<Transaction>) -> Self {
        let mut groups: HashMap<Address, Vec<Transaction>> = HashMap::new();
        let mut net_effects: HashMap<Address, i128> = HashMap::new();
        let mut remainder: Vec<Transaction> = Vec::new();

        for tx in txs {
            if !is_coalesceable(&tx) {
                remainder.push(tx);
                continue;
            }

            if let Some((receiver, amount)) = transfer_target(&tx.body) {
                let amt = amount as i128;
                *net_effects.entry(tx.from).or_insert(0) -= amt;
                *net_effects.entry(receiver).or_insert(0) += amt;
            }

            groups.entry(tx.from).or_default().push(tx);
        }

        for group in groups.values_mut() {
            group.sort_by_key(|tx| tx.nonce);
        }

        Self {
            groups,
            net_effects,
            remainder,
        }
    }

    pub fn execute(&self, state: &StateDB) -> Result<CoalesceStats, StateError> {
        let total_txs = self.groups.values().map(|g| g.len()).sum::<usize>() + self.remainder.len();
        let unique_senders = self.groups.len();

        let mut receiver_credits: HashMap<Address, u64> = HashMap::new();
        let mut failed_txs: usize = 0;
        let mut coalesced_tx_count: usize = 0;

        for (sender_addr, txs) in &self.groups {
            coalesced_tx_count += txs.len();

            let mut sender = state.get_or_create_account(sender_addr);

            for tx in txs {
                if sender.nonce != tx.nonce {
                    failed_txs += 1;
                    continue;
                }

                let (receiver_addr, amount) = match transfer_target(&tx.body) {
                    Some(pair) => pair,
                    None => {
                        failed_txs += 1;
                        continue;
                    }
                };

                if sender.balance < amount {
                    failed_txs += 1;
                    continue;
                }

                sender.balance -= amount;
                sender.nonce += 1;

                *receiver_credits.entry(receiver_addr).or_insert(0) += amount;
            }

            state.update_account(sender_addr, sender);
        }

        let unique_receivers = receiver_credits.len();

        for (receiver_addr, credit) in &receiver_credits {
            let mut receiver = state.get_or_create_account(receiver_addr);
            receiver.balance += credit;
            state.update_account(receiver_addr, receiver);
        }

        for tx in &self.remainder {
            state.mark_tx_accounts_dirty_pub(tx);
            if state.execute_tx_pub(tx).is_err() {
                failed_txs += 1;
            }
        }

        let reads_saved = coalesced_tx_count.saturating_sub(unique_senders)
            + coalesced_tx_count.saturating_sub(unique_receivers);
        let writes_saved = reads_saved;

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

    pub fn is_worthwhile(&self) -> bool {
        let coalesced_tx_count: usize = self.groups.values().map(|g| g.len()).sum();
        let unique_accounts = self.net_effects.len();

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

    fn test_state(accounts: &[(Address, u64)]) -> Arc<StateDB> {
        Arc::new(StateDB::with_genesis(accounts))
    }

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
            Transaction::new_transfer(addr(1), addr(2), 100, 2),
            Transaction::new_transfer(addr(1), addr(3), 200, 0),
            Transaction::new_transfer(addr(1), addr(4), 50, 1),
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

        assert_eq!(batch.net_effects[&addr(1)], -300);
        assert_eq!(batch.net_effects[&addr(2)], 50);
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
        let tx = Transaction::new_settle(addr(1), addr(5), hash_bytes(b"svc"), 500, 10, 0);
        let batch = CoalescedBatch::from_transactions(vec![tx]);

        assert_eq!(batch.groups.len(), 1);
        assert!(batch.remainder.is_empty());
        assert_eq!(batch.net_effects[&addr(1)], -500);
        assert_eq!(batch.net_effects[&addr(5)], 500);
    }

    #[test]
    fn test_execute_single_transfer() {
        let state = test_state(&[(addr(1), 1000), (addr(2), 0)]);

        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 300, 0)];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

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
        assert_eq!(r2.balance, 1500);

        let r3 = state.get_account(&addr(3)).unwrap();
        assert_eq!(r3.balance, 2000);

        assert_eq!(stats.unique_senders, 1);
        assert_eq!(stats.unique_receivers, 2);
        assert_eq!(stats.failed_txs, 0);

        assert_eq!(stats.reads_saved, 3);
        assert_eq!(stats.writes_saved, 3);
    }

    #[test]
    fn test_execute_insufficient_balance_mid_batch() {
        let state = test_state(&[(addr(1), 500), (addr(2), 0)]);

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 300, 0),
            Transaction::new_transfer(addr(1), addr(2), 300, 1),
            Transaction::new_transfer(addr(1), addr(2), 100, 2),
        ];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 200);
        assert_eq!(sender.nonce, 1);

        let receiver = state.get_account(&addr(2)).unwrap();
        assert_eq!(receiver.balance, 300);

        assert_eq!(stats.failed_txs, 2);
    }

    #[test]
    fn test_execute_invalid_nonce_skips_tx() {
        let state = test_state(&[(addr(1), 10_000), (addr(2), 0)]);

        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 100, 1)];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 10_000);
        assert_eq!(sender.nonce, 0);

        assert_eq!(stats.failed_txs, 1);
    }

    #[test]
    fn test_execute_multiple_senders() {
        let state = test_state(&[(addr(1), 5000), (addr(2), 3000), (addr(3), 0)]);

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(3), 1000, 0),
            Transaction::new_transfer(addr(2), addr(3), 500, 0),
            Transaction::new_transfer(addr(1), addr(3), 2000, 1),
        ];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let s1 = state.get_account(&addr(1)).unwrap();
        assert_eq!(s1.balance, 2000);
        assert_eq!(s1.nonce, 2);

        let s2 = state.get_account(&addr(2)).unwrap();
        assert_eq!(s2.balance, 2500);
        assert_eq!(s2.nonce, 1);

        let r = state.get_account(&addr(3)).unwrap();
        assert_eq!(r.balance, 3500);

        assert_eq!(stats.unique_senders, 2);
        assert_eq!(stats.unique_receivers, 1);
        assert_eq!(stats.failed_txs, 0);
    }

    #[test]
    fn test_is_worthwhile_with_overlap() {
        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 100, 0),
            Transaction::new_transfer(addr(1), addr(2), 100, 1),
            Transaction::new_transfer(addr(1), addr(2), 100, 2),
        ];
        let batch = CoalescedBatch::from_transactions(txs);

        assert!(batch.is_worthwhile());
    }

    #[test]
    fn test_is_worthwhile_no_overlap() {
        let txs = vec![
            Transaction::new_transfer(addr(1), addr(4), 100, 0),
            Transaction::new_transfer(addr(2), addr(5), 100, 0),
            Transaction::new_transfer(addr(3), addr(6), 100, 0),
        ];
        let batch = CoalescedBatch::from_transactions(txs);

        assert!(!batch.is_worthwhile());
    }

    #[test]
    fn test_sender_also_receives() {
        let state = test_state(&[(addr(1), 5000), (addr(2), 3000)]);

        let txs = vec![
            Transaction::new_transfer(addr(1), addr(2), 1000, 0),
            Transaction::new_transfer(addr(2), addr(1), 500, 0),
        ];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let a1 = state.get_account(&addr(1)).unwrap();
        assert_eq!(a1.balance, 4500);

        let a2 = state.get_account(&addr(2)).unwrap();
        assert_eq!(a2.balance, 3500);

        assert_eq!(stats.failed_txs, 0);
    }

    #[test]
    fn test_large_batch_savings() {
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

        assert_eq!(stats.reads_saved, 98);
        assert_eq!(stats.writes_saved, 98);
    }

    #[test]
    fn test_stats_include_remainder() {
        let transfer = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let wasm =
            Transaction::new_wasm_call(addr(3), addr(10), "foo".to_string(), vec![], 0, 100_000, 0);

        let batch = CoalescedBatch::from_transactions(vec![transfer, wasm]);
        assert_eq!(batch.remainder.len(), 1);

        let total: usize =
            batch.groups.values().map(|g| g.len()).sum::<usize>() + batch.remainder.len();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_receiver_auto_created() {
        let state = test_state(&[(addr(1), 1000)]);

        let txs = vec![Transaction::new_transfer(addr(1), addr(99), 500, 0)];
        let batch = CoalescedBatch::from_transactions(txs);
        let stats = batch.execute(&state).unwrap();

        let receiver = state.get_account(&addr(99)).unwrap();
        assert_eq!(receiver.balance, 500);
        assert_eq!(stats.failed_txs, 0);
    }
}
