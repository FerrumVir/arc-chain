pub mod protocol;
pub mod transport;

pub use protocol::{
    DagBlockWithTxsMessage, HandshakeMessage, MessageType, PeerExchangeMessage, PexPeerInfo,
    SnapshotChunkRequestMessage, SnapshotChunkResponseMessage, SnapshotManifestRequestMessage,
    SnapshotManifestResponseMessage, TxGossipMessage as GossipMessage,
};
pub use transport::{InboundMessage, OutboundMessage, run_transport};

use arc_crypto::{Hash256, hash_bytes};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::SocketAddr;
use thiserror::Error;
use tracing::{debug, info, warn};

pub const SHRED_DATA_SIZE: usize = 1280;

pub const MAX_TX_PER_GOSSIP: usize = 100;

pub const DEFAULT_FANOUT: usize = 8;

#[derive(Debug, Error)]
pub enum NetError {
    #[error("invalid shred: {0}")]
    InvalidShred(String),

    #[error("insufficient shreds: needed {needed}, got {got}")]
    InsufficientShreds { needed: u16, got: u16 },

    #[error("mismatched block hash across shreds")]
    MismatchedBlockHash,

    #[error("peer not found")]
    PeerNotFound,

    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("serialization error: {0}")]
    SerializationError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShredMessage {
    pub block_hash: Hash256,
    pub block_height: u64,
    pub shard_id: u16,
    pub shred_index: u16,
    pub total_shreds: u16,
    pub coding_shreds: u16,
    pub data: Vec<u8>,
}

pub struct ShredEncoder;

impl ShredEncoder {
    pub fn encode(
        block_data: &[u8],
        block_hash: Hash256,
        block_height: u64,
        shard_id: u16,
    ) -> Vec<ShredMessage> {
        if block_data.is_empty() {
            return vec![ShredMessage {
                block_hash,
                block_height,
                shard_id,
                shred_index: 0,
                total_shreds: 1,
                coding_shreds: 0,
                data: Vec::new(),
            }];
        }

        let total_shreds = block_data.len().div_ceil(SHRED_DATA_SIZE) as u16;
        let num_parity = (total_shreds as usize).div_ceil(2);

        let mut data_shreds: Vec<ShredMessage> = block_data
            .chunks(SHRED_DATA_SIZE)
            .enumerate()
            .map(|(i, chunk)| ShredMessage {
                block_hash,
                block_height,
                shard_id,
                shred_index: i as u16,
                total_shreds,
                coding_shreds: num_parity as u16,
                data: chunk.to_vec(),
            })
            .collect();

        for pair_idx in 0..num_parity {
            let a_idx = pair_idx * 2;
            let b_idx = a_idx + 1;

            let a_data = &data_shreds[a_idx].data;
            let parity_data = if b_idx < data_shreds.len() {
                let b_data = &data_shreds[b_idx].data;
                let max_len = a_data.len().max(b_data.len());
                let mut xor = vec![0u8; max_len];
                for i in 0..max_len {
                    let a_byte = if i < a_data.len() { a_data[i] } else { 0 };
                    let b_byte = if i < b_data.len() { b_data[i] } else { 0 };
                    xor[i] = a_byte ^ b_byte;
                }
                xor
            } else {
                a_data.clone()
            };

            data_shreds.push(ShredMessage {
                block_hash,
                block_height,
                shard_id,
                shred_index: total_shreds + pair_idx as u16,
                total_shreds,
                coding_shreds: num_parity as u16,
                data: parity_data,
            });
        }

        data_shreds
    }

    pub fn decode(shreds: &[ShredMessage]) -> Result<Vec<u8>, NetError> {
        if shreds.is_empty() {
            return Err(NetError::InvalidShred("no shreds provided".into()));
        }

        let expected_hash = shreds[0].block_hash;
        let total_shreds = shreds[0].total_shreds;

        for s in shreds {
            if s.block_hash != expected_hash {
                return Err(NetError::MismatchedBlockHash);
            }
        }

        let mut data_slots: Vec<Option<Vec<u8>>> = vec![None; total_shreds as usize];
        let num_parity = (total_shreds as usize).div_ceil(2);
        let mut parity_slots: Vec<Option<Vec<u8>>> = vec![None; num_parity];

        for s in shreds {
            if s.shred_index < total_shreds {
                data_slots[s.shred_index as usize] = Some(s.data.clone());
            } else {
                let parity_idx = (s.shred_index - total_shreds) as usize;
                if parity_idx < num_parity {
                    parity_slots[parity_idx] = Some(s.data.clone());
                }
            }
        }

        for (pair_idx, parity_slot) in parity_slots.iter().enumerate() {
            let a_idx = pair_idx * 2;
            let b_idx = a_idx + 1;

            let a_present = data_slots[a_idx].is_some();
            let b_present = b_idx < total_shreds as usize && data_slots[b_idx].is_some();
            let parity_present = parity_slot.is_some();

            if a_present && b_present {
                continue;
            }

            if b_idx >= total_shreds as usize {
                if !a_present && parity_present {
                    data_slots[a_idx] = parity_slot.clone();
                }
                continue;
            }

            if !a_present && b_present && parity_present {
                let b_data = data_slots[b_idx].as_ref().unwrap();
                let parity = parity_slot.as_ref().unwrap();
                let max_len = b_data.len().max(parity.len());
                let mut recovered = vec![0u8; max_len];
                for i in 0..max_len {
                    let b_byte = if i < b_data.len() { b_data[i] } else { 0 };
                    let p_byte = if i < parity.len() { parity[i] } else { 0 };
                    recovered[i] = b_byte ^ p_byte;
                }
                data_slots[a_idx] = Some(recovered);
            } else if a_present && !b_present && parity_present {
                let a_data = data_slots[a_idx].as_ref().unwrap();
                let parity = parity_slot.as_ref().unwrap();
                let max_len = a_data.len().max(parity.len());
                let mut recovered = vec![0u8; max_len];
                for i in 0..max_len {
                    let a_byte = if i < a_data.len() { a_data[i] } else { 0 };
                    let p_byte = if i < parity.len() { parity[i] } else { 0 };
                    recovered[i] = a_byte ^ p_byte;
                }
                data_slots[b_idx] = Some(recovered);
            }
        }

        let mut unique_count: u16 = 0;
        for slot in &data_slots {
            if slot.is_some() {
                unique_count += 1;
            }
        }
        if unique_count < total_shreds {
            return Err(NetError::InsufficientShreds {
                needed: total_shreds,
                got: unique_count,
            });
        }

        let mut block_data = Vec::with_capacity(total_shreds as usize * SHRED_DATA_SIZE);
        for slot in &data_slots {
            block_data.extend_from_slice(slot.as_ref().unwrap());
        }

        Ok(block_data)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxGossipMessage {
    pub transactions: Vec<Vec<u8>>,
    pub sender: Hash256,
}

impl TxGossipMessage {
    pub fn new(transactions: Vec<Vec<u8>>, sender: Hash256) -> Vec<Self> {
        if transactions.is_empty() {
            return vec![Self {
                transactions: Vec::new(),
                sender,
            }];
        }

        transactions
            .chunks(MAX_TX_PER_GOSSIP)
            .map(|chunk| Self {
                transactions: chunk.to_vec(),
                sender,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub address: Hash256,
    pub socket_addr: SocketAddr,
    pub stake: u64,
    pub shard: u16,
    pub last_seen: u64,
    pub is_validator: bool,
}

impl fmt::Display for PeerInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Peer({}, shard={}, stake={}, validator={})",
            self.address, self.shard, self.stake, self.is_validator
        )
    }
}

pub struct PeerManager {
    peers: DashMap<Hash256, PeerInfo>,
    known_tx_hashes: DashMap<Hash256, ()>,
}

impl PeerManager {
    pub fn new() -> Self {
        Self {
            peers: DashMap::new(),
            known_tx_hashes: DashMap::new(),
        }
    }

    pub fn add_peer(&self, info: PeerInfo) {
        debug!(address = %info.address, shard = info.shard, "adding peer");
        self.peers.insert(info.address, info);
    }

    pub fn remove_peer(&self, address: &Hash256) -> bool {
        let removed = self.peers.remove(address).is_some();
        if removed {
            debug!(address = %address, "removed peer");
        }
        removed
    }

    pub fn get_peers_for_shard(&self, shard_id: u16) -> Vec<PeerInfo> {
        self.peers
            .iter()
            .filter(|entry| entry.value().shard == shard_id)
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn get_validators(&self) -> Vec<PeerInfo> {
        self.peers
            .iter()
            .filter(|entry| entry.value().is_validator)
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn is_tx_known(&self, hash: &Hash256) -> bool {
        self.known_tx_hashes.contains_key(hash)
    }

    pub fn mark_tx_known(&self, hash: Hash256) {
        const MAX_KNOWN_TX_HASHES: usize = 1_000_000;
        self.known_tx_hashes.insert(hash, ());
        if self.known_tx_hashes.len() > MAX_KNOWN_TX_HASHES {
            self.evict_tx_hashes(MAX_KNOWN_TX_HASHES / 2);
        }
    }

    pub fn get_propagation_targets(&self, shard_id: u16, fanout: usize) -> Vec<PeerInfo> {
        let mut shard_peers = self.get_peers_for_shard(shard_id);
        shard_peers.sort_by(|a, b| {
            b.stake
                .cmp(&a.stake)
                .then_with(|| a.address.0.cmp(&b.address.0))
        });
        shard_peers.truncate(fanout);
        shard_peers
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    pub fn known_tx_count(&self) -> usize {
        self.known_tx_hashes.len()
    }

    pub fn evict_tx_hashes(&self, keep: usize) {
        let current = self.known_tx_hashes.len();
        if current <= keep {
            return;
        }
        let to_remove = current - keep;
        let keys: Vec<Hash256> = self
            .known_tx_hashes
            .iter()
            .take(to_remove)
            .map(|entry| *entry.key())
            .collect();
        for k in keys {
            self.known_tx_hashes.remove(&k);
        }
        info!(evicted = to_remove, remaining = keep, "evicted tx hashes");
    }
}

impl Default for PeerManager {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NetworkNode {
    pub peer_manager: PeerManager,
    pub local_address: Hash256,
    pub listen_addr: SocketAddr,
}

#[derive(Debug)]
pub struct ShredBroadcastResult {
    pub targets: Vec<PeerInfo>,
    pub shreds: Vec<ShredMessage>,
}

#[derive(Debug)]
pub struct TxBroadcastResult {
    pub targets: Vec<PeerInfo>,
    pub messages: Vec<TxGossipMessage>,
    pub deduped: usize,
}

impl NetworkNode {
    pub fn new(local_address: Hash256, listen_addr: SocketAddr) -> Self {
        info!(
            address = %local_address,
            listen = %listen_addr,
            "network node created"
        );
        Self {
            peer_manager: PeerManager::new(),
            local_address,
            listen_addr,
        }
    }

    pub fn broadcast_shreds(
        &self,
        shreds: Vec<ShredMessage>,
        shard: u16,
        fanout: usize,
    ) -> ShredBroadcastResult {
        let targets = self.peer_manager.get_propagation_targets(shard, fanout);
        if targets.is_empty() {
            warn!(shard = shard, "no propagation targets for shard");
        } else {
            debug!(
                shard = shard,
                target_count = targets.len(),
                shred_count = shreds.len(),
                "broadcasting shreds"
            );
        }
        ShredBroadcastResult { targets, shreds }
    }

    pub fn broadcast_transactions(&self, txs: Vec<Vec<u8>>) -> TxBroadcastResult {
        let mut novel_txs: Vec<Vec<u8>> = Vec::with_capacity(txs.len());
        let mut deduped: usize = 0;

        for tx in &txs {
            let tx_hash = hash_bytes(tx);
            if self.peer_manager.is_tx_known(&tx_hash) {
                deduped += 1;
                continue;
            }
            self.peer_manager.mark_tx_known(tx_hash);
            novel_txs.push(tx.clone());
        }

        let messages = TxGossipMessage::new(novel_txs, self.local_address);

        let targets: Vec<PeerInfo> = self
            .peer_manager
            .peers
            .iter()
            .map(|entry| entry.value().clone())
            .collect();

        debug!(
            total = txs.len(),
            novel = txs.len() - deduped,
            deduped = deduped,
            targets = targets.len(),
            "broadcasting transactions"
        );

        TxBroadcastResult {
            targets,
            messages,
            deduped,
        }
    }

    pub fn peer_count(&self) -> usize {
        self.peer_manager.peer_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_hash(seed: u8) -> Hash256 {
        hash_bytes(&[seed])
    }

    fn make_peer(seed: u8, shard: u16, stake: u64, is_validator: bool) -> PeerInfo {
        PeerInfo {
            address: test_hash(seed),
            socket_addr: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, seed)),
                8000 + seed as u16,
            ),
            stake,
            shard,
            last_seen: 1_000_000,
            is_validator,
        }
    }

    #[test]
    fn test_shred_encode_small_block() {
        let data = b"hello ARC chain";
        let hash = hash_bytes(data);
        let shreds = ShredEncoder::encode(data, hash, 42, 0);

        assert_eq!(
            shreds.len(),
            2,
            "small block should produce 1 data + 1 parity shred"
        );
        assert_eq!(shreds[0].block_hash, hash);
        assert_eq!(shreds[0].block_height, 42);
        assert_eq!(shreds[0].shard_id, 0);
        assert_eq!(shreds[0].shred_index, 0);
        assert_eq!(shreds[0].total_shreds, 1);
        assert_eq!(shreds[0].coding_shreds, 1);
        assert_eq!(shreds[0].data, data.to_vec());
        assert_eq!(shreds[1].shred_index, 1);
    }

    #[test]
    fn test_shred_encode_decode_roundtrip() {
        let data: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
        let hash = hash_bytes(&data);
        let shreds = ShredEncoder::encode(&data, hash, 100, 3);

        let total_data = 4u16;
        let num_parity = 2u16;
        assert_eq!(shreds.len(), (total_data + num_parity) as usize);
        for (i, s) in shreds.iter().take(total_data as usize).enumerate() {
            assert_eq!(s.shred_index, i as u16);
            assert_eq!(s.total_shreds, total_data);
            assert_eq!(s.coding_shreds, num_parity);
            assert_eq!(s.block_hash, hash);
        }

        let recovered = ShredEncoder::decode(&shreds).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_shred_decode_mismatched_hash() {
        let hash_a = test_hash(1);
        let hash_b = test_hash(2);

        let shreds = vec![
            ShredMessage {
                block_hash: hash_a,
                block_height: 1,
                shard_id: 0,
                shred_index: 0,
                total_shreds: 2,
                coding_shreds: 1,
                data: vec![0; 100],
            },
            ShredMessage {
                block_hash: hash_b,
                block_height: 1,
                shard_id: 0,
                shred_index: 1,
                total_shreds: 2,
                coding_shreds: 1,
                data: vec![1; 100],
            },
        ];

        let err = ShredEncoder::decode(&shreds).unwrap_err();
        assert!(
            matches!(err, NetError::MismatchedBlockHash),
            "expected MismatchedBlockHash, got: {err}"
        );
    }

    #[test]
    fn test_shred_decode_insufficient_shreds() {
        let hash = test_hash(1);
        let shreds = vec![ShredMessage {
            block_hash: hash,
            block_height: 1,
            shard_id: 0,
            shred_index: 0,
            total_shreds: 3,
            coding_shreds: 2,
            data: vec![0; 100],
        }];

        let err = ShredEncoder::decode(&shreds).unwrap_err();
        match err {
            NetError::InsufficientShreds { needed, got } => {
                assert_eq!(needed, 3);
                assert_eq!(got, 1);
            }
            other => panic!("expected InsufficientShreds, got: {other}"),
        }
    }

    #[test]
    fn test_shred_encode_empty_block() {
        let hash = test_hash(99);
        let shreds = ShredEncoder::encode(&[], hash, 0, 0);

        assert_eq!(shreds.len(), 1);
        assert_eq!(shreds[0].total_shreds, 1);
        assert_eq!(shreds[0].coding_shreds, 0);
        assert!(shreds[0].data.is_empty());

        let recovered = ShredEncoder::decode(&shreds).unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn test_shred_roundtrip_large_block() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 256) as u8).collect();
        let hash = hash_bytes(&data);
        let shreds = ShredEncoder::encode(&data, hash, 999, 7);

        let total_data = 79usize;
        let num_parity = total_data.div_ceil(2);
        assert_eq!(shreds.len(), total_data + num_parity);

        let mut shuffled = shreds.clone();
        shuffled.reverse();

        let recovered = ShredEncoder::decode(&shuffled).unwrap();
        assert_eq!(recovered.len(), data.len());
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_peer_add_remove() {
        let pm = PeerManager::new();
        let peer = make_peer(1, 0, 1000, true);

        pm.add_peer(peer.clone());
        assert_eq!(pm.peer_count(), 1);

        pm.add_peer(peer.clone());
        assert_eq!(pm.peer_count(), 1);

        assert!(pm.remove_peer(&peer.address));
        assert_eq!(pm.peer_count(), 0);

        assert!(!pm.remove_peer(&peer.address));
    }

    #[test]
    fn test_peer_shard_filtering() {
        let pm = PeerManager::new();
        pm.add_peer(make_peer(1, 0, 100, false));
        pm.add_peer(make_peer(2, 0, 200, true));
        pm.add_peer(make_peer(3, 1, 300, true));
        pm.add_peer(make_peer(4, 2, 400, false));
        pm.add_peer(make_peer(5, 0, 50, false));

        let shard0 = pm.get_peers_for_shard(0);
        assert_eq!(shard0.len(), 3);
        for p in &shard0 {
            assert_eq!(p.shard, 0);
        }

        let shard1 = pm.get_peers_for_shard(1);
        assert_eq!(shard1.len(), 1);
        assert_eq!(shard1[0].shard, 1);

        let shard99 = pm.get_peers_for_shard(99);
        assert!(shard99.is_empty());
    }

    #[test]
    fn test_peer_get_validators() {
        let pm = PeerManager::new();
        pm.add_peer(make_peer(1, 0, 100, false));
        pm.add_peer(make_peer(2, 0, 200, true));
        pm.add_peer(make_peer(3, 1, 300, true));
        pm.add_peer(make_peer(4, 2, 400, false));

        let validators = pm.get_validators();
        assert_eq!(validators.len(), 2);
        for v in &validators {
            assert!(v.is_validator);
        }
    }

    #[test]
    fn test_tx_dedup() {
        let pm = PeerManager::new();
        let tx_hash = hash_bytes(b"tx1");

        assert!(!pm.is_tx_known(&tx_hash));
        pm.mark_tx_known(tx_hash);
        assert!(pm.is_tx_known(&tx_hash));

        let other = hash_bytes(b"tx2");
        assert!(!pm.is_tx_known(&other));
    }

    #[test]
    fn test_propagation_targets_stake_order() {
        let pm = PeerManager::new();
        pm.add_peer(make_peer(1, 0, 100, false));
        pm.add_peer(make_peer(2, 0, 500, true));
        pm.add_peer(make_peer(3, 0, 300, false));
        pm.add_peer(make_peer(4, 0, 200, true));
        pm.add_peer(make_peer(5, 1, 9999, true));

        let targets = pm.get_propagation_targets(0, 3);
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].stake, 500);
        assert_eq!(targets[1].stake, 300);
        assert_eq!(targets[2].stake, 200);

        let all = pm.get_propagation_targets(0, 100);
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn test_network_node_broadcast_shreds() {
        let addr = test_hash(0);
        let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000);
        let node = NetworkNode::new(addr, listen);

        node.peer_manager.add_peer(make_peer(1, 0, 500, true));
        node.peer_manager.add_peer(make_peer(2, 0, 300, false));
        node.peer_manager.add_peer(make_peer(3, 1, 999, true));

        let data = vec![42u8; 3000];
        let hash = hash_bytes(&data);
        let shreds = ShredEncoder::encode(&data, hash, 10, 0);
        let result = node.broadcast_shreds(shreds.clone(), 0, 8);

        assert_eq!(result.targets.len(), 2);
        assert_eq!(result.shreds.len(), shreds.len());
    }

    #[test]
    fn test_network_node_broadcast_transactions_with_dedup() {
        let addr = test_hash(0);
        let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000);
        let node = NetworkNode::new(addr, listen);

        node.peer_manager.add_peer(make_peer(1, 0, 100, false));
        node.peer_manager.add_peer(make_peer(2, 0, 200, true));

        let tx1 = b"transfer 100 ARC".to_vec();
        let tx2 = b"transfer 200 ARC".to_vec();
        let tx3 = b"transfer 100 ARC".to_vec();

        let r1 = node.broadcast_transactions(vec![tx1.clone(), tx2.clone()]);
        assert_eq!(r1.deduped, 0);
        assert_eq!(r1.messages.len(), 1);
        assert_eq!(r1.messages[0].transactions.len(), 2);
        assert_eq!(r1.targets.len(), 2);

        let r2 = node.broadcast_transactions(vec![tx1.clone(), tx3, b"brand new tx".to_vec()]);
        assert_eq!(r2.deduped, 2);
        assert_eq!(r2.messages[0].transactions.len(), 1);
    }

    #[test]
    fn test_network_node_peer_count() {
        let node = NetworkNode::new(
            test_hash(0),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000),
        );
        assert_eq!(node.peer_count(), 0);

        node.peer_manager.add_peer(make_peer(1, 0, 100, false));
        assert_eq!(node.peer_count(), 1);

        node.peer_manager.add_peer(make_peer(2, 1, 200, true));
        assert_eq!(node.peer_count(), 2);

        node.peer_manager.remove_peer(&test_hash(1));
        assert_eq!(node.peer_count(), 1);
    }

    #[test]
    fn test_tx_gossip_message_batching() {
        let sender = test_hash(0);
        let txs: Vec<Vec<u8>> = (0..250u16).map(|i| i.to_le_bytes().to_vec()).collect();
        let messages = TxGossipMessage::new(txs, sender);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].transactions.len(), 100);
        assert_eq!(messages[1].transactions.len(), 100);
        assert_eq!(messages[2].transactions.len(), 50);

        for m in &messages {
            assert_eq!(m.sender, sender);
        }
    }

    #[test]
    fn test_tx_gossip_message_empty() {
        let sender = test_hash(0);
        let messages = TxGossipMessage::new(vec![], sender);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].transactions.is_empty());
    }

    #[test]
    fn test_shred_serde_roundtrip() {
        let shred = ShredMessage {
            block_hash: test_hash(42),
            block_height: 12345,
            shard_id: 7,
            shred_index: 3,
            total_shreds: 10,
            coding_shreds: 5,
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };

        let bytes = bincode::serialize(&shred).expect("serialize");
        let deserialized: ShredMessage = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(deserialized.block_hash, shred.block_hash);
        assert_eq!(deserialized.block_height, shred.block_height);
        assert_eq!(deserialized.shard_id, shred.shard_id);
        assert_eq!(deserialized.shred_index, shred.shred_index);
        assert_eq!(deserialized.total_shreds, shred.total_shreds);
        assert_eq!(deserialized.data, shred.data);
    }

    #[test]
    fn test_tx_gossip_serde_roundtrip() {
        let msg = TxGossipMessage {
            transactions: vec![b"tx_one".to_vec(), b"tx_two".to_vec()],
            sender: test_hash(7),
        };

        let bytes = bincode::serialize(&msg).expect("serialize");
        let deserialized: TxGossipMessage = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(deserialized.transactions, msg.transactions);
        assert_eq!(deserialized.sender, msg.sender);
    }

    #[test]
    fn test_tx_hash_eviction() {
        let pm = PeerManager::new();
        for i in 0..100u8 {
            pm.mark_tx_known(test_hash(i));
        }
        assert_eq!(pm.known_tx_count(), 100);

        pm.evict_tx_hashes(30);
        assert!(
            pm.known_tx_count() <= 30,
            "expected <= 30, got {}",
            pm.known_tx_count()
        );
    }

    #[test]
    fn test_shred_exact_boundary() {
        let data = vec![0xABu8; SHRED_DATA_SIZE * 2];
        let hash = hash_bytes(&data);
        let shreds = ShredEncoder::encode(&data, hash, 1, 0);

        assert_eq!(shreds.len(), 3);
        assert_eq!(shreds[0].data.len(), SHRED_DATA_SIZE);
        assert_eq!(shreds[1].data.len(), SHRED_DATA_SIZE);

        let recovered = ShredEncoder::decode(&shreds).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_fec_recover_missing_first_shred() {
        let data = vec![0xABu8; SHRED_DATA_SIZE * 2];
        let hash = hash_bytes(&data);
        let shreds = ShredEncoder::encode(&data, hash, 1, 0);
        assert_eq!(shreds.len(), 3);

        let partial: Vec<ShredMessage> = shreds
            .iter()
            .filter(|s| s.shred_index != 0)
            .cloned()
            .collect();
        assert_eq!(partial.len(), 2);

        let recovered = ShredEncoder::decode(&partial).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_fec_recover_missing_second_shred() {
        let data = vec![0xCDu8; SHRED_DATA_SIZE * 2];
        let hash = hash_bytes(&data);
        let shreds = ShredEncoder::encode(&data, hash, 2, 0);
        assert_eq!(shreds.len(), 3);

        let partial: Vec<ShredMessage> = shreds
            .iter()
            .filter(|s| s.shred_index != 1)
            .cloned()
            .collect();
        assert_eq!(partial.len(), 2);

        let recovered = ShredEncoder::decode(&partial).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_fec_fails_when_both_missing() {
        let data = vec![0xEFu8; SHRED_DATA_SIZE * 2];
        let hash = hash_bytes(&data);
        let shreds = ShredEncoder::encode(&data, hash, 3, 0);

        let partial: Vec<ShredMessage> = shreds
            .iter()
            .filter(|s| s.shred_index >= 2)
            .cloned()
            .collect();
        assert_eq!(partial.len(), 1);

        let err = ShredEncoder::decode(&partial).unwrap_err();
        assert!(matches!(err, NetError::InsufficientShreds { .. }));
    }

    #[test]
    fn test_fec_recovery_large_block() {
        let data: Vec<u8> = (0u8..=255).cycle().take(SHRED_DATA_SIZE * 4).collect();
        let hash = hash_bytes(&data);
        let shreds = ShredEncoder::encode(&data, hash, 10, 0);
        assert_eq!(shreds.len(), 6);

        let partial: Vec<ShredMessage> = shreds
            .iter()
            .filter(|s| s.shred_index != 0 && s.shred_index != 3)
            .cloned()
            .collect();
        assert_eq!(partial.len(), 4);

        let recovered = ShredEncoder::decode(&partial).unwrap();
        assert_eq!(recovered, data);
    }
}
