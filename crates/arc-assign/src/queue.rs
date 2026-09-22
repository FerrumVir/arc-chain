//! A bounded, fair call queue per worker (S6): round-robin across requests,
//! per-request deadlines, cancellation, and a hard capacity - so one large
//! request cannot starve the others and a stuck worker cannot grow memory.

use crate::Hash256;
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub request: Hash256,
    pub call_id: Hash256,
    /// Monotonic deadline (the caller's clock units).
    pub deadline: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    #[error("the queue is at its capacity of {0} calls")]
    Full(usize),
}

/// What `next` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Run(Call),
    /// Calls whose deadline passed; the caller fails them fast.
    Expired(Vec<Call>),
    Idle,
}

pub struct FairQueue {
    capacity: usize,
    len: usize,
    /// Per request, in arrival order; `order` rotates between requests.
    calls: BTreeMap<[u8; 32], VecDeque<Call>>,
    order: VecDeque<[u8; 32]>,
}

impl FairQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            len: 0,
            calls: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, call: Call) -> Result<(), QueueError> {
        if self.len >= self.capacity {
            return Err(QueueError::Full(self.capacity));
        }
        let key = call.request.0;
        let queue = self.calls.entry(key).or_default();
        if queue.is_empty() && !self.order.contains(&key) {
            self.order.push_back(key);
        }
        queue.push_back(call);
        self.len += 1;
        Ok(())
    }

    /// Drop every queued call of a cancelled request.
    pub fn cancel(&mut self, request: &Hash256) -> usize {
        let removed = self.calls.remove(&request.0).map(|q| q.len()).unwrap_or(0);
        self.order.retain(|k| *k != request.0);
        self.len -= removed;
        removed
    }

    /// The next call, taking one request's turn, or the expired calls first.
    pub fn next(&mut self, now: u64) -> Next {
        let mut expired = Vec::new();
        for queue in self.calls.values_mut() {
            let before = queue.len();
            let (keep, gone): (VecDeque<Call>, VecDeque<Call>) =
                queue.drain(..).partition(|c| c.deadline > now);
            *queue = keep;
            expired.extend(gone);
            self.len -= before - queue.len();
        }
        self.calls.retain(|_, q| !q.is_empty());
        self.order.retain(|k| self.calls.contains_key(k));
        if !expired.is_empty() {
            return Next::Expired(expired);
        }
        let Some(key) = self.order.pop_front() else {
            return Next::Idle;
        };
        let queue = self
            .calls
            .get_mut(&key)
            .expect("ordered requests have calls");
        let call = queue.pop_front().expect("non-empty");
        self.len -= 1;
        if queue.is_empty() {
            self.calls.remove(&key);
        } else {
            self.order.push_back(key);
        }
        Next::Run(call)
    }
}
