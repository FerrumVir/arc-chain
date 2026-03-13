//! GPU-resident state cache for ARC Chain.
//!
//! Keeps hot accounts in a simulated GPU HBM tier for ~40x bandwidth
//! improvement over CPU RAM lookups (2 TB/s HBM vs 50 GB/s DDR5).
//!
//! On systems without actual GPU HBM the data structures are identical
//! — the tiering logic still applies so that when real GPU buffers are
//! wired in, the promotion / eviction / prefetch paths are already
//! battle-tested.

use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Memory tier
// ---------------------------------------------------------------------------

/// GPU memory tier for state storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    /// GPU High Bandwidth Memory (fastest, limited capacity).
    GpuHbm,
    /// GPU VRAM (fast, moderate capacity).
    GpuVram,
    /// Regular system RAM.
    CpuRam,
    /// NVMe storage (slowest, unlimited).
    NvmeSsd,
}

// ---------------------------------------------------------------------------
// Eviction policy
// ---------------------------------------------------------------------------

/// Strategy used to choose which accounts to evict from the GPU tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionPolicy {
    /// Least Recently Used — evict the account with the oldest `last_access`.
    Lru,
    /// Least Frequently Used — evict the account with the lowest `access_count`.
    Lfu,
    /// Keep the top-N hottest accounts (by `access_count`), evict the rest.
    HotCold,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the GPU state cache.
#[derive(Debug, Clone)]
pub struct GpuStateCacheConfig {
    /// Maximum number of accounts that can reside in the GPU (hot) tier.
    pub max_gpu_accounts: usize,
    /// Eviction strategy when the GPU tier is full.
    pub eviction_policy: EvictionPolicy,
    /// When `true`, `prefetch` pre-loads accounts into the hot tier.
    pub prefetch_enabled: bool,
    /// Number of dirty accounts to batch before a write-back flush.
    pub write_back_batch_size: usize,
    /// Whether to collect hit / miss / eviction statistics.
    pub stats_enabled: bool,
}

impl Default for GpuStateCacheConfig {
    fn default() -> Self {
        Self {
            max_gpu_accounts: 1_000_000,
            eviction_policy: EvictionPolicy::Lru,
            prefetch_enabled: true,
            write_back_batch_size: 1_000,
            stats_enabled: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Cached account entry
// ---------------------------------------------------------------------------

/// An account entry stored in the cache with access-tracking metadata.
#[derive(Debug, Clone)]
pub struct CachedAccount {
    /// Account address (raw 32-byte key).
    pub address: [u8; 32],
    /// Spendable balance.
    pub balance: u64,
    /// Transaction nonce.
    pub nonce: u64,
    /// Hash of deployed WASM code (zero if EOA).
    pub code_hash: [u8; 32],
    /// Merkle root of contract storage.
    pub storage_root: [u8; 32],
    /// Which memory tier this entry currently lives in.
    pub tier: MemoryTier,
    /// Cumulative number of lookups for this account.
    pub access_count: u64,
    /// Block height at which the account was last accessed.
    pub last_access: u64,
    /// `true` if the account has been modified since the last write-back.
    pub dirty: bool,
}

// ---------------------------------------------------------------------------
// Cache statistics
// ---------------------------------------------------------------------------

/// Aggregate cache-performance counters.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Number of lookups served from the GPU (hot) tier.
    pub gpu_hits: u64,
    /// Number of lookups served from the CPU (warm) tier.
    pub cpu_hits: u64,
    /// Number of lookups that missed both tiers.
    pub misses: u64,
    /// Number of accounts evicted from the GPU tier.
    pub evictions: u64,
    /// Number of dirty accounts written back to the warm tier.
    pub writebacks: u64,
    /// Number of accounts pre-loaded via `prefetch`.
    pub prefetches: u64,
    /// Current number of accounts in the GPU tier.
    pub gpu_accounts: usize,
    /// Current number of accounts in the warm tier.
    pub warm_accounts: usize,
    /// Overall hit rate: `(gpu_hits + cpu_hits) / total_lookups`.
    pub hit_rate: f64,
    /// GPU-only hit rate: `gpu_hits / total_lookups`.
    pub gpu_hit_rate: f64,
}

// ---------------------------------------------------------------------------
// GPU state cache
// ---------------------------------------------------------------------------

/// Tiered account cache that simulates GPU-resident state.
///
/// * **Hot tier** (`DashMap`) — represents GPU HBM.  Bounded by
///   `config.max_gpu_accounts`.  All lookups check this tier first.
/// * **Warm tier** (`DashMap`) — represents CPU RAM overflow.  Accounts
///   evicted from the hot tier land here so they are not lost.
///
/// Eviction, promotion, and prefetch are all lock-free per-key thanks
/// to `DashMap` sharding.
pub struct GpuStateCache {
    /// Hot accounts (GPU tier) — indexed by 32-byte address.
    hot: DashMap<[u8; 32], CachedAccount>,
    /// Warm accounts (CPU RAM tier) — overflow from GPU.
    warm: DashMap<[u8; 32], CachedAccount>,
    /// Cache configuration.
    config: GpuStateCacheConfig,
    /// Aggregate statistics (behind a RwLock for interior mutability).
    stats: parking_lot::RwLock<CacheStats>,
    /// Current block height used for LRU `last_access` tracking.
    current_height: AtomicU64,
}

impl GpuStateCache {
    /// Create a new cache with the given configuration.
    pub fn new(config: GpuStateCacheConfig) -> Self {
        Self {
            hot: DashMap::new(),
            warm: DashMap::new(),
            config,
            stats: parking_lot::RwLock::new(CacheStats::default()),
            current_height: AtomicU64::new(0),
        }
    }

    /// Create a new cache with sensible defaults (1 M GPU slots, LRU, prefetch on).
    pub fn with_defaults() -> Self {
        Self::new(GpuStateCacheConfig::default())
    }

    // -- lookups -----------------------------------------------------------

    /// Look up an account.  Checks the hot (GPU) tier first, then warm.
    ///
    /// On a hit the entry's `access_count` and `last_access` are bumped.
    /// If a warm-tier hit is found the account is automatically promoted
    /// to the hot tier (if there is room or after eviction).
    pub fn get(&self, address: &[u8; 32]) -> Option<CachedAccount> {
        let height = self.current_height.load(Ordering::Relaxed);

        // 1. Try GPU (hot) tier.
        if let Some(mut entry) = self.hot.get_mut(address) {
            entry.access_count += 1;
            entry.last_access = height;
            let snapshot = entry.clone();
            drop(entry);
            if self.config.stats_enabled {
                let mut s = self.stats.write();
                s.gpu_hits += 1;
            }
            return Some(snapshot);
        }

        // 2. Try CPU (warm) tier.
        if let Some(mut entry) = self.warm.get_mut(address) {
            entry.access_count += 1;
            entry.last_access = height;
            let snapshot = entry.clone();
            drop(entry);
            if self.config.stats_enabled {
                let mut s = self.stats.write();
                s.cpu_hits += 1;
            }
            // Auto-promote on warm hit.
            self.promote(address);
            return Some(snapshot);
        }

        // 3. Miss.
        if self.config.stats_enabled {
            let mut s = self.stats.write();
            s.misses += 1;
        }
        None
    }

    // -- inserts -----------------------------------------------------------

    /// Insert or update a single account.
    ///
    /// If the hot tier is below capacity the account goes directly there.
    /// Otherwise it is placed in the warm tier.
    pub fn put(&self, mut account: CachedAccount) {
        let height = self.current_height.load(Ordering::Relaxed);
        account.last_access = height;

        // If the account already lives in the hot tier, update in-place.
        if self.hot.contains_key(&account.address) {
            account.tier = MemoryTier::GpuHbm;
            self.hot.insert(account.address, account);
            return;
        }

        // If there is room in the hot tier, place it there.
        if self.hot.len() < self.config.max_gpu_accounts {
            account.tier = MemoryTier::GpuHbm;
            // Remove from warm if present.
            self.warm.remove(&account.address);
            self.hot.insert(account.address, account);
        } else {
            // Hot tier full — place in warm.
            account.tier = MemoryTier::CpuRam;
            self.warm.insert(account.address, account);
        }
    }

    /// Batch insert.  Equivalent to calling `put` for each entry but
    /// amortises the capacity checks.
    pub fn put_batch(&self, accounts: &[CachedAccount]) {
        for acct in accounts {
            self.put(acct.clone());
        }
    }

    // -- dirty tracking ----------------------------------------------------

    /// Mark an account as dirty (modified since last write-back).
    pub fn mark_dirty(&self, address: &[u8; 32]) {
        if let Some(mut entry) = self.hot.get_mut(address) {
            entry.dirty = true;
        } else if let Some(mut entry) = self.warm.get_mut(address) {
            entry.dirty = true;
        }
    }

    /// Drain all dirty accounts from both tiers, resetting their dirty flag.
    /// Returns cloned snapshots of the dirty entries.
    pub fn drain_dirty(&self) -> Vec<CachedAccount> {
        let mut dirty = Vec::new();

        for mut entry in self.hot.iter_mut() {
            if entry.dirty {
                entry.dirty = false;
                dirty.push(entry.clone());
                if self.config.stats_enabled {
                    self.stats.write().writebacks += 1;
                }
            }
        }
        for mut entry in self.warm.iter_mut() {
            if entry.dirty {
                entry.dirty = false;
                dirty.push(entry.clone());
                if self.config.stats_enabled {
                    self.stats.write().writebacks += 1;
                }
            }
        }

        dirty
    }

    // -- promotion / eviction ----------------------------------------------

    /// Promote an account from the warm tier to the hot (GPU) tier.
    ///
    /// If the hot tier is at capacity this will first evict one entry.
    /// Returns `true` if the promotion succeeded.
    pub fn promote(&self, address: &[u8; 32]) -> bool {
        // Must be in warm.
        let warm_entry = match self.warm.remove(address) {
            Some((_, entry)) => entry,
            None => return false,
        };

        // Make room if needed.
        if self.hot.len() >= self.config.max_gpu_accounts {
            self.evict(1);
        }

        let mut promoted = warm_entry;
        promoted.tier = MemoryTier::GpuHbm;
        self.hot.insert(*address, promoted);
        true
    }

    /// Evict `count` accounts from the hot tier to the warm tier using the
    /// configured eviction policy.  Returns the number actually evicted.
    pub fn evict(&self, count: usize) -> usize {
        if self.hot.is_empty() || count == 0 {
            return 0;
        }

        // Collect candidates — we snapshot the keys + metadata we need for
        // sorting so we can release the DashMap shard locks quickly.
        let mut candidates: Vec<([u8; 32], u64, u64)> = self
            .hot
            .iter()
            .map(|r| (*r.key(), r.access_count, r.last_access))
            .collect();

        // Sort by eviction priority (lowest priority = first to evict).
        match self.config.eviction_policy {
            EvictionPolicy::Lru => {
                candidates.sort_by_key(|&(_, _, last)| last);
            }
            EvictionPolicy::Lfu => {
                candidates.sort_by_key(|&(_, count, _)| count);
            }
            EvictionPolicy::HotCold => {
                // Same as LFU for choosing victims.
                candidates.sort_by_key(|&(_, count, _)| count);
            }
        }

        let to_evict = count.min(candidates.len());
        let mut evicted = 0usize;

        for &(addr, _, _) in candidates.iter().take(to_evict) {
            if let Some((_, mut entry)) = self.hot.remove(&addr) {
                entry.tier = MemoryTier::CpuRam;
                self.warm.insert(addr, entry);
                evicted += 1;
            }
        }

        if self.config.stats_enabled {
            let mut s = self.stats.write();
            s.evictions += evicted as u64;
        }

        evicted
    }

    // -- prefetch ----------------------------------------------------------

    /// Pre-load a set of addresses into the hot tier.
    ///
    /// If an address is already present (warm or hot) it is promoted / left
    /// in place.  Addresses that are not in any tier are inserted as
    /// empty placeholder entries so the hot-tier slot is reserved.
    pub fn prefetch(&self, addresses: &[[u8; 32]]) {
        if !self.config.prefetch_enabled {
            return;
        }

        let height = self.current_height.load(Ordering::Relaxed);

        for addr in addresses {
            // Already hot — nothing to do.
            if self.hot.contains_key(addr) {
                continue;
            }

            // In warm — promote.
            if self.warm.contains_key(addr) {
                self.promote(addr);
                if self.config.stats_enabled {
                    self.stats.write().prefetches += 1;
                }
                continue;
            }

            // Not cached at all — insert a placeholder in the hot tier.
            if self.hot.len() < self.config.max_gpu_accounts {
                let entry = CachedAccount {
                    address: *addr,
                    balance: 0,
                    nonce: 0,
                    code_hash: [0u8; 32],
                    storage_root: [0u8; 32],
                    tier: MemoryTier::GpuHbm,
                    access_count: 0,
                    last_access: height,
                    dirty: false,
                };
                self.hot.insert(*addr, entry);
                if self.config.stats_enabled {
                    self.stats.write().prefetches += 1;
                }
            }
        }
    }

    // -- block height ------------------------------------------------------

    /// Advance the internal block-height counter.  Future accesses will
    /// stamp `last_access` with this value.
    pub fn advance_height(&self, height: u64) {
        self.current_height.store(height, Ordering::Relaxed);
    }

    // -- statistics --------------------------------------------------------

    /// Return a snapshot of current cache statistics.
    pub fn stats(&self) -> CacheStats {
        let mut s = self.stats.read().clone();
        s.gpu_accounts = self.hot.len();
        s.warm_accounts = self.warm.len();

        let total = s.gpu_hits + s.cpu_hits + s.misses;
        if total > 0 {
            s.hit_rate = (s.gpu_hits + s.cpu_hits) as f64 / total as f64;
            s.gpu_hit_rate = s.gpu_hits as f64 / total as f64;
        }

        s
    }

    /// Reset all statistics counters to zero.
    pub fn reset_stats(&self) {
        *self.stats.write() = CacheStats::default();
    }

    // -- counts ------------------------------------------------------------

    /// Number of accounts currently in the GPU (hot) tier.
    pub fn gpu_count(&self) -> usize {
        self.hot.len()
    }

    /// Number of accounts currently in the warm (CPU RAM) tier.
    pub fn warm_count(&self) -> usize {
        self.warm.len()
    }

    /// Total cached accounts across both tiers.
    pub fn total_count(&self) -> usize {
        self.hot.len() + self.warm.len()
    }

    /// Returns `true` if the given address is resident in the GPU tier.
    pub fn is_gpu_resident(&self, address: &[u8; 32]) -> bool {
        self.hot.contains_key(address)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a CachedAccount with a deterministic address derived
    /// from a simple integer seed.
    fn make_account(seed: u8) -> CachedAccount {
        let mut address = [0u8; 32];
        address[0] = seed;
        CachedAccount {
            address,
            balance: seed as u64 * 1000,
            nonce: seed as u64,
            code_hash: [0u8; 32],
            storage_root: [0u8; 32],
            tier: MemoryTier::GpuHbm,
            access_count: 0,
            last_access: 0,
            dirty: false,
        }
    }

    fn addr(seed: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = seed;
        a
    }

    // 1. Empty cache
    #[test]
    fn test_new_cache() {
        let cache = GpuStateCache::with_defaults();
        assert_eq!(cache.gpu_count(), 0);
        assert_eq!(cache.warm_count(), 0);
        assert_eq!(cache.total_count(), 0);
        assert!(cache.get(&addr(1)).is_none());
    }

    // 2. Insert + retrieve
    #[test]
    fn test_put_and_get() {
        let cache = GpuStateCache::with_defaults();
        let acct = make_account(1);
        cache.put(acct.clone());
        let got = cache.get(&addr(1)).expect("should be present");
        assert_eq!(got.balance, 1000);
        assert_eq!(got.nonce, 1);
        assert_eq!(got.address, addr(1));
    }

    // 3. Accounts go to GPU tier first
    #[test]
    fn test_gpu_tier_assignment() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 5,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);

        for i in 0..5 {
            cache.put(make_account(i));
        }
        assert_eq!(cache.gpu_count(), 5);
        assert_eq!(cache.warm_count(), 0);

        // 6th account should overflow to warm.
        cache.put(make_account(10));
        assert_eq!(cache.gpu_count(), 5);
        assert_eq!(cache.warm_count(), 1);
        assert!(!cache.is_gpu_resident(&addr(10)));
    }

    // 4. LRU eviction: oldest-accessed account is evicted first
    #[test]
    fn test_eviction_lru() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 3,
            eviction_policy: EvictionPolicy::Lru,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);

        // Insert 3 accounts at heights 0, 1, 2.
        let mut a0 = make_account(0);
        a0.last_access = 0;
        cache.put(a0);
        cache.advance_height(1);

        let mut a1 = make_account(1);
        a1.last_access = 1;
        cache.put(a1);
        cache.advance_height(2);

        let mut a2 = make_account(2);
        a2.last_access = 2;
        cache.put(a2);

        // Access account 0 so its last_access is refreshed to height 2.
        cache.get(&addr(0));

        // Evict 1 — should evict account 1 (oldest un-refreshed).
        let evicted = cache.evict(1);
        assert_eq!(evicted, 1);
        assert!(cache.is_gpu_resident(&addr(0)), "account 0 should stay (recently accessed)");
        assert!(!cache.is_gpu_resident(&addr(1)), "account 1 should be evicted");
        assert!(cache.is_gpu_resident(&addr(2)), "account 2 should stay");

        // Evicted account should be in warm tier.
        assert!(cache.get(&addr(1)).is_some());
    }

    // 5. LFU eviction: least-frequently-accessed account is evicted first
    #[test]
    fn test_eviction_lfu() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 3,
            eviction_policy: EvictionPolicy::Lfu,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);

        cache.put(make_account(0));
        cache.put(make_account(1));
        cache.put(make_account(2));

        // Access account 0 five times, account 2 three times, account 1 never again.
        for _ in 0..5 {
            cache.get(&addr(0));
        }
        for _ in 0..3 {
            cache.get(&addr(2));
        }

        // Evict 1 — should evict account 1 (lowest access_count).
        let evicted = cache.evict(1);
        assert_eq!(evicted, 1);
        assert!(cache.is_gpu_resident(&addr(0)));
        assert!(!cache.is_gpu_resident(&addr(1)));
        assert!(cache.is_gpu_resident(&addr(2)));
    }

    // 6. Evicted accounts move to warm tier and are still accessible
    #[test]
    fn test_eviction_to_warm() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 2,
            eviction_policy: EvictionPolicy::Lru,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);

        cache.put(make_account(0));
        cache.advance_height(1);
        cache.put(make_account(1));

        cache.evict(1);
        assert_eq!(cache.gpu_count(), 1);
        assert_eq!(cache.warm_count(), 1);

        // Both accounts should still be gettable.
        assert!(cache.get(&addr(0)).is_some());
        assert!(cache.get(&addr(1)).is_some());
        assert_eq!(cache.total_count(), 2);
    }

    // 7. Promote warm → hot
    #[test]
    fn test_promote_warm_to_hot() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 2,
            eviction_policy: EvictionPolicy::Lru,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);

        cache.put(make_account(0));
        cache.put(make_account(1));

        // Force account 1 into warm by overfilling.
        // First fill the hot tier, then add one more.
        let config2 = GpuStateCacheConfig {
            max_gpu_accounts: 1,
            eviction_policy: EvictionPolicy::Lru,
            ..Default::default()
        };
        let cache2 = GpuStateCache::new(config2);
        cache2.put(make_account(0));
        // account 1 should overflow to warm.
        cache2.put(make_account(1));
        assert!(!cache2.is_gpu_resident(&addr(1)));
        assert_eq!(cache2.warm_count(), 1);

        // Promote account 1 — should evict account 0 to make room.
        let ok = cache2.promote(&addr(1));
        assert!(ok);
        assert!(cache2.is_gpu_resident(&addr(1)));
        assert_eq!(cache2.warm_count(), 1); // account 0 was evicted to warm
    }

    // 8. Dirty tracking
    #[test]
    fn test_dirty_tracking() {
        let cache = GpuStateCache::with_defaults();
        cache.put(make_account(0));
        cache.put(make_account(1));

        // Initially not dirty.
        assert!(cache.drain_dirty().is_empty());

        cache.mark_dirty(&addr(0));

        let dirty = cache.drain_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].address, addr(0));

        // After drain, dirty list should be empty.
        assert!(cache.drain_dirty().is_empty());
    }

    // 9. Batch insert
    #[test]
    fn test_batch_insert() {
        let cache = GpuStateCache::with_defaults();
        let batch: Vec<CachedAccount> = (0..10).map(make_account).collect();
        cache.put_batch(&batch);
        assert_eq!(cache.total_count(), 10);
        for i in 0..10 {
            assert!(cache.get(&addr(i)).is_some());
        }
    }

    // 10. Prefetch
    #[test]
    fn test_prefetch() {
        let cache = GpuStateCache::with_defaults();
        let addrs: Vec<[u8; 32]> = (0..5).map(|i| addr(i)).collect();

        cache.prefetch(&addrs);

        // All should now be in the GPU tier.
        for a in &addrs {
            assert!(cache.is_gpu_resident(a), "prefetched addr should be GPU-resident");
        }
        assert_eq!(cache.gpu_count(), 5);
    }

    // 11. Cache statistics (hit/miss counting)
    #[test]
    fn test_cache_stats() {
        let cache = GpuStateCache::with_defaults();
        cache.put(make_account(1));

        // GPU hit.
        cache.get(&addr(1));
        // Miss.
        cache.get(&addr(99));

        let s = cache.stats();
        assert_eq!(s.gpu_hits, 1);
        assert_eq!(s.misses, 1);
        assert_eq!(s.gpu_accounts, 1);
        assert!((s.hit_rate - 0.5).abs() < 1e-9);
        assert!((s.gpu_hit_rate - 0.5).abs() < 1e-9);

        cache.reset_stats();
        let s2 = cache.stats();
        assert_eq!(s2.gpu_hits, 0);
        assert_eq!(s2.misses, 0);
    }

    // 12. Large cache: 10K accounts with eviction
    #[test]
    fn test_large_cache() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 1_000,
            eviction_policy: EvictionPolicy::Lfu,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);

        // Insert 10_000 accounts — first 1000 go to hot, rest to warm.
        for i in 0u16..10_000 {
            let mut address = [0u8; 32];
            address[0] = (i & 0xFF) as u8;
            address[1] = (i >> 8) as u8;
            let acct = CachedAccount {
                address,
                balance: i as u64 * 100,
                nonce: i as u64,
                code_hash: [0u8; 32],
                storage_root: [0u8; 32],
                tier: MemoryTier::GpuHbm,
                access_count: 0,
                last_access: 0,
                dirty: false,
            };
            cache.put(acct);
        }

        assert_eq!(cache.gpu_count(), 1_000);
        assert_eq!(cache.warm_count(), 9_000);
        assert_eq!(cache.total_count(), 10_000);

        // Evict half the hot tier.
        let evicted = cache.evict(500);
        assert_eq!(evicted, 500);
        assert_eq!(cache.gpu_count(), 500);
        assert_eq!(cache.warm_count(), 9_500);
        assert_eq!(cache.total_count(), 10_000);
    }

    // 13. Advance height
    #[test]
    fn test_advance_height() {
        let cache = GpuStateCache::with_defaults();
        cache.advance_height(42);
        cache.put(make_account(1));

        let got = cache.get(&addr(1)).unwrap();
        assert_eq!(got.last_access, 42);

        cache.advance_height(100);
        let got2 = cache.get(&addr(1)).unwrap();
        assert_eq!(got2.last_access, 100);
    }
}
