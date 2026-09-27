//! Worker capacity reservations (S6): a placed slice holds one of its
//! worker's call slots until the member's execution of that request ends, so
//! concurrent requests cannot double-book a worker, and nothing leaks when a
//! request finalizes, becomes refund-eligible, aborts or moves.
//!
//! A request holds one slot per worker for as long as it runs. The slot
//! bounds how many requests share a worker, not how many calls are in flight:
//! calls within one request can overlap (Q, K and V are dispatched together,
//! and duplicates add calls). Placement sees [`ReservationLedger::free`] slots
//! as a candidate's `max_concurrency`, never the lease's maximum.
//!
//! This is the member's own bookkeeping. The requester's escrow is not
//! touched here: the request still settles only through the certificate or
//! the refund.

use crate::{Address, Hash256};
use std::collections::BTreeMap;

/// Hard cap on the slots one worker may offer, whatever its lease claims, so
/// the ledger never holds more than this many entries per worker.
pub const MAX_SLOTS_PER_WORKER: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReservationError {
    #[error("the worker has no current lease in the ledger")]
    UnknownWorker,
    #[error("the worker has no free call slot")]
    NoFreeSlot,
    #[error("the request holds no reservation on that worker")]
    NotHeld,
    #[error("the request is already refund-eligible at this height")]
    AlreadyExpired,
}

#[derive(Debug, Default)]
pub struct ReservationLedger {
    /// Slots per worker, from its current lease (clamped).
    capacity: BTreeMap<[u8; 32], u32>,
    /// (worker, request) -> the request's `expires_at`.
    held: BTreeMap<([u8; 32], [u8; 32]), u64>,
}

/// The chain's refund rule: a request is refund-eligible, so its execution
/// is over, once `height + 1 >= expires_at`.
fn ended(height: u64, expires_at: u64) -> bool {
    height.saturating_add(1) >= expires_at
}

impl ReservationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a worker's slots from its current lease, clamped to
    /// [`MAX_SLOTS_PER_WORKER`]. Lowering it keeps the holds already taken
    /// and blocks new ones until they drain below the new figure.
    pub fn set_capacity(&mut self, worker: Address, slots: u32) {
        self.capacity
            .insert(worker.0, slots.min(MAX_SLOTS_PER_WORKER));
    }

    /// Forget a worker (lease expired, left the committee, excluded): its
    /// capacity and every hold on it. Returns the holds dropped.
    pub fn remove_worker(&mut self, worker: &Address) -> usize {
        self.capacity.remove(&worker.0);
        let before = self.held.len();
        self.held.retain(|(w, _), _| *w != worker.0);
        before - self.held.len()
    }

    fn held_on(&self, worker: &[u8; 32]) -> u32 {
        let holds = self
            .held
            .range((*worker, [0u8; 32])..=(*worker, [0xffu8; 32]))
            .count();
        u32::try_from(holds).unwrap_or(u32::MAX)
    }

    /// Call slots a new request could take on this worker now.
    pub fn free(&self, worker: &Address) -> u32 {
        let capacity = self.capacity.get(&worker.0).copied().unwrap_or(0);
        capacity.saturating_sub(self.held_on(&worker.0))
    }

    fn may_take(&self, worker: &Address, request: &Hash256) -> Result<(), ReservationError> {
        if !self.capacity.contains_key(&worker.0) {
            return Err(ReservationError::UnknownWorker);
        }
        if !self.held.contains_key(&(worker.0, request.0)) && self.free(worker) == 0 {
            return Err(ReservationError::NoFreeSlot);
        }
        Ok(())
    }

    /// Reserve a slot on every worker of one placement, or on none of them.
    /// A slot the request already holds is kept and not taken twice.
    pub fn reserve_all(
        &mut self,
        workers: &[Address],
        request: Hash256,
        expires_at: u64,
        height: u64,
    ) -> Result<(), ReservationError> {
        if ended(height, expires_at) {
            return Err(ReservationError::AlreadyExpired);
        }
        for worker in workers {
            self.may_take(worker, &request)?;
        }
        for worker in workers {
            self.held.entry((worker.0, request.0)).or_insert(expires_at);
        }
        Ok(())
    }

    /// End a request's reservations on every worker: it finalized, the
    /// member abstained or aborted. Returns how many were released.
    pub fn release(&mut self, request: &Hash256) -> usize {
        let before = self.held.len();
        self.held.retain(|(_, r), _| *r != request.0);
        before - self.held.len()
    }

    /// Move one request's slot from a failed worker to another in one step:
    /// on any error nothing changes.
    pub fn reassign(
        &mut self,
        request: &Hash256,
        from: &Address,
        to: Address,
    ) -> Result<(), ReservationError> {
        let Some(&expires_at) = self.held.get(&(from.0, request.0)) else {
            return Err(ReservationError::NotHeld);
        };
        if from.0 == to.0 {
            return Ok(());
        }
        self.may_take(&to, request)?;
        self.held.remove(&(from.0, request.0));
        self.held.entry((to.0, request.0)).or_insert(expires_at);
        Ok(())
    }

    /// Drop every reservation whose request is refund-eligible at `height`.
    /// Called once per height. Returns how many were dropped.
    pub fn sweep(&mut self, height: u64) -> usize {
        let before = self.held.len();
        self.held
            .retain(|_, expires_at| !ended(height, *expires_at));
        before - self.held.len()
    }

    /// Reservations held (the growth gauge).
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Workers with a capacity entry.
    pub fn workers(&self) -> usize {
        self.capacity.len()
    }
}
