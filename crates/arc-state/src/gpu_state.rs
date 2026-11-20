use arc_crypto::Hash256;
use arc_gpu::gpu_memory::{GpuAccountBuffer, GpuAccountRepr, MemoryModel};
use arc_types::Account;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    GpuHbm,
    GpuVram,
    CpuRam,
    NvmeSsd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionPolicy {
    Lru,
    Lfu,
    HotCold,
}

#[derive(Debug, Clone)]
pub struct GpuStateCacheConfig {
    pub max_gpu_accounts: usize,
    pub eviction_policy: EvictionPolicy,
    pub prefetch_enabled: bool,
    pub write_back_batch_size: usize,
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

#[derive(Debug, Clone)]
pub struct CachedAccount {
    pub address: [u8; 32],
    pub balance: u64,
    pub nonce: u64,
    pub code_hash: [u8; 32],
    pub storage_root: [u8; 32],
    pub staked_balance: u64,
    pub tier: MemoryTier,
    pub access_count: u64,
    pub last_access: u64,
    pub dirty: bool,
}

impl CachedAccount {
    fn to_gpu_repr(&self) -> GpuAccountRepr {
        GpuAccountRepr {
            address: self.address,
            balance: self.balance,
            nonce: self.nonce,
            code_hash: self.code_hash,
            storage_root: self.storage_root,
            staked_balance: self.staked_balance,
            _padding: [0u8; 8],
        }
    }

    fn from_gpu_repr(repr: &GpuAccountRepr, meta: &AccessMeta) -> Self {
        Self {
            address: repr.address,
            balance: repr.balance,
            nonce: repr.nonce,
            code_hash: repr.code_hash,
            storage_root: repr.storage_root,
            staked_balance: repr.staked_balance,
            tier: MemoryTier::GpuHbm,
            access_count: meta.access_count,
            last_access: meta.last_access,
            dirty: meta.dirty,
        }
    }

    pub fn to_account(&self) -> Account {
        Account {
            address: Hash256(self.address),
            balance: self.balance,
            nonce: self.nonce,
            code_hash: Hash256(self.code_hash),
            storage_root: Hash256(self.storage_root),
            staked_balance: self.staked_balance,
        }
    }

    pub fn from_account(acct: &Account) -> Self {
        Self {
            address: acct.address.0,
            balance: acct.balance,
            nonce: acct.nonce,
            code_hash: acct.code_hash.0,
            storage_root: acct.storage_root.0,
            staked_balance: acct.staked_balance,
            tier: MemoryTier::CpuRam,
            access_count: 0,
            last_access: 0,
            dirty: false,
        }
    }
}

#[derive(Debug, Clone)]
struct AccessMeta {
    slot: usize,
    access_count: u64,
    last_access: u64,
    dirty: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub gpu_hits: u64,
    pub cpu_hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub writebacks: u64,
    pub prefetches: u64,
    pub gpu_accounts: usize,
    pub warm_accounts: usize,
    pub hit_rate: f64,
    pub gpu_hit_rate: f64,
}

pub struct GpuStateCache {
    gpu_buffer: Arc<GpuAccountBuffer>,
    cpu_mirror: DashMap<[u8; 32], CachedAccount>,
    slot_map: DashMap<[u8; 32], AccessMeta>,
    free_slots: parking_lot::Mutex<Vec<usize>>,
    warm: DashMap<[u8; 32], CachedAccount>,
    config: GpuStateCacheConfig,
    stats: parking_lot::RwLock<CacheStats>,
    current_height: AtomicU64,
}

impl GpuStateCache {
    pub fn new(config: GpuStateCacheConfig) -> Self {
        let gpu_buffer = match GpuAccountBuffer::new(config.max_gpu_accounts) {
            Ok(buf) => {
                info!(
                    memory_model = ?buf.memory_model(),
                    capacity = config.max_gpu_accounts,
                    "GPU state cache: real GPU buffer allocated"
                );
                Arc::new(buf)
            }
            Err(e) => {
                warn!(
                    "GPU buffer allocation failed ({}), using CPU-only fallback",
                    e
                );
                Arc::new(GpuAccountBuffer::cpu_only(config.max_gpu_accounts))
            }
        };

        let free_slots: Vec<usize> = (0..config.max_gpu_accounts).rev().collect();

        Self {
            gpu_buffer,
            cpu_mirror: DashMap::new(),
            slot_map: DashMap::new(),
            free_slots: parking_lot::Mutex::new(free_slots),
            warm: DashMap::new(),
            config,
            stats: parking_lot::RwLock::new(CacheStats::default()),
            current_height: AtomicU64::new(0),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(GpuStateCacheConfig::default())
    }

    pub fn memory_model(&self) -> MemoryModel {
        self.gpu_buffer.memory_model()
    }

    fn alloc_slot(&self) -> Option<usize> {
        self.free_slots.lock().pop()
    }

    fn free_slot(&self, slot: usize) {
        self.free_slots.lock().push(slot);
    }

    pub fn get(&self, address: &[u8; 32]) -> Option<CachedAccount> {
        let height = self.current_height.load(Ordering::Relaxed);

        if let Some(mut meta) = self.slot_map.get_mut(address) {
            meta.access_count += 1;
            meta.last_access = height;
            let meta_snap = meta.clone();
            drop(meta);

            if let Some(cached) = self.cpu_mirror.get(address) {
                let mut result = cached.clone();
                result.access_count = meta_snap.access_count;
                result.last_access = meta_snap.last_access;
                if self.config.stats_enabled {
                    self.stats.write().gpu_hits += 1;
                }
                return Some(result);
            }
        }

        if let Some(mut entry) = self.warm.get_mut(address) {
            entry.access_count += 1;
            entry.last_access = height;
            let snapshot = entry.clone();
            drop(entry);
            if self.config.stats_enabled {
                self.stats.write().cpu_hits += 1;
            }
            self.promote(address);
            return Some(snapshot);
        }

        if self.config.stats_enabled {
            self.stats.write().misses += 1;
        }
        None
    }

    pub fn get_account_fast(&self, address: &[u8; 32]) -> Option<Account> {
        if let Some(cached) = self.cpu_mirror.get(address) {
            return Some(cached.to_account());
        }
        if let Some(entry) = self.warm.get(address) {
            return Some(entry.to_account());
        }
        None
    }

    pub fn put(&self, account: CachedAccount) {
        if self.cpu_mirror.contains_key(&account.address) {
            self.cpu_mirror.insert(account.address, account);
            return;
        }

        if self.warm.contains_key(&account.address) {
            self.warm.insert(account.address, account);
            return;
        }

        let height = self.current_height.load(Ordering::Relaxed);

        if let Some(slot) = self.alloc_slot() {
            self.cpu_mirror.insert(account.address, account.clone());
            self.slot_map.insert(
                account.address,
                AccessMeta {
                    slot,
                    access_count: account.access_count,
                    last_access: height,
                    dirty: account.dirty,
                },
            );
        } else {
            let mut warm_acct = account;
            warm_acct.tier = MemoryTier::CpuRam;
            warm_acct.last_access = height;
            self.warm.insert(warm_acct.address, warm_acct);
        }
    }

    pub fn put_account(&self, acct: &Account) {
        self.put(CachedAccount::from_account(acct));
    }

    pub fn update_account_fast(&self, acct: &Account) {
        self.cpu_mirror
            .insert(acct.address.0, CachedAccount::from_account(acct));
    }

    pub fn put_batch(&self, accounts: &[CachedAccount]) {
        for acct in accounts {
            self.put(acct.clone());
        }
    }

    pub fn mark_dirty(&self, address: &[u8; 32]) {
        if let Some(mut meta) = self.slot_map.get_mut(address) {
            meta.dirty = true;
        } else if let Some(mut entry) = self.warm.get_mut(address) {
            entry.dirty = true;
        }
    }

    pub fn drain_dirty(&self) -> Vec<CachedAccount> {
        let mut dirty = Vec::new();

        for mut meta in self.slot_map.iter_mut() {
            if meta.dirty {
                meta.dirty = false;
                if let Some(cached) = self.cpu_mirror.get(meta.key()) {
                    dirty.push(cached.clone());
                }
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

    pub fn promote(&self, address: &[u8; 32]) -> bool {
        let warm_entry = match self.warm.remove(address) {
            Some((_, entry)) => entry,
            None => return false,
        };

        if self.alloc_slot().is_none() {
            self.evict(1);
        }

        if let Some(slot) = self.alloc_slot() {
            self.cpu_mirror.insert(*address, warm_entry.clone());
            self.slot_map.insert(
                *address,
                AccessMeta {
                    slot,
                    access_count: warm_entry.access_count,
                    last_access: warm_entry.last_access,
                    dirty: warm_entry.dirty,
                },
            );
            true
        } else {
            self.warm.insert(*address, warm_entry);
            false
        }
    }

    pub fn evict(&self, count: usize) -> usize {
        if self.slot_map.is_empty() || count == 0 {
            return 0;
        }

        let mut candidates: Vec<([u8; 32], usize, u64, u64)> = self
            .slot_map
            .iter()
            .map(|r| (*r.key(), r.slot, r.access_count, r.last_access))
            .collect();

        match self.config.eviction_policy {
            EvictionPolicy::Lru => {
                candidates.sort_by_key(|&(_, _, _, last)| last);
            }
            EvictionPolicy::Lfu | EvictionPolicy::HotCold => {
                candidates.sort_by_key(|&(_, _, count, _)| count);
            }
        }

        let to_evict = count.min(candidates.len());
        let mut evicted = 0usize;

        for &(addr, slot, _, _) in candidates.iter().take(to_evict) {
            if let Some((_, meta)) = self.slot_map.remove(&addr) {
                let cached = if let Some((_, mirror_entry)) = self.cpu_mirror.remove(&addr) {
                    let mut c = mirror_entry;
                    c.tier = MemoryTier::CpuRam;
                    c
                } else {
                    let repr = self.gpu_buffer.read_account(meta.slot);
                    let mut c = CachedAccount::from_gpu_repr(&repr, &meta);
                    c.tier = MemoryTier::CpuRam;
                    c
                };
                self.warm.insert(addr, cached);
                self.free_slot(slot);
                evicted += 1;
            }
        }

        if self.config.stats_enabled {
            self.stats.write().evictions += evicted as u64;
        }

        evicted
    }

    pub fn prefetch(&self, addresses: &[[u8; 32]]) {
        if !self.config.prefetch_enabled {
            return;
        }

        let height = self.current_height.load(Ordering::Relaxed);

        for addr in addresses {
            if self.slot_map.contains_key(addr) {
                continue;
            }

            if self.warm.contains_key(addr) {
                self.promote(addr);
                if self.config.stats_enabled {
                    self.stats.write().prefetches += 1;
                }
                continue;
            }

            if let Some(slot) = self.alloc_slot() {
                let placeholder = CachedAccount {
                    address: *addr,
                    balance: 0,
                    nonce: 0,
                    code_hash: [0u8; 32],
                    storage_root: [0u8; 32],
                    staked_balance: 0,
                    tier: MemoryTier::GpuHbm,
                    access_count: 0,
                    last_access: height,
                    dirty: false,
                };
                self.cpu_mirror.insert(*addr, placeholder);
                self.slot_map.insert(
                    *addr,
                    AccessMeta {
                        slot,
                        access_count: 0,
                        last_access: height,
                        dirty: false,
                    },
                );
                if self.config.stats_enabled {
                    self.stats.write().prefetches += 1;
                }
            }
        }
    }

    pub fn flush_to_gpu(&self) -> usize {
        let mut flushed = 0usize;
        for entry in self.slot_map.iter() {
            let addr = entry.key();
            let slot = entry.slot;
            if let Some(cached) = self.cpu_mirror.get(addr) {
                self.gpu_buffer.write_account(slot, &cached.to_gpu_repr());
                flushed += 1;
            }
        }
        flushed
    }

    pub fn sync(&self) {
        self.flush_to_gpu();
        self.gpu_buffer.sync_to_gpu();
    }

    pub fn advance_height(&self, height: u64) {
        self.current_height.store(height, Ordering::Relaxed);
    }

    pub fn shutdown(self) {
        let buffer = Arc::try_unwrap(self.gpu_buffer);
        match buffer {
            Ok(buf) => buf.secure_shutdown(),
            Err(_arc_buf) => {
                warn!(
                    "GPU buffer has multiple owners, cannot secure-shutdown (will be zeroed on drop)"
                );
            }
        }
    }

    pub fn stats(&self) -> CacheStats {
        let mut s = self.stats.read().clone();
        s.gpu_accounts = self.slot_map.len();
        s.warm_accounts = self.warm.len();

        let total = s.gpu_hits + s.cpu_hits + s.misses;
        if total > 0 {
            s.hit_rate = (s.gpu_hits + s.cpu_hits) as f64 / total as f64;
            s.gpu_hit_rate = s.gpu_hits as f64 / total as f64;
        }

        s
    }

    pub fn reset_stats(&self) {
        *self.stats.write() = CacheStats::default();
    }

    pub fn gpu_count(&self) -> usize {
        self.slot_map.len()
    }

    pub fn warm_count(&self) -> usize {
        self.warm.len()
    }

    pub fn total_count(&self) -> usize {
        self.slot_map.len() + self.warm.len()
    }

    pub fn is_gpu_resident(&self, address: &[u8; 32]) -> bool {
        self.slot_map.contains_key(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_account(seed: u8) -> CachedAccount {
        let mut address = [0u8; 32];
        address[0] = seed;
        CachedAccount {
            address,
            balance: seed as u64 * 1000,
            nonce: seed as u64,
            code_hash: [0u8; 32],
            storage_root: [0u8; 32],
            staked_balance: 0,
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

    #[test]
    fn test_new_cache() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 100,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);
        assert_eq!(cache.gpu_count(), 0);
        assert_eq!(cache.warm_count(), 0);
        assert_eq!(cache.total_count(), 0);
        assert!(cache.get(&addr(1)).is_none());
        println!("Memory model: {:?}", cache.memory_model());
    }

    #[test]
    fn test_put_and_get() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 100,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);
        let acct = make_account(1);
        cache.put(acct.clone());
        let got = cache.get(&addr(1)).expect("should be present");
        assert_eq!(got.balance, 1000);
        assert_eq!(got.nonce, 1);
        assert_eq!(got.address, addr(1));
    }

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

        cache.put(make_account(10));
        assert_eq!(cache.gpu_count(), 5);
        assert_eq!(cache.warm_count(), 1);
        assert!(!cache.is_gpu_resident(&addr(10)));
    }

    #[test]
    fn test_eviction_lru() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 3,
            eviction_policy: EvictionPolicy::Lru,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);

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

        cache.get(&addr(0));

        let evicted = cache.evict(1);
        assert_eq!(evicted, 1);
        assert!(cache.is_gpu_resident(&addr(0)), "account 0 should stay");
        assert!(
            !cache.is_gpu_resident(&addr(1)),
            "account 1 should be evicted"
        );
        assert!(cache.is_gpu_resident(&addr(2)), "account 2 should stay");

        assert!(cache.get(&addr(1)).is_some());
    }

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

        for _ in 0..5 {
            cache.get(&addr(0));
        }
        for _ in 0..3 {
            cache.get(&addr(2));
        }

        let evicted = cache.evict(1);
        assert_eq!(evicted, 1);
        assert!(cache.is_gpu_resident(&addr(0)));
        assert!(!cache.is_gpu_resident(&addr(1)));
        assert!(cache.is_gpu_resident(&addr(2)));
    }

    #[test]
    fn test_dirty_tracking() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 100,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);
        cache.put(make_account(0));
        cache.put(make_account(1));

        assert!(cache.drain_dirty().is_empty());

        cache.mark_dirty(&addr(0));

        let dirty = cache.drain_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].address, addr(0));

        assert!(cache.drain_dirty().is_empty());
    }

    #[test]
    fn test_batch_insert() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 100,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);
        let batch: Vec<CachedAccount> = (0..10).map(make_account).collect();
        cache.put_batch(&batch);
        assert_eq!(cache.total_count(), 10);
        for i in 0..10 {
            assert!(cache.get(&addr(i)).is_some());
        }
    }

    #[test]
    fn test_prefetch() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 100,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);
        let addrs: Vec<[u8; 32]> = (0..5).map(addr).collect();

        cache.prefetch(&addrs);

        for a in &addrs {
            assert!(cache.is_gpu_resident(a));
        }
        assert_eq!(cache.gpu_count(), 5);
    }

    #[test]
    fn test_cache_stats() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 100,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);
        cache.put(make_account(1));

        cache.get(&addr(1));
        cache.get(&addr(99));

        let s = cache.stats();
        assert_eq!(s.gpu_hits, 1);
        assert_eq!(s.misses, 1);
        assert_eq!(s.gpu_accounts, 1);
        assert!((s.hit_rate - 0.5).abs() < 1e-9);

        cache.reset_stats();
        let s2 = cache.stats();
        assert_eq!(s2.gpu_hits, 0);
    }

    #[test]
    fn test_large_cache() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 1_000,
            eviction_policy: EvictionPolicy::Lfu,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);

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
                staked_balance: 0,
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

        let evicted = cache.evict(500);
        assert_eq!(evicted, 500);
        assert_eq!(cache.gpu_count(), 500);
        assert_eq!(cache.warm_count(), 9_500);
    }

    #[test]
    fn test_get_account_fast() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 100,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);
        cache.put(make_account(5));

        let acct = cache.get_account_fast(&addr(5)).expect("should be present");
        assert_eq!(acct.balance, 5000);
        assert_eq!(acct.nonce, 5);
    }

    #[test]
    fn test_advance_height() {
        let config = GpuStateCacheConfig {
            max_gpu_accounts: 100,
            ..Default::default()
        };
        let cache = GpuStateCache::new(config);
        cache.advance_height(42);
        cache.put(make_account(1));

        let got = cache.get(&addr(1)).unwrap();
        assert_eq!(got.last_access, 42);

        cache.advance_height(100);
        let got2 = cache.get(&addr(1)).unwrap();
        assert_eq!(got2.last_access, 100);
    }
}
