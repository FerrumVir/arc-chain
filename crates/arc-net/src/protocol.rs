//! Wire protocol — message framing for QUIC streams.
//!
//! Every message on a QUIC stream is framed as:
//!   [1 byte type][4 bytes payload length (u32 BE)][N bytes bincode payload]

use arc_consensus::DagBlock;
use arc_crypto::Hash256;
use arc_types::Transaction;
use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// ─── Message Types ──────────────────────────────────────────────────────────

/// Discriminant for framed messages.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Handshake = 0x01,
    HandshakeAck = 0x02,
    DagBlockWithTxs = 0x03,
    TxGossip = 0x04,
    /// State diff from a proposer node (Propose-Verify protocol).
    StateDiff = 0x05,
    /// Peer Exchange — share known peer list for dynamic discovery.
    PeerExchange = 0x06,
    /// State Sync — request the snapshot manifest from a peer.
    SnapshotManifestRequest = 0x07,
    /// State Sync — response with snapshot manifest.
    SnapshotManifestResponse = 0x08,
    /// State Sync — request a single snapshot chunk by index.
    SnapshotChunkRequest = 0x09,
    /// State Sync — response with a snapshot chunk.
    SnapshotChunkResponse = 0x0A,
    /// Inference request — routed to peers with GPU/model capability.
    InferenceRequest = 0x0B,
    /// Inference response — result from a community GPU node.
    InferenceResponse = 0x0C,
}

impl MessageType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Handshake),
            0x02 => Some(Self::HandshakeAck),
            0x03 => Some(Self::DagBlockWithTxs),
            0x04 => Some(Self::TxGossip),
            0x05 => Some(Self::StateDiff),
            0x06 => Some(Self::PeerExchange),
            0x07 => Some(Self::SnapshotManifestRequest),
            0x08 => Some(Self::SnapshotManifestResponse),
            0x09 => Some(Self::SnapshotChunkRequest),
            0x0A => Some(Self::SnapshotChunkResponse),
            0x0B => Some(Self::InferenceRequest),
            0x0C => Some(Self::InferenceResponse),
            _ => None,
        }
    }
}

// ─── Message Payloads ───────────────────────────────────────────────────────

/// Exchanged on peer connection.
///
/// Each peer proves identity by signing a random nonce with their validator key.
/// The receiver verifies: (1) public_key hashes to validator_address,
/// (2) challenge_sig is a valid Ed25519 signature over
/// `BLAKE3("ARC-peer-auth-v1" || nonce || genesis_hash)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeMessage {
    pub validator_address: Hash256,
    pub stake: u64,
    pub listen_port: u16,
    pub genesis_hash: Hash256,
    /// Ed25519 public key bytes (32 bytes). Receiver verifies it hashes to validator_address.
    pub public_key: Vec<u8>,
    /// Random 32-byte nonce (prevents replay attacks).
    pub nonce: [u8; 32],
    /// Ed25519 signature over BLAKE3("ARC-peer-auth-v1" || nonce || genesis_hash).
    /// Proves the sender controls the private key for validator_address.
    pub challenge_sig: Vec<u8>,
}

/// A DAG block bundled with the full transaction bodies it references,
/// so the receiving node can resolve tx hashes without a separate lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagBlockWithTxsMessage {
    pub block: DagBlock,
    pub transactions: Vec<Transaction>,
}

/// Gossip batch of serialized transactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxGossipMessage {
    pub transactions: Vec<Vec<u8>>,
}

/// State diff broadcast from a proposer node (Propose-Verify protocol).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDiffMessage {
    pub block_hash: Hash256,
    pub diff: arc_types::StateDiff,
    pub block_height: u64,
}

/// Peer Exchange message — shares a list of known peers for dynamic discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerExchangeMessage {
    pub peers: Vec<PexPeerInfo>,
}

/// State Sync — request snapshot manifest from a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifestRequestMessage {
    /// Optionally request a snapshot at a specific height (0 = latest).
    pub prefer_height: u64,
}

/// State Sync — response with snapshot manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifestResponseMessage {
    /// The manifest describing the chunked snapshot.
    pub manifest: arc_state::SnapshotManifest,
}

/// State Sync — request a single snapshot chunk by index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotChunkRequestMessage {
    /// BLAKE3 hash of the manifest (to identify which snapshot).
    pub manifest_hash: Hash256,
    /// Zero-based chunk index.
    pub chunk_index: u32,
}

/// State Sync — response with a snapshot chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotChunkResponseMessage {
    /// The snapshot chunk data (includes BLAKE3 proof for verification).
    pub chunk: arc_state::StateSnapshot,
}

/// Inference request — broadcast to peers with model capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequestMessage {
    /// Unique request ID (BLAKE3 hash of input + timestamp).
    pub request_id: Hash256,
    /// The input prompt / tokens.
    pub input: String,
    /// Max tokens to generate.
    pub max_tokens: u32,
    /// Requester's validator address (for response routing).
    pub requester: Hash256,
}

/// Inference response — result from a community GPU node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponseMessage {
    /// Matches the request_id from InferenceRequestMessage.
    pub request_id: Hash256,
    /// Generated output text.
    pub output: String,
    /// BLAKE3 hash of the output (deterministic — identical on all hardware).
    pub output_hash: Hash256,
    /// Model hash (identifies which model produced this output).
    pub model_hash: Hash256,
    /// Milliseconds per token.
    pub ms_per_token: u64,
    /// Responder's validator address.
    pub responder: Hash256,
}

/// Compact peer info exchanged via PEX protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PexPeerInfo {
    pub address: Hash256,
    pub socket_addr: String,
    pub stake: u64,
}

// ─── Framing ────────────────────────────────────────────────────────────────

/// Maximum message payload size (16 MiB — generous for large blocks).
const MAX_PAYLOAD_SIZE: u32 = 16 * 1024 * 1024;

/// Write a framed message to a QUIC send stream.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: MessageType,
    payload: &[u8],
) -> io::Result<()> {
    let len = payload.len() as u32;
    writer.write_u8(msg_type as u8).await?;
    writer.write_u32(len).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a framed message from a QUIC recv stream.
///
/// Returns `(MessageType, payload_bytes)`.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<(MessageType, Vec<u8>)> {
    let type_byte = reader.read_u8().await?;
    let msg_type = MessageType::from_u8(type_byte)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown message type"))?;

    let len = reader.read_u32().await?;
    if len > MAX_PAYLOAD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload too large: {} bytes", len),
        ));
    }

    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok((msg_type, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_framing() {
        let payload = b"hello world";
        let mut buf = Vec::new();
        write_message(&mut buf, MessageType::Handshake, payload)
            .await
            .unwrap();

        let mut cursor = io::Cursor::new(buf);
        let (msg_type, data) = read_message(&mut cursor).await.unwrap();
        assert_eq!(msg_type, MessageType::Handshake);
        assert_eq!(data, payload);
    }

    #[tokio::test]
    async fn reject_unknown_type() {
        let buf = vec![0xFF, 0, 0, 0, 0]; // unknown type, zero-length payload
        let mut cursor = io::Cursor::new(buf);
        let result = read_message(&mut cursor).await;
        assert!(result.is_err());
    }
}
