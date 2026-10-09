//! Content-addressed prefix cache: requests that share a prompt prefix (a
//! system prompt, an agent's tool preamble, an earlier turn) reuse its KV
//! instead of recomputing it.
//!
//! The KV of positions `0..n` is a pure function of the model and tokens
//! `0..n`. Phase invariance makes it independent of whether those positions
//! were computed in a prefill chunk, token by token or during decoding, so a
//! cached block is valid for any request whose tokens match.
//!
//! The cache stores fixed-size blocks of positions, keyed by a hash chain:
//! `key_0 = H(tag, identity, tokens of block 0)` and
//! `key_i = H(tag, identity, key_{i-1}, tokens of block i)`. A key therefore
//! commits to the model's identity and to every token before the block's
//! end, the way a radix tree's path does. A lookup never trusts a hash on its
//! own: it also compares the stored tokens and parent key, so a collision can
//! only cause a miss, never a wrong reuse. Eviction is least-recently-used,
//! leaves before their parents, with ties broken by key, so cache contents
//! are reproducible.
//!
//! On an island or a pipeline, every stage keys its own planes of a block
//! with the same chain, and a router can send a request to the device that
//! holds its longest prefix ("move the request to the KV").

use std::collections::HashMap;

use super::SeqKv;

/// Domain-separation tag of the block keys.
const KEY_TAG: &[u8] = b"arc.serving.prefix-block.v1";

/// Prefix cache settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixConfig {
    /// Positions per block.
    pub block: usize,
    /// Memory bound for the stored KV and tokens, in bytes.
    pub capacity_bytes: usize,
}

/// Counters for reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrefixStats {
    /// Lookups made.
    pub lookups: u64,
    /// Prompt tokens served from the cache.
    pub hit_tokens: u64,
    /// Blocks stored.
    pub inserted_blocks: u64,
    /// Blocks evicted.
    pub evicted_blocks: u64,
    /// Blocks held now.
    pub blocks: u64,
    /// Bytes held now.
    pub bytes: u64,
}

#[derive(Debug)]
struct Block {
    parent: Option<[u8; 32]>,
    tokens: Vec<u32>,
    kv: Vec<Vec<i32>>,
    depth: usize,
    last_used: u64,
    bytes: usize,
}

/// A content-addressed cache of KV blocks for one model identity.
#[derive(Debug)]
pub struct PrefixCache {
    identity: [u8; 32],
    config: PrefixConfig,
    blocks: HashMap<[u8; 32], Block>,
    clock: u64,
    bytes: usize,
    stats: PrefixStats,
}

impl PrefixCache {
    /// An empty cache for the model named by `identity`.
    pub fn new(identity: [u8; 32], config: PrefixConfig) -> Self {
        Self {
            identity,
            config,
            blocks: HashMap::new(),
            clock: 0,
            bytes: 0,
            stats: PrefixStats::default(),
        }
    }

    /// Settings.
    pub fn config(&self) -> PrefixConfig {
        self.config
    }

    /// Counters, with the current block count and size.
    pub fn stats(&self) -> PrefixStats {
        PrefixStats {
            blocks: self.blocks.len() as u64,
            bytes: self.bytes as u64,
            ..self.stats
        }
    }

    /// Key of a block given its parent's key.
    pub fn block_key(&self, parent: Option<&[u8; 32]>, tokens: &[u32]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(KEY_TAG);
        hasher.update(&self.identity);
        match parent {
            Some(key) => {
                hasher.update(&[1]);
                hasher.update(key);
            }
            None => {
                hasher.update(&[0]);
            }
        }
        let bytes: Vec<u8> = tokens.iter().flat_map(|t| t.to_le_bytes()).collect();
        hasher.update(&bytes);
        *hasher.finalize().as_bytes()
    }

    /// Copy the KV of the longest cached prefix of `tokens[..limit]`, in whole
    /// blocks, into the empty cache `kv`; returns the positions copied.
    ///
    /// Callers pass `limit = prompt.len() - 1` so the last prompt token is
    /// always computed: its logits choose the first generated token.
    pub fn lookup_into(&mut self, tokens: &[u32], limit: usize, kv: &mut SeqKv) -> usize {
        self.stats.lookups += 1;
        let block = self.config.block;
        if block == 0 || !kv.is_empty() {
            return 0;
        }
        let limit = limit.min(tokens.len());
        self.clock += 1;
        let mut parent: Option<[u8; 32]> = None;
        let mut hit = 0usize;
        while hit + block <= limit {
            let chunk = &tokens[hit..hit + block];
            let key = self.block_key(parent.as_ref(), chunk);
            let Some(entry) = self.blocks.get_mut(&key) else {
                break;
            };
            if entry.tokens != chunk || entry.parent != parent {
                break;
            }
            if kv.append(&entry.kv).is_err() {
                kv.rollback(hit);
                break;
            }
            entry.last_used = self.clock;
            parent = Some(key);
            hit += block;
        }
        self.stats.hit_tokens += hit as u64;
        hit
    }

    /// Store every whole block of `tokens[..kv.len()]` that is not cached yet,
    /// then evict down to the memory bound.
    pub fn insert(&mut self, tokens: &[u32], kv: &SeqKv) {
        let block = self.config.block;
        if block == 0 {
            return;
        }
        let limit = kv.len().min(tokens.len());
        self.clock += 1;
        let mut parent: Option<[u8; 32]> = None;
        let mut start = 0usize;
        while start + block <= limit {
            let chunk = &tokens[start..start + block];
            let key = self.block_key(parent.as_ref(), chunk);
            match self.blocks.get_mut(&key) {
                Some(entry) if entry.tokens == chunk && entry.parent == parent => {
                    entry.last_used = self.clock;
                }
                // A different block under this key: keep the stored one.
                Some(_) => break,
                None => {
                    let data = kv.export(start, start + block);
                    let bytes =
                        data.iter().map(|plane| plane.len() * 4).sum::<usize>() + chunk.len() * 4;
                    self.blocks.insert(
                        key,
                        Block {
                            parent,
                            tokens: chunk.to_vec(),
                            kv: data,
                            depth: start / block,
                            last_used: self.clock,
                            bytes,
                        },
                    );
                    self.bytes += bytes;
                    self.stats.inserted_blocks += 1;
                }
            }
            parent = Some(key);
            start += block;
        }
        self.evict();
    }

    /// Evict least-recently-used blocks, deepest first among equals, until the
    /// cache fits its memory bound.
    fn evict(&mut self) {
        while self.bytes > self.config.capacity_bytes {
            let victim = self
                .blocks
                .iter()
                .min_by_key(|(key, entry)| (entry.last_used, std::cmp::Reverse(entry.depth), **key))
                .map(|(key, _)| *key);
            let Some(key) = victim else {
                break;
            };
            if let Some(entry) = self.blocks.remove(&key) {
                self.bytes -= entry.bytes;
                self.stats.evicted_blocks += 1;
            }
        }
    }
}
