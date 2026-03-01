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
}

impl MessageType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Handshake),
            0x02 => Some(Self::HandshakeAck),
            0x03 => Some(Self::DagBlockWithTxs),
            0x04 => Some(Self::TxGossip),
            _ => None,
        }
    }
}

// ─── Message Payloads ───────────────────────────────────────────────────────

/// Exchanged on peer connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeMessage {
    pub validator_address: Hash256,
    pub stake: u64,
    pub listen_port: u16,
    pub genesis_hash: Hash256,
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
