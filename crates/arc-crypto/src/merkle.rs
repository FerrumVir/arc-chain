use crate::hash::{Hash256, hash_pair};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct MerkleTree {
    levels: Vec<Vec<Hash256>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MerkleProof {
    pub leaf: Hash256,
    pub index: usize,
    pub siblings: Vec<(Hash256, bool)>,
    pub root: Hash256,
}

impl MerkleTree {
    pub fn from_leaves(leaves: Vec<Hash256>) -> Self {
        if leaves.is_empty() {
            return Self {
                levels: vec![vec![Hash256::ZERO]],
            };
        }

        let mut levels = Vec::new();
        let mut current = leaves;

        if !current.len().is_multiple_of(2) {
            current.push(*current.last().unwrap());
        }

        levels.push(current.clone());

        while current.len() > 1 {
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
            if padded.len() > 1 && !padded.len().is_multiple_of(2) {
                padded.push(*padded.last().unwrap());
            }
            current = padded.clone();
            levels.push(padded);
        }

        Self { levels }
    }

    pub fn root(&self) -> Hash256 {
        self.levels
            .last()
            .and_then(|l| l.first().copied())
            .unwrap_or(Hash256::ZERO)
    }

    pub fn leaf_count(&self) -> usize {
        self.levels.first().map(|l| l.len()).unwrap_or(0)
    }

    pub fn proof(&self, index: usize) -> Option<MerkleProof> {
        if index >= self.levels[0].len() {
            return None;
        }

        let leaf = self.levels[0][index];
        let mut siblings = Vec::new();
        let mut idx = index;

        for level in &self.levels[..self.levels.len() - 1] {
            let sibling_idx = if idx.is_multiple_of(2) {
                idx + 1
            } else {
                idx - 1
            };
            let sibling = if sibling_idx < level.len() {
                level[sibling_idx]
            } else {
                level[idx]
            };
            let is_left = !idx.is_multiple_of(2);
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

pub struct IncrementalMerkle {
    keys: Vec<[u8; 32]>,
    leaves: Vec<Hash256>,
    levels: Vec<Vec<Hash256>>,
    real_lengths: Vec<usize>,
    key_index: HashMap<[u8; 32], usize>,
}

impl Default for IncrementalMerkle {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalMerkle {
    pub fn new() -> Self {
        Self {
            keys: Vec::new(),
            leaves: Vec::new(),
            levels: vec![vec![Hash256::ZERO]],
            real_lengths: vec![1],
            key_index: HashMap::new(),
        }
    }

    pub fn from_keyed_leaves(mut entries: Vec<([u8; 32], Hash256)>) -> Self {
        entries.sort_unstable_by_key(|entry| entry.0);
        assert!(
            entries.windows(2).all(|window| window[0].0 != window[1].0),
            "incremental Merkle input contains duplicate keys"
        );
        let mut keys = Vec::with_capacity(entries.len());
        let mut leaves = Vec::with_capacity(entries.len());
        let mut key_index = HashMap::with_capacity(entries.len());
        for (index, (key, hash)) in entries.into_iter().enumerate() {
            keys.push(key);
            leaves.push(hash);
            key_index.insert(key, index);
        }
        let mut tree = Self {
            keys,
            leaves,
            levels: Vec::new(),
            real_lengths: Vec::new(),
            key_index,
        };
        tree.rebuild();
        tree
    }

    pub fn update(&mut self, key: [u8; 32], hash: Hash256) -> (usize, bool) {
        if let Some(&idx) = self.key_index.get(&key) {
            self.leaves[idx] = hash;
            (idx, false)
        } else {
            let idx = self.keys.binary_search(&key).unwrap_or_else(|i| i);
            self.keys.insert(idx, key);
            self.leaves.insert(idx, hash);
            self.key_index.clear();
            for (i, k) in self.keys.iter().enumerate() {
                self.key_index.insert(*k, i);
            }
            (idx, true)
        }
    }

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

    pub fn rebuild(&mut self) {
        if self.leaves.is_empty() {
            self.levels = vec![vec![Hash256::ZERO]];
            self.real_lengths = vec![1];
            return;
        }

        let mut current = self.leaves.clone();
        let mut real_lengths = vec![current.len()];

        if !current.len().is_multiple_of(2) {
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
            if padded.len() > 1 && !padded.len().is_multiple_of(2) {
                padded.push(*padded.last().unwrap());
            }
            current = padded.clone();
            levels.push(padded);
        }

        self.levels = levels;
        self.real_lengths = real_lengths;
    }

    pub fn recompute_paths(&mut self, dirty_indices: &[usize]) {
        if self.levels.is_empty() || dirty_indices.is_empty() || self.leaves.is_empty() {
            return;
        }

        let n_real = self.real_lengths[0];

        for &idx in dirty_indices {
            if idx < n_real {
                self.levels[0][idx] = self.leaves[idx];
                if idx == n_real - 1 && self.levels[0].len() > n_real {
                    self.levels[0][n_real] = self.leaves[idx];
                }
            }
        }

        let mut dirty_at_level: HashSet<usize> = dirty_indices.iter().copied().collect();
        if n_real > 0 && self.levels[0].len() > n_real && dirty_at_level.contains(&(n_real - 1)) {
            dirty_at_level.insert(n_real);
        }

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

                if parent_idx == next_real - 1 && self.levels[next_lv].len() > next_real {
                    self.levels[next_lv][next_real] = self.levels[next_lv][parent_idx];
                }
            }

            dirty_at_level = dirty_parents;
            if next_real > 0
                && self.levels[next_lv].len() > next_real
                && dirty_at_level.contains(&(next_real - 1))
            {
                dirty_at_level.insert(next_real);
            }
        }
    }

    pub fn root(&self) -> Hash256 {
        self.levels
            .last()
            .and_then(|l| l.first().copied())
            .unwrap_or(Hash256::ZERO)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn get_index(&self, key: &[u8; 32]) -> Option<usize> {
        self.key_index.get(key).copied()
    }

    pub fn proof(&self, index: usize) -> Option<MerkleProof> {
        if self.levels.is_empty() || self.real_lengths.is_empty() || index >= self.real_lengths[0] {
            return None;
        }

        let leaf = self.levels[0][index];
        let mut siblings = Vec::new();
        let mut idx = index;

        for level in &self.levels[..self.levels.len() - 1] {
            let sibling_idx = if idx.is_multiple_of(2) {
                idx + 1
            } else {
                idx - 1
            };
            let sibling = if sibling_idx < level.len() {
                level[sibling_idx]
            } else {
                level[idx]
            };
            let is_left = !idx.is_multiple_of(2);
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
        let leaves: Vec<Hash256> = (0..1000u32).map(|i| hash_bytes(&i.to_le_bytes())).collect();
        let tree = MerkleTree::from_leaves(leaves);

        let proof = tree.proof(42).unwrap();
        assert!(MerkleTree::verify_proof(&proof));
    }

    #[test]
    fn test_proof_fails_with_wrong_leaf() {
        let leaves: Vec<Hash256> = (0..100u32).map(|i| hash_bytes(&i.to_le_bytes())).collect();
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

        for idx in [0, 999, 500_000, 999_999] {
            let proof = tree.proof(idx).unwrap();
            assert!(MerkleTree::verify_proof(&proof));
        }
    }

    #[test]
    fn test_incremental_matches_full_rebuild() {
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

        let mut sorted_pairs: Vec<([u8; 32], Hash256)> =
            keys.iter().copied().zip(hashes.iter().copied()).collect();
        sorted_pairs.sort_by_key(|(k, _)| *k);
        let sorted_hashes: Vec<Hash256> = sorted_pairs.iter().map(|(_, h)| *h).collect();

        let full_tree = MerkleTree::from_leaves(sorted_hashes);
        assert_eq!(im.root(), full_tree.root());
    }

    #[test]
    fn bulk_keyed_build_matches_all_sizes_and_key_order_permutations() {
        for size in [0u32, 1, 2, 3, 31, 32, 33, 257, 1_000] {
            let ascending: Vec<_> = (0..size)
                .map(|index| {
                    let mut key = [0u8; 32];
                    key[..4].copy_from_slice(&index.to_be_bytes());
                    (key, hash_bytes(&index.to_le_bytes()))
                })
                .collect();
            let expected = IncrementalMerkle::from_keyed_leaves(ascending.clone());

            let mut reversed = ascending.clone();
            reversed.reverse();
            let mut permuted = ascending.clone();
            permuted.sort_unstable_by_key(|(key, _)| hash_bytes(key).0);
            if !permuted.is_empty() {
                let rotation = permuted.len() / 3;
                permuted.rotate_left(rotation);
            }

            for entries in [ascending, reversed, permuted] {
                let bulk = IncrementalMerkle::from_keyed_leaves(entries);
                assert_eq!(bulk.root(), expected.root(), "size {size}");
                assert_eq!(bulk.len(), size as usize);
            }
        }
    }

    #[test]
    #[should_panic(expected = "duplicate keys")]
    fn bulk_keyed_build_rejects_duplicate_keys() {
        let key = [7u8; 32];
        IncrementalMerkle::from_keyed_leaves(vec![
            (key, hash_bytes(b"first")),
            (key, hash_bytes(b"second")),
        ]);
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

        let new_hash_10 = hash_bytes(b"updated-10");
        let new_hash_25 = hash_bytes(b"updated-25");
        let new_hash_49 = hash_bytes(b"updated-49");

        let (idx10, new10) = im.update(keys[10], new_hash_10);
        let (idx25, new25) = im.update(keys[25], new_hash_25);
        let (idx49, new49) = im.update(keys[49], new_hash_49);
        assert!(!new10);
        assert!(!new25);
        assert!(!new49);

        im.recompute_paths(&[idx10, idx25, idx49]);

        let mut expected_hashes = hashes.clone();
        expected_hashes[10] = new_hash_10;
        expected_hashes[25] = new_hash_25;
        expected_hashes[49] = new_hash_49;
        let full_tree = MerkleTree::from_leaves(expected_hashes);
        assert_eq!(im.root(), full_tree.root());
    }

    #[test]
    fn test_incremental_odd_leaf_count() {
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

        for idx in [0, 1, 50, 98, 99] {
            let proof = im.proof(idx).unwrap();
            assert!(
                MerkleTree::verify_proof(&proof),
                "proof failed at index {idx}"
            );
        }
    }
}
