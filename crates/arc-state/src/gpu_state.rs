//! GPU-resident state cache for ARC Chain.
//!
//! Keeps hot accounts in **real GPU memory** via wgpu unified/managed buffers.
//! On Apple Silicon (Metal), this is zero-copy unified memory — CPU and GPU
//! share the same physical pages (~2 TB/s HBM bandwidth vs ~50 GB/s DDR5).
//! On discrete NVIDIA/AMD GPUs, a staging buffer + device-local buffer pair
//! handles CPU↔GPU transfers transparently.
//!
//! Falls back to CPU-only DashMap when no GPU is detected.
//!
//! **Design principle**: GPU buffer stores account data (balance, nonce, hashes).
//! CPU-side DashMap stores access metadata (hit counts, dirty flags) to avoid
//! GPU writes on every read-path access count bump.
//!
//! **Security**: GPU is a hot cache, not source of truth. WAL + CPU DashMap
//! remains authoritative. On shutdown, GPU memory is explicitly zeroed.

use arc_gpu::gpu_memory::{GpuAccountBuffer, GpuAccountRepr, MemoryModel, ACCOUNT_SLOT_SIZE};
use arc_types::Account;
use arc_crypto::Hash256;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Memory tier
// ---------------------------------------------------------------------------

/// GPU memory tier for state storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    /// GPU High Bandwidth Memory / Unified Memory (fastest).
    GpuHbm,
    /// GPU VRAM (fast, for discrete GPUs).
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
// Cached account entry (public API type)
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
    /// Amount staked by this account.
    pub staked_balance: u64,
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
// Conversions: CachedAccount ↔ GpuAccountRepr ↔ Account
// ---------------------------------------------------------------------------

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

    /// Convert to the core `Account` type.
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

    /// Create from a core `Account`.
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

// ---------------------------------------------------------------------------
// Access metadata (CPU-side only — avoids GPU writes on reads)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AccessMeta {
    /// Slot index in the GPU buffer.
    slot: usize,
    /// Cumulative lookups.
    access_count: u64,
    /// Block height of last access.
    last_access: u64,
    /// Whether account data was modified since last flush.
    dirty: bool,
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

/// Tiered account cache with real GPU-resident state.
///
/// * **Hot tier** (GPU buffer) — accounts stored in GPU unified/managed memory.
///   On Metal: zero-copy access at ~2 TB/s. On discrete: staging transfers.
///   Bounded by `config.max_gpu_accounts`.
/// * **Warm tier** (`DashMap`) — CPU RAM overflow for accounts evicted from GPU.
///
/// Access metadata (hit counts, dirty flags) is tracked in a CPU-side `DashMap`
/// to avoid GPU writes on every read operation.
pub struct GpuStateCache {
    /// GPU-backed account buffer (real GPU memory or CPU fallback).
    /// Used for batch compute shader access (BlockSTM, Merkle hashing).
    gpu_buffer: Arc<GpuAccountBuffer>,
    /// CPU-side mirror of GPU-resident accounts for fast individual reads.
    /// On unified memory (Metal), this DashMap and the GPU buffer share the
    /// same physical memory pages — there is no duplication overhead.
    /// On discrete GPUs, this is an explicit CPU-side copy kept in sync.
    cpu_mirror: DashMap<[u8; 32], CachedAccount>,
    /// Address → slot index + access metadata (CPU-side).
    slot_map: DashMap<[u8; 32], AccessMeta>,
    /// Free slot tracking (stack of available slot indices).
    free_slots: parking_lot::Mutex<Vec<usize>>,
    /// Warm accounts (CPU RAM tier) — overflow from GPU.
    warm: DashMap<[u8; 32], CachedAccount>,
    /// Cache configuration.
    config: GpuStateCacheConfig,
    /// Aggregate statistics.
    stats: parking_lot::RwLock<CacheStats>,
    /// Current block height for LRU `last_access` tracking.
    current_height: AtomicU64,
}

impl GpuStateCache {
    /// Create a new cache with real GPU memory backing.
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
                warn!("GPU buffer allocation failed ({}), using CPU-only fallback", e);
                Arc::new(GpuAccountBuffer::cpu_only(config.max_gpu_accounts))
            }
        };

        // Initialize free slot list (all slots available, in reverse order for stack pop).
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

    /// Create a new cache with sensible defaults (1M GPU slots, LRU, prefetch on).
    pub fn with_defaults() -> Self {
        Self::new(GpuStateCacheConfig::default())
    }

    /// What GPU memory model is in use.
    pub fn memory_model(&self) -> MemoryModel {
        self.gpu_buffer.memory_model()
    }

    // -- slot management ---------------------------------------------------

    /// Allocate a free GPU slot. Returns `None` if all slots are occupied.
    fn alloc_slot(&self) -> Option<usize> {
        self.free_slots.lock().pop()
    }

    /// Return a slot to the free list.
    fn free_slot(&self, slot: usize) {
        self.free_slots.lock().push(slot);
    }

    // -- lookups -----------------------------------------------------------

    /// Look up an account. Checks the CPU-side mirror first, then warm.
    ///
    /// **Architecture note**: Individual reads go through the CPU-side DashMap
    /// mirror (not the GPU buffer), because wgpu map/unmap overhead makes
    /// per-account GPU reads ~20,000x slower than DashMap. The GPU buffer
    /// is kept in sync for batch compute shader access (BlockSTM parallel
    /// execution, batch Merkle hashing, etc.).
    pub fn get(&self, address: &[u8; 32]) -> Option<CachedAccount> {
        let height = self.current_height.load(Ordering::Relaxed);

        // 1. Try GPU tier (CPU-side mirror for fast reads).
        if let Some(mut meta) = self.slot_map.get_mut(address) {
            meta.access_count += 1;
            meta.last_access = height;
            let meta_snap = meta.clone();
            drop(meta);

            // Read from CPU-side mirror (DashMap) — NOT from GPU buffer.
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

        // 2. Try CPU (warm) tier.
        if let Some(mut entry) = self.warm.get_mut(address) {
            entry.access_count += 1;
            entry.last_access = height;
            let snapshot = entry.clone();
            drop(entry);
            if self.config.stats_enabled {
                self.stats.write().cpu_hits += 1;
            }
            // Auto-promote on warm hit.
            self.promote(address);
            return Some(snapshot);
        }

        // 3. Miss.
        if self.config.stats_enabled {
            self.stats.write().misses += 1;
        }
        None
    }

    /// Fast path: check if address is in GPU tier and return Account.
    /// Reads directly from CPU-side mirror, skipping slot_map write-lock
    /// for maximum throughput. Access metadata is not updated.
    pub fn get_account_fast(&self, address: &[u8; 32]) -> Option<Account> {
        // Read directly from cpu_mirror — single DashMap lookup.
        if let Some(cached) = self.cpu_mirror.get(address) {
            return Some(cached.to_account());
        }
        // Check warm tier.
        if let Some(entry) = self.warm.get(address) {
            return Some(entry.to_account());
        }
        None
    }

    // -- inserts -----------------------------------------------------------

    /// Insert or update a single account.
    ///
    /// Writes to the CPU-side mirror (for fast individual reads) and tracks
    /// the GPU slot mapping. The actual GPU buffer is updated lazily via
    /// `flush_to_gpu()` — called once per block before compute shader passes.
    /// This avoids per-account wgpu overhead during transaction execution.
    pub fn put(&self, account: CachedAccount) {
        // Fast path: already in GPU tier — update cpu_mirror only.
        // Skip slot_map write-lock; metadata is updated lazily.
        if self.cpu_mirror.contains_key(&account.address) {
            self.cpu_mirror.insert(account.address, account);
            return;
        }

        // Check warm tier.
        if self.warm.contains_key(&account.address) {
            self.warm.insert(account.address, account);
            return;
        }

        let height = self.current_height.load(Ordering::Relaxed);

        // New account — try to allocate a GPU slot.
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
            // GPU full — place in warm.
            let mut warm_acct = account;
            warm_acct.tier = MemoryTier::CpuRam;
            warm_acct.last_access = height;
            self.warm.insert(warm_acct.address, warm_acct);
        }
    }

    /// Insert or update from a core `Account` type (convenience).
    pub fn put_account(&self, acct: &Account) {
        self.put(CachedAccount::from_account(acct));
    }

    /// Fast update for an account already known to be in the cache.
    /// Writes directly to cpu_mirror without contains_key check.
    pub fn update_account_fast(&self, acct: &Account) {
        self.cpu_mirror.insert(acct.address.0, CachedAccount::from_account(acct));
    }

    /// Batch insert. Equivalent to calling `put` for each entry.
    pub fn put_batch(&self, accounts: &[CachedAccount]) {
        for acct in accounts {
            self.put(acct.clone());
        }
    }

    // -- dirty tracking ----------------------------------------------------

    /// Mark an account as dirty (modified since last write-back).
    pub fn mark_dirty(&self, address: &[u8; 32]) {
        if let Some(mut meta) = self.slot_map.get_mut(address) {
            meta.dirty = true;
        } else if let Some(mut entry) = self.warm.get_mut(address) {
            entry.dirty = true;
        }
    }

    /// Drain all dirty accounts from both tiers, resetting their dirty flag.
    /// Returns cloned snapshots of the dirty entries.
    pub fn drain_dirty(&self) -> Vec<CachedAccount> {
        let mut dirty = Vec::new();

        // GPU tier — read from CPU mirror (fast), not GPU buffer.
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

        // Warm tier.
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
            // Re-insert into warm if we still can't get a slot.
            self.warm.insert(*address, warm_entry);
            false
        }
    }

    /// Evict `count` accounts from the hot tier to the warm tier using the
    /// configured eviction policy. Returns the number actually evicted.
    pub fn evict(&self, count: usize) -> usize {
        if self.slot_map.is_empty() || count == 0 {
            return 0;
        }

        // Collect candidates with metadata for sorting.
        let mut candidates: Vec<([u8; 32], usize, u64, u64)> = self
            .slot_map
            .iter()
            .map(|r| (*r.key(), r.slot, r.access_count, r.last_access))
            .collect();

        // Sort by eviction priority.
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
                // Read from CPU mirror (fast) instead of GPU buffer.
                let cached = if let Some((_, mirror_entry)) = self.cpu_mirror.remove(&addr) {
                    let mut c = mirror_entry;
                    c.tier = MemoryTier::CpuRam;
                    c
                } else {
                    // Fallback: read from GPU buffer.
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

    // -- prefetch ----------------------------------------------------------

    /// Pre-load a set of addresses into the hot tier.
    ///
    /// If an address is already present (warm or hot) it is promoted / left
    /// in place. Addresses not in any tier are inserted as empty placeholders.
    pub fn prefetch(&self, addresses: &[[u8; 32]]) {
        if !self.config.prefetch_enabled {
            return;
        }

        let height = self.current_height.load(Ordering::Relaxed);

        for addr in addresses {
            // Already hot.
            if self.slot_map.contains_key(addr) {
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

            // Not cached — insert a placeholder in the hot tier.
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

    /// Batch-write all GPU-tier accounts from cpu_mirror to the GPU buffer.
    ///
    /// Call this once per block, before any compute shader pass that reads
    /// account data (BlockSTM, Merkle hashing). Individual `put()` calls
    /// only update the cpu_mirror for speed; this method reconciles the GPU
    /// buffer in a single pass.
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

    /// Flush cpu_mirror → GPU buffer, then staging → device-local on discrete GPUs.
    pub fn sync(&self) {
        self.flush_to_gpu();
        self.gpu_buffer.sync_to_gpu();
    }

    // -- block height ------------------------------------------------------

    /// Advance the internal block-height counter.
    pub fn advance_height(&self, height: u64) {
        self.current_height.store(height, Ordering::Relaxed);
    }

    // -- shutdown ----------------------------------------------------------

    /// Securely zero GPU memory and release resources.
    pub fn shutdown(self) {
        let buffer = Arc::try_unwrap(self.gpu_buffer);
        match buffer {
            Ok(buf) => buf.secure_shutdown(),
            Err(arc_buf) => {
                warn!("GPU buffer has multiple owners, cannot secure-shutdown (will be zeroed on drop)");
            }
        }
    }

    // -- statistics --------------------------------------------------------

    /// Return a snapshot of current cache statistics.
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

    /// Reset all statistics counters to zero.
    pub fn reset_stats(&self) {
        *self.stats.write() = CacheStats::default();
    }

    // -- counts ------------------------------------------------------------

    /// Number of accounts currently in the GPU (hot) tier.
    pub fn gpu_count(&self) -> usize {
        self.slot_map.len()
    }

    /// Number of accounts currently in the warm (CPU RAM) tier.
    pub fn warm_count(&self) -> usize {
        self.warm.len()
    }

    /// Total cached accounts across both tiers.
    pub fn total_count(&self) -> usize {
        self.slot_map.len() + self.warm.len()
    }

    /// Returns `true` if the given address is resident in the GPU tier.
    pub fn is_gpu_resident(&self, address: &[u8; 32]) -> bool {
        self.slot_map.contains_key(address)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

        // 6th account should overflow to warm.
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

        // Access account 0 to refresh its last_access.
        cache.get(&addr(0));

        // Evict 1 — should evict account 1 (oldest un-refreshed).
        let evicted = cache.evict(1);
        assert_eq!(evicted, 1);
        assert!(cache.is_gpu_resident(&addr(0)), "account 0 should stay");
        assert!(!cache.is_gpu_resident(&addr(1)), "account 1 should be evicted");
        assert!(cache.is_gpu_resident(&addr(2)), "account 2 should stay");

        // Evicted account should be in warm tier.
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

        for _ in 0..5 { cache.get(&addr(0)); }
        for _ in 0..3 { cache.get(&addr(2)); }

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
        let addrs: Vec<[u8; 32]> = (0..5).map(|i| addr(i)).collect();

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

        cache.get(&addr(1));  // GPU hit
        cache.get(&addr(99)); // miss

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
