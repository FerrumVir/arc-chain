use arc_crypto::Hash256;
use arc_state::StateDB;
use arc_types::{Address, EventLog};
use revm::{
    Database, Evm,
    primitives::{
        AccountInfo, Address as RevmAddress, B256, Bytecode, Bytes, ExecutionResult, KECCAK_EMPTY,
        Log, Output, SpecId, TxKind, U256,
    },
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, serde::Serialize)]
pub struct EvmResult {
    pub success: bool,
    pub gas_used: u64,
    pub return_data: Vec<u8>,
    pub deployed_address: Option<Address>,
    pub revert_reason: Option<String>,
    pub logs: Vec<EventLog>,
}

fn arc_to_evm_address(addr: &Address) -> RevmAddress {
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(&addr.0[..20]);
    RevmAddress::from(bytes)
}

fn evm_to_arc_address(addr: &RevmAddress) -> Address {
    let mut bytes = [0u8; 32];
    bytes[..20].copy_from_slice(addr.as_slice());
    Hash256(bytes)
}

struct ArcStateDb {
    state: Arc<StateDB>,
    code_cache: HashMap<B256, Bytecode>,
}

impl ArcStateDb {
    fn new(state: Arc<StateDB>) -> Self {
        Self {
            state,
            code_cache: HashMap::new(),
        }
    }
}

impl Database for ArcStateDb {
    type Error = String;

    fn basic(&mut self, address: RevmAddress) -> Result<Option<AccountInfo>, Self::Error> {
        let arc_addr = evm_to_arc_address(&address);
        match self.state.get_account(&arc_addr) {
            Some(account) => {
                let (code_hash, code) = match self.state.get_contract(&arc_addr) {
                    Some(bytecode) if !bytecode.is_empty() => {
                        let raw = Bytecode::new_raw(Bytes::from(bytecode));
                        let hash = raw.hash_slow();
                        self.code_cache.insert(hash, raw.clone());
                        (hash, Some(raw))
                    }
                    _ => (KECCAK_EMPTY, None),
                };

                Ok(Some(AccountInfo {
                    balance: U256::from(account.balance),
                    nonce: account.nonce,
                    code_hash,
                    code,
                }))
            }
            None => Ok(None),
        }
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if code_hash == KECCAK_EMPTY {
            return Ok(Bytecode::default());
        }
        self.code_cache
            .get(&code_hash)
            .cloned()
            .ok_or_else(|| format!("code not found for hash {code_hash}"))
    }

    fn storage(&mut self, address: RevmAddress, index: U256) -> Result<U256, Self::Error> {
        let arc_addr = evm_to_arc_address(&address);
        let key = Hash256(index.to_be_bytes::<32>());
        match self.state.get_storage(&arc_addr, &key) {
            Some(value) => {
                let mut buf = [0u8; 32];
                let start = 32usize.saturating_sub(value.len());
                let copy_len = value.len().min(32);
                buf[start..start + copy_len].copy_from_slice(&value[..copy_len]);
                Ok(U256::from_be_bytes(buf))
            }
            None => Ok(U256::ZERO),
        }
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        match self.state.get_block(number) {
            Some(block) => Ok(B256::from(block.hash.0)),
            None => Ok(B256::ZERO),
        }
    }
}

fn convert_logs(logs: Vec<Log>, block_height: u64, tx_hash: Hash256) -> Vec<EventLog> {
    logs.into_iter()
        .enumerate()
        .map(|(i, log)| {
            let topics = log.topics().iter().map(|t| Hash256(t.0)).collect();
            EventLog {
                address: evm_to_arc_address(&log.address),
                topics,
                data: log.data.data.to_vec(),
                block_height,
                tx_hash,
                log_index: i as u32,
            }
        })
        .collect()
}

fn apply_state_changes(
    state: &Arc<StateDB>,
    changes: HashMap<RevmAddress, revm::primitives::Account>,
) {
    for (addr, account) in changes {
        let arc_addr = evm_to_arc_address(&addr);

        if !account.is_touched() {
            continue;
        }

        let mut acct = state.get_or_create_account(&arc_addr);
        acct.balance = if account.info.balance > revm::primitives::U256::from(u64::MAX) {
            tracing::warn!("EVM balance exceeds u64::MAX for {:?}, clamping", arc_addr);
            u64::MAX
        } else {
            account.info.balance.as_limbs()[0]
        };
        acct.nonce = account.info.nonce;
        state.update_account(&arc_addr, acct);

        if let Some(ref code) = account.info.code {
            let bytecode = code.original_bytes();
            if !bytecode.is_empty() {
                state.deploy_contract(&arc_addr, bytecode.to_vec());
                info!(address = %arc_addr, size = bytecode.len(), "EVM contract bytecode persisted");
            }
        }

        for (slot, value) in &account.storage {
            let key = Hash256(slot.to_be_bytes::<32>());
            let val = value.present_value.to_be_bytes::<32>().to_vec();
            state.set_storage(&arc_addr, key, val);
        }
    }
}

fn process_result(result_and_state: revm::primitives::ResultAndState) -> EvmResult {
    match result_and_state.result {
        ExecutionResult::Success {
            output,
            gas_used,
            logs,
            ..
        } => {
            let (return_data, deployed_address) = match output {
                Output::Call(data) => (data.to_vec(), None),
                Output::Create(data, addr) => {
                    let arc_addr = addr.map(|a| evm_to_arc_address(&a));
                    if let Some(ref a) = arc_addr {
                        debug!(contract = ?a, "EVM contract deployed");
                    }
                    (data.to_vec(), arc_addr)
                }
            };
            let event_logs = convert_logs(logs, 0, Hash256::ZERO);
            EvmResult {
                success: true,
                gas_used,
                return_data,
                deployed_address,
                revert_reason: None,
                logs: event_logs,
            }
        }
        ExecutionResult::Revert { output, gas_used } => EvmResult {
            success: false,
            gas_used,
            return_data: output.to_vec(),
            deployed_address: None,
            revert_reason: Some("execution reverted".to_string()),
            logs: vec![],
        },
        ExecutionResult::Halt { reason, gas_used } => EvmResult {
            success: false,
            gas_used,
            return_data: vec![],
            deployed_address: None,
            revert_reason: Some(format!("halted: {:?}", reason)),
            logs: vec![],
        },
    }
}

pub fn evm_call(
    state: &Arc<StateDB>,
    from: Address,
    to: Address,
    calldata: Vec<u8>,
    value: u64,
    gas_limit: u64,
) -> EvmResult {
    let mut evm = Evm::builder()
        .with_db(ArcStateDb::new(state.clone()))
        .with_spec_id(SpecId::SHANGHAI)
        .modify_tx_env(|tx| {
            tx.caller = arc_to_evm_address(&from);
            tx.transact_to = TxKind::Call(arc_to_evm_address(&to));
            tx.data = Bytes::from(calldata);
            tx.value = U256::from(value);
            tx.gas_limit = gas_limit;
        })
        .build();

    match evm.transact() {
        Ok(result) => process_result(result),
        Err(e) => {
            warn!("EVM transact error: {:?}", e);
            EvmResult {
                success: false,
                gas_used: 0,
                return_data: vec![],
                deployed_address: None,
                revert_reason: Some(format!("transaction error: {:?}", e)),
                logs: vec![],
            }
        }
    }
}

pub fn evm_deploy(
    state: &Arc<StateDB>,
    from: Address,
    bytecode: Vec<u8>,
    value: u64,
    gas_limit: u64,
) -> EvmResult {
    let mut evm = Evm::builder()
        .with_db(ArcStateDb::new(state.clone()))
        .with_spec_id(SpecId::SHANGHAI)
        .modify_tx_env(|tx| {
            tx.caller = arc_to_evm_address(&from);
            tx.transact_to = TxKind::Create;
            tx.data = Bytes::from(bytecode);
            tx.value = U256::from(value);
            tx.gas_limit = gas_limit;
        })
        .build();

    match evm.transact() {
        Ok(result_and_state) => {
            let state_changes = result_and_state.state.clone();
            let result = process_result(result_and_state);
            if result.success {
                apply_state_changes(state, state_changes);
            }
            result
        }
        Err(e) => EvmResult {
            success: false,
            gas_used: 0,
            return_data: vec![],
            deployed_address: None,
            revert_reason: Some(format!("deploy error: {:?}", e)),
            logs: vec![],
        },
    }
}

pub fn evm_execute(
    state: &Arc<StateDB>,
    from: Address,
    to: Address,
    calldata: Vec<u8>,
    value: u64,
    gas_limit: u64,
) -> EvmResult {
    let mut evm = Evm::builder()
        .with_db(ArcStateDb::new(state.clone()))
        .with_spec_id(SpecId::SHANGHAI)
        .modify_tx_env(|tx| {
            tx.caller = arc_to_evm_address(&from);
            tx.transact_to = TxKind::Call(arc_to_evm_address(&to));
            tx.data = Bytes::from(calldata);
            tx.value = U256::from(value);
            tx.gas_limit = gas_limit;
        })
        .build();

    match evm.transact() {
        Ok(result_and_state) => {
            let state_changes = result_and_state.state.clone();
            let result = process_result(result_and_state);
            if result.success {
                apply_state_changes(state, state_changes);
            }
            result
        }
        Err(e) => {
            warn!("EVM execute error: {:?}", e);
            EvmResult {
                success: false,
                gas_used: 0,
                return_data: vec![],
                deployed_address: None,
                revert_reason: Some(format!("execute error: {:?}", e)),
                logs: vec![],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_addr(n: u8) -> Address {
        arc_crypto::hash_bytes(&[n])
    }

    #[test]
    fn test_arc_evm_address_roundtrip() {
        let addr = test_addr(42);
        let evm_addr = arc_to_evm_address(&addr);
        let back = evm_to_arc_address(&evm_addr);
        assert_eq!(&addr.0[..20], &back.0[..20]);
    }

    #[test]
    fn test_evm_call_empty_contract() {
        let state = Arc::new(StateDB::new());
        let result = evm_call(&state, test_addr(1), test_addr(2), vec![], 0, 1_000_000);
        assert!(result.success);
    }

    #[test]
    fn test_evm_deploy_persists_contract() {
        let state = Arc::new(StateDB::with_genesis(&[(test_addr(1), 1_000_000_000)]));
        let bytecode = vec![0x60, 0x42, 0x60, 0x00, 0x52, 0x60, 0x01, 0x60, 0x1f, 0xf3];
        let result = evm_deploy(&state, test_addr(1), bytecode, 0, 1_000_000);
        assert!(result.success);
        if let Some(addr) = result.deployed_address {
            let stored = state.get_contract(&addr);
            assert!(
                stored.is_some(),
                "deployed contract bytecode should be persisted"
            );
        }
    }

    #[test]
    fn test_evm_deploy_simple_bytecode() {
        let state = Arc::new(StateDB::new());
        let bytecode = vec![0x60, 0x00, 0x60, 0x00, 0xF3];
        let result = evm_deploy(&state, test_addr(1), bytecode, 0, 1_000_000);
        assert!(result.success);
    }
}
