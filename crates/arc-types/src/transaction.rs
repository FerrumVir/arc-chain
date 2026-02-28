use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};

use crate::account::Address;

/// Transaction type discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum TxType {
    /// Simple value transfer between accounts.
    Transfer = 0x01,
    /// Agent-to-agent service settlement.
    Settle = 0x02,
    /// Asset swap (atomic exchange).
    Swap = 0x03,
    /// Escrow creation or release.
    Escrow = 0x04,
    /// Stake or unstake.
    Stake = 0x05,
    /// WASM smart contract call.
    WasmCall = 0x06,
    /// Multi-signature authorization.
    MultiSig = 0x07,
}

/// A transaction on the ARC chain.
/// Supports multiple transaction types via the `body` field.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    /// Transaction type.
    pub tx_type: TxType,
    /// Sender address.
    pub from: Address,
    /// Sender nonce (replay protection).
    pub nonce: u64,
    /// Transaction body (type-specific payload).
    pub body: TxBody,
    /// BLAKE3 hash of the serialized transaction (computed on creation).
    pub hash: Hash256,
}

/// Type-specific transaction payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TxBody {
    Transfer(TransferBody),
    Settle(SettleBody),
    Swap(SwapBody),
    Escrow(EscrowBody),
    Stake(StakeBody),
    WasmCall(WasmCallBody),
    MultiSig(MultiSigBody),
}

/// Simple value transfer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferBody {
    pub to: Address,
    pub amount: u64,
    /// Pedersen commitment to the amount (for shielded transfers).
    pub amount_commitment: Option<[u8; 32]>,
}

/// Agent-to-agent service settlement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettleBody {
    pub agent_id: Address,
    pub service_hash: Hash256,
    pub amount: u64,
    pub usage_units: u64,
    pub amount_commitment: Option<[u8; 32]>,
}

/// Atomic asset swap.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SwapBody {
    pub counterparty: Address,
    pub offer_amount: u64,
    pub receive_amount: u64,
    pub offer_asset: Hash256,
    pub receive_asset: Hash256,
}

/// Escrow creation/release.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EscrowBody {
    pub beneficiary: Address,
    pub amount: u64,
    pub conditions_hash: Hash256,
    /// true = create, false = release
    pub is_create: bool,
}

/// Stake/unstake.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StakeBody {
    pub amount: u64,
    /// true = stake, false = unstake
    pub is_stake: bool,
    pub validator: Address,
}

/// WASM smart contract call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WasmCallBody {
    pub contract: Address,
    pub function: String,
    pub calldata: Vec<u8>,
    pub value: u64,
    pub gas_limit: u64,
}

/// Multi-signature transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MultiSigBody {
    pub inner_tx: Box<TxBody>,
    pub signers: Vec<Address>,
    pub threshold: u32,
}

/// Transaction receipt (result of execution).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxReceipt {
    pub tx_hash: Hash256,
    pub block_height: u64,
    pub block_hash: Hash256,
    pub index: u32,
    pub success: bool,
    pub gas_used: u64,
    /// Pedersen commitment for privacy proof.
    pub value_commitment: Option<[u8; 32]>,
    /// Merkle proof of inclusion in the block.
    pub inclusion_proof: Option<Vec<u8>>,
}

/// Compact transfer transaction — optimized for throughput benchmarks.
/// Fixed-size 250-byte layout: less memory bandwidth = more TPS.
///
/// Layout:
///   tx_type:   1 byte
///   from:     32 bytes
///   to:       32 bytes
///   amount:    8 bytes
///   nonce:     8 bytes
///   hash:     32 bytes
///   padding: 137 bytes  (total = 250 bytes)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactTransfer {
    pub from: Address,
    pub to: Address,
    pub amount: u64,
    pub nonce: u64,
    pub hash: Hash256,
}

/// Target size for compact transfers (bytes).
pub const COMPACT_TX_SIZE: usize = 250;

impl CompactTransfer {
    /// Create a compact transfer and compute its hash.
    pub fn new(from: Address, to: Address, amount: u64, nonce: u64) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
        hasher.update(&[TxType::Transfer as u8]);
        hasher.update(from.as_ref());
        hasher.update(&nonce.to_le_bytes());
        hasher.update(to.as_ref());
        hasher.update(&amount.to_le_bytes());
        let hash = Hash256(*hasher.finalize().as_bytes());
        Self { from, to, amount, nonce, hash }
    }

    /// Serialize into a fixed-size 250-byte buffer.
    /// This is the hot-path representation for hashing throughput.
    pub fn to_bytes(&self) -> [u8; COMPACT_TX_SIZE] {
        let mut buf = [0u8; COMPACT_TX_SIZE];
        buf[0] = TxType::Transfer as u8;
        buf[1..33].copy_from_slice(&self.from.0);
        buf[33..65].copy_from_slice(&self.to.0);
        buf[65..73].copy_from_slice(&self.amount.to_le_bytes());
        buf[73..81].copy_from_slice(&self.nonce.to_le_bytes());
        buf[81..113].copy_from_slice(&self.hash.0);
        // bytes 113..250 are zero padding
        buf
    }
}

impl Transaction {
    /// Create a new transfer transaction.
    pub fn new_transfer(from: Address, to: Address, amount: u64, nonce: u64) -> Self {
        let body = TxBody::Transfer(TransferBody {
            to,
            amount,
            amount_commitment: None,
        });
        let mut tx = Self {
            tx_type: TxType::Transfer,
            from,
            nonce,
            body,
            hash: Hash256::ZERO,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    /// Create a new settlement transaction.
    pub fn new_settle(
        from: Address,
        agent_id: Address,
        service_hash: Hash256,
        amount: u64,
        usage_units: u64,
        nonce: u64,
    ) -> Self {
        let body = TxBody::Settle(SettleBody {
            agent_id,
            service_hash,
            amount,
            usage_units,
            amount_commitment: None,
        });
        let mut tx = Self {
            tx_type: TxType::Settle,
            from,
            nonce,
            body,
            hash: Hash256::ZERO,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    /// Create a new WASM contract call transaction.
    pub fn new_wasm_call(
        from: Address,
        contract: Address,
        function: String,
        calldata: Vec<u8>,
        value: u64,
        gas_limit: u64,
        nonce: u64,
    ) -> Self {
        let body = TxBody::WasmCall(WasmCallBody {
            contract,
            function,
            calldata,
            value,
            gas_limit,
        });
        let mut tx = Self {
            tx_type: TxType::WasmCall,
            from,
            nonce,
            body,
            hash: Hash256::ZERO,
        };
        tx.hash = tx.compute_hash();
        tx
    }

    /// Compute the BLAKE3 hash of the serialized transaction.
    pub fn compute_hash(&self) -> Hash256 {
        let bytes = bincode::serialize(&self.body).expect("serializable");
        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
        hasher.update(&[self.tx_type as u8]);
        hasher.update(self.from.as_ref());
        hasher.update(&self.nonce.to_le_bytes());
        hasher.update(&bytes);
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Serialized size in bytes (approximate).
    pub fn size(&self) -> usize {
        bincode::serialize(self).map(|b| b.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn test_addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    #[test]
    fn test_transfer() {
        let tx = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        assert_eq!(tx.tx_type, TxType::Transfer);
        assert_ne!(tx.hash, Hash256::ZERO);
    }

    #[test]
    fn test_hash_deterministic() {
        let a = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        let b = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn test_hash_changes_with_nonce() {
        let a = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        let b = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 1);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn test_settle() {
        let tx = Transaction::new_settle(
            test_addr(1),
            test_addr(2),
            hash_bytes(b"api-service"),
            500,
            100,
            0,
        );
        assert_eq!(tx.tx_type, TxType::Settle);
    }
}
