use blake3;
use dashmap::DashMap;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

pub const PAGE_SIZE: usize = 4096;

pub const PAGE_HEADER_SIZE: usize = 48;

pub const PAGE_SLOT_SIZE: usize = PAGE_HEADER_SIZE + PAGE_SIZE;

const PAGE_MAGIC: u32 = 0x4152_4350;

#[derive(Error, Debug)]
pub enum MmapError {
    #[error("page not found: {0}")]
    PageNotFound(u64),
    #[error("data too large for page: {size} > {max}")]
    DataTooLarge { size: usize, max: usize },
    #[error("checksum mismatch on page {page_id}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        page_id: u64,
        expected: String,
        actual: String,
    },
    #[error("backing store full: need {need} bytes, capacity {capacity}")]
    BackingFull { need: usize, capacity: usize },
    #[error("resize exceeds max_size ({max_size} bytes)")]
    ResizeExceedsMax { max_size: usize },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid page header magic on page {page_id}")]
    InvalidMagic { page_id: u64 },
}

pub type MmapResult<T> = Result<T, MmapError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    pub magic: u32,
    pub page_id: u64,
    pub data_len: u32,
    pub checksum: [u8; 32],
}

impl PageHeader {
    pub const SIZE: usize = PAGE_HEADER_SIZE;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..12].copy_from_slice(&self.page_id.to_le_bytes());
        buf[12..16].copy_from_slice(&self.data_len.to_le_bytes());
        buf[16..48].copy_from_slice(&self.checksum);
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().ok()?);
        let page_id = u64::from_le_bytes(buf[4..12].try_into().ok()?);
        let data_len = u32::from_le_bytes(buf[12..16].try_into().ok()?);
        let mut checksum = [0u8; 32];
        checksum.copy_from_slice(&buf[16..48]);
        Some(Self {
            magic,
            page_id,
            data_len,
            checksum,
        })
    }
}

pub trait MmapBackend: Send + Sync {
    fn capacity(&self) -> usize;

    fn read(&self, offset: usize, len: usize) -> MmapResult<Vec<u8>>;

    fn write(&self, offset: usize, data: &[u8]) -> MmapResult<()>;

    fn flush(&self) -> MmapResult<()>;

    fn resize(&self, new_capacity: usize) -> MmapResult<()>;
}

pub struct VecBackend {
    buf: RwLock<Vec<u8>>,
}

impl VecBackend {
    pub fn new(initial_capacity: usize) -> Self {
        Self {
            buf: RwLock::new(vec![0u8; initial_capacity]),
        }
    }
}

impl MmapBackend for VecBackend {
    fn capacity(&self) -> usize {
        self.buf.read().len()
    }

    fn read(&self, offset: usize, len: usize) -> MmapResult<Vec<u8>> {
        let guard = self.buf.read();
        if offset + len > guard.len() {
            return Err(MmapError::BackingFull {
                need: offset + len,
                capacity: guard.len(),
            });
        }
        Ok(guard[offset..offset + len].to_vec())
    }

    fn write(&self, offset: usize, data: &[u8]) -> MmapResult<()> {
        let mut guard = self.buf.write();
        if offset + data.len() > guard.len() {
            return Err(MmapError::BackingFull {
                need: offset + data.len(),
                capacity: guard.len(),
            });
        }
        guard[offset..offset + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn flush(&self) -> MmapResult<()> {
        Ok(())
    }

    fn resize(&self, new_capacity: usize) -> MmapResult<()> {
        let mut guard = self.buf.write();
        guard.resize(new_capacity, 0);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MmapConfig {
    pub initial_size: usize,
    pub max_size: usize,
    pub growth_factor: f64,
}

impl Default for MmapConfig {
    fn default() -> Self {
        Self {
            initial_size: PAGE_SLOT_SIZE * 256,
            max_size: PAGE_SLOT_SIZE * 1_048_576,
            growth_factor: 2.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MmapRegion {
    pub offset: usize,
    pub length: usize,
    pub path: PathBuf,
}

pub struct MmapStateStore<B: MmapBackend = VecBackend> {
    backend: Arc<B>,
    index: DashMap<u64, MmapRegion>,
    next_offset: RwLock<usize>,
    config: MmapConfig,
    path: Option<PathBuf>,
}

impl MmapStateStore<VecBackend> {
    pub fn open<P: AsRef<Path>>(path: P, config: MmapConfig) -> MmapResult<Self> {
        let backend = Arc::new(VecBackend::new(config.initial_size));
        Ok(Self {
            backend,
            index: DashMap::new(),
            next_offset: RwLock::new(0),
            config,
            path: Some(path.as_ref().to_path_buf()),
        })
    }

    pub fn open_default<P: AsRef<Path>>(path: P) -> MmapResult<Self> {
        Self::open(path, MmapConfig::default())
    }
}

impl<B: MmapBackend> MmapStateStore<B> {
    pub fn with_backend(backend: Arc<B>, config: MmapConfig) -> Self {
        Self {
            backend,
            index: DashMap::new(),
            next_offset: RwLock::new(0),
            config,
            path: None,
        }
    }

    pub fn get_page(&self, page_id: u64) -> MmapResult<Vec<u8>> {
        let region = self
            .index
            .get(&page_id)
            .ok_or(MmapError::PageNotFound(page_id))?;

        let raw = self.backend.read(region.offset, PAGE_SLOT_SIZE)?;

        let header = PageHeader::from_bytes(&raw[..PAGE_HEADER_SIZE])
            .ok_or(MmapError::InvalidMagic { page_id })?;

        if header.magic != PAGE_MAGIC {
            return Err(MmapError::InvalidMagic { page_id });
        }

        let data_len = header.data_len as usize;
        let payload = &raw[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + data_len];

        let actual_hash = blake3::hash(payload);
        if actual_hash.as_bytes() != &header.checksum {
            return Err(MmapError::ChecksumMismatch {
                page_id,
                expected: hex_encode(&header.checksum),
                actual: hex_encode(actual_hash.as_bytes()),
            });
        }

        Ok(payload.to_vec())
    }

    pub fn put_page(&self, page_id: u64, data: &[u8]) -> MmapResult<()> {
        if data.len() > PAGE_SIZE {
            return Err(MmapError::DataTooLarge {
                size: data.len(),
                max: PAGE_SIZE,
            });
        }

        let checksum: [u8; 32] = *blake3::hash(data).as_bytes();

        let header = PageHeader {
            magic: PAGE_MAGIC,
            page_id,
            data_len: data.len() as u32,
            checksum,
        };

        let offset = if let Some(existing) = self.index.get(&page_id) {
            existing.offset
        } else {
            self.allocate_slot()?
        };

        self.backend.write(offset, &header.to_bytes())?;

        let mut padded = vec![0u8; PAGE_SIZE];
        padded[..data.len()].copy_from_slice(data);
        self.backend.write(offset + PAGE_HEADER_SIZE, &padded)?;

        self.index.insert(
            page_id,
            MmapRegion {
                offset,
                length: PAGE_SLOT_SIZE,
                path: self.path.clone().unwrap_or_default(),
            },
        );

        Ok(())
    }

    pub fn flush(&self) -> MmapResult<()> {
        self.backend.flush()
    }

    pub fn resize(&self, new_size: usize) -> MmapResult<()> {
        if new_size > self.config.max_size {
            return Err(MmapError::ResizeExceedsMax {
                max_size: self.config.max_size,
            });
        }
        self.backend.resize(new_size)
    }

    pub fn page_count(&self) -> usize {
        self.index.len()
    }

    pub fn contains_page(&self, page_id: u64) -> bool {
        self.index.contains_key(&page_id)
    }

    pub fn remove_page(&self, page_id: u64) -> MmapResult<()> {
        self.index
            .remove(&page_id)
            .ok_or(MmapError::PageNotFound(page_id))?;
        Ok(())
    }

    pub fn page_ids(&self) -> Vec<u64> {
        self.index.iter().map(|entry| *entry.key()).collect()
    }

    fn allocate_slot(&self) -> MmapResult<usize> {
        let mut next = self.next_offset.write();
        let offset = *next;
        let required = offset + PAGE_SLOT_SIZE;

        if required > self.backend.capacity() {
            let new_cap = std::cmp::max(
                (self.backend.capacity() as f64 * self.config.growth_factor) as usize,
                required,
            );
            if new_cap > self.config.max_size {
                return Err(MmapError::ResizeExceedsMax {
                    max_size: self.config.max_size,
                });
            }
            self.backend.resize(new_cap)?;
        }

        *next = required;
        Ok(offset)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> MmapStateStore<VecBackend> {
        MmapStateStore::open_default("/tmp/test_mmap").unwrap()
    }

    #[test]
    fn test_put_get_roundtrip() {
        let store = make_store();
        let data = b"hello mmap pages";
        store.put_page(1, data).unwrap();
        let got = store.get_page(1).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn test_page_not_found() {
        let store = make_store();
        let err = store.get_page(999).unwrap_err();
        assert!(matches!(err, MmapError::PageNotFound(999)));
    }

    #[test]
    fn test_overwrite_page() {
        let store = make_store();
        store.put_page(1, b"version_1").unwrap();
        store.put_page(1, b"version_2").unwrap();
        let got = store.get_page(1).unwrap();
        assert_eq!(got, b"version_2");
    }

    #[test]
    fn test_data_too_large() {
        let store = make_store();
        let big = vec![0xAB; PAGE_SIZE + 1];
        let err = store.put_page(1, &big).unwrap_err();
        assert!(matches!(err, MmapError::DataTooLarge { .. }));
    }

    #[test]
    fn test_multiple_pages() {
        let store = make_store();
        for i in 0..50 {
            let data = format!("page-{i}");
            store.put_page(i, data.as_bytes()).unwrap();
        }
        assert_eq!(store.page_count(), 50);
        for i in 0..50 {
            let expected = format!("page-{i}");
            let got = store.get_page(i).unwrap();
            assert_eq!(got, expected.as_bytes());
        }
    }

    #[test]
    fn test_checksum_corruption() {
        let backend = Arc::new(VecBackend::new(PAGE_SLOT_SIZE * 4));
        let store = MmapStateStore::with_backend(backend.clone(), MmapConfig::default());
        store.put_page(1, b"important data").unwrap();

        let region = store.index.get(&1).unwrap();
        let corrupt_offset = region.offset + PAGE_HEADER_SIZE + 2;
        backend.write(corrupt_offset, &[0xFF]).unwrap();

        let err = store.get_page(1).unwrap_err();
        assert!(matches!(err, MmapError::ChecksumMismatch { .. }));
    }

    #[test]
    fn test_flush() {
        let store = make_store();
        store.put_page(1, b"data").unwrap();
        store.flush().unwrap();
    }

    #[test]
    fn test_resize_within_bounds() {
        let config = MmapConfig {
            initial_size: PAGE_SLOT_SIZE * 4,
            max_size: PAGE_SLOT_SIZE * 100,
            growth_factor: 2.0,
        };
        let store = MmapStateStore::open("/tmp/resize_test", config).unwrap();
        store.resize(PAGE_SLOT_SIZE * 50).unwrap();
    }

    #[test]
    fn test_resize_exceeds_max() {
        let config = MmapConfig {
            initial_size: PAGE_SLOT_SIZE * 4,
            max_size: PAGE_SLOT_SIZE * 8,
            growth_factor: 2.0,
        };
        let store = MmapStateStore::open("/tmp/resize_max_test", config).unwrap();
        let err = store.resize(PAGE_SLOT_SIZE * 100).unwrap_err();
        assert!(matches!(err, MmapError::ResizeExceedsMax { .. }));
    }

    #[test]
    fn test_remove_page() {
        let store = make_store();
        store.put_page(42, b"ephemeral").unwrap();
        assert!(store.contains_page(42));
        store.remove_page(42).unwrap();
        assert!(!store.contains_page(42));
        assert!(store.get_page(42).is_err());
    }

    #[test]
    fn test_remove_nonexistent() {
        let store = make_store();
        let err = store.remove_page(7).unwrap_err();
        assert!(matches!(err, MmapError::PageNotFound(7)));
    }

    #[test]
    fn test_auto_grow() {
        let config = MmapConfig {
            initial_size: PAGE_SLOT_SIZE * 2,
            max_size: PAGE_SLOT_SIZE * 100,
            growth_factor: 2.0,
        };
        let store = MmapStateStore::open("/tmp/auto_grow_test", config).unwrap();
        for i in 0..10 {
            store.put_page(i, b"grow me").unwrap();
        }
        assert_eq!(store.page_count(), 10);
        for i in 0..10 {
            assert_eq!(store.get_page(i).unwrap(), b"grow me");
        }
    }

    #[test]
    fn test_page_ids() {
        let store = make_store();
        store.put_page(10, b"a").unwrap();
        store.put_page(20, b"b").unwrap();
        store.put_page(30, b"c").unwrap();
        let mut ids = store.page_ids();
        ids.sort();
        assert_eq!(ids, vec![10, 20, 30]);
    }

    #[test]
    fn test_page_header_serde() {
        let hdr = PageHeader {
            magic: PAGE_MAGIC,
            page_id: 0xDEAD_BEEF_CAFE_BABE,
            data_len: 1234,
            checksum: [0xAA; 32],
        };
        let bytes = hdr.to_bytes();
        let decoded = PageHeader::from_bytes(&bytes).unwrap();
        assert_eq!(hdr, decoded);
    }

    #[test]
    fn test_full_page_data() {
        let store = make_store();
        let full = vec![0x42u8; PAGE_SIZE];
        store.put_page(99, &full).unwrap();
        let got = store.get_page(99).unwrap();
        assert_eq!(got, full);
    }
}
