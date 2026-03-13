//! Memory-mapped state pages for ARC Chain.
//!
//! Provides a page-oriented storage backend backed by a simulated memory-mapped
//! region (a `Vec<u8>` that can be swapped for a real `mmap` later via the
//! [`MmapBackend`] trait).  Every 4 KiB page carries a [`PageHeader`] with a
//! BLAKE3 checksum so corruption is caught on read.

use blake3;
use dashmap::DashMap;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Logical page payload size (4 KiB).
pub const PAGE_SIZE: usize = 4096;

/// Size of the on-disk page header that prefixes every page.
pub const PAGE_HEADER_SIZE: usize = 48;

/// Total bytes per slot: header + payload.
pub const PAGE_SLOT_SIZE: usize = PAGE_HEADER_SIZE + PAGE_SIZE;

/// Magic number written at the start of every page header.
const PAGE_MAGIC: u32 = 0x4152_4350; // "ARCP"

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Page header (48 bytes)
// ---------------------------------------------------------------------------

/// Fixed-size header prepended to every page slot.
///
/// Layout (48 bytes):
/// ```text
/// [ magic: 4 ][ page_id: 8 ][ data_len: 4 ][ checksum: 32 ]
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    pub magic: u32,
    pub page_id: u64,
    pub data_len: u32,
    pub checksum: [u8; 32],
}

impl PageHeader {
    pub const SIZE: usize = PAGE_HEADER_SIZE;

    /// Serialize the header into a 48-byte buffer.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..12].copy_from_slice(&self.page_id.to_le_bytes());
        buf[12..16].copy_from_slice(&self.data_len.to_le_bytes());
        buf[16..48].copy_from_slice(&self.checksum);
        buf
    }

    /// Deserialize a header from a 48-byte slice.
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

// ---------------------------------------------------------------------------
// MmapBackend trait
// ---------------------------------------------------------------------------

/// Abstraction over the backing storage so we can swap `Vec<u8>` for a real
/// `mmap` (e.g. via `memmap2`) later without changing consumer code.
pub trait MmapBackend: Send + Sync {
    /// Total capacity of the backing store in bytes.
    fn capacity(&self) -> usize;

    /// Read `len` bytes starting at `offset`.
    fn read(&self, offset: usize, len: usize) -> MmapResult<Vec<u8>>;

    /// Write `data` starting at `offset`.
    fn write(&self, offset: usize, data: &[u8]) -> MmapResult<()>;

    /// Ensure all pending writes are durable.
    fn flush(&self) -> MmapResult<()>;

    /// Grow the backing store to `new_capacity` bytes.
    fn resize(&self, new_capacity: usize) -> MmapResult<()>;
}

// ---------------------------------------------------------------------------
// VecBackend — in-process simulation
// ---------------------------------------------------------------------------

/// A `Vec<u8>` that implements [`MmapBackend`].
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
        // Vec is already in memory — nothing to flush.
        Ok(())
    }

    fn resize(&self, new_capacity: usize) -> MmapResult<()> {
        let mut guard = self.buf.write();
        guard.resize(new_capacity, 0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MmapConfig
// ---------------------------------------------------------------------------

/// Tuning knobs for the mmap state store.
#[derive(Debug, Clone)]
pub struct MmapConfig {
    /// Size of the backing store on first open (bytes).
    pub initial_size: usize,
    /// Hard upper limit the store may grow to (bytes).
    pub max_size: usize,
    /// Multiplicative factor when auto-growing (e.g. 2.0 = double).
    pub growth_factor: f64,
}

impl Default for MmapConfig {
    fn default() -> Self {
        Self {
            initial_size: PAGE_SLOT_SIZE * 256, // ~1 MiB
            max_size: PAGE_SLOT_SIZE * 1_048_576, // ~4 GiB
            growth_factor: 2.0,
        }
    }
}

// ---------------------------------------------------------------------------
// MmapRegion
// ---------------------------------------------------------------------------

/// One contiguous mapped region inside the backing store.
#[derive(Debug, Clone)]
pub struct MmapRegion {
    /// Byte offset into the backing store where this region starts.
    pub offset: usize,
    /// Length of the region in bytes.
    pub length: usize,
    /// Logical path associated with this region (for bookkeeping / debugging).
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// MmapStateStore
// ---------------------------------------------------------------------------

/// Page-oriented state store backed by an [`MmapBackend`].
///
/// Pages are 4 KiB payloads, each prefixed by a 48-byte [`PageHeader`] that
/// carries a BLAKE3 checksum.  An internal `DashMap` tracks where each
/// `page_id` lives inside the backing store so lookups are O(1).
pub struct MmapStateStore<B: MmapBackend = VecBackend> {
    backend: Arc<B>,
    /// Maps page_id → MmapRegion describing where the page lives.
    index: DashMap<u64, MmapRegion>,
    /// Next free byte offset in the backing store.
    next_offset: RwLock<usize>,
    config: MmapConfig,
    /// Optional filesystem path (used when backed by a real file).
    path: Option<PathBuf>,
}

impl MmapStateStore<VecBackend> {
    /// Open (or create) an in-memory state store at a logical `path`.
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

    /// Convenience: open with default config.
    pub fn open_default<P: AsRef<Path>>(path: P) -> MmapResult<Self> {
        Self::open(path, MmapConfig::default())
    }
}

impl<B: MmapBackend> MmapStateStore<B> {
    /// Create a store around an existing backend.
    pub fn with_backend(backend: Arc<B>, config: MmapConfig) -> Self {
        Self {
            backend,
            index: DashMap::new(),
            next_offset: RwLock::new(0),
            config,
            path: None,
        }
    }

    /// Read the page payload for `page_id`, verifying its checksum.
    pub fn get_page(&self, page_id: u64) -> MmapResult<Vec<u8>> {
        let region = self
            .index
            .get(&page_id)
            .ok_or(MmapError::PageNotFound(page_id))?;

        // Read the full slot (header + payload).
        let raw = self.backend.read(region.offset, PAGE_SLOT_SIZE)?;

        // Decode header.
        let header = PageHeader::from_bytes(&raw[..PAGE_HEADER_SIZE])
            .ok_or(MmapError::InvalidMagic { page_id })?;

        if header.magic != PAGE_MAGIC {
            return Err(MmapError::InvalidMagic { page_id });
        }

        let data_len = header.data_len as usize;
        let payload = &raw[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + data_len];

        // Verify checksum.
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

    /// Write `data` into the page for `page_id`.
    ///
    /// If the page already exists its slot is reused; otherwise a new slot is
    /// allocated (auto-growing the backing store if necessary).
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

        // Write header.
        self.backend.write(offset, &header.to_bytes())?;

        // Write payload (zero-pad to full PAGE_SIZE to keep slots uniform).
        let mut padded = vec![0u8; PAGE_SIZE];
        padded[..data.len()].copy_from_slice(data);
        self.backend.write(offset + PAGE_HEADER_SIZE, &padded)?;

        // Update index.
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

    /// Flush all pending writes to the underlying backend.
    pub fn flush(&self) -> MmapResult<()> {
        self.backend.flush()
    }

    /// Grow the backing store to `new_size` bytes.
    pub fn resize(&self, new_size: usize) -> MmapResult<()> {
        if new_size > self.config.max_size {
            return Err(MmapError::ResizeExceedsMax {
                max_size: self.config.max_size,
            });
        }
        self.backend.resize(new_size)
    }

    /// Number of pages currently stored.
    pub fn page_count(&self) -> usize {
        self.index.len()
    }

    /// Returns `true` if `page_id` is in the index.
    pub fn contains_page(&self, page_id: u64) -> bool {
        self.index.contains_key(&page_id)
    }

    /// Remove a page from the store (marks the slot as freed but does not
    /// compact — compaction is a future optimisation).
    pub fn remove_page(&self, page_id: u64) -> MmapResult<()> {
        self.index
            .remove(&page_id)
            .ok_or(MmapError::PageNotFound(page_id))?;
        Ok(())
    }

    /// Iterate over all stored page IDs.
    pub fn page_ids(&self) -> Vec<u64> {
        self.index.iter().map(|entry| *entry.key()).collect()
    }

    // -- private helpers ---------------------------------------------------

    /// Allocate the next free slot, auto-growing if necessary.
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> MmapStateStore<VecBackend> {
        MmapStateStore::open_default("/tmp/test_mmap").unwrap()
    }

    // 1. Basic put / get round-trip.
    #[test]
    fn test_put_get_roundtrip() {
        let store = make_store();
        let data = b"hello mmap pages";
        store.put_page(1, data).unwrap();
        let got = store.get_page(1).unwrap();
        assert_eq!(got, data);
    }

    // 2. Page not found.
    #[test]
    fn test_page_not_found() {
        let store = make_store();
        let err = store.get_page(999).unwrap_err();
        assert!(matches!(err, MmapError::PageNotFound(999)));
    }

    // 3. Overwrite an existing page.
    #[test]
    fn test_overwrite_page() {
        let store = make_store();
        store.put_page(1, b"version_1").unwrap();
        store.put_page(1, b"version_2").unwrap();
        let got = store.get_page(1).unwrap();
        assert_eq!(got, b"version_2");
    }

    // 4. Data too large for page.
    #[test]
    fn test_data_too_large() {
        let store = make_store();
        let big = vec![0xAB; PAGE_SIZE + 1];
        let err = store.put_page(1, &big).unwrap_err();
        assert!(matches!(err, MmapError::DataTooLarge { .. }));
    }

    // 5. Multiple pages.
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

    // 6. Checksum corruption detection.
    #[test]
    fn test_checksum_corruption() {
        let backend = Arc::new(VecBackend::new(PAGE_SLOT_SIZE * 4));
        let store = MmapStateStore::with_backend(backend.clone(), MmapConfig::default());
        store.put_page(1, b"important data").unwrap();

        // Corrupt one byte in the payload area.
        let region = store.index.get(&1).unwrap();
        let corrupt_offset = region.offset + PAGE_HEADER_SIZE + 2;
        backend.write(corrupt_offset, &[0xFF]).unwrap();

        let err = store.get_page(1).unwrap_err();
        assert!(matches!(err, MmapError::ChecksumMismatch { .. }));
    }

    // 7. Flush succeeds.
    #[test]
    fn test_flush() {
        let store = make_store();
        store.put_page(1, b"data").unwrap();
        store.flush().unwrap();
    }

    // 8. Resize within bounds succeeds.
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

    // 9. Resize beyond max_size fails.
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

    // 10. Remove page.
    #[test]
    fn test_remove_page() {
        let store = make_store();
        store.put_page(42, b"ephemeral").unwrap();
        assert!(store.contains_page(42));
        store.remove_page(42).unwrap();
        assert!(!store.contains_page(42));
        assert!(store.get_page(42).is_err());
    }

    // 11. Remove nonexistent page is an error.
    #[test]
    fn test_remove_nonexistent() {
        let store = make_store();
        let err = store.remove_page(7).unwrap_err();
        assert!(matches!(err, MmapError::PageNotFound(7)));
    }

    // 12. Auto-grow on allocation.
    #[test]
    fn test_auto_grow() {
        let config = MmapConfig {
            initial_size: PAGE_SLOT_SIZE * 2,
            max_size: PAGE_SLOT_SIZE * 100,
            growth_factor: 2.0,
        };
        let store = MmapStateStore::open("/tmp/auto_grow_test", config).unwrap();
        // Write more pages than initial capacity allows.
        for i in 0..10 {
            store.put_page(i, b"grow me").unwrap();
        }
        assert_eq!(store.page_count(), 10);
        for i in 0..10 {
            assert_eq!(store.get_page(i).unwrap(), b"grow me");
        }
    }

    // 13. page_ids returns all stored IDs.
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

    // 14. PageHeader round-trip serialization.
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

    // 15. Full page (exactly PAGE_SIZE bytes) round-trip.
    #[test]
    fn test_full_page_data() {
        let store = make_store();
        let full = vec![0x42u8; PAGE_SIZE];
        store.put_page(99, &full).unwrap();
        let got = store.get_page(99).unwrap();
        assert_eq!(got, full);
    }
}
