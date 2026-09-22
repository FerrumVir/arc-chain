//! A bounded holding area for authenticated DAG blocks that arrived early.
//!
//! A block can be refused for two reasons that say nothing against it:
//!
//! * a parent it names is not in this node's DAG yet ([`MissingParents`]), or
//! * it is more than one round ahead of this node ([`RoundTooFarAhead`]).
//!
//! Both are facts about the LOCAL node's arrival order. The node used to drop
//! such blocks and rely on history requests, which are throttled. The soak
//! diagnostics showed what that costs: a node that falls two rounds behind
//! finds every live block either too far ahead or missing parents - one node
//! accepted 2 of 83 - so it can only advance at the history throttle's pace,
//! and at N=4, where the tip needs three blocks, two such nodes pin the whole
//! network there.
//!
//! Holding changes nothing about validation. [`ConsensusEngine::receive_block`]
//! checks membership, hash integrity, ordering and the signature BEFORE it
//! looks at the round or the parents, so a block refused for either reason is
//! already authenticated. A held block is only ever released back into that
//! same `receive_block`; the one-round-ahead rule and every parent check still
//! apply at insertion. What changes is only that an early block waits instead
//! of vanishing. A block whose present parents are from the wrong round is
//! malformed, not early, and is never held.
//!
//! The area is bounded in count, per author, and in age.
//!
//! [`MissingParents`]: crate::ConsensusError::MissingParents
//! [`RoundTooFarAhead`]: crate::ConsensusError::RoundTooFarAhead
//! [`ConsensusEngine::receive_block`]: crate::ConsensusEngine::receive_block

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use arc_crypto::Hash256;
use arc_types::Transaction;

use crate::{ConsensusEngine, ConsensusError, DagBlock};

/// Blocks held at once - a few hundred rounds of a small committee.
pub const MAX_PENDING_BLOCKS: usize = 2_048;

/// Blocks held for any one author, so one validator cannot fill the area with
/// validly signed blocks for rounds far in the future.
pub const MAX_PENDING_PER_AUTHOR: usize = 256;

/// How long a held block may wait before it is dropped. Long enough for a
/// history round trip; short enough that a block whose parents never come does
/// not occupy the area.
pub const MAX_PENDING_AGE: Duration = Duration::from_secs(30);

struct Held {
    block: DagBlock,
    transactions: Vec<Transaction>,
    /// Parents still absent. Empty means the block is waiting only for this
    /// node's round to come within one of it.
    missing: HashSet<Hash256>,
    since: Instant,
}

/// What happened to a block offered through [`receive_or_hold`].
#[derive(Debug)]
pub enum Offered {
    /// Inserted into the DAG. The caller does its usual persistence and round
    /// handling, then calls [`PendingBlocks::release_on`] with its hash.
    Accepted(DagBlock, Vec<Transaction>),
    /// Early: held until its parents arrive or its round becomes reachable.
    Held,
    /// Early, but the area refused it (duplicate or per-author cap).
    NotHeld(ConsensusError),
    /// Refused on its merits.
    Rejected(ConsensusError),
}

#[derive(Default)]
pub struct PendingBlocks {
    held: HashMap<Hash256, Held>,
    waiting_on: HashMap<Hash256, HashSet<Hash256>>,
    /// arrival order, keyed on the hash's bytes (Hash256 is not Ord)
    order: BTreeMap<(Instant, [u8; 32]), ()>,
    per_author: HashMap<Hash256, usize>,
    pub evicted_full: u64,
    pub evicted_expired: u64,
    pub refused_per_author: u64,
}

impl PendingBlocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    pub fn contains(&self, hash: &Hash256) -> bool {
        self.held.contains_key(hash)
    }

    /// Hold `block` until every hash in `missing` has been released, and - if
    /// `missing` is empty - until its round is within one of this node's.
    pub fn hold(
        &mut self,
        block: DagBlock,
        transactions: Vec<Transaction>,
        missing: Vec<Hash256>,
        now: Instant,
    ) -> bool {
        if self.held.contains_key(&block.hash) {
            return false;
        }
        if self.per_author.get(&block.author).copied().unwrap_or(0) >= MAX_PENDING_PER_AUTHOR {
            self.refused_per_author += 1;
            return false;
        }
        while self.held.len() >= MAX_PENDING_BLOCKS {
            let Some((&(_, oldest), _)) = self.order.iter().next() else {
                break;
            };
            self.remove(&Hash256(oldest));
            self.evicted_full += 1;
        }
        let hash = block.hash;
        let missing: HashSet<Hash256> = missing.into_iter().collect();
        for parent in &missing {
            self.waiting_on.entry(*parent).or_default().insert(hash);
        }
        *self.per_author.entry(block.author).or_default() += 1;
        self.order.insert((now, hash.0), ());
        self.held.insert(
            hash,
            Held {
                block,
                transactions,
                missing,
                since: now,
            },
        );
        true
    }

    /// `parent` is now in the DAG. Returns the held blocks that were waiting on
    /// nothing else, lowest round first, removed from the area.
    pub fn release_on(&mut self, parent: &Hash256) -> Vec<(DagBlock, Vec<Transaction>)> {
        let Some(children) = self.waiting_on.remove(parent) else {
            return Vec::new();
        };
        let mut ready = Vec::new();
        for child in children {
            let now_ready = match self.held.get_mut(&child) {
                Some(held) => {
                    held.missing.remove(parent);
                    held.missing.is_empty()
                }
                None => false,
            };
            if now_ready && let Some(held) = self.remove(&child) {
                ready.push((held.block, held.transactions));
            }
        }
        ready.sort_by_key(|(block, _)| block.round);
        ready
    }

    /// Held blocks waiting only on this node's round, now within reach.
    pub fn release_up_to_round(&mut self, current_round: u64) -> Vec<(DagBlock, Vec<Transaction>)> {
        let reachable: Vec<Hash256> = self
            .held
            .iter()
            .filter(|(_, held)| {
                held.missing.is_empty() && held.block.round <= current_round.saturating_add(1)
            })
            .map(|(hash, _)| *hash)
            .collect();
        let mut out: Vec<_> = reachable
            .into_iter()
            .filter_map(|hash| self.remove(&hash))
            .map(|held| (held.block, held.transactions))
            .collect();
        out.sort_by_key(|(block, _)| block.round);
        out
    }

    /// Everything held, lowest round first, removed - for a retry after a
    /// history import that may have filled many parents at once.
    pub fn drain_all(&mut self) -> Vec<(DagBlock, Vec<Transaction>)> {
        let hashes: Vec<Hash256> = self.held.keys().copied().collect();
        let mut out: Vec<_> = hashes
            .into_iter()
            .filter_map(|h| self.remove(&h))
            .map(|held| (held.block, held.transactions))
            .collect();
        out.sort_by_key(|(block, _)| block.round);
        out
    }

    /// Drop blocks that have waited too long.
    pub fn expire(&mut self, now: Instant) -> usize {
        let stale: Vec<Hash256> = self
            .order
            .keys()
            .take_while(|(since, _)| now.saturating_duration_since(*since) > MAX_PENDING_AGE)
            .map(|(_, hash)| Hash256(*hash))
            .collect();
        for hash in &stale {
            self.remove(hash);
        }
        self.evicted_expired += stale.len() as u64;
        stale.len()
    }

    /// The authors of held blocks, with the lowest round each is waiting
    /// behind - who to ask for history, and from where.
    pub fn fetch_targets(&self) -> Vec<(Hash256, u64)> {
        let mut by_author: HashMap<Hash256, u64> = HashMap::new();
        for held in self.held.values() {
            let from = held.block.round.saturating_sub(1);
            by_author
                .entry(held.block.author)
                .and_modify(|r| *r = (*r).min(from))
                .or_insert(from);
        }
        by_author.into_iter().collect()
    }

    fn remove(&mut self, hash: &Hash256) -> Option<Held> {
        let held = self.held.remove(hash)?;
        self.order.remove(&(held.since, hash.0));
        if let Some(count) = self.per_author.get_mut(&held.block.author) {
            *count -= 1;
            if *count == 0 {
                self.per_author.remove(&held.block.author);
            }
        }
        for parent in &held.missing {
            if let Some(set) = self.waiting_on.get_mut(parent) {
                set.remove(hash);
                if set.is_empty() {
                    self.waiting_on.remove(parent);
                }
            }
        }
        Some(held)
    }
}

/// Decide what to do with a block `receive_block` just refused with `error`.
///
/// This is the one place the decision is made. The node loop calls it on its
/// error path, and [`receive_or_hold`] - used by the tests - calls it too, so
/// both exercise the same rule. It clones only when it actually holds.
pub fn hold_if_early(
    engine: &ConsensusEngine,
    pending: &mut PendingBlocks,
    block: &DagBlock,
    transactions: &[Transaction],
    error: ConsensusError,
    now: Instant,
) -> Offered {
    match error {
        ConsensusError::MissingParents { .. } | ConsensusError::RoundTooFarAhead { .. } => {
            match engine.absent_parents(block) {
                // A present parent from the wrong round: malformed, not early.
                None => Offered::Rejected(error),
                Some(missing) => {
                    if pending.hold(block.clone(), transactions.to_vec(), missing, now) {
                        Offered::Held
                    } else {
                        Offered::NotHeld(error)
                    }
                }
            }
        }
        other => Offered::Rejected(other),
    }
}

/// Offer a block to the engine; hold it if it is merely early.
pub fn receive_or_hold(
    engine: &ConsensusEngine,
    pending: &mut PendingBlocks,
    block: DagBlock,
    transactions: Vec<Transaction>,
    now: Instant,
) -> Offered {
    match engine.receive_block(&block) {
        Ok(()) => Offered::Accepted(block, transactions),
        Err(error) => hold_if_early(engine, pending, &block, &transactions, error, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn block(round: u64, tag: &[u8], parents: Vec<Hash256>) -> DagBlock {
        let mut b = DagBlock {
            author: hash_bytes(tag),
            round,
            parents,
            transactions: Vec::new(),
            timestamp: round,
            hash: Hash256::ZERO,
            signature: Vec::new(),
            ordering_commitment: DagBlock::compute_ordering_commitment(&[]),
        };
        b.hash = b.compute_hash();
        b
    }

    #[test]
    fn a_block_is_released_only_when_every_missing_parent_has_arrived() {
        let mut p = PendingBlocks::new();
        let (a, b) = (hash_bytes(b"parent-a"), hash_bytes(b"parent-b"));
        let child = block(5, b"child", vec![a, b]);
        assert!(p.hold(child.clone(), vec![], vec![a, b], Instant::now()));
        assert!(p.release_on(&a).is_empty(), "still waiting on b");
        let ready = p.release_on(&b);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0.hash, child.hash);
        assert!(p.is_empty());
    }

    #[test]
    fn a_round_gated_block_waits_for_the_node_to_come_within_one_round() {
        let mut p = PendingBlocks::new();
        let b = block(10, b"ahead", vec![]);
        assert!(p.hold(b.clone(), vec![], vec![], Instant::now()));
        assert!(
            p.release_up_to_round(7).is_empty(),
            "round 10 is not reachable from 7"
        );
        let ready = p.release_up_to_round(9);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0.hash, b.hash);
    }

    #[test]
    fn the_area_is_bounded_and_evicts_the_oldest() {
        let mut p = PendingBlocks::new();
        let t0 = Instant::now();
        for i in 0..(MAX_PENDING_BLOCKS + 10) {
            let parent = hash_bytes(&(i as u64).to_le_bytes());
            // distinct authors so the per-author cap is not what bounds it
            let b = block(i as u64 + 1, &(i as u64).to_be_bytes(), vec![parent]);
            p.hold(
                b,
                vec![],
                vec![parent],
                t0 + Duration::from_micros(i as u64),
            );
        }
        assert_eq!(p.len(), MAX_PENDING_BLOCKS);
        assert_eq!(p.evicted_full, 10);
        assert!(p.release_on(&hash_bytes(&0u64.to_le_bytes())).is_empty());
    }

    #[test]
    fn one_author_cannot_fill_the_area() {
        let mut p = PendingBlocks::new();
        let t0 = Instant::now();
        for i in 0..(MAX_PENDING_PER_AUTHOR + 5) {
            let parent = hash_bytes(&(i as u64).to_le_bytes());
            let mut b = block(i as u64 + 1, b"same-author", vec![parent]);
            b.timestamp = i as u64; // distinct hashes, same author
            b.hash = b.compute_hash();
            p.hold(b, vec![], vec![parent], t0);
        }
        assert_eq!(p.len(), MAX_PENDING_PER_AUTHOR);
        assert_eq!(p.refused_per_author, 5);
    }

    #[test]
    fn stale_blocks_expire_and_leave_no_index_behind() {
        let mut p = PendingBlocks::new();
        let t0 = Instant::now();
        let parent = hash_bytes(b"never");
        p.hold(block(3, b"old", vec![parent]), vec![], vec![parent], t0);
        assert_eq!(p.expire(t0 + MAX_PENDING_AGE / 2), 0);
        assert_eq!(p.expire(t0 + MAX_PENDING_AGE + Duration::from_secs(1)), 1);
        assert!(p.is_empty());
        assert!(p.release_on(&parent).is_empty());
    }

    #[test]
    fn release_orders_by_round_so_parents_are_retried_before_children() {
        let mut p = PendingBlocks::new();
        let root = hash_bytes(b"root");
        let t = Instant::now();
        p.hold(block(9, b"late", vec![root]), vec![], vec![root], t);
        p.hold(block(7, b"early", vec![root]), vec![], vec![root], t);
        let ready = p.release_on(&root);
        assert_eq!(
            ready.iter().map(|(b, _)| b.round).collect::<Vec<_>>(),
            vec![7, 9]
        );
    }

    #[test]
    fn a_duplicate_is_not_held_twice() {
        let mut p = PendingBlocks::new();
        let parent = hash_bytes(b"p");
        let b = block(2, b"dup", vec![parent]);
        assert!(p.hold(b.clone(), vec![], vec![parent], Instant::now()));
        assert!(!p.hold(b, vec![], vec![parent], Instant::now()));
        assert_eq!(p.len(), 1);
    }
}
