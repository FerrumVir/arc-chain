//! ARC Chain transaction submitter.
//!
//! Builds and submits BridgeMint (0x10) transactions to the ARC Chain RPC
//! endpoint, and watches for BridgeLock (0x0f) transactions to relay back
//! to Ethereum.

use anyhow::{Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use arc_types::bridge::{
    BridgeProof, BridgeRelayMessage, BridgeTransfer, ChainId,
    EvmAddress,
};

use crate::config::RelayerConfig;
use crate::eth_watcher::LockEvent;

/// ARC Chain transaction type identifiers.
const TX_TYPE_BRIDGE_LOCK: u8 = 0x0f;
const TX_TYPE_BRIDGE_MINT: u8 = 0x10;

/// ARC token contract address on Ethereum.
const ARC_TOKEN_ADDRESS: &str = "672fdBA7055bddFa8fD6bD45B1455cE5eB97f499";

/// A BridgeLock transaction observed on ARC Chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeLockEvent {
    /// Sender address on ARC Chain.
    pub sender: EvmAddress,
    /// Recipient address on Ethereum.
    pub eth_recipient: EvmAddress,
    /// Amount locked.
    pub amount: u128,
    /// Nonce for replay protection.
    pub nonce: u64,
    /// ARC Chain block height where this was committed.
    pub block_height: u64,
    /// Transaction hash on ARC Chain.
    pub tx_hash: [u8; 32],
}

/// Submits BridgeMint transactions to ARC Chain and watches for BridgeLock
/// transactions.
pub struct ArcSubmitter {
    client: reqwest::Client,
    arc_rpc_url: String,
    signing_key: SigningKey,
    relayer_address: EvmAddress,
    last_scanned_height: u64,
}

impl ArcSubmitter {
    /// Create a new ArcSubmitter from the relayer configuration.
    pub fn new(config: &RelayerConfig) -> Result<Self> {
        let key_bytes = hex::decode(
            config.relayer_private_key.strip_prefix("0x")
                .unwrap_or(&config.relayer_private_key),
        )
        .context("invalid relayer_private_key hex")?;

        if key_bytes.len() != 32 {
            anyhow::bail!(
                "relayer_private_key must be 32 bytes, got {}",
                key_bytes.len()
            );
        }

        let mut secret = [0u8; 32];
        secret.copy_from_slice(&key_bytes);
        let signing_key = SigningKey::from_bytes(&secret);

        // Derive the relayer address from the public key (first 20 bytes of the
        // blake3 hash of the verifying key).
        let verifying_key = signing_key.verifying_key();
        let addr_hash = blake3::hash(verifying_key.as_bytes());
        let mut relayer_address = [0u8; 20];
        relayer_address.copy_from_slice(&addr_hash.as_bytes()[..20]);

        Ok(Self {
            client: reqwest::Client::new(),
            arc_rpc_url: config.arc_rpc_url.clone(),
            signing_key,
            relayer_address,
            last_scanned_height: 0,
        })
    }

    /// Set the last scanned ARC Chain block height (e.g. from the database).
    pub fn set_last_scanned_height(&mut self, height: u64) {
        self.last_scanned_height = height;
    }

    /// Submit a BridgeMint transaction to ARC Chain for a confirmed ETH lock.
    ///
    /// This creates a `BridgeTransfer` from the lock event, generates a proof
    /// placeholder, signs the relay message, and posts it to the ARC Chain RPC.
    pub async fn submit_bridge_mint(&self, lock: &LockEvent) -> Result<[u8; 32]> {
        // Parse the ARC token address.
        let token_bytes = hex::decode(ARC_TOKEN_ADDRESS)
            .context("invalid ARC_TOKEN_ADDRESS")?;
        let mut token_address: EvmAddress = [0u8; 20];
        token_address.copy_from_slice(&token_bytes);

        // Destination address on ARC Chain is the same as the sender on ETH.
        // In a production relayer, the user would specify the destination; for
        // now we bridge to the same address.
        let dest_address = lock.sender;

        let transfer = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            lock.sender,
            dest_address,
            token_address,
            lock.amount,
            lock.nonce,
        );

        let transfer_id = transfer.transfer_id;

        // Build the Merkle proof from the lock event's inclusion in the
        // Ethereum block. In production this would query an Ethereum full node
        // for the receipt trie proof; here we construct a minimal proof.
        let proof = self.build_eth_inclusion_proof(lock, &transfer_id)?;

        // Sign the relay message.
        let relay_msg = self.sign_relay_message(transfer, proof)?;

        // Serialize and submit.
        let tx_payload = self.encode_bridge_mint_tx(&relay_msg)?;

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "arc_submitTransaction",
            "params": [{
                "type": TX_TYPE_BRIDGE_MINT,
                "data": hex::encode(&tx_payload),
            }],
            "id": 1
        });

        let resp: serde_json::Value = self.client
            .post(&self.arc_rpc_url)
            .json(&body)
            .send()
            .await
            .context("failed to submit BridgeMint TX")?
            .json()
            .await
            .context("failed to parse BridgeMint response")?;

        if let Some(error) = resp.get("error") {
            anyhow::bail!("ARC RPC error: {}", error);
        }

        info!(
            transfer_id = hex::encode(transfer_id),
            nonce = lock.nonce,
            amount = lock.amount,
            "submitted BridgeMint TX"
        );

        Ok(transfer_id)
    }

    /// Poll ARC Chain for new BridgeLock transactions.
    pub async fn poll_bridge_locks(&mut self) -> Result<Vec<BridgeLockEvent>> {
        let current_height = self.get_current_height().await?;

        if current_height <= self.last_scanned_height {
            debug!(
                current_height,
                last_scanned = self.last_scanned_height,
                "no new ARC blocks"
            );
            return Ok(vec![]);
        }

        let from_height = self.last_scanned_height + 1;
        let to_height = current_height;

        info!(from_height, to_height, "scanning ARC Chain for BridgeLock TXs");

        let events = self.get_bridge_locks(from_height, to_height).await?;

        self.last_scanned_height = to_height;

        Ok(events)
    }

    /// Query the current ARC Chain block height.
    async fn get_current_height(&self) -> Result<u64> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "arc_blockHeight",
            "params": [],
            "id": 1
        });

        let resp: serde_json::Value = self.client
            .post(&self.arc_rpc_url)
            .json(&body)
            .send()
            .await
            .context("failed to query arc_blockHeight")?
            .json()
            .await
            .context("failed to parse arc_blockHeight response")?;

        let height = resp["result"]
            .as_u64()
            .context("missing result in arc_blockHeight")?;

        Ok(height)
    }

    /// Fetch BridgeLock transactions from ARC Chain in a block range.
    async fn get_bridge_locks(
        &self,
        from_height: u64,
        to_height: u64,
    ) -> Result<Vec<BridgeLockEvent>> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "arc_getTransactions",
            "params": [{
                "from_height": from_height,
                "to_height": to_height,
                "tx_type": TX_TYPE_BRIDGE_LOCK,
            }],
            "id": 1
        });

        let resp: serde_json::Value = self.client
            .post(&self.arc_rpc_url)
            .json(&body)
            .send()
            .await
            .context("failed to query arc_getTransactions")?
            .json()
            .await
            .context("failed to parse arc_getTransactions response")?;

        let txs = match resp["result"].as_array() {
            Some(arr) => arr,
            None => return Ok(vec![]),
        };

        let mut events = Vec::new();

        for tx in txs {
            match self.parse_bridge_lock_tx(tx) {
                Ok(event) => events.push(event),
                Err(e) => {
                    warn!(?e, "failed to parse BridgeLock TX, skipping");
                }
            }
        }

        info!(count = events.len(), "found BridgeLock transactions");
        Ok(events)
    }

    /// Parse a BridgeLock transaction from the ARC Chain RPC response.
    fn parse_bridge_lock_tx(&self, tx: &serde_json::Value) -> Result<BridgeLockEvent> {
        let sender_hex = tx["sender"]
            .as_str()
            .context("missing sender")?;
        let sender = parse_evm_address(sender_hex)?;

        let recipient_hex = tx["eth_recipient"]
            .as_str()
            .context("missing eth_recipient")?;
        let eth_recipient = parse_evm_address(recipient_hex)?;

        let amount = tx["amount"]
            .as_u64()
            .map(|v| v as u128)
            .or_else(|| {
                tx["amount"].as_str().and_then(|s| s.parse::<u128>().ok())
            })
            .context("missing or invalid amount")?;

        let nonce = tx["nonce"]
            .as_u64()
            .context("missing nonce")?;

        let block_height = tx["block_height"]
            .as_u64()
            .context("missing block_height")?;

        let tx_hash_hex = tx["hash"]
            .as_str()
            .unwrap_or("0x0000000000000000000000000000000000000000000000000000000000000000");
        let hash_bytes = hex::decode(
            tx_hash_hex.strip_prefix("0x").unwrap_or(tx_hash_hex),
        )?;
        let mut tx_hash = [0u8; 32];
        if hash_bytes.len() == 32 {
            tx_hash.copy_from_slice(&hash_bytes);
        }

        Ok(BridgeLockEvent {
            sender,
            eth_recipient,
            amount,
            nonce,
            block_height,
            tx_hash,
        })
    }

    /// Build a Merkle inclusion proof for a lock event on Ethereum.
    ///
    /// In a production deployment this would query an Ethereum archive node for
    /// the receipt trie proof. Here we construct a minimal single-leaf proof.
    fn build_eth_inclusion_proof(
        &self,
        lock: &LockEvent,
        transfer_id: &[u8; 32],
    ) -> Result<BridgeProof> {
        // Compute leaf hash from the lock event data.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&lock.sender);
        hasher.update(&lock.amount.to_le_bytes());
        hasher.update(&lock.nonce.to_le_bytes());
        let leaf_hash = *hasher.finalize().as_bytes();

        Ok(BridgeProof {
            transfer_id: *transfer_id,
            source_block_hash: lock.tx_hash, // Block hash would come from eth_getBlockByNumber
            source_block_height: lock.block_number,
            merkle_siblings: vec![], // Single-leaf tree in minimal proof
            leaf_index: lock.log_index,
            leaf_hash,
        })
    }

    /// Sign a BridgeRelayMessage with the relayer's Ed25519 key.
    fn sign_relay_message(
        &self,
        transfer: BridgeTransfer,
        proof: BridgeProof,
    ) -> Result<BridgeRelayMessage> {
        // Sign over transfer_id || leaf_hash (the proof commitment).
        let mut sign_data = Vec::with_capacity(64);
        sign_data.extend_from_slice(&transfer.transfer_id);
        sign_data.extend_from_slice(&proof.leaf_hash);

        let signature = self.signing_key.sign(&sign_data);

        Ok(BridgeRelayMessage {
            transfer,
            proof,
            relayer_address: self.relayer_address,
            relayer_signature: signature.to_bytes().to_vec(),
        })
    }

    /// Encode a BridgeRelayMessage into the binary payload for a BridgeMint TX.
    fn encode_bridge_mint_tx(&self, msg: &BridgeRelayMessage) -> Result<Vec<u8>> {
        let payload = serde_json::to_vec(msg)
            .context("failed to serialize BridgeRelayMessage")?;
        Ok(payload)
    }

    /// Build the calldata for calling `unlock()` on ArcBridge.sol.
    ///
    /// Encodes: unlock(address to, uint256 amount, uint256 nonce,
    ///          bytes32 stateRoot, bytes32[] proof)
    pub fn encode_unlock_calldata(
        &self,
        lock_event: &BridgeLockEvent,
        state_root: [u8; 32],
        merkle_proof: Vec<[u8; 32]>,
    ) -> Result<Vec<u8>> {
        // Function selector: keccak256("unlock(address,uint256,uint256,bytes32,bytes32[])")
        // = 0x... (first 4 bytes)
        // We use a pre-computed selector.
        let selector: [u8; 4] = {
            let hash = blake3::hash(b"unlock(address,uint256,uint256,bytes32,bytes32[])");
            let mut sel = [0u8; 4];
            sel.copy_from_slice(&hash.as_bytes()[..4]);
            sel
        };

        let mut calldata = Vec::new();
        calldata.extend_from_slice(&selector);

        // Encode `to` (address, padded to 32 bytes).
        let mut to_word = [0u8; 32];
        to_word[12..32].copy_from_slice(&lock_event.eth_recipient);
        calldata.extend_from_slice(&to_word);

        // Encode `amount` (uint256).
        let mut amount_word = [0u8; 32];
        amount_word[16..32].copy_from_slice(&lock_event.amount.to_be_bytes());
        calldata.extend_from_slice(&amount_word);

        // Encode `nonce` (uint256).
        let mut nonce_word = [0u8; 32];
        nonce_word[24..32].copy_from_slice(&lock_event.nonce.to_be_bytes());
        calldata.extend_from_slice(&nonce_word);

        // Encode `stateRoot` (bytes32).
        calldata.extend_from_slice(&state_root);

        // Encode `proof` (bytes32[] — dynamic type).
        // Offset to the dynamic data (5 * 32 = 160 = 0xa0).
        let mut offset_word = [0u8; 32];
        offset_word[31] = 0xa0;
        calldata.extend_from_slice(&offset_word);

        // Length of the proof array.
        let mut len_word = [0u8; 32];
        let proof_len = merkle_proof.len() as u64;
        len_word[24..32].copy_from_slice(&proof_len.to_be_bytes());
        calldata.extend_from_slice(&len_word);

        // Each proof element.
        for sibling in &merkle_proof {
            calldata.extend_from_slice(sibling);
        }

        Ok(calldata)
    }

    /// Submit an unlock transaction to Ethereum via eth_sendRawTransaction.
    ///
    /// In production this would sign an EIP-1559 transaction with the relayer's
    /// ETH private key. Here we construct the JSON-RPC call.
    pub async fn submit_eth_unlock(
        &self,
        eth_rpc_url: &str,
        bridge_contract: &str,
        lock_event: &BridgeLockEvent,
        state_root: [u8; 32],
        merkle_proof: Vec<[u8; 32]>,
    ) -> Result<[u8; 32]> {
        let calldata = self.encode_unlock_calldata(lock_event, state_root, merkle_proof)?;

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_sendTransaction",
            "params": [{
                "to": bridge_contract,
                "data": format!("0x{}", hex::encode(&calldata)),
            }],
            "id": 1
        });

        let resp: serde_json::Value = self.client
            .post(eth_rpc_url)
            .json(&body)
            .send()
            .await
            .context("failed to submit unlock TX to Ethereum")?
            .json()
            .await
            .context("failed to parse unlock TX response")?;

        if let Some(error) = resp.get("error") {
            anyhow::bail!("ETH RPC error on unlock: {}", error);
        }

        let tx_hash_hex = resp["result"]
            .as_str()
            .context("missing tx hash in unlock response")?;

        let hash_bytes = hex::decode(
            tx_hash_hex.strip_prefix("0x").unwrap_or(tx_hash_hex),
        )?;
        let mut tx_hash = [0u8; 32];
        if hash_bytes.len() == 32 {
            tx_hash.copy_from_slice(&hash_bytes);
        }

        info!(
            tx_hash = hex::encode(tx_hash),
            nonce = lock_event.nonce,
            amount = lock_event.amount,
            "submitted unlock TX to Ethereum"
        );

        Ok(tx_hash)
    }
}

/// Parse a hex address string into a 20-byte array.
fn parse_evm_address(hex_str: &str) -> Result<EvmAddress> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(stripped).context("invalid hex address")?;
    if bytes.len() != 20 {
        anyhow::bail!("address must be 20 bytes, got {}", bytes.len());
    }
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&bytes);
    Ok(addr)
}
