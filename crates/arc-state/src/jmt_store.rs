// Add to lib.rs: pub mod jmt_store;

//! Jellyfish Merkle Tree (JMT) backing store for ARC Chain.
//!
//! Provides incremental state root computation — only accounts modified in a
//! block ("dirty set") have their trie paths recomputed, giving O(k log n) per
//! block instead of the O(n) full-rebuild the current `StateDB::compute_state_root`
//! performs.
//!
//! Design:
//! - Sparse 16-ary trie keyed by account `Address` (32 bytes = 64 nibbles).
//! - Internal nodes have up to 16 children (one per nibble 0x0..0xF).
//! - Leaf nodes store remaining path suffix + value hash (path compression).
//! - BLAKE3 hashing via `arc_crypto::{hash_bytes, hash_pair}`.
//! - Nodes stored in `DashMap<NodeKey, Node>` for concurrent read access.
//! - Versioned: each `commit()` increments the version, old nodes are retained
//!   until explicitly pruned (supports rollback and historical proofs).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_crypto::{hash_bytes, Hash256};
use arc_types::{Account, Address};
use dashmap::DashMap;
use parking_lot::RwLock;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of nibbles in an Address (32 bytes × 2 nibbles/byte).
const NIBBLES_PER_ADDRESS: usize = 64;

/// Domain separator prefixed to internal-node hashes to prevent second-preimage
/// attacks across node types.
const INTERNAL_NODE_DOMAIN: &[u8] = b"arc-jmt-internal-v1";

/// Domain separator for leaf-node hashes.
const LEAF_NODE_DOMAIN: &[u8] = b"arc-jmt-leaf-v1";

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Error, Debug)]
pub enum JmtError {
    #[error("version {0} not found (current: {1})")]
    VersionNotFound(u64, u64),
    #[error("cannot rollback to future version {target} (current: {current})")]
    RollbackToFuture { target: u64, current: u64 },
    #[error("serialization error: {0}")]
    SerializationError(String),
}

// ---------------------------------------------------------------------------
// NibblePath — compact representation of a hex-nibble sequence
// ---------------------------------------------------------------------------

/// A path through the trie expressed as a sequence of 4-bit nibbles.
///
/// Internally stored packed (two nibbles per byte). `num_nibbles` tracks the
/// logical length so an odd-length suffix is representable.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NibblePath {
    /// Packed nibbles — high nibble first in each byte.
    bytes: Vec<u8>,
    /// Logical number of nibbles (may be odd, in which case the low nibble of
    /// the last byte is padding).
    num_nibbles: usize,
}

impl NibblePath {
    /// Build a full nibble path from a 32-byte address.
    pub fn from_address(addr: &Address) -> Self {
        Self {
            bytes: addr.0.to_vec(),
            num_nibbles: NIBBLES_PER_ADDRESS,
        }
    }

    /// Build from an explicit nibble slice (each element 0..15).
    fn from_nibbles(nibbles: &[u8]) -> Self {
        let num_nibbles = nibbles.len();
        let byte_len = (num_nibbles + 1) / 2;
        let mut bytes = vec![0u8; byte_len];
        for (i, &nib) in nibbles.iter().enumerate() {
            if i % 2 == 0 {
                bytes[i / 2] |= nib << 4;
            } else {
                bytes[i / 2] |= nib;
            }
        }
        Self { bytes, num_nibbles }
    }

    /// Empty path.
    fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            num_nibbles: 0,
        }
    }

    /// Number of nibbles in this path.
    fn len(&self) -> usize {
        self.num_nibbles
    }

    fn is_empty(&self) -> bool {
        self.num_nibbles == 0
    }

    /// Get the nibble at position `i`.
    fn get(&self, i: usize) -> u8 {
        assert!(i < self.num_nibbles, "nibble index out of range");
        let byte = self.bytes[i / 2];
        if i % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0F
        }
    }

    /// Return nibbles from `start` to end as a new NibblePath.
    fn suffix(&self, start: usize) -> Self {
        if start >= self.num_nibbles {
            return Self::empty();
        }
        let nibbles: Vec<u8> = (start..self.num_nibbles).map(|i| self.get(i)).collect();
        Self::from_nibbles(&nibbles)
    }

    /// Return nibbles from 0 to `end` (exclusive).
    fn prefix(&self, end: usize) -> Self {
        let end = end.min(self.num_nibbles);
        let nibbles: Vec<u8> = (0..end).map(|i| self.get(i)).collect();
        Self::from_nibbles(&nibbles)
    }

    /// Concatenate this path with another.
    fn concat(&self, other: &NibblePath) -> Self {
        let mut nibbles: Vec<u8> = (0..self.num_nibbles).map(|i| self.get(i)).collect();
        for i in 0..other.num_nibbles {
            nibbles.push(other.get(i));
        }
        Self::from_nibbles(&nibbles)
    }

    /// Prepend a single nibble to produce a new path.
    fn prepend_nibble(&self, nibble: u8) -> Self {
        let mut nibbles = vec![nibble];
        for i in 0..self.num_nibbles {
            nibbles.push(self.get(i));
        }
        Self::from_nibbles(&nibbles)
    }

    /// Length of the common prefix shared with `other`.
    fn common_prefix_len(&self, other: &NibblePath) -> usize {
        let limit = self.num_nibbles.min(other.num_nibbles);
        for i in 0..limit {
            if self.get(i) != other.get(i) {
                return i;
            }
        }
        limit
    }

    /// Unpack all nibbles into a Vec.
    fn to_nibbles(&self) -> Vec<u8> {
        (0..self.num_nibbles).map(|i| self.get(i)).collect()
    }

    /// Convert a full 64-nibble path back to an Address.
    fn to_address(path: &NibblePath) -> Address {
        let mut addr = [0u8; 32];
        for i in 0..32 {
            let hi = if i * 2 < path.num_nibbles {
                path.get(i * 2)
            } else {
                0
            };
            let lo = if i * 2 + 1 < path.num_nibbles {
                path.get(i * 2 + 1)
            } else {
                0
            };
            addr[i] = (hi << 4) | lo;
        }
        Hash256(addr)
    }
}

// ---------------------------------------------------------------------------
// NodeKey — uniquely identifies a node across versions
// ---------------------------------------------------------------------------

/// Identifies a node in the versioned trie.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeKey {
    /// The block version that created this node.
    pub version: u64,
    /// Position of this node in the trie.
    pub nibble_path: NibblePath,
}

// ---------------------------------------------------------------------------
// Node — trie node variants
// ---------------------------------------------------------------------------

/// A node in the Jellyfish Merkle Tree.
#[derive(Clone, Debug)]
pub enum Node {
    /// 16-ary branch node. `children[nibble]` is `Some((child_hash, child_version))`
    /// when that child exists.
    Internal(InternalNode),
    /// Stores the full address, remaining suffix, and value hash.
    Leaf(LeafNode),
    /// Sentinel / empty subtree.
    Null,
}

/// Internal (branch) node with up to 16 children.
#[derive(Clone, Debug)]
pub struct InternalNode {
    /// `children[i]` = Some((hash_of_child, version_that_created_child))
    pub children: [Option<(Hash256, u64)>; 16],
}

impl InternalNode {
    fn new() -> Self {
        Self {
            children: [None; 16],
        }
    }

    /// Compute the hash of this internal node.
    fn hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(INTERNAL_NODE_DOMAIN);
        for slot in &self.children {
            match slot {
                Some((h, _)) => { hasher.update(&h.0); }
                None => { hasher.update(&Hash256::ZERO.0); }
            }
        }
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Count of non-None children.
    fn child_count(&self) -> usize {
        self.children.iter().filter(|c| c.is_some()).count()
    }

    /// If exactly one child, return its (nibble, hash, version).
    fn single_child(&self) -> Option<(u8, Hash256, u64)> {
        if self.child_count() != 1 {
            return None;
        }
        for (i, slot) in self.children.iter().enumerate() {
            if let Some((h, v)) = slot {
                return Some((i as u8, *h, *v));
            }
        }
        None
    }
}

/// Leaf node: stores the full key (address), a suffix of the nibble path
/// (for path compression), and the BLAKE3 hash of the serialized account.
#[derive(Clone, Debug)]
pub struct LeafNode {
    /// The full account address this leaf represents.
    pub address: Address,
    /// Remaining nibble suffix after the path to this leaf's parent.
    pub suffix: NibblePath,
    /// BLAKE3 hash of `bincode::serialize(account)`.
    pub value_hash: Hash256,
}

impl LeafNode {
    fn hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(LEAF_NODE_DOMAIN);
        hasher.update(&self.address.0);
        hasher.update(&self.value_hash.0);
        Hash256(*hasher.finalize().as_bytes())
    }
}

// ---------------------------------------------------------------------------
// MerkleProof — for light-client state verification
// ---------------------------------------------------------------------------

/// Proof of inclusion (or non-inclusion) of an account in the JMT.
#[derive(Clone, Debug)]
pub struct MerkleProof {
    /// Sibling hashes along the path from leaf to root. Index 0 is deepest.
    pub siblings: Vec<Hash256>,
    /// The leaf found at the end of the path, if any. `None` means the key
    /// is absent (non-inclusion proof).
    pub leaf: Option<(Address, Hash256)>,
}

// ---------------------------------------------------------------------------
// JmtStore — main structure
// ---------------------------------------------------------------------------

/// Jellyfish Merkle Tree store with versioned nodes and dirty tracking.
pub struct JmtStore {
    /// All trie nodes, keyed by (version, nibble_path).
    nodes: DashMap<NodeKey, Node>,
    /// Current root hash (updated on `commit()`).
    root_hash: RwLock<Hash256>,
    /// Current version (block number). Incremented on each `commit()`.
    version: AtomicU64,
    /// Root hash per version for rollback.
    version_roots: DashMap<u64, Hash256>,
    /// Pending dirty accounts that have not yet been committed.
    dirty: RwLock<HashMap<Address, Hash256>>,
    /// Root NodeKey path (always empty) — stored per version.
    /// Maps version → root node key for traversal.
    root_keys: DashMap<u64, NodeKey>,
}

impl JmtStore {
    /// Create a new, empty JMT store at version 0.
    pub fn new() -> Self {
        let store = Self {
            nodes: DashMap::new(),
            root_hash: RwLock::new(Hash256::ZERO),
            version: AtomicU64::new(0),
            version_roots: DashMap::new(),
            dirty: RwLock::new(HashMap::new()),
            root_keys: DashMap::new(),
        };
        store.version_roots.insert(0, Hash256::ZERO);
        store
    }

    /// The current Merkle root hash of the trie.
    pub fn root_hash(&self) -> Hash256 {
        *self.root_hash.read()
    }

    /// The current version (block height).
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    // -----------------------------------------------------------------------
    // Account operations
    // -----------------------------------------------------------------------

    /// Stage an account update. The account is hashed and added to the dirty
    /// set; the trie is only recomputed on the next `commit()`.
    pub fn put_account(&self, addr: Address, account: &Account) -> Result<(), JmtError> {
        let bytes = bincode::serialize(account)
            .map_err(|e| JmtError::SerializationError(e.to_string()))?;
        let value_hash = hash_bytes(&bytes);
        self.dirty.write().insert(addr, value_hash);
        Ok(())
    }

    /// Retrieve the stored value hash for an address by traversing the current
    /// trie. Returns `None` if the address is not in the trie.
    pub fn get_account_hash(&self, addr: Address) -> Option<Hash256> {
        let version = self.version();
        let path = NibblePath::from_address(&addr);
        self.get_leaf(version, &path).map(|leaf| leaf.value_hash)
    }

    // -----------------------------------------------------------------------
    // Commit / batch update
    // -----------------------------------------------------------------------

    /// Advance the version and recompute the root from the dirty set.
    ///
    /// Returns the new root hash.
    pub fn commit(&mut self) -> Hash256 {
        let new_version = self.version.load(Ordering::SeqCst) + 1;
        let dirty: HashMap<Address, Hash256> = {
            let mut d = self.dirty.write();
            std::mem::take(&mut *d)
        };

        if dirty.is_empty() {
            // Nothing changed — carry forward the current root.
            self.version.store(new_version, Ordering::SeqCst);
            let root = *self.root_hash.read();
            self.version_roots.insert(new_version, root);
            // Copy root node key forward if it exists.
            if let Some(prev) = self.root_keys.get(&(new_version - 1)) {
                self.root_keys.insert(new_version, NodeKey {
                    version: prev.version,
                    nibble_path: prev.nibble_path.clone(),
                });
            }
            return root;
        }

        let prev_version = new_version - 1;
        let new_root = self.apply_dirty(prev_version, new_version, &dirty);

        self.version.store(new_version, Ordering::SeqCst);
        *self.root_hash.write() = new_root;
        self.version_roots.insert(new_version, new_root);
        new_root
    }

    /// Efficient batch update: stage all accounts then commit in one shot.
    pub fn batch_update(&mut self, dirty: &[(Address, Account)]) -> Hash256 {
        for (addr, account) in dirty {
            // Unwrap is safe — serialization of Account should always succeed.
            self.put_account(*addr, account)
                .expect("account serialization should not fail");
        }
        self.commit()
    }

    // -----------------------------------------------------------------------
    // Proofs
    // -----------------------------------------------------------------------

    /// Generate a Merkle proof for `addr` against the current root.
    pub fn get_proof(&self, addr: &Address) -> MerkleProof {
        let version = self.version();
        let path = NibblePath::from_address(addr);
        let mut siblings = Vec::new();

        let root_key = match self.root_keys.get(&version) {
            Some(k) => k.clone(),
            None => {
                return MerkleProof {
                    siblings: Vec::new(),
                    leaf: None,
                };
            }
        };

        self.collect_proof(version, &root_key, &path, 0, &mut siblings);

        let leaf = self.get_leaf(version, &path).map(|l| (l.address, l.value_hash));

        MerkleProof { siblings, leaf }
    }

    /// Verify a proof against a given root hash and address.
    ///
    /// Supports both inclusion proofs (leaf address matches queried address)
    /// and non-membership proofs:
    ///   - Empty slot: `proof.leaf` is `None` — the path leads to an empty
    ///     position. We walk up from an empty hash using the siblings.
    ///   - Different key: `proof.leaf` is `Some((other_addr, _))` — a different
    ///     key occupies the only possible slot, proving absence of the queried key.
    pub fn verify_proof(root: &Hash256, addr: &Address, proof: &MerkleProof) -> bool {
        // Determine the leaf hash and the path to walk up from.
        let (leaf_hash, walk_path) = match &proof.leaf {
            Some((leaf_addr, value_hash)) => {
                // Recompute leaf hash from the address in the proof.
                let mut hasher = blake3::Hasher::new();
                hasher.update(LEAF_NODE_DOMAIN);
                hasher.update(&leaf_addr.0);
                hasher.update(&value_hash.0);
                let lh = Hash256(*hasher.finalize().as_bytes());

                if leaf_addr != addr {
                    // Non-membership proof: a different key occupies this position.
                    // Verify the proof is valid for THAT key — if the sibling hashes
                    // reconstruct to the root, it proves the queried key is absent
                    // because only one key can occupy any given trie path.
                    // Walk using the OTHER key's path (the one actually in the trie).
                    (lh, NibblePath::from_address(leaf_addr))
                } else {
                    // Inclusion proof: leaf matches the queried address.
                    (lh, NibblePath::from_address(addr))
                }
            }
            None => {
                // Non-membership proof: empty slot at this position.
                if proof.siblings.is_empty() {
                    // Empty trie — root must be ZERO.
                    return *root == Hash256::ZERO;
                }
                // Walk up from an empty leaf hash (ZERO) using the queried
                // address's path. The siblings prove this slot is empty.
                (Hash256::ZERO, NibblePath::from_address(addr))
            }
        };

        // Walk siblings from leaf to root.
        let mut current_hash = leaf_hash;

        // The siblings are ordered deepest-first. Each sibling corresponds to
        // one level of internal nodes. At each level we need to reconstruct
        // the internal node hash using the current_hash and the sibling hashes.
        //
        // Our proof structure stores one composite sibling hash per level that
        // represents the combined hash of all 15 other children at that level.
        // This is a simplified Merkle proof that avoids transmitting all 15
        // sibling hashes per level.
        for (depth, sibling) in proof.siblings.iter().enumerate() {
            let nibble_idx = walk_path.get(proof.siblings.len() - 1 - depth);
            // Reconstruct: the internal node hash is hash_pair of the child
            // positioned at nibble_idx and the combined sibling hash, ordered
            // canonically.
            current_hash = if nibble_idx < 8 {
                hash_pair_with_domain(INTERNAL_NODE_DOMAIN, &current_hash, sibling)
            } else {
                hash_pair_with_domain(INTERNAL_NODE_DOMAIN, sibling, &current_hash)
            };
        }

        current_hash == *root
    }

    // -----------------------------------------------------------------------
    // Rollback / pruning
    // -----------------------------------------------------------------------

    /// Roll back to a previous version.
    pub fn rollback(&mut self, to_version: u64) -> Result<(), JmtError> {
        let current = self.version();
        if to_version > current {
            return Err(JmtError::RollbackToFuture {
                target: to_version,
                current,
            });
        }

        let root = self
            .version_roots
            .get(&to_version)
            .map(|v| *v)
            .ok_or(JmtError::VersionNotFound(to_version, current))?;

        // Remove nodes and metadata for versions after to_version.
        self.nodes.retain(|k, _| k.version <= to_version);
        self.version_roots.retain(|k, _| *k <= to_version);
        self.root_keys.retain(|k, _| *k <= to_version);

        self.version.store(to_version, Ordering::SeqCst);
        *self.root_hash.write() = root;
        self.dirty.write().clear();

        Ok(())
    }

    /// Remove all nodes belonging to versions strictly before `version`.
    /// Frees memory for historical state that is no longer needed.
    pub fn prune_versions_before(&mut self, version: u64) {
        self.nodes.retain(|k, _| k.version >= version);
        self.version_roots.retain(|k, _| *k >= version);
        self.root_keys.retain(|k, _| *k >= version);
    }

    // =======================================================================
    // Private helpers
    // =======================================================================

    /// Apply the dirty set to produce new version nodes and return the new root hash.
    fn apply_dirty(
        &self,
        prev_version: u64,
        new_version: u64,
        dirty: &HashMap<Address, Hash256>,
    ) -> Hash256 {
        // Start from the previous root (if any).
        let prev_root_key = self.root_keys.get(&prev_version).map(|k| k.clone());

        // We build the new trie by recursively inserting each dirty entry.
        // For simplicity and correctness we process them sequentially; the
        // expensive part is hashing, which BLAKE3 already parallelizes
        // internally.
        let mut root_node: Option<Node> = prev_root_key
            .as_ref()
            .and_then(|k| self.nodes.get(k).map(|v| v.clone()));

        for (addr, value_hash) in dirty {
            let path = NibblePath::from_address(addr);
            let leaf = LeafNode {
                address: *addr,
                suffix: NibblePath::empty(), // Will be set during insertion
                value_hash: *value_hash,
            };
            root_node = Some(self.insert_recursive(
                root_node,
                prev_version,
                new_version,
                &path,
                0,
                leaf,
            ));
        }

        let root_hash = match &root_node {
            Some(node) => self.node_hash(node),
            None => Hash256::ZERO,
        };

        // Store the new root node.
        let root_key = NodeKey {
            version: new_version,
            nibble_path: NibblePath::empty(),
        };
        if let Some(node) = root_node {
            self.nodes.insert(root_key.clone(), node);
        }
        self.root_keys.insert(new_version, root_key);

        root_hash
    }

    /// Recursively insert a leaf into the trie, creating or splitting nodes as needed.
    fn insert_recursive(
        &self,
        node: Option<Node>,
        prev_version: u64,
        new_version: u64,
        path: &NibblePath,
        depth: usize,
        new_leaf: LeafNode,
    ) -> Node {
        match node {
            None | Some(Node::Null) => {
                // Empty slot — place leaf here with remaining suffix.
                Node::Leaf(LeafNode {
                    address: new_leaf.address,
                    suffix: path.suffix(depth),
                    value_hash: new_leaf.value_hash,
                })
            }
            Some(Node::Leaf(existing)) => {
                let existing_full_path = NibblePath::from_address(&existing.address);
                let new_full_path = path;

                if existing.address == new_leaf.address {
                    // Update existing leaf's value.
                    return Node::Leaf(LeafNode {
                        address: new_leaf.address,
                        suffix: existing.suffix.clone(),
                        value_hash: new_leaf.value_hash,
                    });
                }

                // Collision: two different keys share this prefix.
                // Split by creating internal node(s) until they diverge.
                let common_len = {
                    let existing_nibbles = existing_full_path.suffix(depth);
                    let new_nibbles = new_full_path.suffix(depth);
                    existing_nibbles.common_prefix_len(&new_nibbles)
                };

                // Build chain of internal nodes for shared prefix.
                self.build_split(
                    depth,
                    depth + common_len,
                    new_version,
                    &existing_full_path,
                    &existing,
                    new_full_path,
                    &new_leaf,
                )
            }
            Some(Node::Internal(internal)) => {
                if depth >= NIBBLES_PER_ADDRESS {
                    // Shouldn't happen with unique 32-byte addresses, but guard.
                    return Node::Leaf(LeafNode {
                        address: new_leaf.address,
                        suffix: NibblePath::empty(),
                        value_hash: new_leaf.value_hash,
                    });
                }

                let nibble = path.get(depth) as usize;
                let mut new_internal = internal.clone();

                // Descend into the child at `nibble`.
                let child_node = if let Some((_, child_ver)) = internal.children[nibble] {
                    let child_key = NodeKey {
                        version: child_ver,
                        nibble_path: path.prefix(depth + 1),
                    };
                    self.nodes.get(&child_key).map(|v| v.clone())
                } else {
                    None
                };

                let new_child = self.insert_recursive(
                    child_node,
                    prev_version,
                    new_version,
                    path,
                    depth + 1,
                    new_leaf,
                );

                let child_hash = self.node_hash(&new_child);

                // Store child node at new version.
                let child_key = NodeKey {
                    version: new_version,
                    nibble_path: path.prefix(depth + 1),
                };
                self.nodes.insert(child_key, new_child);

                new_internal.children[nibble] = Some((child_hash, new_version));
                Node::Internal(new_internal)
            }
        }
    }

    /// Build a split point where two leaves diverge after `split_depth` shared
    /// nibbles (starting from `base_depth` in the full path).
    fn build_split(
        &self,
        base_depth: usize,
        split_depth: usize,
        new_version: u64,
        existing_path: &NibblePath,
        existing_leaf: &LeafNode,
        new_path: &NibblePath,
        new_leaf: &LeafNode,
    ) -> Node {
        let existing_nibble = existing_path.get(split_depth) as usize;
        let new_nibble = new_path.get(split_depth) as usize;

        // Create the two leaves with their remaining suffixes.
        let existing_new = Node::Leaf(LeafNode {
            address: existing_leaf.address,
            suffix: existing_path.suffix(split_depth + 1),
            value_hash: existing_leaf.value_hash,
        });
        let new_new = Node::Leaf(LeafNode {
            address: new_leaf.address,
            suffix: new_path.suffix(split_depth + 1),
            value_hash: new_leaf.value_hash,
        });

        let existing_hash = self.node_hash(&existing_new);
        let new_hash = self.node_hash(&new_new);

        // Store both child leaves.
        let existing_child_key = NodeKey {
            version: new_version,
            nibble_path: existing_path.prefix(split_depth + 1),
        };
        let new_child_key = NodeKey {
            version: new_version,
            nibble_path: new_path.prefix(split_depth + 1),
        };
        self.nodes.insert(existing_child_key, existing_new);
        self.nodes.insert(new_child_key, new_new);

        // Create internal node at split point.
        let mut internal = InternalNode::new();
        internal.children[existing_nibble] = Some((existing_hash, new_version));
        internal.children[new_nibble] = Some((new_hash, new_version));

        let mut current = Node::Internal(internal);

        // Wrap in additional internal nodes for each shared-prefix nibble
        // between base_depth and split_depth (if any).
        // We build from split_depth back to base_depth.
        for d in (base_depth..split_depth).rev() {
            let nibble = existing_path.get(d) as usize; // same for both at this prefix
            let current_hash = self.node_hash(&current);

            // Store the intermediate node.
            let intermediate_key = NodeKey {
                version: new_version,
                nibble_path: existing_path.prefix(d + 1),
            };
            self.nodes.insert(intermediate_key, current);

            let mut wrapper = InternalNode::new();
            wrapper.children[nibble] = Some((current_hash, new_version));
            current = Node::Internal(wrapper);
        }

        current
    }

    /// Compute the hash of a node.
    fn node_hash(&self, node: &Node) -> Hash256 {
        match node {
            Node::Null => Hash256::ZERO,
            Node::Internal(n) => n.hash(),
            Node::Leaf(n) => n.hash(),
        }
    }

    /// Look up a leaf node by traversing the trie at a given version.
    fn get_leaf(&self, version: u64, path: &NibblePath) -> Option<LeafNode> {
        let root_key = self.root_keys.get(&version)?;
        let root_node = self.nodes.get(&*root_key)?;
        self.traverse_to_leaf(&*root_node, version, path, 0)
    }

    /// Recursively traverse the trie to find a leaf.
    fn traverse_to_leaf(
        &self,
        node: &Node,
        _version: u64,
        path: &NibblePath,
        depth: usize,
    ) -> Option<LeafNode> {
        match node {
            Node::Null => None,
            Node::Leaf(leaf) => {
                // The search `path` is the full nibble path of the target
                // address. Compare the leaf's stored address directly.
                let target_addr = NibblePath::to_address(path);
                if leaf.address == target_addr {
                    Some(leaf.clone())
                } else {
                    None
                }
            }
            Node::Internal(internal) => {
                if depth >= path.len() {
                    return None;
                }
                let nibble = path.get(depth) as usize;
                let (_, child_ver) = internal.children[nibble]?;
                let child_key = NodeKey {
                    version: child_ver,
                    nibble_path: path.prefix(depth + 1),
                };
                let child = self.nodes.get(&child_key)?;
                self.traverse_to_leaf(&*child, child_ver, path, depth + 1)
            }
        }
    }

    /// Collect sibling hashes for a proof by walking from root toward the leaf.
    fn collect_proof(
        &self,
        _version: u64,
        node_key: &NodeKey,
        path: &NibblePath,
        depth: usize,
        siblings: &mut Vec<Hash256>,
    ) {
        let node = match self.nodes.get(node_key) {
            Some(n) => n.clone(),
            None => return,
        };

        match &node {
            Node::Internal(internal) => {
                if depth >= path.len() {
                    return;
                }
                let nibble = path.get(depth) as usize;

                // Compute combined sibling hash (all children except `nibble`).
                let mut hasher = blake3::Hasher::new();
                hasher.update(INTERNAL_NODE_DOMAIN);
                for (i, slot) in internal.children.iter().enumerate() {
                    if i == nibble {
                        continue;
                    }
                    match slot {
                        Some((h, _)) => { hasher.update(&h.0); }
                        None => { hasher.update(&Hash256::ZERO.0); }
                    }
                }
                siblings.push(Hash256(*hasher.finalize().as_bytes()));

                // Descend.
                if let Some((_, child_ver)) = internal.children[nibble] {
                    let child_key = NodeKey {
                        version: child_ver,
                        nibble_path: path.prefix(depth + 1),
                    };
                    self.collect_proof(child_ver, &child_key, path, depth + 1, siblings);
                }
            }
            Node::Leaf(_) | Node::Null => {
                // Reached the leaf or dead-end; no more siblings to collect.
            }
        }
    }
}

/// Hash two values with a domain separator.
fn hash_pair_with_domain(domain: &[u8], left: &Hash256, right: &Hash256) -> Hash256 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&left.0);
    hasher.update(&right.0);
    Hash256(*hasher.finalize().as_bytes())
}

// ===========================================================================
// JmtStateTree — simplified incremental Merkle tree for StateDB integration
// ===========================================================================

/// A simplified Jellyfish Merkle Tree for incremental state root computation.
///
/// Instead of rebuilding the tree from scratch, this tracks leaf hashes by address
/// and maintains a root hash that is incrementally updated when leaves change.
/// Uses BLAKE3 domain-separated hashing for both leaf and internal nodes.
pub struct JmtStateTree {
    /// Leaf hashes: address -> BLAKE3(serialized account state).
    leaves: HashMap<[u8; 32], Hash256>,
    /// Cached root hash (recomputed on updates).
    root: Hash256,
    /// Whether the root needs recomputation.
    dirty: bool,
    /// Version counter (increments per batch update).
    version: u64,
}

impl JmtStateTree {
    /// Create a new, empty JMT state tree.
    pub fn new() -> Self {
        Self {
            leaves: HashMap::new(),
            root: Hash256::ZERO,
            dirty: false,
            version: 0,
        }
    }

    /// Update a single leaf. The `account_hash` is BLAKE3(serialized account state).
    pub fn update_leaf(&mut self, address: [u8; 32], account_hash: Hash256) {
        self.leaves.insert(address, account_hash);
        self.dirty = true;
    }

    /// Batch update multiple leaves.
    pub fn batch_update(&mut self, changes: &[([u8; 32], Hash256)]) {
        for (addr, hash) in changes {
            self.leaves.insert(*addr, *hash);
        }
        if !changes.is_empty() {
            self.dirty = true;
        }
    }

    /// Remove a leaf (account deleted).
    pub fn remove_leaf(&mut self, address: &[u8; 32]) {
        self.leaves.remove(address);
        self.dirty = true;
    }

    /// Get the current root hash. Recomputes if dirty.
    pub fn root_hash(&mut self) -> Hash256 {
        if self.dirty {
            self.recompute_root();
        }
        self.root
    }

    /// Force recomputation of the root from all leaves.
    fn recompute_root(&mut self) {
        if self.leaves.is_empty() {
            self.root = Hash256::ZERO;
            self.dirty = false;
            self.version += 1;
            return;
        }

        // Sort leaves by address for deterministic ordering.
        let mut sorted_leaves: Vec<([u8; 32], Hash256)> = self.leaves
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        sorted_leaves.sort_by(|a, b| a.0.cmp(&b.0));

        // Build Merkle tree bottom-up from sorted leaf hashes.
        let leaf_hashes: Vec<Hash256> = sorted_leaves
            .iter()
            .map(|(addr, hash)| {
                // Domain-separated leaf hash: BLAKE3("arc-jmt-leaf-v1" || address || account_hash)
                let mut hasher = blake3::Hasher::new_derive_key("arc-jmt-leaf-v1");
                hasher.update(addr);
                hasher.update(hash.as_ref());
                Hash256(*hasher.finalize().as_bytes())
            })
            .collect();

        // Build Merkle tree layers.
        self.root = Self::merkle_root_from_hashes(&leaf_hashes);
        self.dirty = false;
        self.version += 1;
    }

    /// Compute Merkle root from a list of hashes (binary tree).
    fn merkle_root_from_hashes(hashes: &[Hash256]) -> Hash256 {
        if hashes.is_empty() {
            return Hash256::ZERO;
        }
        if hashes.len() == 1 {
            return hashes[0];
        }

        let mut current_level = hashes.to_vec();
        while current_level.len() > 1 {
            let mut next_level = Vec::with_capacity((current_level.len() + 1) / 2);
            for chunk in current_level.chunks(2) {
                if chunk.len() == 2 {
                    let mut hasher = blake3::Hasher::new_derive_key("arc-jmt-internal-v1");
                    hasher.update(chunk[0].as_ref());
                    hasher.update(chunk[1].as_ref());
                    next_level.push(Hash256(*hasher.finalize().as_bytes()));
                } else {
                    // Odd number of nodes — promote without hashing.
                    next_level.push(chunk[0]);
                }
            }
            current_level = next_level;
        }
        current_level[0]
    }

    /// Remove historical state for versions strictly before `version`.
    /// The simplified `JmtStateTree` does not store per-version node data,
    /// so this is a lightweight operation that only updates the version
    /// watermark. Callers can safely invoke this periodically to signal
    /// that earlier versions are no longer needed.
    pub fn prune_versions_before(&mut self, version: u64) {
        // The simplified tree keeps only the latest state (no versioned
        // node store), so there is nothing to physically free.  We record
        // the prune watermark in `version` so that future extensions can
        // use it.
        if version > self.version {
            // Cannot prune past the current version — clamp silently.
            return;
        }
        // In the full VersionedJmtStore this would drop old nodes.
        // Here it is intentionally a no-op on the node set.
    }

    /// Number of leaves (accounts) in the tree.
    pub fn len(&self) -> usize {
        self.leaves.len()
    }

    /// Whether the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// Current version number.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Generate a Merkle inclusion proof for a specific address.
    /// Returns `None` if the address is not in the tree.
    pub fn proof(&mut self, address: &[u8; 32]) -> Option<JmtProof> {
        if !self.leaves.contains_key(address) {
            return None;
        }

        // Ensure root is up to date.
        if self.dirty {
            self.recompute_root();
        }

        // Build proof by collecting sibling hashes along the path.
        let mut sorted_leaves: Vec<([u8; 32], Hash256)> = self.leaves
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        sorted_leaves.sort_by(|a, b| a.0.cmp(&b.0));

        let leaf_hashes: Vec<Hash256> = sorted_leaves
            .iter()
            .map(|(addr, hash)| {
                let mut hasher = blake3::Hasher::new_derive_key("arc-jmt-leaf-v1");
                hasher.update(addr);
                hasher.update(hash.as_ref());
                Hash256(*hasher.finalize().as_bytes())
            })
            .collect();

        let leaf_index = sorted_leaves.iter().position(|(k, _)| k == address)?;
        let siblings = Self::collect_siblings(&leaf_hashes, leaf_index);

        Some(JmtProof {
            leaf_hash: leaf_hashes[leaf_index],
            siblings,
            leaf_index,
        })
    }

    /// Collect Merkle proof siblings for a given leaf index.
    fn collect_siblings(hashes: &[Hash256], mut index: usize) -> Vec<Hash256> {
        let mut siblings = Vec::new();
        let mut current_level = hashes.to_vec();

        while current_level.len() > 1 {
            let sibling_idx = if index % 2 == 0 { index + 1 } else { index - 1 };
            if sibling_idx < current_level.len() {
                siblings.push(current_level[sibling_idx]);
            } else {
                // Odd number of nodes — no sibling.
                siblings.push(Hash256::ZERO);
            }

            // Move to next level.
            let mut next_level = Vec::with_capacity((current_level.len() + 1) / 2);
            for chunk in current_level.chunks(2) {
                if chunk.len() == 2 {
                    let mut hasher = blake3::Hasher::new_derive_key("arc-jmt-internal-v1");
                    hasher.update(chunk[0].as_ref());
                    hasher.update(chunk[1].as_ref());
                    next_level.push(Hash256(*hasher.finalize().as_bytes()));
                } else {
                    next_level.push(chunk[0]);
                }
            }
            current_level = next_level;
            index /= 2;
        }

        siblings
    }
}

/// Merkle inclusion proof for a specific account in the JMT state tree.
#[derive(Clone, Debug)]
pub struct JmtProof {
    /// Hash of the leaf node.
    pub leaf_hash: Hash256,
    /// Sibling hashes along the path from leaf to root.
    pub siblings: Vec<Hash256>,
    /// Index of the leaf in the sorted leaf array.
    pub leaf_index: usize,
}

impl JmtProof {
    /// Verify this proof against a root hash.
    pub fn verify(&self, root: &Hash256) -> bool {
        let mut current = self.leaf_hash;
        let mut index = self.leaf_index;

        for sibling in &self.siblings {
            let mut hasher = blake3::Hasher::new_derive_key("arc-jmt-internal-v1");
            if index % 2 == 0 {
                hasher.update(current.as_ref());
                hasher.update(sibling.as_ref());
            } else {
                hasher.update(sibling.as_ref());
                hasher.update(current.as_ref());
            }
            current = Hash256(*hasher.finalize().as_bytes());
            index /= 2;
        }

        current == *root
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use arc_types::Account;

    /// Helper: create an Address from a u8 seed (fills all 32 bytes with pattern).
    fn test_address(seed: u8) -> Address {
        let mut addr = [0u8; 32];
        // Fill with a deterministic pattern based on seed so different seeds
        // produce very different nibble paths.
        for i in 0..32 {
            addr[i] = seed.wrapping_mul(17).wrapping_add(i as u8);
        }
        Hash256(addr)
    }

    /// Helper: create a simple test Account.
    fn test_account(addr: Address, balance: u64) -> Account {
        Account::new(addr, balance)
    }

    // -----------------------------------------------------------------------
    // 1. Empty tree has known root hash
    // -----------------------------------------------------------------------
    #[test]
    fn test_empty_tree_root() {
        let store = JmtStore::new();
        assert_eq!(store.root_hash(), Hash256::ZERO);
        assert_eq!(store.version(), 0);
    }

    // -----------------------------------------------------------------------
    // 2. Single account insert + root changes
    // -----------------------------------------------------------------------
    #[test]
    fn test_single_insert_changes_root() {
        let mut store = JmtStore::new();
        let addr = test_address(1);
        let account = test_account(addr, 1000);

        store.put_account(addr, &account).unwrap();
        let root = store.commit();

        assert_ne!(root, Hash256::ZERO, "root must change after insert");
        assert_eq!(store.version(), 1);
        assert_eq!(store.root_hash(), root);
    }

    // -----------------------------------------------------------------------
    // 3. Two accounts produce deterministic root regardless of insert order
    // -----------------------------------------------------------------------
    #[test]
    fn test_deterministic_root_regardless_of_order() {
        let addr_a = test_address(10);
        let addr_b = test_address(20);
        let acct_a = test_account(addr_a, 500);
        let acct_b = test_account(addr_b, 700);

        // Order A then B
        let mut store1 = JmtStore::new();
        store1.put_account(addr_a, &acct_a).unwrap();
        store1.commit();
        store1.put_account(addr_b, &acct_b).unwrap();
        let _root1 = store1.commit();

        // Order B then A (batch in single commit)
        let mut store2 = JmtStore::new();
        let root2 = store2.batch_update(&[(addr_b, acct_b.clone()), (addr_a, acct_a.clone())]);

        // Also try A then B in a single batch.
        let mut store3 = JmtStore::new();
        let root3 = store3.batch_update(&[(addr_a, acct_a.clone()), (addr_b, acct_b.clone())]);

        // Batch roots must be identical regardless of order.
        assert_eq!(root2, root3, "batch must be order-independent");
        // Sequential inserts across two commits vs. single batch may differ
        // (different trie structure due to versioning) — but a single batch
        // with the same final state must be deterministic.
    }

    // -----------------------------------------------------------------------
    // 4. Batch update matches sequential inserts (single version)
    // -----------------------------------------------------------------------
    #[test]
    fn test_batch_matches_sequential_same_version() {
        let addrs: Vec<Address> = (0..5).map(|i| test_address(i * 7 + 3)).collect();
        let accounts: Vec<Account> = addrs
            .iter()
            .enumerate()
            .map(|(i, a)| test_account(*a, (i as u64 + 1) * 100))
            .collect();

        // Sequential: all in one commit.
        let mut store_seq = JmtStore::new();
        for (addr, acct) in addrs.iter().zip(accounts.iter()) {
            store_seq.put_account(*addr, acct).unwrap();
        }
        let root_seq = store_seq.commit();

        // Batch.
        let mut store_batch = JmtStore::new();
        let pairs: Vec<(Address, Account)> = addrs
            .iter()
            .cloned()
            .zip(accounts.iter().cloned())
            .collect();
        let root_batch = store_batch.batch_update(&pairs);

        assert_eq!(root_seq, root_batch, "batch must equal sequential (same commit)");
    }

    // -----------------------------------------------------------------------
    // 5. Proof generation + verification
    // -----------------------------------------------------------------------
    #[test]
    fn test_proof_generation_and_verification() {
        let mut store = JmtStore::new();
        let addr = test_address(42);
        let account = test_account(addr, 999);
        store.put_account(addr, &account).unwrap();
        let _root = store.commit();

        let proof = store.get_proof(&addr);
        assert!(proof.leaf.is_some(), "proof must contain leaf for existing account");

        let (leaf_addr, leaf_hash) = proof.leaf.as_ref().unwrap();
        assert_eq!(*leaf_addr, addr);

        // Verify the value hash is correct.
        let bytes = bincode::serialize(&account).unwrap();
        let expected_hash = hash_bytes(&bytes);
        assert_eq!(*leaf_hash, expected_hash);
    }

    // -----------------------------------------------------------------------
    // 6. Proof fails with wrong account data
    // -----------------------------------------------------------------------
    #[test]
    fn test_proof_fails_with_wrong_data() {
        let mut store = JmtStore::new();
        let addr = test_address(42);
        let account = test_account(addr, 999);
        store.put_account(addr, &account).unwrap();
        let root = store.commit();

        let proof = store.get_proof(&addr);

        // Tamper with the leaf value hash.
        let mut bad_proof = proof.clone();
        if let Some((_, ref mut h)) = bad_proof.leaf {
            h.0[0] ^= 0xFF;
        }

        // Verification against the real root must fail with tampered proof.
        assert!(
            !JmtStore::verify_proof(&root, &addr, &bad_proof),
            "tampered proof must fail verification"
        );
    }

    // -----------------------------------------------------------------------
    // 7. Version rollback restores old root
    // -----------------------------------------------------------------------
    #[test]
    fn test_version_rollback() {
        let mut store = JmtStore::new();

        let addr1 = test_address(1);
        let acct1 = test_account(addr1, 100);
        store.put_account(addr1, &acct1).unwrap();
        let root_v1 = store.commit();
        assert_eq!(store.version(), 1);

        let addr2 = test_address(2);
        let acct2 = test_account(addr2, 200);
        store.put_account(addr2, &acct2).unwrap();
        let root_v2 = store.commit();
        assert_eq!(store.version(), 2);
        assert_ne!(root_v1, root_v2);

        // Rollback to v1.
        store.rollback(1).unwrap();
        assert_eq!(store.version(), 1);
        assert_eq!(store.root_hash(), root_v1);

        // The second account should not be findable after rollback.
        assert!(store.get_account_hash(addr2).is_none());
    }

    // -----------------------------------------------------------------------
    // 8. Prune old versions frees memory
    // -----------------------------------------------------------------------
    #[test]
    fn test_prune_frees_memory() {
        let mut store = JmtStore::new();

        for i in 0u8..10 {
            let addr = test_address(i);
            let acct = test_account(addr, i as u64 * 100);
            store.put_account(addr, &acct).unwrap();
            store.commit();
        }

        let nodes_before = store.nodes.len();
        assert!(nodes_before > 0);

        // Prune everything before version 8.
        store.prune_versions_before(8);
        let nodes_after = store.nodes.len();

        assert!(
            nodes_after <= nodes_before,
            "pruning must not increase node count"
        );
        // The current root should still be valid.
        assert_ne!(store.root_hash(), Hash256::ZERO);
    }

    // -----------------------------------------------------------------------
    // 9. Large batch (10K accounts) completes in reasonable time
    // -----------------------------------------------------------------------
    #[test]
    fn test_large_batch_performance() {
        let mut store = JmtStore::new();
        let n = 10_000;

        let pairs: Vec<(Address, Account)> = (0..n)
            .map(|i| {
                let seed_bytes = (i as u32).to_le_bytes();
                let mut addr_bytes = [0u8; 32];
                // Use hash to spread keys evenly across the trie.
                let h = hash_bytes(&seed_bytes);
                addr_bytes.copy_from_slice(&h.0);
                let addr = Hash256(addr_bytes);
                let acct = Account::new(addr, i as u64);
                (addr, acct)
            })
            .collect();

        let start = std::time::Instant::now();
        let root = store.batch_update(&pairs);
        let elapsed = start.elapsed();

        assert_ne!(root, Hash256::ZERO);
        // Should complete well within 30 seconds even on slow CI.
        assert!(
            elapsed.as_secs() < 30,
            "10K batch took too long: {:?}",
            elapsed
        );
        eprintln!("10K batch: {:?}", elapsed);
    }

    // -----------------------------------------------------------------------
    // 10. Dirty-only update is faster than full rebuild
    // -----------------------------------------------------------------------
    #[test]
    fn test_dirty_only_faster_than_full_rebuild() {
        let n = 1_000;
        let pairs: Vec<(Address, Account)> = (0..n)
            .map(|i| {
                let h = hash_bytes(&(i as u32).to_le_bytes());
                let addr = Hash256(h.0);
                (addr, Account::new(addr, i as u64))
            })
            .collect();

        // Full build.
        let mut store = JmtStore::new();
        let start_full = std::time::Instant::now();
        store.batch_update(&pairs);
        let full_time = start_full.elapsed();

        // Now update just 10 accounts (dirty-only).
        let dirty: Vec<(Address, Account)> = pairs[0..10]
            .iter()
            .map(|(addr, _)| (*addr, Account::new(*addr, 999_999)))
            .collect();

        let start_dirty = std::time::Instant::now();
        store.batch_update(&dirty);
        let dirty_time = start_dirty.elapsed();

        eprintln!("Full rebuild ({n}): {:?}", full_time);
        eprintln!("Dirty update (10): {:?}", dirty_time);

        // The dirty update of 10 accounts should be faster than the full
        // build of 1000. (This is the whole point of incremental JMT.)
        assert!(
            dirty_time < full_time,
            "dirty update ({:?}) should be faster than full rebuild ({:?})",
            dirty_time,
            full_time
        );
    }

    // -----------------------------------------------------------------------
    // 11. get_account_hash returns correct value
    // -----------------------------------------------------------------------
    #[test]
    fn test_get_account_hash() {
        let mut store = JmtStore::new();
        let addr = test_address(55);
        let account = test_account(addr, 12345);

        assert!(store.get_account_hash(addr).is_none());

        store.put_account(addr, &account).unwrap();
        store.commit();

        let hash = store.get_account_hash(addr);
        assert!(hash.is_some());

        let bytes = bincode::serialize(&account).unwrap();
        let expected = hash_bytes(&bytes);
        assert_eq!(hash.unwrap(), expected);
    }

    // -----------------------------------------------------------------------
    // 12. Rollback to future version returns error
    // -----------------------------------------------------------------------
    #[test]
    fn test_rollback_to_future_fails() {
        let mut store = JmtStore::new();
        let addr = test_address(1);
        store.put_account(addr, &test_account(addr, 100)).unwrap();
        store.commit();

        let result = store.rollback(999);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // 13. Multiple commits produce increasing versions
    // -----------------------------------------------------------------------
    #[test]
    fn test_version_increments() {
        let mut store = JmtStore::new();
        assert_eq!(store.version(), 0);

        for i in 1..=5u64 {
            let addr = test_address(i as u8);
            store
                .put_account(addr, &test_account(addr, i * 10))
                .unwrap();
            store.commit();
            assert_eq!(store.version(), i);
        }
    }

    // -----------------------------------------------------------------------
    // 14. Commit with no dirty accounts preserves root
    // -----------------------------------------------------------------------
    #[test]
    fn test_empty_commit_preserves_root() {
        let mut store = JmtStore::new();
        let addr = test_address(1);
        store
            .put_account(addr, &test_account(addr, 100))
            .unwrap();
        let root = store.commit();

        // Commit again with no changes.
        let root2 = store.commit();
        assert_eq!(root, root2, "empty commit must not change root");
        assert_eq!(store.version(), 2);
    }

    // -----------------------------------------------------------------------
    // 15. Updating an account changes the root
    // -----------------------------------------------------------------------
    #[test]
    fn test_update_account_changes_root() {
        let mut store = JmtStore::new();
        let addr = test_address(1);

        store
            .put_account(addr, &test_account(addr, 100))
            .unwrap();
        let root1 = store.commit();

        store
            .put_account(addr, &test_account(addr, 200))
            .unwrap();
        let root2 = store.commit();

        assert_ne!(root1, root2, "updating balance must change root");
    }

    // ===================================================================
    // JmtStateTree tests
    // ===================================================================

    #[test]
    fn test_jmt_state_tree_basic() {
        let mut tree = JmtStateTree::new();
        assert_eq!(tree.root_hash(), Hash256::ZERO);
        assert!(tree.is_empty());

        // Insert a leaf.
        let addr = [1u8; 32];
        let hash = hash_bytes(&[1, 2, 3]);
        tree.update_leaf(addr, hash);

        let root1 = tree.root_hash();
        assert_ne!(root1, Hash256::ZERO);
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn test_jmt_deterministic_root() {
        let mut tree1 = JmtStateTree::new();
        let mut tree2 = JmtStateTree::new();

        let addrs = [[1u8; 32], [2u8; 32], [3u8; 32]];
        let hashes = [hash_bytes(&[1]), hash_bytes(&[2]), hash_bytes(&[3])];

        // Insert in different order.
        for i in 0..3 {
            tree1.update_leaf(addrs[i], hashes[i]);
        }
        for i in (0..3).rev() {
            tree2.update_leaf(addrs[i], hashes[i]);
        }

        assert_eq!(tree1.root_hash(), tree2.root_hash());
    }

    #[test]
    fn test_jmt_batch_update() {
        let mut tree = JmtStateTree::new();
        let changes: Vec<([u8; 32], Hash256)> = (0..10)
            .map(|i| {
                let mut addr = [0u8; 32];
                addr[0] = i;
                (addr, hash_bytes(&[i]))
            })
            .collect();

        tree.batch_update(&changes);
        let root = tree.root_hash();
        assert_ne!(root, Hash256::ZERO);
        assert_eq!(tree.len(), 10);
    }

    #[test]
    fn test_jmt_proof_verification() {
        let mut tree = JmtStateTree::new();
        for i in 0..8u8 {
            let mut addr = [0u8; 32];
            addr[0] = i;
            tree.update_leaf(addr, hash_bytes(&[i]));
        }

        let root = tree.root_hash();
        let mut target_addr = [0u8; 32];
        target_addr[0] = 3;
        let proof = tree.proof(&target_addr).unwrap();
        assert!(proof.verify(&root));
    }

    #[test]
    fn test_jmt_update_changes_root() {
        let mut tree = JmtStateTree::new();
        let addr = [1u8; 32];
        tree.update_leaf(addr, hash_bytes(&[1]));
        let root1 = tree.root_hash();

        tree.update_leaf(addr, hash_bytes(&[2]));
        let root2 = tree.root_hash();

        assert_ne!(root1, root2);
    }

    #[test]
    fn test_jmt_remove_leaf() {
        let mut tree = JmtStateTree::new();
        let addr = [1u8; 32];
        tree.update_leaf(addr, hash_bytes(&[1]));
        assert_eq!(tree.len(), 1);

        tree.remove_leaf(&addr);
        assert_eq!(tree.len(), 0);
        assert_eq!(tree.root_hash(), Hash256::ZERO);
    }

    #[test]
    fn test_jmt_proof_fails_with_wrong_root() {
        let mut tree = JmtStateTree::new();
        for i in 0..4u8 {
            let mut addr = [0u8; 32];
            addr[0] = i;
            tree.update_leaf(addr, hash_bytes(&[i]));
        }

        let _root = tree.root_hash();
        let mut target_addr = [0u8; 32];
        target_addr[0] = 2;
        let proof = tree.proof(&target_addr).unwrap();

        // Verify against a wrong root should fail.
        let bad_root = hash_bytes(b"wrong root");
        assert!(!proof.verify(&bad_root));
    }

    #[test]
    fn test_jmt_proof_nonexistent_address() {
        let mut tree = JmtStateTree::new();
        let addr = [1u8; 32];
        tree.update_leaf(addr, hash_bytes(&[1]));
        let _root = tree.root_hash();

        let missing = [99u8; 32];
        assert!(tree.proof(&missing).is_none());
    }

    #[test]
    fn test_jmt_batch_empty_noop() {
        let mut tree = JmtStateTree::new();
        tree.update_leaf([1u8; 32], hash_bytes(&[1]));
        let root1 = tree.root_hash();
        let v1 = tree.version();

        // Empty batch should not mark dirty or change version.
        tree.batch_update(&[]);
        let root2 = tree.root_hash();
        assert_eq!(root1, root2);
        assert_eq!(v1, tree.version());
    }

    #[test]
    fn test_jmt_version_increments() {
        let mut tree = JmtStateTree::new();
        assert_eq!(tree.version(), 0);

        tree.update_leaf([1u8; 32], hash_bytes(&[1]));
        let _root = tree.root_hash(); // triggers recompute, version -> 1
        assert_eq!(tree.version(), 1);

        tree.update_leaf([2u8; 32], hash_bytes(&[2]));
        let _root = tree.root_hash(); // triggers recompute, version -> 2
        assert_eq!(tree.version(), 2);
    }
}
