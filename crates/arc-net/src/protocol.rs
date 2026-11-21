use arc_consensus::DagBlock;
use arc_crypto::Hash256;
use arc_types::Transaction;
use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Handshake = 0x01,
    HandshakeAck = 0x02,
    DagBlockWithTxs = 0x03,
    TxGossip = 0x04,
    StateDiff = 0x05,
    PeerExchange = 0x06,
    SnapshotManifestRequest = 0x07,
    SnapshotManifestResponse = 0x08,
    SnapshotChunkRequest = 0x09,
    SnapshotChunkResponse = 0x0A,
    InferenceRequest = 0x0B,
    InferenceResponse = 0x0C,
    Heartbeat = 0x0D,
    ShardForward = 0x0E,
    ShardResult = 0x0F,
    ShardAnnounce = 0x10,
    RoundSyncRequest = 0x11,
    RoundSyncResponse = 0x12,
    ConsensusAbsenceVote = 0x13,
    ConsensusAbsenceCertificate = 0x14,
    ConsensusFinalityVote = 0x15,
    ConsensusFinalityCertificate = 0x16,
    DagHistoryRequest = 0x17,
    DagHistoryResponse = 0x18,
    CheckpointRequest = 0x19,
    CheckpointResponse = 0x1A,
    NativeInferenceVote = 0x1B,
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
            0x13 => Some(Self::ConsensusAbsenceVote),
            0x14 => Some(Self::ConsensusAbsenceCertificate),
            0x15 => Some(Self::ConsensusFinalityVote),
            0x16 => Some(Self::ConsensusFinalityCertificate),
            0x17 => Some(Self::DagHistoryRequest),
            0x18 => Some(Self::DagHistoryResponse),
            0x19 => Some(Self::CheckpointRequest),
            0x1A => Some(Self::CheckpointResponse),
            0x1B => Some(Self::NativeInferenceVote),
            _ => None,
        }
    }
}

pub const PROTOCOL_VERSION: u32 = 3;
pub const MIN_COMPATIBLE_VERSION: u32 = 3;

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
    pub public_key: Vec<u8>,
    pub nonce: [u8; 32],
    pub challenge_sig: Vec<u8>,
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub min_compatible_version: u32,
    #[serde(default)]
    pub dag_round: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagBlockWithTxsMessage {
    pub block: DagBlock,
    pub transactions: Vec<Transaction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagHistoryRequestMessage {
    pub from_round: u64,
    pub max_rounds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagHistoryResponseMessage {
    pub blocks: Vec<DagBlock>,
    pub transactions: Vec<Transaction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRequestMessage {
    pub needed_below_height: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointResponseMessage {
    pub envelope: arc_consensus::view_change::CheckpointEnvelope,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeInferenceVoteMessage {
    pub request_id: arc_crypto::Hash256,
    pub tokens: Vec<u32>,
    pub vote: arc_types::inference_contract::InferenceVote,
}

pub const MAX_NATIVE_VOTE_TOKENS: usize = 65_536;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusAbsenceVoteMessage {
    pub vote: arc_consensus::view_change::SkipVote,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusAbsenceCertificateMessage {
    pub certificate: arc_consensus::view_change::SkipCertificate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusFinalityVoteMessage {
    pub vote: arc_consensus::view_change::FinalityVote,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusFinalityCertificateMessage {
    pub certificate: arc_consensus::view_change::FinalityCertificate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxGossipMessage {
    pub transactions: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDiffMessage {
    pub block_hash: Hash256,
    pub diff: arc_types::StateDiff,
    pub block_height: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerExchangeMessage {
    pub peers: Vec<PexPeerInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifestRequestMessage {
    pub prefer_height: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifestResponseMessage {
    pub manifest: arc_state::SnapshotManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotChunkRequestMessage {
    pub manifest_hash: Hash256,
    pub chunk_index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotChunkResponseMessage {
    pub chunk: arc_state::StateSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequestMessage {
    pub request_id: Hash256,
    pub input: String,
    pub max_tokens: u32,
    pub requester: Hash256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponseMessage {
    pub request_id: Hash256,
    pub output: String,
    pub output_hash: Hash256,
    pub model_hash: Hash256,
    pub ms_per_token: u64,
    pub responder: Hash256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PexPeerInfo {
    pub address: Hash256,
    pub socket_addr: String,
    pub stake: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardForwardMessage {
    pub request_id: Hash256,
    pub model_id: Hash256,
    pub next_layer: u32,
    pub total_layers: u32,
    pub token_position: u32,
    pub activations: Vec<u8>,
    pub activation_hash: Hash256,
    pub kv_cache_update: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardResultMessage {
    pub request_id: Hash256,
    pub token_id: u32,
    pub logits_hash: Hash256,
    pub responder: Hash256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardAnnounceMessage {
    pub model_id: Hash256,
    pub start_layer: u32,
    pub end_layer: u32,
    pub expert_indices: Vec<u32>,
    pub node_address: Hash256,
    pub available_memory: u64,
    pub gpu_tier: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundSyncRequestMessage {
    pub my_round: u64,
    pub my_committed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundSyncResponseMessage {
    pub current_round: u64,
    pub last_committed_round: u64,
    pub validator_count: u32,
    pub total_stake: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatMessage {
    pub dag_round: u64,
    pub committed_round: u64,
    #[serde(default)]
    pub protocol_version: u32,
}

pub(crate) const MAX_PAYLOAD_SIZE: u32 = 16 * 1024 * 1024;

pub(crate) async fn read_message_header<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<(MessageType, u32)> {
    let type_byte = reader.read_u8().await?;
    let len = reader.read_u32().await?;
    if len > MAX_PAYLOAD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload too large: {len} bytes"),
        ));
    }
    let msg_type = MessageType::from_u8(type_byte).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown message type: 0x{type_byte:02x}"),
        )
    })?;
    Ok((msg_type, len))
}

pub(crate) async fn read_message_payload<R: AsyncRead + Unpin>(
    reader: &mut R,
    len: u32,
) -> io::Result<Vec<u8>> {
    debug_assert!(len <= MAX_PAYLOAD_SIZE);
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

pub(crate) fn deserialize_message<'a, T>(payload: &'a [u8]) -> bincode::Result<T>
where
    T: Deserialize<'a>,
{
    bincode::deserialize_limited_exact::<T, { MAX_PAYLOAD_SIZE as usize }>(payload)
}

pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: MessageType,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD_SIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("payload too large: {} bytes", payload.len()),
        ));
    }
    let len = payload.len() as u32;
    writer.write_u8(msg_type as u8).await?;
    writer.write_u32(len).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<(MessageType, Vec<u8>)> {
    let (msg_type, len) = read_message_header(reader).await?;
    let payload = read_message_payload(reader, len).await?;
    Ok((msg_type, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, PartialEq, Serialize)]
    struct StringPayload {
        text: String,
    }

    #[test]
    fn v3_cutover_is_mutually_incompatible_with_v2() {
        assert_eq!(PROTOCOL_VERSION, 3);
        assert_eq!(MIN_COMPATIBLE_VERSION, 3);

        assert!(!protocol_ranges_overlap(
            PROTOCOL_VERSION,
            MIN_COMPATIBLE_VERSION,
            2,
            1,
        ));

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
    async fn reject_unknown_type_before_reading_its_payload() {
        let buf = vec![0xFF, 0, 0, 0, 4];
        let mut cursor = io::Cursor::new(buf);
        let error = read_message(&mut cursor).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unknown message type"));
    }

    #[tokio::test]
    async fn reject_oversized_header_before_allocating_payload() {
        let mut buf = vec![MessageType::DagBlockWithTxs as u8];
        buf.extend_from_slice(&(MAX_PAYLOAD_SIZE + 1).to_be_bytes());
        let mut cursor = io::Cursor::new(buf);
        let error = read_message(&mut cursor).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("payload too large"));
    }

    #[tokio::test]
    async fn writer_rejects_oversized_payload_without_writing_a_header() {
        let payload = vec![0_u8; MAX_PAYLOAD_SIZE as usize + 1];
        let mut output = Vec::new();
        let error = write_message(&mut output, MessageType::DagBlockWithTxs, &payload)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(output.is_empty());
    }

    #[test]
    fn bounded_deserializer_rejects_forged_string_length() {
        let forged = (u64::from(MAX_PAYLOAD_SIZE) + 1).to_le_bytes();
        assert!(deserialize_message::<StringPayload>(&forged).is_err());
    }

    #[test]
    fn bounded_deserializer_accepts_exact_canonical_payload_only() {
        let value = StringPayload {
            text: "bounded".to_string(),
        };
        let canonical = bincode::serialize(&value).unwrap();
        assert_eq!(
            deserialize_message::<StringPayload>(&canonical).unwrap(),
            value
        );

        let mut suffixed = canonical;
        suffixed.push(0);
        assert!(deserialize_message::<StringPayload>(&suffixed).is_err());
    }
}
