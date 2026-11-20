use crate::{Address, Hash256};
use std::collections::BTreeMap;

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
    capacity: BTreeMap<[u8; 32], u32>,
    held: BTreeMap<([u8; 32], [u8; 32]), u64>,
}

fn ended(height: u64, expires_at: u64) -> bool {
    height.saturating_add(1) >= expires_at
}

impl ReservationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_capacity(&mut self, worker: Address, slots: u32) {
        self.capacity
            .insert(worker.0, slots.min(MAX_SLOTS_PER_WORKER));
    }

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

    pub fn release(&mut self, request: &Hash256) -> usize {
        let before = self.held.len();
        self.held.retain(|(_, r), _| *r != request.0);
        before - self.held.len()
    }

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

    pub fn sweep(&mut self, height: u64) -> usize {
        let before = self.held.len();
        self.held
            .retain(|_, expires_at| !ended(height, *expires_at));
        before - self.held.len()
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    pub fn workers(&self) -> usize {
        self.capacity.len()
    }
}
