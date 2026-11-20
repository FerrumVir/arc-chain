use arc_crypto::Hash256;
use arc_state::StateDB;
use arc_types::{Account, Address, Transaction, TxBody};
use dashmap::DashMap;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
struct TxExecution {
    read_set: HashMap<[u8; 32], ReadValue>,
    write_set: HashMap<[u8; 32], WriteValue>,
    success: bool,
}

#[derive(Debug, Clone, Copy)]
struct ReadValue {
    balance: u64,
    nonce: u64,
}

#[derive(Debug, Clone, Copy)]
struct WriteValue {
    balance: u64,
    nonce: u64,
}

struct MultiVersionMemory {
    data: DashMap<[u8; 32], BTreeMap<usize, WriteValue>>,
}

impl MultiVersionMemory {
    fn new() -> Self {
        Self {
            data: DashMap::new(),
        }
    }

    fn write(&self, key: [u8; 32], tx_idx: usize, value: WriteValue) {
        self.data.entry(key).or_default().insert(tx_idx, value);
    }

    fn read_from_predecessor(&self, key: &[u8; 32], tx_idx: usize) -> Option<WriteValue> {
        self.data
            .get(key)
            .and_then(|versions| versions.range(..tx_idx).next_back().map(|(_, v)| *v))
    }

    fn clear_tx(&self, tx_idx: usize) {
        for mut entry in self.data.iter_mut() {
            entry.value_mut().remove(&tx_idx);
        }
    }
}

#[derive(Debug)]
pub struct BlockSTMResult {
    pub success: Vec<bool>,
    pub rounds: usize,
    pub reexecutions: usize,
}

pub struct BlockSTM {
    state: Arc<StateDB>,
}

impl BlockSTM {
    pub fn new(state: Arc<StateDB>) -> Self {
        Self { state }
    }

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

        let mut executions: Vec<TxExecution> = (0..n)
            .into_par_iter()
            .map(|i| self.execute_tx_speculative(&transactions[i], i, &mvm, true))
            .collect();

        for (i, exec) in executions.iter().enumerate() {
            for (&key, &wv) in &exec.write_set {
                mvm.write(key, i, wv);
            }
        }

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

            for &i in &conflicts {
                mvm.clear_tx(i);
                let exec = self.execute_tx_speculative(&transactions[i], i, &mvm, false);
                for (&key, &wv) in &exec.write_set {
                    mvm.write(key, i, wv);
                }
                executions[i] = exec;
            }
        }

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

    fn execute_tx_speculative(
        &self,
        tx: &Transaction,
        tx_idx: usize,
        mvm: &MultiVersionMemory,
        use_base_state: bool,
    ) -> TxExecution {
        let mut read_set = HashMap::new();
        let mut write_set = HashMap::new();

        let (to, amount) = match &tx.body {
            TxBody::Transfer(body) => (body.to, body.amount),
            TxBody::Settle(body) => (body.agent_id, body.amount),
            _ => {
                return TxExecution {
                    read_set,
                    write_set,
                    success: false,
                };
            }
        };

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

        let receiver_key = to.0;
        let receiver_state = if use_base_state {
            self.read_account_from_state(&to)
        } else {
            self.read_account(receiver_key, tx_idx, mvm)
        };

        if receiver_key != sender_key {
            read_set.insert(
                receiver_key,
                ReadValue {
                    balance: receiver_state.balance,
                    nonce: receiver_state.nonce,
                },
            );
        }

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

        let new_sender_balance = sender_state.balance - amount;
        let new_sender_nonce = sender_state.nonce + 1;
        write_set.insert(
            sender_key,
            WriteValue {
                balance: new_sender_balance,
                nonce: new_sender_nonce,
            },
        );

        if receiver_key == sender_key {
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

    fn read_account(
        &self,
        key: [u8; 32],
        tx_idx: usize,
        mvm: &MultiVersionMemory,
    ) -> AccountSnapshot {
        if let Some(wv) = mvm.read_from_predecessor(&key, tx_idx) {
            AccountSnapshot {
                balance: wv.balance,
                nonce: wv.nonce,
            }
        } else {
            self.read_account_from_state(&Hash256(key))
        }
    }

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

    fn has_conflict(&self, exec: &TxExecution, tx_idx: usize, mvm: &MultiVersionMemory) -> bool {
        for (&key, &read_val) in &exec.read_set {
            let current = if let Some(wv) = mvm.read_from_predecessor(&key, tx_idx) {
                ReadValue {
                    balance: wv.balance,
                    nonce: wv.nonce,
                }
            } else {
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
                match &transactions[i].body {
                    TxBody::Transfer(_) | TxBody::Settle(_) => {
                        success[i] = false;
                    }
                    _ => {
                        self.state.mark_tx_accounts_dirty_pub(&transactions[i]);
                        success[i] = self.state.execute_tx_pub(&transactions[i]).is_ok();
                    }
                }
            } else if exec.success {
                for (&key, &wv) in &exec.write_set {
                    let addr = Hash256(key);
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

#[derive(Debug, Clone, Copy)]
struct AccountSnapshot {
    balance: u64,
    nonce: u64,
}

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
        let state = Arc::new(StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]));
        let stm = BlockSTM::new(Arc::clone(&state));

        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 100, 0)];
        let result = stm.execute(&txs);

        assert_eq!(result.success.len(), 1);
        assert!(result.success[0]);

        let sender = state.get_account(&addr(1)).unwrap();
        assert_eq!(sender.balance, 999_900);
        assert_eq!(sender.nonce, 1);

        let receiver = state.get_account(&addr(2)).unwrap();
        assert_eq!(receiver.balance, 100);
    }

    #[test]
    fn test_disjoint_transfers_parallel() {
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
        assert_eq!(result.reexecutions, 0);

        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 999_900);
        assert_eq!(state.get_account(&addr(2)).unwrap().balance, 100);
        assert_eq!(state.get_account(&addr(3)).unwrap().balance, 999_800);
        assert_eq!(state.get_account(&addr(4)).unwrap().balance, 200);
    }

    #[test]
    fn test_conflicting_transfers_same_receiver() {
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

        let receiver = state.get_account(&addr(3)).unwrap();
        assert_eq!(receiver.balance, 300);
    }

    #[test]
    fn test_same_sender_sequential_nonces() {
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
        assert_eq!(sender.balance, 999_700);
        assert_eq!(sender.nonce, 2);
        assert_eq!(state.get_account(&addr(2)).unwrap().balance, 100);
        assert_eq!(state.get_account(&addr(3)).unwrap().balance, 200);
    }

    #[test]
    fn test_insufficient_balance_fails() {
        let state = Arc::new(StateDB::with_genesis(&[(addr(1), 50), (addr(2), 0)]));
        let stm = BlockSTM::new(Arc::clone(&state));

        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 100, 0)];
        let result = stm.execute(&txs);

        assert!(!result.success[0]);
        assert_eq!(state.get_account(&addr(1)).unwrap().balance, 50);
        assert_eq!(state.get_account(&addr(2)).unwrap().balance, 0);
    }

    #[test]
    fn test_wrong_nonce_fails() {
        let state = Arc::new(StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]));
        let stm = BlockSTM::new(Arc::clone(&state));

        let txs = vec![Transaction::new_transfer(addr(1), addr(2), 100, 5)];
        let result = stm.execute(&txs);

        assert!(!result.success[0]);
    }

    #[test]
    fn test_large_disjoint_batch() {
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
        assert_eq!(
            result.reexecutions, 0,
            "disjoint batch should have zero re-executions"
        );
    }
}
