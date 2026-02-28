use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};

/// Block header — compact representation anchoring all transactions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockHeader {
    /// Block height (monotonically increasing).
    pub height: u64,
    /// Timestamp (unix millis).
    pub timestamp: u64,
    /// Hash of the previous block header.
    pub parent_hash: Hash256,
    /// Merkle root of all transaction hashes in this block.
    pub tx_root: Hash256,
    /// Merkle root of the state tree after applying this block.
    pub state_root: Hash256,
    /// Aggregate proof hash (ZK batch proof for this block's transactions).
    pub proof_hash: Hash256,
    /// Number of transactions in this block.
    pub tx_count: u32,
    /// Block producer (validator address).
    pub producer: Hash256,
}

/// A full block including header and transaction hashes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    /// Transaction hashes included in this block (ordered).
    pub tx_hashes: Vec<Hash256>,
    /// Block hash (BLAKE3 of the serialized header).
    pub hash: Hash256,
}

impl Block {
    /// Create a new block from a header and transaction hashes.
    pub fn new(header: BlockHeader, tx_hashes: Vec<Hash256>) -> Self {
        let hash = Self::compute_hash(&header);
        Self {
            header,
            tx_hashes,
            hash,
        }
    }

    /// Compute the block hash from the header.
    pub fn compute_hash(header: &BlockHeader) -> Hash256 {
        let bytes = bincode::serialize(header).expect("serializable header");
        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-block-v1");
        hasher.update(&bytes);
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Genesis block (block 0).
    pub fn genesis() -> Self {
        let header = BlockHeader {
            height: 0,
            timestamp: 0,
            parent_hash: Hash256::ZERO,
            tx_root: Hash256::ZERO,
            state_root: Hash256::ZERO,
            proof_hash: Hash256::ZERO,
            tx_count: 0,
            producer: Hash256::ZERO,
        };
        Self::new(header, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_genesis() {
        let genesis = Block::genesis();
        assert_eq!(genesis.header.height, 0);
        assert_ne!(genesis.hash, Hash256::ZERO);
    }

    #[test]
    fn test_block_hash_deterministic() {
        let a = Block::genesis();
        let b = Block::genesis();
        assert_eq!(a.hash, b.hash);
    }
}
