use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};

/// 32-byte account address, derived from public key hash.
pub type Address = Hash256;

/// Account state stored in the state tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    /// Account address.
    pub address: Address,
    /// Spendable balance (in smallest unit).
    pub balance: u64,
    /// Transaction nonce (prevents replay).
    pub nonce: u64,
    /// Hash of deployed WASM code (zero if not a contract).
    pub code_hash: Hash256,
    /// Storage root (Merkle root of contract storage).
    pub storage_root: Hash256,
}

impl Account {
    /// Create a new externally-owned account (no contract code).
    pub fn new(address: Address, balance: u64) -> Self {
        Self {
            address,
            balance,
            nonce: 0,
            code_hash: Hash256::ZERO,
            storage_root: Hash256::ZERO,
        }
    }

    /// Create a contract account.
    pub fn new_contract(address: Address, code_hash: Hash256) -> Self {
        Self {
            address,
            balance: 0,
            nonce: 0,
            code_hash,
            storage_root: Hash256::ZERO,
        }
    }

    /// Returns true if this account has deployed contract code.
    pub fn is_contract(&self) -> bool {
        self.code_hash != Hash256::ZERO
    }
}
