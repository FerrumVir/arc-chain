use arc_crypto::Hash256;
use arc_crypto::signature::{Signature, KeyPair, SignatureError};
use serde::{Deserialize, Serialize};

use crate::account::Address;

/// Transaction type discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum TxType {
    /// Simple value transfer between accounts.
    Transfer = 0x01,
    /// Agent-to-agent service settlement (zero fee).
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
    /// Deploy a WASM smart contract.
    DeployContract = 0x08,
    /// Register an agent on-chain.
    RegisterAgent = 0x09,
}

/// A transaction on the ARC chain.
///
/// The `hash` is computed over all fields *except* `hash` and `signature`.
/// The `signature` is a cryptographic proof that the holder of the private key
/// corresponding to `from` authorizes this transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    /// Transaction type.
    pub tx_type: TxType,
    /// Sender address (derived from public key).
    pub from: Address,
    /// Sender nonce (replay protection).
    pub nonce: u64,
    /// Transaction body (type-specific payload).
    pub body: TxBody,
    /// Fee in ARC (can be 0 for settlements).
    pub fee: u64,
    /// Max gas for WASM calls (0 for non-WASM transactions).
    pub gas_limit: u64,
    /// BLAKE3 hash of the signable content (computed on creation).
    pub hash: Hash256,
    /// Cryptographic signature (null for unsigned/benchmark transactions).
    pub signature: Signature,
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
    DeployContract(DeployBody),
    RegisterAgent(RegisterBody),
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

/// Deploy a WASM smart contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployBody {
    /// WASM binary bytecode.
    pub bytecode: Vec<u8>,
    /// ABI-encoded constructor arguments.
    pub constructor_args: Vec<u8>,
    /// Pre-paid state rent deposit (in ARC).
    pub state_rent_deposit: u64,
}

/// Register an agent on-chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegisterBody {
    /// Human-readable agent name.
    pub agent_name: String,
    /// Capability bitmap or descriptor.
    pub capabilities: Vec<u8>,
    /// Agent endpoint URL.
    pub endpoint: String,
    /// Protocol hash (identifies the agent protocol version).
    pub protocol: Hash256,
    /// Arbitrary metadata (JSON, CBOR, etc).
    pub metadata: Vec<u8>,
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
    /// Create a new transfer transaction (unsigned, zero fee).
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
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
        };
        tx.hash = tx.compute_hash();
        tx
    }

    /// Create a new settlement transaction (unsigned, zero fee — settlements are always free).
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
            fee: 0,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
        };
        tx.hash = tx.compute_hash();
        tx
    }

    /// Create a new WASM contract call transaction (unsigned).
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
            fee: 0,
            gas_limit,
            hash: Hash256::ZERO,
            signature: Signature::null(),
        };
        tx.hash = tx.compute_hash();
        tx
    }

    /// Create a new contract deployment transaction (unsigned).
    pub fn new_deploy(
        from: Address,
        bytecode: Vec<u8>,
        constructor_args: Vec<u8>,
        state_rent_deposit: u64,
        fee: u64,
        gas_limit: u64,
        nonce: u64,
    ) -> Self {
        let body = TxBody::DeployContract(DeployBody {
            bytecode,
            constructor_args,
            state_rent_deposit,
        });
        let mut tx = Self {
            tx_type: TxType::DeployContract,
            from,
            nonce,
            body,
            fee,
            gas_limit,
            hash: Hash256::ZERO,
            signature: Signature::null(),
        };
        tx.hash = tx.compute_hash();
        tx
    }

    /// Create a new agent registration transaction (unsigned).
    pub fn new_register_agent(
        from: Address,
        agent_name: String,
        capabilities: Vec<u8>,
        endpoint: String,
        protocol: Hash256,
        metadata: Vec<u8>,
        fee: u64,
        nonce: u64,
    ) -> Self {
        let body = TxBody::RegisterAgent(RegisterBody {
            agent_name,
            capabilities,
            endpoint,
            protocol,
            metadata,
        });
        let mut tx = Self {
            tx_type: TxType::RegisterAgent,
            from,
            nonce,
            body,
            fee,
            gas_limit: 0,
            hash: Hash256::ZERO,
            signature: Signature::null(),
        };
        tx.hash = tx.compute_hash();
        tx
    }

    /// Compute the BLAKE3 signing hash.
    ///
    /// Covers: `tx_type || from || nonce || body || fee || gas_limit`
    /// Does NOT include the hash or signature fields.
    pub fn compute_hash(&self) -> Hash256 {
        let body_bytes = bincode::serialize(&self.body).expect("serializable");
        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
        hasher.update(&[self.tx_type as u8]);
        hasher.update(self.from.as_ref());
        hasher.update(&self.nonce.to_le_bytes());
        hasher.update(&body_bytes);
        hasher.update(&self.fee.to_le_bytes());
        hasher.update(&self.gas_limit.to_le_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Sign this transaction in place.
    ///
    /// 1. Recomputes the hash from the current fields.
    /// 2. Signs the hash with the given key pair.
    /// 3. Sets both `hash` and `signature` on `self`.
    pub fn sign(&mut self, keypair: &KeyPair) -> Result<(), SignatureError> {
        self.hash = self.compute_hash();
        self.signature = keypair.sign(&self.hash)?;
        Ok(())
    }

    /// Verify this transaction's signature.
    ///
    /// 1. Recomputes the expected hash from fields.
    /// 2. Checks `self.hash` matches.
    /// 3. Verifies the signature against the hash and `self.from`.
    ///
    /// Null signatures (benchmark mode) always fail verification.
    pub fn verify_signature(&self) -> Result<(), SignatureError> {
        // Integrity: recompute hash and compare
        let expected = self.compute_hash();
        if expected != self.hash {
            return Err(SignatureError::HashMismatch);
        }
        // Authorization: verify signature matches `from`
        self.signature.verify(&self.hash, &self.from)
    }

    /// Returns true if this transaction is unsigned (null signature).
    pub fn is_unsigned(&self) -> bool {
        self.signature.is_null()
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

    // ── Basic construction ──

    #[test]
    fn test_transfer() {
        let tx = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        assert_eq!(tx.tx_type, TxType::Transfer);
        assert_ne!(tx.hash, Hash256::ZERO);
        assert!(tx.is_unsigned());
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
        assert_eq!(tx.fee, 0, "settlements are always zero fee");
    }

    #[test]
    fn test_deploy_contract() {
        let tx = Transaction::new_deploy(
            test_addr(1),
            vec![0x00, 0x61, 0x73, 0x6d], // WASM magic
            vec![],
            1000,
            50,
            100_000,
            0,
        );
        assert_eq!(tx.tx_type, TxType::DeployContract);
        assert_eq!(tx.fee, 50);
        assert_eq!(tx.gas_limit, 100_000);
    }

    #[test]
    fn test_register_agent() {
        let tx = Transaction::new_register_agent(
            test_addr(1),
            "my-agent".to_string(),
            vec![0x01],
            "https://agent.arc.ai".to_string(),
            hash_bytes(b"arc-agent-v1"),
            vec![],
            10,
            0,
        );
        assert_eq!(tx.tx_type, TxType::RegisterAgent);
    }

    // ── Signing & verification ──

    #[test]
    fn test_ed25519_sign_verify_transfer() {
        let kp = KeyPair::generate_ed25519();
        let address = kp.address();

        let mut tx = Transaction::new_transfer(address, test_addr(2), 1000, 0);
        assert!(tx.is_unsigned());

        tx.sign(&kp).expect("sign ok");
        assert!(!tx.is_unsigned());

        tx.verify_signature().expect("verify ok");
    }

    #[test]
    fn test_secp256k1_sign_verify_transfer() {
        let kp = KeyPair::generate_secp256k1();
        let address = kp.address();

        let mut tx = Transaction::new_transfer(address, test_addr(2), 500, 1);
        tx.sign(&kp).expect("sign ok");
        tx.verify_signature().expect("verify ok");
    }

    #[test]
    fn test_signature_fails_after_tamper() {
        let kp = KeyPair::generate_ed25519();
        let address = kp.address();

        let mut tx = Transaction::new_transfer(address, test_addr(2), 1000, 0);
        tx.sign(&kp).expect("sign ok");

        // Tamper with the amount
        tx.body = TxBody::Transfer(TransferBody {
            to: test_addr(2),
            amount: 9999,
            amount_commitment: None,
        });

        // Verification must fail (hash mismatch)
        assert!(tx.verify_signature().is_err());
    }

    #[test]
    fn test_wrong_signer_fails() {
        let kp = KeyPair::generate_ed25519();
        let wrong_kp = KeyPair::generate_ed25519();

        // Transaction says it's from kp, but we sign with wrong_kp
        let mut tx = Transaction::new_transfer(kp.address(), test_addr(2), 1000, 0);
        tx.hash = tx.compute_hash();
        tx.signature = wrong_kp.sign(&tx.hash).expect("sign ok");

        // Verification must fail (address mismatch)
        assert!(tx.verify_signature().is_err());
    }

    #[test]
    fn test_unsigned_verify_fails() {
        let tx = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        // Null signature fails verification (key is all zeros → address mismatch)
        assert!(tx.verify_signature().is_err());
    }

    #[test]
    fn test_fee_included_in_hash() {
        let mut a = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        a.fee = 10;
        let hash_a = a.compute_hash();

        let mut b = Transaction::new_transfer(test_addr(1), test_addr(2), 1000, 0);
        b.fee = 20;
        let hash_b = b.compute_hash();

        assert_ne!(hash_a, hash_b, "different fees must produce different hashes");
    }
}
