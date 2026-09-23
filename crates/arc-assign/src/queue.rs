//! A bounded, fair call queue per worker (S6): round-robin across requesters,
//! per-call deadlines, cancellation, and a hard capacity - so one busy
//! requester cannot starve the others of SERVICE, and a stuck worker cannot
//! grow memory.
//!
//! The fairness here is in service, not in admission, and the difference
//! matters. `next` rotates between requesters, so whoever is already in the
//! queue gets an even share of turns. `push` is first-come-first-served
//! against one global capacity, so a single requester that pushes `capacity`
//! calls before anyone else WILL make the next requester's push fail with
//! `Full`. Nothing in this type prevents that.
//!
//! What prevents it is the caller. `arc-node`'s native worker offers pending
//! requests round-robin by requester (`poll_once`), so admissions interleave
//! before they ever reach this queue, and a `Full` there is treated as
//! backpressure - the rest are offered again on the next poll. Any other
//! caller that pushes in bulk from one requester inherits the admission
//! problem and has to solve it the same way.

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

#[cfg(test)]
mod tests {
    use super::*;

    fn h(byte: u8) -> Hash256 {
        Hash256([byte; 32])
    }

    fn call(requester: u8, id: u8, deadline: u64) -> Call {
        Call {
            request: h(requester),
            call_id: h(id),
            deadline,
        }
    }

    fn run(q: &mut FairQueue, now: u64) -> Call {
        match q.next(now) {
            Next::Run(call) => call,
            other => panic!("expected a call to run, got {other:?}"),
        }
    }

    /// The property this type exists for: one requester with a backlog does
    /// not get the worker to itself.
    #[test]
    fn service_rotates_between_requesters_so_a_backlog_cannot_monopolise_a_worker() {
        let mut q = FairQueue::new(8);
        // A arrives first with three calls; B queues one behind them.
        for id in 1..=3 {
            q.push(call(0xAA, id, 100)).unwrap();
        }
        q.push(call(0xBB, 9, 100)).unwrap();
        assert_eq!(q.len(), 4);

        let served: Vec<Hash256> = (0..4).map(|_| run(&mut q, 0).request).collect();
        // A, B, A, A: B is served second despite arriving last, and A keeps
        // its own arrival order within its turns.
        assert_eq!(served, vec![h(0xAA), h(0xBB), h(0xAA), h(0xAA)]);
        assert!(q.is_empty());
        assert_eq!(q.next(0), Next::Idle);
    }

    #[test]
    fn a_served_requester_goes_to_the_back_of_the_rotation() {
        let mut q = FairQueue::new(8);
        q.push(call(0xAA, 1, 100)).unwrap();
        q.push(call(0xAA, 2, 100)).unwrap();
        q.push(call(0xBB, 3, 100)).unwrap();
        q.push(call(0xCC, 4, 100)).unwrap();

        let order: Vec<Hash256> = (0..4).map(|_| run(&mut q, 0).request).collect();
        assert_eq!(order, vec![h(0xAA), h(0xBB), h(0xCC), h(0xAA)]);
    }

    #[test]
    fn the_queue_is_bounded_and_names_its_capacity() {
        let mut q = FairQueue::new(3);
        for id in 1..=3 {
            q.push(call(0xAA, id, 100)).unwrap();
        }
        assert_eq!(q.len(), 3);
        assert_eq!(q.push(call(0xBB, 4, 100)), Err(QueueError::Full(3)));
        // A refused call leaves nothing behind.
        assert_eq!(q.len(), 3);
    }

    /// Admission is first-come, not fair. This is the honest shape of the
    /// type, and the reason the node's worker interleaves before pushing.
    #[test]
    fn admission_is_first_come_so_one_requester_can_fill_the_queue() {
        let mut q = FairQueue::new(2);
        q.push(call(0xAA, 1, 100)).unwrap();
        q.push(call(0xAA, 2, 100)).unwrap();
        // B never gets in, though it would be served fairly if it had.
        assert_eq!(q.push(call(0xBB, 3, 100)), Err(QueueError::Full(2)));
        // Once A is served the slot frees: backpressure, not loss.
        assert_eq!(run(&mut q, 0).request, h(0xAA));
        q.push(call(0xBB, 3, 100)).unwrap();
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn expired_calls_are_reported_before_anything_runs_and_free_their_slots() {
        let mut q = FairQueue::new(8);
        q.push(call(0xAA, 1, 5)).unwrap();
        q.push(call(0xBB, 2, 50)).unwrap();
        assert_eq!(q.len(), 2);

        match q.next(10) {
            Next::Expired(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].call_id, h(1));
            }
            other => panic!("expected the expired call first, got {other:?}"),
        }
        assert_eq!(q.len(), 1, "the expired call freed its slot");
        assert_eq!(run(&mut q, 10).call_id, h(2));
    }

    /// `deadline == now` is already expired. The node passes
    /// `now = height + 1`, and the contract refunds when
    /// `height + 1 >= expires_at`, so a call must not run in the same block
    /// its escrow becomes refundable.
    #[test]
    fn a_deadline_equal_to_now_is_already_expired() {
        let mut q = FairQueue::new(4);
        q.push(call(0xAA, 1, 10)).unwrap();
        match q.next(10) {
            Next::Expired(calls) => assert_eq!(calls[0].call_id, h(1)),
            other => panic!("a call due exactly now must not run: {other:?}"),
        }
        assert!(q.is_empty());
    }

    #[test]
    fn cancelling_one_requester_leaves_the_others_untouched() {
        let mut q = FairQueue::new(8);
        q.push(call(0xAA, 1, 100)).unwrap();
        q.push(call(0xAA, 2, 100)).unwrap();
        q.push(call(0xBB, 3, 100)).unwrap();

        assert_eq!(q.cancel(&h(0xAA)), 2);
        assert_eq!(q.len(), 1, "only B's call remains");
        // Cancelling something absent is a no-op, not an underflow.
        assert_eq!(q.cancel(&h(0xAA)), 0);
        assert_eq!(q.cancel(&h(0xCC)), 0);
        assert_eq!(q.len(), 1);

        assert_eq!(run(&mut q, 0).call_id, h(3));
        assert!(q.is_empty());
        assert_eq!(q.next(0), Next::Idle);
    }

    #[test]
    fn length_stays_consistent_through_expiry_cancellation_and_service() {
        let mut q = FairQueue::new(16);
        for id in 0..4u8 {
            q.push(call(0xAA, id, 5)).unwrap(); // expire at 10
            q.push(call(0xBB, 100 + id, 100)).unwrap(); // live
        }
        assert_eq!(q.len(), 8);

        match q.next(10) {
            Next::Expired(calls) => assert_eq!(calls.len(), 4),
            other => panic!("{other:?}"),
        }
        assert_eq!(q.len(), 4, "only B's four live calls remain");

        assert_eq!(run(&mut q, 10).request, h(0xBB));
        assert_eq!(q.len(), 3);
        assert_eq!(q.cancel(&h(0xBB)), 3);
        assert_eq!(q.len(), 0);
        assert!(q.is_empty());
        assert_eq!(q.next(10), Next::Idle);
    }

    /// A requester that empties and comes back rejoins the rotation instead of
    /// being dropped from it.
    #[test]
    fn a_requester_that_drains_and_returns_is_queued_again() {
        let mut q = FairQueue::new(4);
        q.push(call(0xAA, 1, 100)).unwrap();
        assert_eq!(run(&mut q, 0).call_id, h(1));
        assert!(q.is_empty());

        q.push(call(0xBB, 2, 100)).unwrap();
        q.push(call(0xAA, 3, 100)).unwrap();
        // B was queued first this time, so B goes first.
        assert_eq!(run(&mut q, 0).request, h(0xBB));
        assert_eq!(run(&mut q, 0).request, h(0xAA));
    }
}
