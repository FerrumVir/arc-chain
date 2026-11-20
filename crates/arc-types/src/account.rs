use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};

pub type Address = Hash256;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub address: Address,
    pub balance: u64,
    pub nonce: u64,
    pub code_hash: Hash256,
    pub storage_root: Hash256,
    #[serde(default)]
    pub staked_balance: u64,
}

impl Account {
    pub fn new(address: Address, balance: u64) -> Self {
        Self {
            address,
            balance,
            nonce: 0,
            code_hash: Hash256::ZERO,
            storage_root: Hash256::ZERO,
            staked_balance: 0,
        }
    }

    pub fn new_contract(address: Address, code_hash: Hash256) -> Self {
        Self {
            address,
            balance: 0,
            nonce: 0,
            code_hash,
            storage_root: Hash256::ZERO,
            staked_balance: 0,
        }
    }

    pub fn is_contract(&self) -> bool {
        self.code_hash != Hash256::ZERO
    }
}
