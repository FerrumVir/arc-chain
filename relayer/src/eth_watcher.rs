//! Ethereum event watcher.
//!
//! Polls an Ethereum JSON-RPC endpoint for `Locked(address,uint256,uint256)` events
//! emitted by the ArcBridge.sol contract and returns parsed lock events.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::RelayerConfig;

// keccak256("Locked(address,uint256,uint256)")
// Precomputed topic0 for the Lock event.
pub const LOCKED_EVENT_TOPIC: &str =
    "0x9f1ec8c880f76798e7b793325d625e9b60e4082a553c98f42b6cda368dd60008";

/// A parsed Lock event from ArcBridge.sol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEvent {
    /// Ethereum address that locked tokens.
    pub sender: [u8; 20],
    /// Amount of ARC tokens locked (in wei).
    pub amount: u128,
    /// Lock nonce from the contract.
    pub nonce: u64,
    /// Block number the event was emitted in.
    pub block_number: u64,
    /// Transaction hash containing the event.
    pub tx_hash: [u8; 32],
    /// Log index within the block.
    pub log_index: u64,
}

/// Watches Ethereum for Lock events on the ArcBridge contract.
pub struct EthWatcher {
    client: reqwest::Client,
    eth_rpc_url: String,
    bridge_contract: String,
    confirmations: u64,
    last_scanned_block: u64,
}

impl EthWatcher {
    /// Create a new EthWatcher from the relayer configuration.
    pub fn new(config: &RelayerConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            eth_rpc_url: config.eth_rpc_url.clone(),
            bridge_contract: config.bridge_contract.clone(),
            confirmations: config.confirmations,
            last_scanned_block: 0,
        }
    }

    /// Set the starting block for scanning (e.g. loaded from the database).
    pub fn set_last_scanned_block(&mut self, block: u64) {
        self.last_scanned_block = block;
    }

    /// Get the current block number from the Ethereum node.
    pub async fn get_current_block(&self) -> Result<u64> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_blockNumber",
            "params": [],
            "id": 1
        });

        let resp: serde_json::Value = self.client
            .post(&self.eth_rpc_url)
            .json(&body)
            .send()
            .await
            .context("failed to query eth_blockNumber")?
            .json()
            .await
            .context("failed to parse eth_blockNumber response")?;

        let hex_str = resp["result"]
            .as_str()
            .context("missing result in eth_blockNumber")?;

        parse_hex_u64(hex_str)
    }

    /// Poll for new Lock events between `last_scanned_block+1` and
    /// `current_block - confirmations`.
    pub async fn poll_lock_events(&mut self) -> Result<Vec<LockEvent>> {
        let current_block = self.get_current_block().await?;
        let confirmed_block = current_block.saturating_sub(self.confirmations);

        if confirmed_block <= self.last_scanned_block {
            debug!(
                current_block,
                confirmed_block,
                last_scanned = self.last_scanned_block,
                "no new confirmed blocks"
            );
            return Ok(vec![]);
        }

        let from_block = self.last_scanned_block + 1;
        let to_block = confirmed_block;

        info!(from_block, to_block, "scanning for Lock events");

        let events = self.get_logs(from_block, to_block).await?;

        self.last_scanned_block = to_block;

        Ok(events)
    }

    /// Fetch logs matching the Locked event from the Ethereum node.
    async fn get_logs(&self, from_block: u64, to_block: u64) -> Result<Vec<LockEvent>> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getLogs",
            "params": [{
                "fromBlock": format!("0x{:x}", from_block),
                "toBlock": format!("0x{:x}", to_block),
                "address": self.bridge_contract,
                "topics": [LOCKED_EVENT_TOPIC]
            }],
            "id": 1
        });

        let resp: serde_json::Value = self.client
            .post(&self.eth_rpc_url)
            .json(&body)
            .send()
            .await
            .context("failed to query eth_getLogs")?
            .json()
            .await
            .context("failed to parse eth_getLogs response")?;

        let logs = resp["result"]
            .as_array()
            .context("missing result array in eth_getLogs")?;

        let mut events = Vec::new();

        for log in logs {
            match parse_lock_event(log) {
                Ok(event) => events.push(event),
                Err(e) => {
                    warn!(?e, "failed to parse lock event log, skipping");
                }
            }
        }

        info!(count = events.len(), "parsed Lock events");
        Ok(events)
    }
}

/// Parse a single Ethereum log entry into a LockEvent.
///
/// Log layout for `Locked(address indexed sender, uint256 amount, uint256 indexed nonce)`:
/// - topics[0]: event signature hash
/// - topics[1]: sender address (indexed, left-padded to 32 bytes)
/// - topics[2]: nonce (indexed, left-padded to 32 bytes)
/// - data: amount (uint256, 32 bytes)
fn parse_lock_event(log: &serde_json::Value) -> Result<LockEvent> {
    let topics = log["topics"]
        .as_array()
        .context("missing topics")?;

    if topics.len() < 3 {
        anyhow::bail!("expected at least 3 topics, got {}", topics.len());
    }

    // topics[1] = sender address (last 20 bytes of 32-byte word)
    let sender_hex = topics[1]
        .as_str()
        .context("missing sender topic")?;
    let sender_bytes = decode_hex_bytes(sender_hex)?;
    let mut sender = [0u8; 20];
    if sender_bytes.len() >= 20 {
        sender.copy_from_slice(&sender_bytes[sender_bytes.len() - 20..]);
    }

    // topics[2] = nonce
    let nonce_hex = topics[2]
        .as_str()
        .context("missing nonce topic")?;
    let nonce = parse_hex_u64(nonce_hex)?;

    // data = amount (uint256)
    let data_hex = log["data"]
        .as_str()
        .context("missing data field")?;
    let data_bytes = decode_hex_bytes(data_hex)?;
    let amount = parse_uint256_as_u128(&data_bytes)?;

    // Block number
    let block_hex = log["blockNumber"]
        .as_str()
        .context("missing blockNumber")?;
    let block_number = parse_hex_u64(block_hex)?;

    // Transaction hash
    let tx_hash_hex = log["transactionHash"]
        .as_str()
        .context("missing transactionHash")?;
    let tx_hash_bytes = decode_hex_bytes(tx_hash_hex)?;
    let mut tx_hash = [0u8; 32];
    if tx_hash_bytes.len() == 32 {
        tx_hash.copy_from_slice(&tx_hash_bytes);
    }

    // Log index
    let log_index_hex = log["logIndex"]
        .as_str()
        .unwrap_or("0x0");
    let log_index = parse_hex_u64(log_index_hex)?;

    Ok(LockEvent {
        sender,
        amount,
        nonce,
        block_number,
        tx_hash,
        log_index,
    })
}

/// Decode a hex string (with optional 0x prefix) into bytes.
fn decode_hex_bytes(hex_str: &str) -> Result<Vec<u8>> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    hex::decode(stripped).context("invalid hex string")
}

/// Parse a hex string (with optional 0x prefix) into a u64.
fn parse_hex_u64(hex_str: &str) -> Result<u64> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    u64::from_str_radix(stripped, 16).context("invalid hex u64")
}

/// Parse the first 32 bytes of data as a big-endian uint256, returning as u128.
/// Returns an error if the value exceeds u128::MAX.
fn parse_uint256_as_u128(data: &[u8]) -> Result<u128> {
    if data.len() < 32 {
        anyhow::bail!("data too short for uint256: {} bytes", data.len());
    }
    // Check that the upper 16 bytes are zero (fits in u128).
    for &b in &data[..16] {
        if b != 0 {
            anyhow::bail!("uint256 value exceeds u128::MAX");
        }
    }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&data[16..32]);
    Ok(u128::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hex_u64() {
        assert_eq!(parse_hex_u64("0x1").unwrap(), 1);
        assert_eq!(parse_hex_u64("0xff").unwrap(), 255);
        assert_eq!(parse_hex_u64("0x12ab34").unwrap(), 0x12ab34);
        assert_eq!(parse_hex_u64("100").unwrap(), 256);
    }

    #[test]
    fn test_parse_uint256_as_u128() {
        let mut data = [0u8; 32];
        data[31] = 1;
        assert_eq!(parse_uint256_as_u128(&data).unwrap(), 1);

        // 1 ETH = 10^18
        let eth = 1_000_000_000_000_000_000u128;
        let eth_bytes = eth.to_be_bytes();
        data[16..32].copy_from_slice(&eth_bytes);
        assert_eq!(parse_uint256_as_u128(&data).unwrap(), eth);
    }

    #[test]
    fn test_decode_hex_bytes() {
        let bytes = decode_hex_bytes("0xabcd").unwrap();
        assert_eq!(bytes, vec![0xab, 0xcd]);

        let bytes2 = decode_hex_bytes("1234").unwrap();
        assert_eq!(bytes2, vec![0x12, 0x34]);
    }
}
