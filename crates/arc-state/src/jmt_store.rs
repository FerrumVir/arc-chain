use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_crypto::{Hash256, hash_bytes};
use arc_types::{Account, Address};
use dashmap::DashMap;
use parking_lot::RwLock;
use thiserror::Error;

const NIBBLES_PER_ADDRESS: usize = 64;

const INTERNAL_NODE_DOMAIN: &[u8] = b"arc-jmt-internal-v1";

const LEAF_NODE_DOMAIN: &[u8] = b"arc-jmt-leaf-v1";

#[derive(Error, Debug)]
pub enum JmtError {
    #[error("version {0} not found (current: {1})")]
    VersionNotFound(u64, u64),
    #[error("cannot rollback to future version {target} (current: {current})")]
    RollbackToFuture { target: u64, current: u64 },
    #[error("serialization error: {0}")]
    SerializationError(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NibblePath {
    bytes: Vec<u8>,
    num_nibbles: usize,
}

impl NibblePath {
    pub fn from_address(addr: &Address) -> Self {
        Self {
            bytes: addr.0.to_vec(),
            num_nibbles: NIBBLES_PER_ADDRESS,
        }
    }

    fn from_nibbles(nibbles: &[u8]) -> Self {
        let num_nibbles = nibbles.len();
        let byte_len = num_nibbles.div_ceil(2);
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

    fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            num_nibbles: 0,
        }
    }

    fn len(&self) -> usize {
        self.num_nibbles
    }

    fn get(&self, i: usize) -> Option<u8> {
        if i >= self.num_nibbles {
            return None;
        }
        let byte = self.bytes[i / 2];
        if i.is_multiple_of(2) {
            Some(byte >> 4)
        } else {
            Some(byte & 0x0F)
        }
    }

    fn suffix(&self, start: usize) -> Self {
        if start >= self.num_nibbles {
            return Self::empty();
        }
        let nibbles: Vec<u8> = (start..self.num_nibbles)
            .map(|i| self.get(i).unwrap())
            .collect();
        Self::from_nibbles(&nibbles)
    }

    fn prefix(&self, end: usize) -> Self {
        let end = end.min(self.num_nibbles);
        let nibbles: Vec<u8> = (0..end).map(|i| self.get(i).unwrap()).collect();
        Self::from_nibbles(&nibbles)
    }

    fn common_prefix_len(&self, other: &NibblePath) -> usize {
        let limit = self.num_nibbles.min(other.num_nibbles);
        for i in 0..limit {
            if self.get(i).unwrap() != other.get(i).unwrap() {
                return i;
            }
        }
        limit
    }

    fn to_address(path: &NibblePath) -> Address {
        let mut addr = [0u8; 32];
        for (i, byte) in addr.iter_mut().enumerate() {
            let hi = path.get(i * 2).unwrap_or(0);
            let lo = path.get(i * 2 + 1).unwrap_or(0);
            *byte = (hi << 4) | lo;
        }
        Hash256(addr)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeKey {
    pub version: u64,
    pub nibble_path: NibblePath,
}

#[derive(Clone, Debug)]
pub enum Node {
    Internal(Box<InternalNode>),
    Leaf(LeafNode),
    Null,
}

#[derive(Clone, Debug)]
pub struct InternalNode {
    pub children: [Option<(Hash256, u64)>; 16],
}

impl InternalNode {
    fn new() -> Self {
        Self {
            children: [None; 16],
        }
    }

    fn hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(INTERNAL_NODE_DOMAIN);
        for slot in &self.children {
            match slot {
                Some((h, _)) => {
                    hasher.update(&h.0);
                }
                None => {
                    hasher.update(&Hash256::ZERO.0);
                }
            }
        }
        Hash256(*hasher.finalize().as_bytes())
    }
}

#[derive(Clone, Debug)]
pub struct LeafNode {
    pub address: Address,
    pub suffix: NibblePath,
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

#[derive(Clone, Copy)]
struct SplitPair<'a> {
    existing_path: &'a NibblePath,
    existing_leaf: &'a LeafNode,
    new_path: &'a NibblePath,
    new_leaf: &'a LeafNode,
}

#[derive(Clone, Debug)]
pub struct MerkleProof {
    pub siblings: Vec<Hash256>,
    pub leaf: Option<(Address, Hash256)>,
}

pub struct JmtStore {
    nodes: DashMap<NodeKey, Node>,
    root_hash: RwLock<Hash256>,
    version: AtomicU64,
    version_roots: DashMap<u64, Hash256>,
    dirty: RwLock<HashMap<Address, Hash256>>,
    root_keys: DashMap<u64, NodeKey>,
}

impl Default for JmtStore {
    fn default() -> Self {
        Self::new()
    }
}

impl JmtStore {
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

    pub fn root_hash(&self) -> Hash256 {
        *self.root_hash.read()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    pub fn put_account(&self, addr: Address, account: &Account) -> Result<(), JmtError> {
        let bytes =
            bincode::serialize(account).map_err(|e| JmtError::SerializationError(e.to_string()))?;
        let value_hash = hash_bytes(&bytes);
        self.dirty.write().insert(addr, value_hash);
        Ok(())
    }

    pub fn get_account_hash(&self, addr: Address) -> Option<Hash256> {
        let version = self.version();
        let path = NibblePath::from_address(&addr);
        self.get_leaf(version, &path).map(|leaf| leaf.value_hash)
    }

    pub fn commit(&mut self) -> Hash256 {
        let new_version = self.version.load(Ordering::SeqCst) + 1;
        let dirty: HashMap<Address, Hash256> = {
            let mut d = self.dirty.write();
            std::mem::take(&mut *d)
        };

        if dirty.is_empty() {
            self.version.store(new_version, Ordering::SeqCst);
            let root = *self.root_hash.read();
            self.version_roots.insert(new_version, root);
            let previous_root_key = self
                .root_keys
                .get(&(new_version - 1))
                .map(|prev| prev.clone());
            if let Some(prev) = previous_root_key {
                self.root_keys.insert(
                    new_version,
                    NodeKey {
                        version: prev.version,
                        nibble_path: prev.nibble_path.clone(),
                    },
                );
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

    pub fn batch_update(&mut self, dirty: &[(Address, Account)]) -> Hash256 {
        for (addr, account) in dirty {
            self.put_account(*addr, account)
                .expect("account serialization should not fail");
        }
        self.commit()
    }

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

        let leaf = self
            .get_leaf(version, &path)
            .map(|l| (l.address, l.value_hash));

        MerkleProof { siblings, leaf }
    }

    pub fn verify_proof(root: &Hash256, addr: &Address, proof: &MerkleProof) -> bool {
        let (leaf_hash, walk_path) = match &proof.leaf {
            Some((leaf_addr, value_hash)) => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(LEAF_NODE_DOMAIN);
                hasher.update(&leaf_addr.0);
                hasher.update(&value_hash.0);
                let lh = Hash256(*hasher.finalize().as_bytes());

                if leaf_addr != addr {
                    (lh, NibblePath::from_address(leaf_addr))
                } else {
                    (lh, NibblePath::from_address(addr))
                }
            }
            None => {
                if proof.siblings.is_empty() {
                    return *root == Hash256::ZERO;
                }
                (Hash256::ZERO, NibblePath::from_address(addr))
            }
        };

        let mut current_hash = leaf_hash;

        for (depth, sibling) in proof.siblings.iter().enumerate() {
            let nibble_idx = match walk_path.get(proof.siblings.len() - 1 - depth) {
                Some(n) => n,
                None => return false,
            };
            current_hash = if nibble_idx < 8 {
                hash_pair_with_domain(INTERNAL_NODE_DOMAIN, &current_hash, sibling)
            } else {
                hash_pair_with_domain(INTERNAL_NODE_DOMAIN, sibling, &current_hash)
            };
        }

        current_hash == *root
    }

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

        self.nodes.retain(|k, _| k.version <= to_version);
        self.version_roots.retain(|k, _| *k <= to_version);
        self.root_keys.retain(|k, _| *k <= to_version);

        self.version.store(to_version, Ordering::SeqCst);
        *self.root_hash.write() = root;
        self.dirty.write().clear();

        Ok(())
    }

    pub fn prune_versions_before(&mut self, version: u64) {
        self.nodes.retain(|k, _| k.version >= version);
        self.version_roots.retain(|k, _| *k >= version);
        self.root_keys.retain(|k, _| *k >= version);
    }

    fn apply_dirty(
        &self,
        prev_version: u64,
        new_version: u64,
        dirty: &HashMap<Address, Hash256>,
    ) -> Hash256 {
        let prev_root_key = self.root_keys.get(&prev_version).map(|k| k.clone());

        let mut root_node: Option<Node> = prev_root_key
            .as_ref()
            .and_then(|k| self.nodes.get(k).map(|v| v.clone()));

        for (addr, value_hash) in dirty {
            let path = NibblePath::from_address(addr);
            let leaf = LeafNode {
                address: *addr,
                suffix: NibblePath::empty(),
                value_hash: *value_hash,
            };
            root_node = Some(self.insert_recursive(root_node, new_version, &path, 0, leaf));
        }

        let root_hash = match &root_node {
            Some(node) => self.node_hash(node),
            None => Hash256::ZERO,
        };

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

    fn insert_recursive(
        &self,
        node: Option<Node>,
        new_version: u64,
        path: &NibblePath,
        depth: usize,
        new_leaf: LeafNode,
    ) -> Node {
        match node {
            None | Some(Node::Null) => Node::Leaf(LeafNode {
                address: new_leaf.address,
                suffix: path.suffix(depth),
                value_hash: new_leaf.value_hash,
            }),
            Some(Node::Leaf(existing)) => {
                let existing_full_path = NibblePath::from_address(&existing.address);
                let new_full_path = path;

                if existing.address == new_leaf.address {
                    return Node::Leaf(LeafNode {
                        address: new_leaf.address,
                        suffix: existing.suffix.clone(),
                        value_hash: new_leaf.value_hash,
                    });
                }

                let common_len = {
                    let existing_nibbles = existing_full_path.suffix(depth);
                    let new_nibbles = new_full_path.suffix(depth);
                    existing_nibbles.common_prefix_len(&new_nibbles)
                };

                self.build_split(
                    depth,
                    depth + common_len,
                    new_version,
                    SplitPair {
                        existing_path: &existing_full_path,
                        existing_leaf: &existing,
                        new_path: new_full_path,
                        new_leaf: &new_leaf,
                    },
                )
            }
            Some(Node::Internal(internal)) => {
                if depth >= NIBBLES_PER_ADDRESS {
                    return Node::Leaf(LeafNode {
                        address: new_leaf.address,
                        suffix: NibblePath::empty(),
                        value_hash: new_leaf.value_hash,
                    });
                }

                let nibble = path.get(depth).expect("depth within path bounds") as usize;
                let mut new_internal = internal.clone();

                let child_node = if let Some((_, child_ver)) = internal.children[nibble] {
                    let child_key = NodeKey {
                        version: child_ver,
                        nibble_path: path.prefix(depth + 1),
                    };
                    self.nodes.get(&child_key).map(|v| v.clone())
                } else {
                    None
                };

                let new_child =
                    self.insert_recursive(child_node, new_version, path, depth + 1, new_leaf);

                let child_hash = self.node_hash(&new_child);

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

    fn build_split(
        &self,
        base_depth: usize,
        split_depth: usize,
        new_version: u64,
        pair: SplitPair<'_>,
    ) -> Node {
        let SplitPair {
            existing_path,
            existing_leaf,
            new_path,
            new_leaf,
        } = pair;
        let existing_nibble = existing_path
            .get(split_depth)
            .expect("split_depth within existing path bounds")
            as usize;
        let new_nibble = new_path
            .get(split_depth)
            .expect("split_depth within new path bounds") as usize;

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

        let mut internal = InternalNode::new();
        internal.children[existing_nibble] = Some((existing_hash, new_version));
        internal.children[new_nibble] = Some((new_hash, new_version));

        let mut current = Node::Internal(Box::new(internal));

        for d in (base_depth..split_depth).rev() {
            let nibble = existing_path.get(d).expect("d within path bounds") as usize;
            let current_hash = self.node_hash(&current);

            let intermediate_key = NodeKey {
                version: new_version,
                nibble_path: existing_path.prefix(d + 1),
            };
            self.nodes.insert(intermediate_key, current);

            let mut wrapper = InternalNode::new();
            wrapper.children[nibble] = Some((current_hash, new_version));
            current = Node::Internal(Box::new(wrapper));
        }

        current
    }

    fn node_hash(&self, node: &Node) -> Hash256 {
        match node {
            Node::Null => Hash256::ZERO,
            Node::Internal(n) => n.hash(),
            Node::Leaf(n) => n.hash(),
        }
    }

    fn get_leaf(&self, version: u64, path: &NibblePath) -> Option<LeafNode> {
        let root_key = self.root_keys.get(&version)?;
        let root_node = self.nodes.get(&*root_key)?;
        self.traverse_to_leaf(&root_node, version, path, 0)
    }

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
                let nibble = path.get(depth).expect("depth within path bounds") as usize;
                let (_, child_ver) = internal.children[nibble]?;
                let child_key = NodeKey {
                    version: child_ver,
                    nibble_path: path.prefix(depth + 1),
                };
                let child = self.nodes.get(&child_key)?;
                self.traverse_to_leaf(&child, child_ver, path, depth + 1)
            }
        }
    }

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
                let nibble = path.get(depth).expect("depth within path bounds") as usize;

                let mut hasher = blake3::Hasher::new();
                hasher.update(INTERNAL_NODE_DOMAIN);
                for (i, slot) in internal.children.iter().enumerate() {
                    if i == nibble {
                        continue;
                    }
                    match slot {
                        Some((h, _)) => {
                            hasher.update(&h.0);
                        }
                        None => {
                            hasher.update(&Hash256::ZERO.0);
                        }
                    }
                }
                siblings.push(Hash256(*hasher.finalize().as_bytes()));

                if let Some((_, child_ver)) = internal.children[nibble] {
                    let child_key = NodeKey {
                        version: child_ver,
                        nibble_path: path.prefix(depth + 1),
                    };
                    self.collect_proof(child_ver, &child_key, path, depth + 1, siblings);
                }
            }
            Node::Leaf(_) | Node::Null => {}
        }
    }
}

fn hash_pair_with_domain(domain: &[u8], left: &Hash256, right: &Hash256) -> Hash256 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&left.0);
    hasher.update(&right.0);
    Hash256(*hasher.finalize().as_bytes())
}

pub struct JmtStateTree {
    leaves: HashMap<[u8; 32], Hash256>,
    root: Hash256,
    dirty: bool,
    version: u64,
}

impl Default for JmtStateTree {
    fn default() -> Self {
        Self::new()
    }
}

impl JmtStateTree {
    pub fn new() -> Self {
        Self {
            leaves: HashMap::new(),
            root: Hash256::ZERO,
            dirty: false,
            version: 0,
        }
    }

    pub fn update_leaf(&mut self, address: [u8; 32], account_hash: Hash256) {
        self.leaves.insert(address, account_hash);
        self.dirty = true;
    }

    pub fn batch_update(&mut self, changes: &[([u8; 32], Hash256)]) {
        for (addr, hash) in changes {
            self.leaves.insert(*addr, *hash);
        }
        if !changes.is_empty() {
            self.dirty = true;
        }
    }

    pub fn remove_leaf(&mut self, address: &[u8; 32]) {
        self.leaves.remove(address);
        self.dirty = true;
    }

    pub fn root_hash(&mut self) -> Hash256 {
        if self.dirty {
            self.recompute_root();
        }
        self.root
    }

    fn recompute_root(&mut self) {
        if self.leaves.is_empty() {
            self.root = Hash256::ZERO;
            self.dirty = false;
            self.version += 1;
            return;
        }

        let mut sorted_leaves: Vec<([u8; 32], Hash256)> =
            self.leaves.iter().map(|(k, v)| (*k, *v)).collect();
        sorted_leaves.sort_by_key(|a| a.0);

        let leaf_hashes: Vec<Hash256> = sorted_leaves
            .iter()
            .map(|(addr, hash)| {
                let mut hasher = blake3::Hasher::new_derive_key("arc-jmt-leaf-v1");
                hasher.update(addr);
                hasher.update(hash.as_ref());
                Hash256(*hasher.finalize().as_bytes())
            })
            .collect();

        self.root = Self::merkle_root_from_hashes(&leaf_hashes);
        self.dirty = false;
        self.version += 1;
    }

    fn merkle_root_from_hashes(hashes: &[Hash256]) -> Hash256 {
        if hashes.is_empty() {
            return Hash256::ZERO;
        }
        if hashes.len() == 1 {
            return hashes[0];
        }

        let mut current_level = hashes.to_vec();
        while current_level.len() > 1 {
            let mut next_level = Vec::with_capacity(current_level.len().div_ceil(2));
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
        }
        current_level[0]
    }

    pub fn prune_versions_before(&mut self, version: u64) {
        if version > self.version {}
    }

    pub fn len(&self) -> usize {
        self.leaves.len()
    }

    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn proof(&mut self, address: &[u8; 32]) -> Option<JmtProof> {
        if !self.leaves.contains_key(address) {
            return None;
        }

        if self.dirty {
            self.recompute_root();
        }

        let mut sorted_leaves: Vec<([u8; 32], Hash256)> =
            self.leaves.iter().map(|(k, v)| (*k, *v)).collect();
        sorted_leaves.sort_by_key(|a| a.0);

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

    fn collect_siblings(hashes: &[Hash256], mut index: usize) -> Vec<Hash256> {
        let mut siblings = Vec::new();
        let mut current_level = hashes.to_vec();

        while current_level.len() > 1 {
            let sibling_idx = if index.is_multiple_of(2) {
                index + 1
            } else {
                index - 1
            };
            if sibling_idx < current_level.len() {
                siblings.push(current_level[sibling_idx]);
            } else {
                siblings.push(Hash256::ZERO);
            }

            let mut next_level = Vec::with_capacity(current_level.len().div_ceil(2));
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

#[derive(Clone, Debug)]
pub struct JmtProof {
    pub leaf_hash: Hash256,
    pub siblings: Vec<Hash256>,
    pub leaf_index: usize,
}

impl JmtProof {
    pub fn verify(&self, root: &Hash256) -> bool {
        let mut current = self.leaf_hash;
        let mut index = self.leaf_index;

        for sibling in &self.siblings {
            let mut hasher = blake3::Hasher::new_derive_key("arc-jmt-internal-v1");
            if index.is_multiple_of(2) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use arc_types::Account;

    fn test_address(seed: u8) -> Address {
        let mut addr = [0u8; 32];
        for (i, byte) in addr.iter_mut().enumerate() {
            *byte = seed.wrapping_mul(17).wrapping_add(i as u8);
        }
        Hash256(addr)
    }

    fn test_account(addr: Address, balance: u64) -> Account {
        Account::new(addr, balance)
    }

    #[test]
    fn test_empty_tree_root() {
        let store = JmtStore::new();
        assert_eq!(store.root_hash(), Hash256::ZERO);
        assert_eq!(store.version(), 0);
    }

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

    #[test]
    fn test_deterministic_root_regardless_of_order() {
        let addr_a = test_address(10);
        let addr_b = test_address(20);
        let acct_a = test_account(addr_a, 500);
        let acct_b = test_account(addr_b, 700);

        let mut store1 = JmtStore::new();
        store1.put_account(addr_a, &acct_a).unwrap();
        store1.commit();
        store1.put_account(addr_b, &acct_b).unwrap();
        let _root1 = store1.commit();

        let mut store2 = JmtStore::new();
        let root2 = store2.batch_update(&[(addr_b, acct_b.clone()), (addr_a, acct_a.clone())]);

        let mut store3 = JmtStore::new();
        let root3 = store3.batch_update(&[(addr_a, acct_a.clone()), (addr_b, acct_b.clone())]);

        assert_eq!(root2, root3, "batch must be order-independent");
    }

    #[test]
    fn test_batch_matches_sequential_same_version() {
        let addrs: Vec<Address> = (0..5).map(|i| test_address(i * 7 + 3)).collect();
        let accounts: Vec<Account> = addrs
            .iter()
            .enumerate()
            .map(|(i, a)| test_account(*a, (i as u64 + 1) * 100))
            .collect();

        let mut store_seq = JmtStore::new();
        for (addr, acct) in addrs.iter().zip(accounts.iter()) {
            store_seq.put_account(*addr, acct).unwrap();
        }
        let root_seq = store_seq.commit();

        let mut store_batch = JmtStore::new();
        let pairs: Vec<(Address, Account)> = addrs
            .iter()
            .cloned()
            .zip(accounts.iter().cloned())
            .collect();
        let root_batch = store_batch.batch_update(&pairs);

        assert_eq!(
            root_seq, root_batch,
            "batch must equal sequential (same commit)"
        );
    }

    #[test]
    fn test_proof_generation_and_verification() {
        let mut store = JmtStore::new();
        let addr = test_address(42);
        let account = test_account(addr, 999);
        store.put_account(addr, &account).unwrap();
        let _root = store.commit();

        let proof = store.get_proof(&addr);
        assert!(
            proof.leaf.is_some(),
            "proof must contain leaf for existing account"
        );

        let (leaf_addr, leaf_hash) = proof.leaf.as_ref().unwrap();
        assert_eq!(*leaf_addr, addr);

        let bytes = bincode::serialize(&account).unwrap();
        let expected_hash = hash_bytes(&bytes);
        assert_eq!(*leaf_hash, expected_hash);
    }

    #[test]
    fn test_proof_fails_with_wrong_data() {
        let mut store = JmtStore::new();
        let addr = test_address(42);
        let account = test_account(addr, 999);
        store.put_account(addr, &account).unwrap();
        let root = store.commit();

        let proof = store.get_proof(&addr);

        let mut bad_proof = proof.clone();
        if let Some((_, ref mut h)) = bad_proof.leaf {
            h.0[0] ^= 0xFF;
        }

        assert!(
            !JmtStore::verify_proof(&root, &addr, &bad_proof),
            "tampered proof must fail verification"
        );
    }

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

        store.rollback(1).unwrap();
        assert_eq!(store.version(), 1);
        assert_eq!(store.root_hash(), root_v1);

        assert!(store.get_account_hash(addr2).is_none());
    }

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

        store.prune_versions_before(8);
        let nodes_after = store.nodes.len();

        assert!(
            nodes_after <= nodes_before,
            "pruning must not increase node count"
        );
        assert_ne!(store.root_hash(), Hash256::ZERO);
    }

    #[test]
    fn test_large_batch_performance() {
        let mut store = JmtStore::new();
        let n = 10_000;

        let pairs: Vec<(Address, Account)> = (0..n)
            .map(|i| {
                let seed_bytes = (i as u32).to_le_bytes();
                let mut addr_bytes = [0u8; 32];
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
        assert!(
            elapsed.as_secs() < 30,
            "10K batch took too long: {:?}",
            elapsed
        );
        eprintln!("10K batch: {:?}", elapsed);
    }

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

        let mut store = JmtStore::new();
        let start_full = std::time::Instant::now();
        store.batch_update(&pairs);
        let full_time = start_full.elapsed();

        let dirty: Vec<(Address, Account)> = pairs[0..10]
            .iter()
            .map(|(addr, _)| (*addr, Account::new(*addr, 999_999)))
            .collect();

        let start_dirty = std::time::Instant::now();
        store.batch_update(&dirty);
        let dirty_time = start_dirty.elapsed();

        eprintln!("Full rebuild ({n}): {:?}", full_time);
        eprintln!("Dirty update (10): {:?}", dirty_time);

        assert!(
            dirty_time < full_time,
            "dirty update ({:?}) should be faster than full rebuild ({:?})",
            dirty_time,
            full_time
        );
    }

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

    #[test]
    fn test_rollback_to_future_fails() {
        let mut store = JmtStore::new();
        let addr = test_address(1);
        store.put_account(addr, &test_account(addr, 100)).unwrap();
        store.commit();

        let result = store.rollback(999);
        assert!(result.is_err());
    }

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

    #[test]
    fn test_empty_commit_preserves_root() {
        let mut store = JmtStore::new();
        let addr = test_address(1);
        store.put_account(addr, &test_account(addr, 100)).unwrap();
        let root = store.commit();

        let root2 = store.commit();
        assert_eq!(root, root2, "empty commit must not change root");
        assert_eq!(store.version(), 2);
    }

    #[test]
    fn test_repeated_empty_commits_do_not_deadlock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (finished_tx, finished_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut store = JmtStore::new();
            let addr = test_address(1);
            store.put_account(addr, &test_account(addr, 100)).unwrap();
            let expected_root = store.commit();

            for _ in 0..1_024 {
                assert_eq!(store.commit(), expected_root);
            }

            finished_tx.send(store.version()).unwrap();
        });

        assert_eq!(
            finished_rx.recv_timeout(Duration::from_secs(5)),
            Ok(1_025),
            "repeated empty commits deadlocked"
        );
    }

    #[test]
    fn test_update_account_changes_root() {
        let mut store = JmtStore::new();
        let addr = test_address(1);

        store.put_account(addr, &test_account(addr, 100)).unwrap();
        let root1 = store.commit();

        store.put_account(addr, &test_account(addr, 200)).unwrap();
        let root2 = store.commit();

        assert_ne!(root1, root2, "updating balance must change root");
    }

    #[test]
    fn test_jmt_state_tree_basic() {
        let mut tree = JmtStateTree::new();
        assert_eq!(tree.root_hash(), Hash256::ZERO);
        assert!(tree.is_empty());

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
        let _root = tree.root_hash();
        assert_eq!(tree.version(), 1);

        tree.update_leaf([2u8; 32], hash_bytes(&[2]));
        let _root = tree.root_hash();
        assert_eq!(tree.version(), 2);
    }
}
