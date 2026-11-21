use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct IndexFile {
    entries: BTreeMap<String, u64>,
}

pub struct ChunkCache {
    cache_dir: PathBuf,
    cap_bytes: u64,
    last_served: BTreeMap<String, u64>,
}

pub const DEFAULT_CAP_BYTES: u64 = 50 * 1024 * 1024 * 1024;

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

        let index_path = cache_dir.join("_index.json");
        let mut last_served = if index_path.exists() {
            let bytes = fs::read(&index_path)?;
            let parsed: IndexFile = serde_json::from_slice(&bytes).unwrap_or_default();
            parsed.entries
        } else {
            BTreeMap::new()
        };

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

    pub fn touch(&mut self, hash_hex: &str) {
        if self.last_served.contains_key(hash_hex) {
            self.last_served.insert(hash_hex.to_string(), now_millis());
        }
    }

    pub fn get(&mut self, hash_hex: &str) -> Option<Vec<u8>> {
        let path = self.cache_dir.join(hash_hex);
        let bytes = fs::read(&path).ok()?;
        self.touch(hash_hex);
        Some(bytes)
    }

    pub fn put(&mut self, hash_hex: &str, bytes: &[u8]) -> io::Result<()> {
        let needed = bytes.len() as u64;
        if needed > self.cap_bytes {
            return Err(io::Error::other("chunk larger than cap"));
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

    pub fn purge_to_cap(&mut self) -> io::Result<()> {
        while self.total_bytes()? > self.cap_bytes {
            if !self.evict_one()? {
                break;
            }
        }
        Ok(())
    }

    pub fn save_index(&self) -> io::Result<()> {
        let index = IndexFile {
            entries: self.last_served.clone(),
        };
        let bytes = serde_json::to_vec(&index).map_err(io::Error::other)?;
        fs::write(self.cache_dir.join("_index.json"), bytes)
    }

    pub fn cached_hashes(&self) -> Vec<String> {
        self.last_served.keys().cloned().collect()
    }
}

fn is_hex_name(name: &str) -> bool {
    !name.is_empty() && name.len().is_multiple_of(2) && name.chars().all(|c| c.is_ascii_hexdigit())
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
        let mut cache = ChunkCache::load_or_init(&dir, 20).unwrap();
        cache.put("aa", &[0u8; 10]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        cache.put("bb", &[0u8; 10]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        cache.put("cc", &[0u8; 10]).unwrap();
        assert!(
            cache.get("aa").is_none(),
            "oldest chunk 'aa' should have been evicted"
        );
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
