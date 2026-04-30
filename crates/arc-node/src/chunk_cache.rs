//! Milestone E (#39): on-disk LRU chunk cache.
//!
//! Community workers download model chunks on demand (per Milestone C's
//! `/chunks/get/{hash}` plumbing). Without eviction, a node advertising
//! 10 GB of capacity but asked to serve 30+ different models over a
//! day of simulated demand would run out of disk in minutes. The LRU
//! cap enforces the node's advertised storage ceiling deterministically.
//!
//! # Shape
//!
//! - One file per chunk under `cache_dir/<hex-hash>`.
//! - A separate JSON sidecar `cache_dir/_index.json` records
//!   `{hash: last_served_unix_millis}` so last-served timestamps
//!   survive restarts (warm-set persistence - a restarted node prefers
//!   re-taking ranges it already has cached over downloading fresh).
//! - Eviction runs whenever `put` would push total size over `cap_bytes`;
//!   it evicts in ascending last_served order until under cap.
//!
//! # Non-goals
//!
//! This module is intentionally simple - no background GC thread, no
//! async tokio file I/O, no compaction. Calls are synchronous; the
//! expected call frequency is tens of chunks per minute, not per second.
//! A future tighter implementation can layer those on without changing
//! the public surface (`touch`, `get`, `put`, `purge_to_cap`,
//! `total_bytes`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Stored as JSON alongside the chunk files so a restart doesn't
/// forget which chunks are the hottest (warm-set persistence -
/// planner prefers re-assigning ranges the node already has cached).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct IndexFile {
    entries: BTreeMap<String, u64>,
}

/// On-disk LRU chunk cache, parameterized by a byte cap. Safe to
/// construct per node at startup; the first `load_or_init` call
/// reconciles on-disk state with the in-memory index.
pub struct ChunkCache {
    cache_dir: PathBuf,
    cap_bytes: u64,
    // Raw `BTreeMap` of chunk-hash (hex) -> last-served unix millis.
    // Sorted for deterministic eviction order when two chunks tie.
    last_served: BTreeMap<String, u64>,
}

/// Default cap - 50 GB. Same as the value called out in PLAN.md's
/// Milestone E acceptance.
pub const DEFAULT_CAP_BYTES: u64 = 50 * 1024 * 1024 * 1024;

/// Unix millis-to-u64 helper used both when recording a fresh
/// `touch()` and when backstopping `load_or_init` against a corrupted
/// sidecar.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ChunkCache {
    pub fn load_or_init(cache_dir: impl AsRef<Path>, cap_bytes: u64) -> io::Result<Self> {
        let cache_dir = cache_dir.as_ref().to_path_buf();
        fs::create_dir_all(&cache_dir)?;

        // Load the index sidecar if present; a missing or corrupt sidecar
        // is recoverable - we scan the directory and fabricate "now" for
        // every chunk so the next put sorts on real usage.
        let index_path = cache_dir.join("_index.json");
        let mut last_served = if index_path.exists() {
            let bytes = fs::read(&index_path)?;
            let parsed: IndexFile = serde_json::from_slice(&bytes).unwrap_or_default();
            parsed.entries
        } else {
            BTreeMap::new()
        };

        // Reconcile with the actual on-disk set - purge index rows for
        // chunks that got deleted out-of-band, and add rows for chunks
        // present on disk but missing from the index.
        let mut seen: BTreeMap<String, ()> = BTreeMap::new();
        for entry in fs::read_dir(&cache_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "_index.json" {
                continue;
            }
            if !is_hex_name(&name) {
                continue;
            }
            seen.insert(name.clone(), ());
            last_served.entry(name).or_insert_with(now_millis);
        }
        last_served.retain(|k, _| seen.contains_key(k));

        Ok(Self {
            cache_dir,
            cap_bytes,
            last_served,
        })
    }

    /// Record a "serve" event on an existing chunk. Updates the in-memory
    /// index but does NOT flush the sidecar - callers batch flushes via
    /// `save_index()` to avoid thrashing the JSON file per serve.
    pub fn touch(&mut self, hash_hex: &str) {
        if self.last_served.contains_key(hash_hex) {
            self.last_served.insert(hash_hex.to_string(), now_millis());
        }
    }

    /// Read a chunk's bytes off disk. Also calls `touch()` so the
    /// read counts as a hot-access for LRU purposes.
    pub fn get(&mut self, hash_hex: &str) -> Option<Vec<u8>> {
        let path = self.cache_dir.join(hash_hex);
        let bytes = fs::read(&path).ok()?;
        self.touch(hash_hex);
        Some(bytes)
    }

    /// Insert a freshly-downloaded chunk. Evicts LRU entries beforehand
    /// so inserting the new chunk stays under `cap_bytes`.
    pub fn put(&mut self, hash_hex: &str, bytes: &[u8]) -> io::Result<()> {
        // Evict until we have room for `bytes.len()`.
        let needed = bytes.len() as u64;
        if needed > self.cap_bytes {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "chunk larger than cap",
            ));
        }
        while self.total_bytes()? + needed > self.cap_bytes {
            if !self.evict_one()? {
                break;
            }
        }
        let path = self.cache_dir.join(hash_hex);
        fs::write(&path, bytes)?;
        self.last_served.insert(hash_hex.to_string(), now_millis());
        Ok(())
    }

    /// Compute total bytes of all cached chunks on disk. Fresh each
    /// time - avoids stale in-memory accounting drifting from reality
    /// if a user manually deletes cache files.
    pub fn total_bytes(&self) -> io::Result<u64> {
        let mut total = 0u64;
        for entry in fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "_index.json" {
                continue;
            }
            if !is_hex_name(&name) {
                continue;
            }
            total += entry.metadata()?.len();
        }
        Ok(total)
    }

    /// Evict the single oldest chunk by last_served timestamp. Returns
    /// true if something was evicted, false if the cache was empty.
    fn evict_one(&mut self) -> io::Result<bool> {
        let victim = self
            .last_served
            .iter()
            .min_by(|(_, a), (_, b)| a.cmp(b).then_with(|| std::cmp::Ordering::Equal))
            .map(|(k, _)| k.clone());
        if let Some(name) = victim {
            let path = self.cache_dir.join(&name);
            let _ = fs::remove_file(&path);
            self.last_served.remove(&name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Evict until total_bytes <= cap. Public variant of the loop used
    /// inside `put` - callers can invoke it manually after a series of
    /// touches that didn't themselves grow the cache.
    pub fn purge_to_cap(&mut self) -> io::Result<()> {
        while self.total_bytes()? > self.cap_bytes {
            if !self.evict_one()? {
                break;
            }
        }
        Ok(())
    }

    /// Flush the in-memory index sidecar to disk. Callers batch this
    /// (end of serving cycle, before shutdown, etc.) to avoid the
    /// write amplification of flushing on every touch.
    pub fn save_index(&self) -> io::Result<()> {
        let index = IndexFile {
            entries: self.last_served.clone(),
        };
        let bytes = serde_json::to_vec(&index)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        fs::write(self.cache_dir.join("_index.json"), bytes)
    }

    /// Snapshot of every cached chunk hash. The planner reads this to
    /// weight assignments: a node that already has chunks cached is
    /// cheaper to reassign the same range to (Milestone E warm-set
    /// stickiness).
    pub fn cached_hashes(&self) -> Vec<String> {
        self.last_served.keys().cloned().collect()
    }
}

fn is_hex_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() % 2 == 0
        && name.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmpdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("arc-chunk-cache-{}", tag));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn load_or_init_creates_dir() {
        let dir = tmpdir("init");
        let _cache = ChunkCache::load_or_init(&dir, DEFAULT_CAP_BYTES).unwrap();
        assert!(dir.exists());
    }

    #[test]
    fn put_get_roundtrip() {
        let dir = tmpdir("rt");
        let mut cache = ChunkCache::load_or_init(&dir, 1024 * 1024).unwrap();
        let h = "deadbeef";
        cache.put(h, b"hello chunk").unwrap();
        let got = cache.get(h).unwrap();
        assert_eq!(got, b"hello chunk");
    }

    #[test]
    fn put_evicts_oldest_to_stay_under_cap() {
        let dir = tmpdir("evict");
        // Cap of 20 bytes; first put (10B) fits, second (10B) fits,
        // third (10B) should evict the first since it's oldest.
        let mut cache = ChunkCache::load_or_init(&dir, 20).unwrap();
        cache.put("aa", &[0u8; 10]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        cache.put("bb", &[0u8; 10]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        cache.put("cc", &[0u8; 10]).unwrap();
        assert!(cache.get("aa").is_none(), "oldest chunk 'aa' should have been evicted");
        assert!(cache.get("bb").is_some());
        assert!(cache.get("cc").is_some());
        assert!(cache.total_bytes().unwrap() <= 20);
    }

    #[test]
    fn touch_moves_to_most_recent() {
        let dir = tmpdir("touch");
        let mut cache = ChunkCache::load_or_init(&dir, 20).unwrap();
        cache.put("aa", &[0u8; 10]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        cache.put("bb", &[0u8; 10]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        // Touch 'aa' so it's newest; next put should evict 'bb' instead.
        cache.touch("aa");
        std::thread::sleep(Duration::from_millis(5));
        cache.put("cc", &[0u8; 10]).unwrap();
        assert!(cache.get("aa").is_some());
        assert!(cache.get("bb").is_none());
        assert!(cache.get("cc").is_some());
    }

    #[test]
    fn save_index_survives_restart_warm_set() {
        let dir = tmpdir("warm");
        {
            let mut cache = ChunkCache::load_or_init(&dir, 1024).unwrap();
            cache.put("abab", b"warm").unwrap();
            cache.save_index().unwrap();
        }
        // Reopen; the cached hash should still be listed so the planner
        // can prefer re-assigning this chunk's range to this node.
        let cache = ChunkCache::load_or_init(&dir, 1024).unwrap();
        assert!(cache.cached_hashes().contains(&"abab".to_string()));
    }

    #[test]
    fn rejects_chunk_bigger_than_cap() {
        let dir = tmpdir("toobig");
        let mut cache = ChunkCache::load_or_init(&dir, 100).unwrap();
        let err = cache.put("deadbeef", &[0u8; 200]);
        assert!(err.is_err());
    }
}
