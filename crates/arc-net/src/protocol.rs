//! Wire protocol - message framing for QUIC streams.
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
    /// Peer Exchange - share known peer list for dynamic discovery.
    PeerExchange = 0x06,
    /// State Sync - request the snapshot manifest from a peer.
    SnapshotManifestRequest = 0x07,
    /// State Sync - response with snapshot manifest.
    SnapshotManifestResponse = 0x08,
    /// State Sync - request a single snapshot chunk by index.
    SnapshotChunkRequest = 0x09,
    /// State Sync - response with a snapshot chunk.
    SnapshotChunkResponse = 0x0A,
    /// Inference request - routed to peers with GPU/model capability.
    InferenceRequest = 0x0B,
    /// Inference response - result from a community GPU node.
    InferenceResponse = 0x0C,
    /// Heartbeat - lightweight liveness probe with round info.
    /// Sent during reconnect to detect dead QUIC streams and partitions.
    Heartbeat = 0x0D,
    /// Shard activation forward - send layer activations to next shard holder.
    ShardForward = 0x0E,
    /// Shard result - final logits/token from last shard back to coordinator.
    ShardResult = 0x0F,
    /// Shard registration - announce which model layers this node holds.
    ShardAnnounce = 0x10,
    /// DAG round sync request - ask peer for their current round state.
    RoundSyncRequest = 0x11,
    /// DAG round sync response - reply with current round and committed round.
    RoundSyncResponse = 0x12,
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
            0x0D => Some(Self::Heartbeat),
            0x0E => Some(Self::ShardForward),
            0x0F => Some(Self::ShardResult),
            0x10 => Some(Self::ShardAnnounce),
            0x11 => Some(Self::RoundSyncRequest),
            0x12 => Some(Self::RoundSyncResponse),
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
/// Current wire protocol version. Version 3 is the explicit cutover for the
/// incompatible consensus/network behavior introduced after the v2 fleet.
pub const PROTOCOL_VERSION: u32 = 3;
/// Minimum protocol version we can talk to.
///
/// This intentionally equals [`PROTOCOL_VERSION`] for the v3 cutover. A v3
/// node rejects v1/v2, and the advertised minimum makes an unmodified v2 node
/// reject v3 as too new, preventing a mixed fleet from appearing connected
/// while interpreting consensus traffic differently.
pub const MIN_COMPATIBLE_VERSION: u32 = 3;

/// Whether the inclusive protocol ranges advertised by two peers overlap.
///
/// Kept as the single compatibility predicate so the handshake path and the
/// cutover regression tests cannot drift into one-sided version acceptance.
pub(crate) const fn protocol_ranges_overlap(
    local_current: u32,
    local_minimum: u32,
    peer_current: u32,
    peer_minimum: u32,
) -> bool {
    peer_minimum <= local_current && local_minimum <= peer_current
}

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
    /// Protocol version (added in v2). Old nodes deserializing via bincode
    /// will use serde default (0) which we treat as v1.
    #[serde(default)]
    pub protocol_version: u32,
    /// Minimum version this node can interoperate with.
    #[serde(default)]
    pub min_compatible_version: u32,
    /// Current DAG round (for partition detection on connect).
    #[serde(default)]
    pub dag_round: u64,
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

/// Peer Exchange message - shares a list of known peers for dynamic discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerExchangeMessage {
    pub peers: Vec<PexPeerInfo>,
}

/// State Sync - request snapshot manifest from a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifestRequestMessage {
    /// Optionally request a snapshot at a specific height (0 = latest).
    pub prefer_height: u64,
}

/// State Sync - response with snapshot manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifestResponseMessage {
    /// The manifest describing the chunked snapshot.
    pub manifest: arc_state::SnapshotManifest,
}

/// State Sync - request a single snapshot chunk by index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotChunkRequestMessage {
    /// BLAKE3 hash of the manifest (to identify which snapshot).
    pub manifest_hash: Hash256,
    /// Zero-based chunk index.
    pub chunk_index: u32,
}

/// State Sync - response with a snapshot chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotChunkResponseMessage {
    /// The snapshot chunk data (includes BLAKE3 proof for verification).
    pub chunk: arc_state::StateSnapshot,
}

/// Inference request - broadcast to peers with model capability.
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

/// Inference response - result from a community GPU node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponseMessage {
    /// Matches the request_id from InferenceRequestMessage.
    pub request_id: Hash256,
    /// Generated output text.
    pub output: String,
    /// BLAKE3 hash of the output (deterministic - identical on all hardware).
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

// ─── Distributed Inference (Model Sharding) Messages ──────────────────────

/// Forward activations from one shard holder to the next in pipeline-parallel inference.
/// The coordinator assigns each node a range of transformer layers. After computing
/// its layers, the node sends the hidden state to the next shard holder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardForwardMessage {
    /// Unique inference request this activation belongs to.
    pub request_id: Hash256,
    /// Model being inferred (BLAKE3 of weights).
    pub model_id: Hash256,
    /// The layer index where the NEXT node should resume (exclusive end of sender's range).
    pub next_layer: u32,
    /// Total layers in the model (so receiver knows when it has the last shard).
    pub total_layers: u32,
    /// Current token position in the sequence.
    pub token_position: u32,
    /// Hidden state activations (i64 fixed-point Q16, little-endian bytes).
    /// Size: d_model * 8 bytes.
    pub activations: Vec<u8>,
    /// BLAKE3 hash of `activations` for integrity verification.
    pub activation_hash: Hash256,
    /// KV cache entries for layers computed so far (compressed).
    /// Empty on first forward; populated as pipeline progresses.
    pub kv_cache_update: Vec<u8>,
}

/// Result from the final shard holder back to the coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardResultMessage {
    /// Matches the request_id from ShardForwardMessage.
    pub request_id: Hash256,
    /// The generated token ID (argmax of final logits).
    pub token_id: u32,
    /// BLAKE3 hash of the full logits vector (for determinism verification).
    pub logits_hash: Hash256,
    /// Responder's validator address.
    pub responder: Hash256,
}

/// Announce which model layers/experts this node holds.
/// Broadcast on join and when shard assignment changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardAnnounceMessage {
    /// The model this shard belongs to.
    pub model_id: Hash256,
    /// Layer range this node holds: [start_layer, end_layer).
    pub start_layer: u32,
    pub end_layer: u32,
    /// For MoE: which expert indices this node holds (empty for dense models).
    pub expert_indices: Vec<u32>,
    /// Node's validator address.
    pub node_address: Hash256,
    /// Available memory (bytes) for additional shards.
    pub available_memory: u64,
    /// GPU capability tier (0 = CPU only, 1-4 per existing tier system).
    pub gpu_tier: u8,
}

/// Request a peer's current DAG round state (for partition detection/healing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundSyncRequestMessage {
    /// Requester's current round (so peer can see the gap).
    pub my_round: u64,
    pub my_committed: u64,
}

/// Response with current DAG round state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundSyncResponseMessage {
    pub current_round: u64,
    pub last_committed_round: u64,
    pub validator_count: u32,
    pub total_stake: u64,
}

/// Heartbeat payload - now includes round info for partition detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatMessage {
    /// Sender's current DAG round.
    pub dag_round: u64,
    /// Sender's last committed round.
    pub committed_round: u64,
    /// Sender's protocol version.
    #[serde(default)]
    pub protocol_version: u32,
}

// ─── Framing ────────────────────────────────────────────────────────────────

/// Maximum message payload size (16 MiB - generous for large blocks).
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
/// Unknown message types are properly skipped (reads and discards the payload)
/// to keep the stream synchronized for forward compatibility.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<(MessageType, Vec<u8>)> {
    loop {
        let type_byte = reader.read_u8().await?;
        let len = reader.read_u32().await?;
        if len > MAX_PAYLOAD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload too large: {} bytes", len),
            ));
        }

        match MessageType::from_u8(type_byte) {
            Some(msg_type) => {
                let mut buf = vec![0u8; len as usize];
                reader.read_exact(&mut buf).await?;
                return Ok((msg_type, buf));
            }
            None => {
                // Unknown message type from a newer protocol version.
                // Read and discard the payload to keep the stream in sync,
                // then continue reading the next message.
                let mut discard = vec![0u8; len as usize];
                reader.read_exact(&mut discard).await?;
                tracing::debug!(
                    "Skipped unknown message type 0x{:02x} ({} bytes) - peer may be newer version",
                    type_byte,
                    len
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_cutover_is_mutually_incompatible_with_v2() {
        assert_eq!(PROTOCOL_VERSION, 3);
        assert_eq!(MIN_COMPATIBLE_VERSION, 3);

        // New v3 node receives the version range advertised by the old v2
        // implementation (current=2, minimum=1).
        assert!(!protocol_ranges_overlap(
            PROTOCOL_VERSION,
            MIN_COMPATIBLE_VERSION,
            2,
            1,
        ));

        // Old v2 node receives the new v3 range (current=3, minimum=3).
        assert!(!protocol_ranges_overlap(
            2,
            1,
            PROTOCOL_VERSION,
            MIN_COMPATIBLE_VERSION,
        ));
    }

    #[test]
    fn v3_peers_remain_compatible_with_each_other() {
        assert!(protocol_ranges_overlap(
            PROTOCOL_VERSION,
            MIN_COMPATIBLE_VERSION,
            PROTOCOL_VERSION,
            MIN_COMPATIBLE_VERSION,
        ));
    }

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
