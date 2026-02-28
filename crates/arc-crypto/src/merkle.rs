use crate::hash::{Hash256, hash_pair};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Parallel Merkle tree for verifiable transaction inclusion.
/// Supports millions of leaves with O(log n) proof size.
#[derive(Clone, Debug)]
pub struct MerkleTree {
    /// All levels of the tree. levels[0] = leaves, levels[last] = root.
    levels: Vec<Vec<Hash256>>,
}

/// Merkle inclusion proof for a single leaf.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MerkleProof {
    /// The leaf hash being proved.
    pub leaf: Hash256,
    /// Index of the leaf in the tree.
    pub index: usize,
    /// Sibling hashes from leaf to root.
    pub siblings: Vec<(Hash256, bool)>, // (hash, is_left_sibling)
    /// Expected root.
    pub root: Hash256,
}

impl MerkleTree {
    /// Build a Merkle tree from leaf hashes using parallel construction.
    /// Each level is computed in parallel using Rayon.
    pub fn from_leaves(leaves: Vec<Hash256>) -> Self {
        if leaves.is_empty() {
            return Self {
                levels: vec![vec![Hash256::ZERO]],
            };
        }

        let mut levels = Vec::new();
        let mut current = leaves;

        // Pad to even length if needed
        if current.len() % 2 != 0 {
            current.push(*current.last().unwrap());
        }

        levels.push(current.clone());

        while current.len() > 1 {
            // Parallel pair hashing
            let next: Vec<Hash256> = current
                .par_chunks(2)
                .map(|pair| {
                    if pair.len() == 2 {
                        hash_pair(&pair[0], &pair[1])
                    } else {
                        hash_pair(&pair[0], &pair[0])
                    }
                })
                .collect();

            let mut padded = next;
            if padded.len() > 1 && padded.len() % 2 != 0 {
                padded.push(*padded.last().unwrap());
            }
            current = padded.clone();
            levels.push(padded);
        }

        Self { levels }
    }

    /// Get the Merkle root.
    pub fn root(&self) -> Hash256 {
        self.levels
            .last()
            .and_then(|l| l.first().copied())
            .unwrap_or(Hash256::ZERO)
    }

    /// Number of leaves.
    pub fn leaf_count(&self) -> usize {
        self.levels.first().map(|l| l.len()).unwrap_or(0)
    }

    /// Generate an inclusion proof for a leaf at the given index.
    pub fn proof(&self, index: usize) -> Option<MerkleProof> {
        if index >= self.levels[0].len() {
            return None;
        }

        let leaf = self.levels[0][index];
        let mut siblings = Vec::new();
        let mut idx = index;

        for level in &self.levels[..self.levels.len() - 1] {
            let sibling_idx = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
            let sibling = if sibling_idx < level.len() {
                level[sibling_idx]
            } else {
                level[idx] // duplicate for odd-length levels
            };
            let is_left = idx % 2 != 0; // sibling is on the left if our index is odd
            siblings.push((sibling, is_left));
            idx /= 2;
        }

        Some(MerkleProof {
            leaf,
            index,
            siblings,
            root: self.root(),
        })
    }

    /// Verify a Merkle inclusion proof.
    pub fn verify_proof(proof: &MerkleProof) -> bool {
        let mut current = proof.leaf;
        for (sibling, is_left) in &proof.siblings {
            current = if *is_left {
                hash_pair(sibling, &current)
            } else {
                hash_pair(&current, sibling)
            };
        }
        current == proof.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;

    #[test]
    fn test_single_leaf() {
        let leaf = hash_bytes(b"tx-0");
        let tree = MerkleTree::from_leaves(vec![leaf]);
        // Single leaf padded to 2, root = hash(leaf, leaf)
        assert_ne!(tree.root(), Hash256::ZERO);
    }

    #[test]
    fn test_two_leaves() {
        let a = hash_bytes(b"tx-0");
        let b = hash_bytes(b"tx-1");
        let tree = MerkleTree::from_leaves(vec![a, b]);
        assert_eq!(tree.root(), hash_pair(&a, &b));
    }

    #[test]
    fn test_proof_verify() {
        let leaves: Vec<Hash256> = (0..1000u32)
            .map(|i| hash_bytes(&i.to_le_bytes()))
            .collect();
        let tree = MerkleTree::from_leaves(leaves);

        // Prove leaf 42
        let proof = tree.proof(42).unwrap();
        assert!(MerkleTree::verify_proof(&proof));
    }

    #[test]
    fn test_proof_fails_with_wrong_leaf() {
        let leaves: Vec<Hash256> = (0..100u32)
            .map(|i| hash_bytes(&i.to_le_bytes()))
            .collect();
        let tree = MerkleTree::from_leaves(leaves);

        let mut proof = tree.proof(42).unwrap();
        proof.leaf = hash_bytes(b"wrong");
        assert!(!MerkleTree::verify_proof(&proof));
    }

    #[test]
    fn test_large_tree() {
        let leaves: Vec<Hash256> = (0..1_000_000u32)
            .map(|i| hash_bytes(&i.to_le_bytes()))
            .collect();
        let tree = MerkleTree::from_leaves(leaves);
        assert_eq!(tree.leaf_count(), 1_000_000);

        // Verify random proofs
        for idx in [0, 999, 500_000, 999_999] {
            let proof = tree.proof(idx).unwrap();
            assert!(MerkleTree::verify_proof(&proof));
        }
    }
}
