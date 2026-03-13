use crate::hash::{Hash256, hash_pair};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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

/// Persistent incremental Merkle tree — O(k log n) updates.
///
/// Maintains a sorted set of (key, leaf_hash) pairs and caches all tree
/// levels between calls.  When only existing leaves change, only the
/// affected paths from leaf to root are recomputed.  Structural changes
/// (inserts / deletes) trigger a full O(n) rebuild.
///
/// Produces the **exact same root** as `MerkleTree::from_leaves` for the
/// same sorted sequence of leaf hashes — the padding rules are identical.
pub struct IncrementalMerkle {
    /// Sorted keys (e.g. account addresses).
    keys: Vec<[u8; 32]>,
    /// Leaf hashes corresponding to `keys` (before padding).
    leaves: Vec<Hash256>,
    /// Cached tree levels.  `levels[0]` = leaf hashes (with padding),
    /// `levels[last]` = `[root]`.
    levels: Vec<Vec<Hash256>>,
    /// Number of *real* (non-padding) elements at each level.
    real_lengths: Vec<usize>,
    /// Key → index in `keys` for O(1) lookup.
    key_index: HashMap<[u8; 32], usize>,
}

impl IncrementalMerkle {
    /// Create an empty tree.
    pub fn new() -> Self {
        Self {
            keys: Vec::new(),
            leaves: Vec::new(),
            levels: vec![vec![Hash256::ZERO]],
            real_lengths: vec![1],
            key_index: HashMap::new(),
        }
    }

    /// Update an existing leaf or insert a new one.
    ///
    /// Returns `(leaf_index, is_new_key)`.  If `is_new_key` is true the
    /// caller must call [`rebuild`] before reading the root.
    pub fn update(&mut self, key: [u8; 32], hash: Hash256) -> (usize, bool) {
        if let Some(&idx) = self.key_index.get(&key) {
            self.leaves[idx] = hash;
            (idx, false)
        } else {
            // Insert in sorted position.
            let idx = self.keys.binary_search(&key).unwrap_or_else(|i| i);
            self.keys.insert(idx, key);
            self.leaves.insert(idx, hash);
            // Rebuild the index — indices after `idx` shifted by 1.
            self.key_index.clear();
            for (i, k) in self.keys.iter().enumerate() {
                self.key_index.insert(*k, i);
            }
            (idx, true)
        }
    }

    /// Remove a leaf by key.  Returns `true` if it existed.
    ///
    /// The caller must call [`rebuild`] after structural changes.
    pub fn remove(&mut self, key: &[u8; 32]) -> bool {
        if let Some(idx) = self.key_index.remove(key) {
            self.keys.remove(idx);
            self.leaves.remove(idx);
            self.key_index.clear();
            for (i, k) in self.keys.iter().enumerate() {
                self.key_index.insert(*k, i);
            }
            true
        } else {
            false
        }
    }

    /// Full rebuild of the cached tree from the current leaves.  O(n).
    ///
    /// Matches the padding behaviour of [`MerkleTree::from_leaves`] exactly
    /// so the roots are identical for the same leaf sequence.
    pub fn rebuild(&mut self) {
        if self.leaves.is_empty() {
            self.levels = vec![vec![Hash256::ZERO]];
            self.real_lengths = vec![1];
            return;
        }

        let mut current = self.leaves.clone();
        let mut real_lengths = vec![current.len()];

        // Pad to even length (duplicate last).
        if current.len() % 2 != 0 {
            current.push(*current.last().unwrap());
        }

        let mut levels = vec![current.clone()];

        while current.len() > 1 {
            let next: Vec<Hash256> = current
                .chunks(2)
                .map(|pair| {
                    if pair.len() == 2 {
                        hash_pair(&pair[0], &pair[1])
                    } else {
                        hash_pair(&pair[0], &pair[0])
                    }
                })
                .collect();

            real_lengths.push(next.len());

            let mut padded = next;
            if padded.len() > 1 && padded.len() % 2 != 0 {
                padded.push(*padded.last().unwrap());
            }
            current = padded.clone();
            levels.push(padded);
        }

        self.levels = levels;
        self.real_lengths = real_lengths;
    }

    /// Recompute only the tree paths affected by changed leaf indices.
    /// O(k log n) for k changed leaves among n total.
    ///
    /// **Precondition**: no structural changes (inserts/removes) since the
    /// last [`rebuild`].  Call `rebuild()` first if the structure changed.
    pub fn recompute_paths(&mut self, dirty_indices: &[usize]) {
        if self.levels.is_empty() || dirty_indices.is_empty() || self.leaves.is_empty() {
            return;
        }

        let n_real = self.real_lengths[0];

        // ── Update level 0 from self.leaves ──
        for &idx in dirty_indices {
            if idx < n_real {
                self.levels[0][idx] = self.leaves[idx];
                // Last real leaf may have been duplicated as padding.
                if idx == n_real - 1 && self.levels[0].len() > n_real {
                    self.levels[0][n_real] = self.leaves[idx];
                }
            }
        }

        // Collect dirty set at level 0 (include padding twin if needed).
        let mut dirty_at_level: HashSet<usize> = dirty_indices.iter().copied().collect();
        if n_real > 0 && self.levels[0].len() > n_real && dirty_at_level.contains(&(n_real - 1)) {
            dirty_at_level.insert(n_real);
        }

        // ── Propagate upwards ──
        for lv in 0..self.levels.len() - 1 {
            let mut dirty_parents: HashSet<usize> = HashSet::new();
            for &idx in &dirty_at_level {
                dirty_parents.insert(idx / 2);
            }

            let level_len = self.levels[lv].len();
            let next_lv = lv + 1;
            let next_real = self.real_lengths[next_lv];

            for &parent_idx in &dirty_parents {
                let left = parent_idx * 2;
                let right = left + 1;
                let left_hash = self.levels[lv][left];
                let right_hash = if right < level_len {
                    self.levels[lv][right]
                } else {
                    left_hash
                };
                self.levels[next_lv][parent_idx] = hash_pair(&left_hash, &right_hash);

                // Propagate padding at next level if needed.
                if parent_idx == next_real - 1 && self.levels[next_lv].len() > next_real {
                    self.levels[next_lv][next_real] = self.levels[next_lv][parent_idx];
                }
            }

            // Prepare dirty set for next level.
            dirty_at_level = dirty_parents;
            if next_real > 0 && self.levels[next_lv].len() > next_real {
                if dirty_at_level.contains(&(next_real - 1)) {
                    dirty_at_level.insert(next_real);
                }
            }
        }
    }

    /// Current Merkle root.  O(1).
    pub fn root(&self) -> Hash256 {
        self.levels
            .last()
            .and_then(|l| l.first().copied())
            .unwrap_or(Hash256::ZERO)
    }

    /// Number of leaves (accounts).
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Look up a key's leaf index.
    pub fn get_index(&self, key: &[u8; 32]) -> Option<usize> {
        self.key_index.get(key).copied()
    }

    /// Generate a Merkle inclusion proof for the leaf at `index`.
    pub fn proof(&self, index: usize) -> Option<MerkleProof> {
        if self.levels.is_empty()
            || self.real_lengths.is_empty()
            || index >= self.real_lengths[0]
        {
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
                level[idx]
            };
            let is_left = idx % 2 != 0;
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

    // ── IncrementalMerkle tests ───────────────────────────────────────

    #[test]
    fn test_incremental_matches_full_rebuild() {
        // Build with IncrementalMerkle and compare against MerkleTree.
        let mut im = IncrementalMerkle::new();
        let keys: Vec<[u8; 32]> = (0u32..100)
            .map(|i| {
                let mut k = [0u8; 32];
                k[..4].copy_from_slice(&i.to_be_bytes());
                k
            })
            .collect();
        let hashes: Vec<Hash256> = keys.iter().map(|k| hash_bytes(k)).collect();

        for (k, h) in keys.iter().zip(hashes.iter()) {
            im.update(*k, *h);
        }
        im.rebuild();

        // The sorted order of keys is 0,1,2,...,99 because big-endian.
        let mut sorted_pairs: Vec<([u8; 32], Hash256)> =
            keys.iter().copied().zip(hashes.iter().copied()).collect();
        sorted_pairs.sort_by_key(|(k, _)| *k);
        let sorted_hashes: Vec<Hash256> = sorted_pairs.iter().map(|(_, h)| *h).collect();

        let full_tree = MerkleTree::from_leaves(sorted_hashes);
        assert_eq!(im.root(), full_tree.root());
    }

    #[test]
    fn test_incremental_update_matches_full() {
        let mut im = IncrementalMerkle::new();
        let n = 50u32;
        let keys: Vec<[u8; 32]> = (0..n)
            .map(|i| {
                let mut k = [0u8; 32];
                k[..4].copy_from_slice(&i.to_be_bytes());
                k
            })
            .collect();
        let hashes: Vec<Hash256> = keys.iter().map(|k| hash_bytes(k)).collect();

        for (k, h) in keys.iter().zip(hashes.iter()) {
            im.update(*k, *h);
        }
        im.rebuild();

        // Change a few leaves incrementally.
        let new_hash_10 = hash_bytes(b"updated-10");
        let new_hash_25 = hash_bytes(b"updated-25");
        let new_hash_49 = hash_bytes(b"updated-49"); // last leaf

        let (idx10, new10) = im.update(keys[10], new_hash_10);
        let (idx25, new25) = im.update(keys[25], new_hash_25);
        let (idx49, new49) = im.update(keys[49], new_hash_49);
        assert!(!new10);
        assert!(!new25);
        assert!(!new49);

        im.recompute_paths(&[idx10, idx25, idx49]);

        // Build the expected tree from scratch.
        let mut expected_hashes = hashes.clone();
        expected_hashes[10] = new_hash_10;
        expected_hashes[25] = new_hash_25;
        expected_hashes[49] = new_hash_49;
        // Keys are already in sorted order (0,1,...,49 big-endian).
        let full_tree = MerkleTree::from_leaves(expected_hashes);
        assert_eq!(im.root(), full_tree.root());
    }

    #[test]
    fn test_incremental_odd_leaf_count() {
        // Odd number of leaves — tests padding edge case.
        let mut im = IncrementalMerkle::new();
        let n = 7u32;
        let keys: Vec<[u8; 32]> = (0..n)
            .map(|i| {
                let mut k = [0u8; 32];
                k[..4].copy_from_slice(&i.to_be_bytes());
                k
            })
            .collect();
        let hashes: Vec<Hash256> = keys.iter().map(|k| hash_bytes(k)).collect();

        for (k, h) in keys.iter().zip(hashes.iter()) {
            im.update(*k, *h);
        }
        im.rebuild();

        let full = MerkleTree::from_leaves(hashes.clone());
        assert_eq!(im.root(), full.root());

        // Update the last leaf (padding boundary).
        let new_last = hash_bytes(b"new-last");
        let (idx, is_new) = im.update(keys[6], new_last);
        assert!(!is_new);
        im.recompute_paths(&[idx]);

        let mut updated = hashes;
        updated[6] = new_last;
        let full2 = MerkleTree::from_leaves(updated);
        assert_eq!(im.root(), full2.root());
    }

    #[test]
    fn test_incremental_proof_verifies() {
        let mut im = IncrementalMerkle::new();
        let n = 100u32;
        let keys: Vec<[u8; 32]> = (0..n)
            .map(|i| {
                let mut k = [0u8; 32];
                k[..4].copy_from_slice(&i.to_be_bytes());
                k
            })
            .collect();
        let hashes: Vec<Hash256> = keys.iter().map(|k| hash_bytes(k)).collect();

        for (k, h) in keys.iter().zip(hashes.iter()) {
            im.update(*k, *h);
        }
        im.rebuild();

        // Generate and verify proofs at various positions.
        for idx in [0, 1, 50, 98, 99] {
            let proof = im.proof(idx).unwrap();
            assert!(MerkleTree::verify_proof(&proof), "proof failed at index {idx}");
        }
    }
}
