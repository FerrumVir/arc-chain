use arc_crypto::Hash256;
use arc_types::Transaction;
use crossbeam::queue::SegQueue;
use dashmap::DashMap;
use parking_lot::RwLock;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MempoolError {
    #[error("duplicate transaction: {0:?}")]
    Duplicate(Hash256),
    #[error("mempool full (capacity: {0})")]
    Full(usize),
}

/// Lock-free transaction mempool.
/// Uses crossbeam's SegQueue for wait-free concurrent push/pop
/// and DashMap for O(1) deduplication.
pub struct Mempool {
    /// Ordered queue of pending transactions.
    queue: SegQueue<Transaction>,
    /// Deduplication set (tx_hash → exists).
    seen: DashMap<[u8; 32], ()>,
    /// Maximum mempool size.
    capacity: usize,
    /// Current size (atomic via DashMap len).
    count: RwLock<usize>,
}

impl Mempool {
    /// Create a new mempool with given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: SegQueue::new(),
            seen: DashMap::new(),
            capacity,
            count: RwLock::new(0),
        }
    }

    /// Add a transaction to the mempool.
    /// Returns error if duplicate or mempool is full.
    pub fn insert(&self, tx: Transaction) -> Result<(), MempoolError> {
        // Check capacity
        {
            let count = self.count.read();
            if *count >= self.capacity {
                return Err(MempoolError::Full(self.capacity));
            }
        }

        // Check deduplication
        if self.seen.contains_key(&tx.hash.0) {
            return Err(MempoolError::Duplicate(tx.hash));
        }

        self.seen.insert(tx.hash.0, ());
        self.queue.push(tx);
        {
            let mut count = self.count.write();
            *count += 1;
        }

        Ok(())
    }

    /// Drain up to `max` transactions for block production.
    /// Returns transactions in FIFO order.
    pub fn drain(&self, max: usize) -> Vec<Transaction> {
        let mut batch = Vec::with_capacity(max);
        for _ in 0..max {
            match self.queue.pop() {
                Some(tx) => {
                    self.seen.remove(&tx.hash.0);
                    batch.push(tx);
                }
                None => break,
            }
        }
        {
            let mut count = self.count.write();
            *count = count.saturating_sub(batch.len());
        }
        batch
    }

    /// Current number of pending transactions.
    pub fn len(&self) -> usize {
        *self.count.read()
    }

    /// Whether the mempool is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Check if a transaction is already in the mempool.
    pub fn contains(&self, hash: &Hash256) -> bool {
        self.seen.contains_key(&hash.0)
    }

    /// Clear all pending transactions.
    pub fn clear(&self) {
        while self.queue.pop().is_some() {}
        self.seen.clear();
        *self.count.write() = 0;
    }
}

impl Default for Mempool {
    fn default() -> Self {
        Self::new(1_000_000) // 1M tx default capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn addr(n: u8) -> Hash256 {
        hash_bytes(&[n])
    }

    #[test]
    fn test_insert_and_drain() {
        let pool = Mempool::new(100);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        pool.insert(tx).unwrap();
        assert_eq!(pool.len(), 1);

        let batch = pool.drain(10);
        assert_eq!(batch.len(), 1);
        assert!(pool.is_empty());
    }

    #[test]
    fn test_dedup() {
        let pool = Mempool::new(100);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        pool.insert(tx.clone()).unwrap();
        assert!(pool.insert(tx).is_err());
    }

    #[test]
    fn test_capacity() {
        let pool = Mempool::new(2);
        pool.insert(Transaction::new_transfer(addr(1), addr(2), 1, 0)).unwrap();
        pool.insert(Transaction::new_transfer(addr(1), addr(2), 2, 1)).unwrap();
        let result = pool.insert(Transaction::new_transfer(addr(1), addr(2), 3, 2));
        assert!(result.is_err());
    }

    #[test]
    fn test_fifo_order() {
        let pool = Mempool::new(100);
        for i in 0..10u64 {
            pool.insert(Transaction::new_transfer(addr(1), addr(2), i, i)).unwrap();
        }
        let batch = pool.drain(10);
        assert_eq!(batch.len(), 10);
        // First drained should have nonce 0
        assert_eq!(batch[0].nonce, 0);
        assert_eq!(batch[9].nonce, 9);
    }
}
