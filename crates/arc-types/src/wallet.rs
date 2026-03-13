// Add to lib.rs: pub mod wallet;

//! Wallet SDK types for ARC Chain integration.
//!
//! Provides key management types, transaction building helpers,
//! JSON-RPC request/response types, and chain info for wallet developers.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Key types
// ---------------------------------------------------------------------------

/// Supported key types for wallets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyType {
    Ed25519,
    /// Ethereum-compatible elliptic curve.
    Secp256k1,
    /// Post-quantum lattice-based signature (NIST round-3).
    Falcon512,
    /// NIST post-quantum ML-DSA-65 (formerly Dilithium).
    MlDsa65,
}

/// Wallet key pair (public info only -- private key is never serialized).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletKey {
    pub key_type: KeyType,
    pub public_key: Vec<u8>,
    pub address: [u8; 32],
    /// BIP-44 derivation path, if applicable.
    pub derivation_path: Option<String>,
    pub label: String,
}

// ---------------------------------------------------------------------------
// Transaction request / signed payload
// ---------------------------------------------------------------------------

/// High-level transaction request -- what the user intends to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRequest {
    pub from: [u8; 32],
    pub to: [u8; 32],
    pub value: u64,
    pub tx_type: TxRequestType,
    /// `None` means "fetch from chain automatically".
    pub nonce: Option<u64>,
    pub gas_limit: Option<u64>,
    pub priority_fee: Option<u64>,
    /// Arbitrary call data (contract calls).
    pub data: Option<Vec<u8>>,
}

/// Discriminant for [`TxRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TxRequestType {
    Transfer,
    ContractCall,
    ContractDeploy,
    Stake,
    Unstake,
    Bridge {
        dest_chain: u64,
        dest_address: [u8; 20],
    },
    Swap {
        token_out: [u8; 32],
        min_amount_out: u128,
    },
}

/// A fully signed transaction payload ready for submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedTxPayload {
    pub tx_hash: [u8; 32],
    pub raw_tx: Vec<u8>,
    pub signature: Vec<u8>,
    pub signer: [u8; 32],
    pub key_type: KeyType,
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0
// ---------------------------------------------------------------------------

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub method: String,
    pub params: RpcParams,
    pub id: u64,
}

/// JSON-RPC parameter variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcParams {
    None,
    Array(Vec<serde_json::Value>),
    Object(serde_json::Map<String, serde_json::Value>),
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<RpcError>,
    pub id: u64,
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Chain info
// ---------------------------------------------------------------------------

/// Describes a chain endpoint that a wallet connects to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainInfo {
    pub chain_id: u64,
    pub chain_name: String,
    pub rpc_url: String,
    pub ws_url: Option<String>,
    pub explorer_url: Option<String>,
    pub native_currency: CurrencyInfo,
    pub is_testnet: bool,
}

/// Metadata for a chain's native currency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrencyInfo {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
}

// ---------------------------------------------------------------------------
// Balance / token queries
// ---------------------------------------------------------------------------

/// Result of a balance query for a single address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceResult {
    pub address: [u8; 32],
    pub native_balance: u128,
    pub tokens: Vec<TokenBalance>,
    pub staked_balance: u64,
    pub pending_rewards: u64,
}

/// Balance of a single token held by an address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBalance {
    pub token_address: [u8; 32],
    pub symbol: String,
    pub decimals: u8,
    pub balance: u128,
}

// ---------------------------------------------------------------------------
// Transaction receipt / events
// ---------------------------------------------------------------------------

/// Receipt returned to the wallet after a transaction is confirmed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletTxReceipt {
    pub tx_hash: [u8; 32],
    pub block_height: u64,
    pub block_hash: [u8; 32],
    pub status: WalletTxStatus,
    pub gas_used: u64,
    pub fee_paid: u64,
    pub events: Vec<WalletEvent>,
    pub confirmations: u64,
}

/// Transaction lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalletTxStatus {
    Pending,
    Confirmed,
    Finalized,
    Failed,
}

/// Simplified event log for wallet display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletEvent {
    pub event_type: String,
    pub contract: [u8; 32],
    pub data: serde_json::Value,
}

// ===========================================================================
// Implementations
// ===========================================================================

impl WalletKey {
    /// Create a new wallet key with a derived address (BLAKE3 hash of public key).
    pub fn new(key_type: KeyType, public_key: Vec<u8>, label: String) -> Self {
        let hash = blake3::hash(&public_key);
        let address: [u8; 32] = *hash.as_bytes();
        Self {
            key_type,
            public_key,
            address,
            derivation_path: None,
            label,
        }
    }

    /// Human-readable short address: `"0xabcd...ef01"`.
    pub fn short_address(&self) -> String {
        let full = hex::encode(self.address);
        format!("0x{}...{}", &full[..4], &full[full.len() - 4..])
    }
}

impl TxRequest {
    /// Convenience: simple value transfer.
    pub fn transfer(from: [u8; 32], to: [u8; 32], value: u64) -> Self {
        Self {
            from,
            to,
            value,
            tx_type: TxRequestType::Transfer,
            nonce: None,
            gas_limit: None,
            priority_fee: None,
            data: None,
        }
    }

    /// Convenience: contract call with raw calldata.
    pub fn contract_call(from: [u8; 32], contract: [u8; 32], data: Vec<u8>) -> Self {
        Self {
            from,
            to: contract,
            value: 0,
            tx_type: TxRequestType::ContractCall,
            nonce: None,
            gas_limit: None,
            priority_fee: None,
            data: Some(data),
        }
    }

    /// Rough gas estimate based on transaction type.
    pub fn estimated_gas(&self) -> u64 {
        match &self.tx_type {
            TxRequestType::Transfer => 21_000,
            TxRequestType::ContractCall => self.gas_limit.unwrap_or(100_000),
            TxRequestType::ContractDeploy => self.gas_limit.unwrap_or(500_000),
            TxRequestType::Stake | TxRequestType::Unstake => 50_000,
            TxRequestType::Bridge { .. } => 150_000,
            TxRequestType::Swap { .. } => 200_000,
        }
    }
}

impl ChainInfo {
    /// ARC mainnet defaults.
    pub fn arc_mainnet() -> Self {
        Self {
            chain_id: 1,
            chain_name: "ARC Mainnet".to_string(),
            rpc_url: "https://rpc.arc.ai".to_string(),
            ws_url: Some("wss://ws.arc.ai".to_string()),
            explorer_url: Some("https://explorer.arc.ai".to_string()),
            native_currency: CurrencyInfo {
                name: "ARC".to_string(),
                symbol: "ARC".to_string(),
                decimals: 18,
            },
            is_testnet: false,
        }
    }

    /// ARC testnet defaults.
    pub fn arc_testnet() -> Self {
        Self {
            chain_id: 100,
            chain_name: "ARC Testnet".to_string(),
            rpc_url: "https://rpc-testnet.arc.ai".to_string(),
            ws_url: Some("wss://ws-testnet.arc.ai".to_string()),
            explorer_url: Some("https://explorer-testnet.arc.ai".to_string()),
            native_currency: CurrencyInfo {
                name: "ARC".to_string(),
                symbol: "tARC".to_string(),
                decimals: 18,
            },
            is_testnet: true,
        }
    }
}

impl RpcRequest {
    /// Build a new JSON-RPC 2.0 request.
    pub fn new(method: &str, params: RpcParams, id: u64) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id,
        }
    }

    /// `arc_getBalance` for the given address.
    pub fn get_balance(address: [u8; 32]) -> Self {
        let addr_hex = serde_json::Value::String(hex::encode(address));
        Self::new(
            "arc_getBalance",
            RpcParams::Array(vec![addr_hex]),
            1,
        )
    }

    /// `arc_getBlock` by height.
    pub fn get_block(height: u64) -> Self {
        let h = serde_json::Value::Number(serde_json::Number::from(height));
        Self::new("arc_getBlock", RpcParams::Array(vec![h]), 1)
    }

    /// `arc_sendTransaction` with a raw signed transaction.
    pub fn send_transaction(raw_tx: Vec<u8>) -> Self {
        let tx_hex = serde_json::Value::String(hex::encode(raw_tx));
        Self::new(
            "arc_sendTransaction",
            RpcParams::Array(vec![tx_hex]),
            1,
        )
    }
}

impl RpcResponse {
    /// Build a success response.
    pub fn success(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

    /// Build an error response.
    pub fn error(id: u64, code: i64, message: String) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(RpcError {
                code,
                message,
                data: None,
            }),
            id,
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers -----------------------------------------------------------

    fn dummy_address(seed: u8) -> [u8; 32] {
        let mut addr = [0u8; 32];
        addr[0] = seed;
        addr
    }

    fn dummy_pubkey() -> Vec<u8> {
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04]
    }

    // ---- key tests ---------------------------------------------------------

    #[test]
    fn test_wallet_key_creation() {
        let key = WalletKey::new(KeyType::Ed25519, dummy_pubkey(), "main".to_string());
        assert_eq!(key.key_type, KeyType::Ed25519);
        assert_eq!(key.label, "main");
        assert_ne!(key.address, [0u8; 32]); // derived, not zero
        assert!(key.derivation_path.is_none());
    }

    #[test]
    fn test_wallet_key_short_address() {
        let key = WalletKey::new(KeyType::Ed25519, dummy_pubkey(), "test".to_string());
        let short = key.short_address();
        assert!(short.starts_with("0x"));
        assert!(short.contains("..."));
        assert_eq!(short.len(), 13); // "0x" + 4 + "..." + 4
    }

    // ---- tx request tests --------------------------------------------------

    #[test]
    fn test_tx_request_transfer() {
        let tx = TxRequest::transfer(dummy_address(1), dummy_address(2), 1000);
        assert_eq!(tx.from, dummy_address(1));
        assert_eq!(tx.to, dummy_address(2));
        assert_eq!(tx.value, 1000);
        assert!(matches!(tx.tx_type, TxRequestType::Transfer));
        assert!(tx.nonce.is_none());
        assert!(tx.data.is_none());
    }

    #[test]
    fn test_tx_request_contract_call() {
        let data = vec![0xAB, 0xCD];
        let tx = TxRequest::contract_call(dummy_address(1), dummy_address(3), data.clone());
        assert_eq!(tx.to, dummy_address(3));
        assert_eq!(tx.value, 0);
        assert!(matches!(tx.tx_type, TxRequestType::ContractCall));
        assert_eq!(tx.data, Some(data));
    }

    #[test]
    fn test_estimated_gas() {
        let transfer = TxRequest::transfer(dummy_address(1), dummy_address(2), 100);
        assert_eq!(transfer.estimated_gas(), 21_000);

        let call = TxRequest::contract_call(dummy_address(1), dummy_address(2), vec![]);
        assert_eq!(call.estimated_gas(), 100_000);
    }

    // ---- chain info tests --------------------------------------------------

    #[test]
    fn test_chain_info_mainnet() {
        let info = ChainInfo::arc_mainnet();
        assert_eq!(info.chain_id, 1);
        assert_eq!(info.chain_name, "ARC Mainnet");
        assert_eq!(info.native_currency.symbol, "ARC");
        assert!(!info.is_testnet);
    }

    #[test]
    fn test_chain_info_testnet() {
        let info = ChainInfo::arc_testnet();
        assert_eq!(info.chain_id, 100);
        assert!(info.is_testnet);
        assert_eq!(info.native_currency.symbol, "tARC");
    }

    // ---- RPC tests ---------------------------------------------------------

    #[test]
    fn test_rpc_request_creation() {
        let req = RpcRequest::new("arc_blockNumber", RpcParams::None, 42);
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "arc_blockNumber");
        assert_eq!(req.id, 42);
    }

    #[test]
    fn test_rpc_request_get_balance() {
        let req = RpcRequest::get_balance(dummy_address(5));
        assert_eq!(req.method, "arc_getBalance");
        if let RpcParams::Array(ref params) = req.params {
            assert_eq!(params.len(), 1);
            assert!(params[0].is_string());
        } else {
            panic!("expected Array params");
        }
    }

    #[test]
    fn test_rpc_response_success() {
        let resp = RpcResponse::success(1, serde_json::json!({"balance": "1000"}));
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        assert_eq!(resp.id, 1);
    }

    #[test]
    fn test_rpc_response_error() {
        let resp = RpcResponse::error(1, -32600, "Invalid request".to_string());
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "Invalid request");
    }

    // ---- serde roundtrip ---------------------------------------------------

    #[test]
    fn test_balance_result_serde() {
        let bal = BalanceResult {
            address: dummy_address(1),
            native_balance: 1_000_000_000_000_000_000,
            tokens: vec![TokenBalance {
                token_address: dummy_address(99),
                symbol: "USDC".to_string(),
                decimals: 6,
                balance: 500_000_000,
            }],
            staked_balance: 100,
            pending_rewards: 5,
        };
        let json = serde_json::to_string(&bal).expect("serialize");
        let back: BalanceResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.address, bal.address);
        assert_eq!(back.native_balance, bal.native_balance);
        assert_eq!(back.tokens.len(), 1);
        assert_eq!(back.tokens[0].symbol, "USDC");
        assert_eq!(back.staked_balance, 100);
        assert_eq!(back.pending_rewards, 5);
    }

    // ---- key types ---------------------------------------------------------

    #[test]
    fn test_key_types() {
        let types = [
            KeyType::Ed25519,
            KeyType::Secp256k1,
            KeyType::Falcon512,
            KeyType::MlDsa65,
        ];
        for kt in &types {
            let key = WalletKey::new(*kt, dummy_pubkey(), format!("{:?}", kt));
            assert_eq!(key.key_type, *kt);
            // serde roundtrip
            let json = serde_json::to_string(&key).expect("serialize");
            let back: WalletKey = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back.key_type, *kt);
        }
    }
}
