//! GPU-resident account buffer using wgpu unified/managed memory.
//!
//! On Apple Silicon (Metal), wgpu buffers with `MAP_READ | MAP_WRITE | STORAGE`
//! are backed by unified memory — CPU and GPU share the same physical pages.
//! Zero copy, zero sync overhead.
//!
//! On discrete GPUs (Vulkan/DX12), a staging buffer handles CPU↔GPU transfers.
//! The `sync_to_gpu()` call copies staging → device-local; `sync_from_gpu()` does
//! the reverse.
//!
//! On systems without a GPU, `CpuOnly` mode uses a plain `Vec<u8>` backing store
//! so callers don't need conditional logic.

use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{info, warn};

/// How GPU memory is managed on this system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryModel {
    /// Apple Silicon — `MAP_READ | MAP_WRITE | STORAGE` on a single buffer.
    /// CPU pointer and GPU pointer are the same physical memory. Zero copy.
    UnifiedMetal,
    /// Discrete GPU — separate host-visible staging buffer + device-local storage buffer.
    /// Requires explicit `sync_to_gpu()` / `sync_from_gpu()` calls.
    ManagedDiscrete,
    /// No GPU available — backed by a CPU-side `Vec<u8>`.
    CpuOnly,
}

/// 128-byte GPU-friendly representation of an account.
///
/// Aligned to 128 bytes to match GPU cache lines on both Metal (128B) and
/// NVIDIA (128B L1 cache line). All fields are fixed-size for direct memcpy
/// into GPU storage buffers.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GpuAccountRepr {
    pub address: [u8; 32],      // 32
    pub balance: u64,           // 8
    pub nonce: u64,             // 8
    pub code_hash: [u8; 32],    // 32
    pub storage_root: [u8; 32], // 32
    pub staked_balance: u64,    // 8
    pub _padding: [u8; 8],     // 8  → total = 128
}

// Safety: GpuAccountRepr is #[repr(C)], all fields are plain data, no padding gaps.
unsafe impl bytemuck::Pod for GpuAccountRepr {}
unsafe impl bytemuck::Zeroable for GpuAccountRepr {}

impl Default for GpuAccountRepr {
    fn default() -> Self {
        Self {
            address: [0u8; 32],
            balance: 0,
            nonce: 0,
            code_hash: [0u8; 32],
            storage_root: [0u8; 32],
            staked_balance: 0,
            _padding: [0u8; 8],
        }
    }
}

/// Size of one account slot in bytes.
pub const ACCOUNT_SLOT_SIZE: usize = std::mem::size_of::<GpuAccountRepr>(); // 128

/// GPU-resident account buffer.
///
/// Manages a flat array of `GpuAccountRepr` entries in GPU-accessible memory.
/// Slot-based addressing: each account occupies a fixed 128-byte slot indexed
/// by a `usize` slot number.
pub struct GpuAccountBuffer {
    /// The memory model detected for this system.
    memory_model: MemoryModel,
    /// Maximum number of account slots.
    capacity: usize,
    // --- Unified / Metal path ---
    /// On Metal: a single wgpu buffer that is both GPU-accessible and host-mapped.
    /// On discrete: the device-local STORAGE buffer (not host-visible).
    gpu_buffer: Option<wgpu::Buffer>,
    /// On discrete GPUs: host-visible staging buffer for CPU reads/writes.
    staging_buffer: Option<wgpu::Buffer>,
    /// wgpu device handle (kept alive for sync operations).
    device: Option<wgpu::Device>,
    /// wgpu queue handle (kept alive for sync operations).
    queue: Option<wgpu::Queue>,
    // --- CPU-only fallback ---
    /// Plain memory backing when no GPU is available.
    cpu_backing: Option<Vec<u8>>,
    /// Number of slots currently written.
    len: AtomicUsize,
}

// Safety: wgpu Device/Queue/Buffer are Send+Sync. cpu_backing is Vec which is Send+Sync.
unsafe impl Send for GpuAccountBuffer {}
unsafe impl Sync for GpuAccountBuffer {}

impl GpuAccountBuffer {
    /// Probe the GPU and allocate a buffer for `max_accounts` account slots.
    ///
    /// Detects Metal (unified memory) vs Vulkan/DX12 (discrete) vs no-GPU
    /// and creates the appropriate buffer configuration.
    pub fn new(max_accounts: usize) -> Result<Self, crate::GpuError> {
        let buf_size = (max_accounts * ACCOUNT_SLOT_SIZE) as u64;

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        })) {
            Ok(a) => a,
            Err(_) => {
                info!("No GPU adapter found, using CPU-only backing for state cache");
                return Ok(Self::cpu_only(max_accounts));
            }
        };

        let gpu_info = adapter.get_info();
        let max_buf = adapter.limits().max_buffer_size;

        if buf_size > max_buf {
            warn!(
                requested = buf_size,
                max = max_buf,
                "Requested buffer exceeds GPU max_buffer_size, using CPU-only fallback"
            );
            return Ok(Self::cpu_only(max_accounts));
        }

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("ARC GPU State"),
                required_features: wgpu::Features::MAPPABLE_PRIMARY_BUFFERS,
                required_limits: wgpu::Limits {
                    max_buffer_size: buf_size.max(wgpu::Limits::default().max_buffer_size),
                    ..wgpu::Limits::default()
                },
                ..Default::default()
            },
        ))
        .or_else(|_| {
            // Retry without MAPPABLE_PRIMARY_BUFFERS — discrete GPUs may not support it.
            pollster::block_on(adapter.request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("ARC GPU State"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits {
                        max_buffer_size: buf_size.max(wgpu::Limits::default().max_buffer_size),
                        ..wgpu::Limits::default()
                    },
                    ..Default::default()
                },
            ))
        })
        .map_err(|e| crate::GpuError::DeviceError(e.to_string()))?;

        // Detect memory model from backend.
        let is_metal = gpu_info.backend == wgpu::Backend::Metal;
        let memory_model = if is_metal {
            MemoryModel::UnifiedMetal
        } else {
            MemoryModel::ManagedDiscrete
        };

        match memory_model {
            MemoryModel::UnifiedMetal => {
                // Single buffer: STORAGE + MAP_READ + MAP_WRITE.
                // On Metal unified memory this is zero-copy.
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("ARC GPU State (unified)"),
                    size: buf_size,
                    usage: wgpu::BufferUsages::STORAGE
                        | wgpu::BufferUsages::MAP_READ
                        | wgpu::BufferUsages::MAP_WRITE
                        | wgpu::BufferUsages::COPY_SRC
                        | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: true,
                });

                // Zero-initialize the mapped region.
                {
                    let mut view = buffer.slice(..).get_mapped_range_mut();
                    view.fill(0);
                }
                buffer.unmap();

                info!(
                    gpu = %gpu_info.name,
                    backend = ?gpu_info.backend,
                    memory_model = "UnifiedMetal",
                    capacity = max_accounts,
                    buffer_mb = buf_size / (1024 * 1024),
                    "GPU state buffer allocated (unified memory)"
                );

                Ok(Self {
                    memory_model,
                    capacity: max_accounts,
                    gpu_buffer: Some(buffer),
                    staging_buffer: None,
                    device: Some(device),
                    queue: Some(queue),
                    cpu_backing: None,
                    len: AtomicUsize::new(0),
                })
            }
            MemoryModel::ManagedDiscrete => {
                // Two buffers: device-local STORAGE + host-visible staging.
                let storage = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("ARC GPU State (device)"),
                    size: buf_size,
                    usage: wgpu::BufferUsages::STORAGE
                        | wgpu::BufferUsages::COPY_SRC
                        | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                let staging = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("ARC GPU State (staging)"),
                    size: buf_size,
                    usage: wgpu::BufferUsages::MAP_READ
                        | wgpu::BufferUsages::MAP_WRITE
                        | wgpu::BufferUsages::COPY_SRC
                        | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: true,
                });

                // Zero-initialize staging.
                {
                    let mut view = staging.slice(..).get_mapped_range_mut();
                    view.fill(0);
                }
                staging.unmap();

                info!(
                    gpu = %gpu_info.name,
                    backend = ?gpu_info.backend,
                    memory_model = "ManagedDiscrete",
                    capacity = max_accounts,
                    buffer_mb = buf_size / (1024 * 1024),
                    "GPU state buffer allocated (staging + device-local)"
                );

                Ok(Self {
                    memory_model,
                    capacity: max_accounts,
                    gpu_buffer: Some(storage),
                    staging_buffer: Some(staging),
                    device: Some(device),
                    queue: Some(queue),
                    cpu_backing: None,
                    len: AtomicUsize::new(0),
                })
            }
            MemoryModel::CpuOnly => unreachable!(),
        }
    }

    /// Create a CPU-only fallback (no GPU).
    pub fn cpu_only(max_accounts: usize) -> Self {
        let buf_size = max_accounts * ACCOUNT_SLOT_SIZE;
        info!(
            capacity = max_accounts,
            buffer_mb = buf_size / (1024 * 1024),
            "GPU state buffer: CPU-only fallback"
        );
        Self {
            memory_model: MemoryModel::CpuOnly,
            capacity: max_accounts,
            gpu_buffer: None,
            staging_buffer: None,
            device: None,
            queue: None,
            cpu_backing: Some(vec![0u8; buf_size]),
            len: AtomicUsize::new(0),
        }
    }

    /// Which memory model this buffer is using.
    pub fn memory_model(&self) -> MemoryModel {
        self.memory_model
    }

    /// Maximum number of account slots.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of slots that have been written at least once.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Write an account into a specific slot.
    ///
    /// On Metal: writes directly to unified memory (zero copy).
    /// On discrete: writes to the staging buffer (call `sync_to_gpu()` to flush).
    /// On CPU-only: writes to the backing Vec.
    pub fn write_account(&self, slot: usize, account: &GpuAccountRepr) {
        assert!(slot < self.capacity, "slot {} out of range (capacity {})", slot, self.capacity);
        let offset = slot * ACCOUNT_SLOT_SIZE;
        let bytes = bytemuck::bytes_of(account);

        match self.memory_model {
            MemoryModel::UnifiedMetal => {
                // On Metal unified memory, write via queue.write_buffer which is
                // the standard zero-copy path for mapped buffers.
                if let (Some(queue), Some(buffer)) = (&self.queue, &self.gpu_buffer) {
                    queue.write_buffer(buffer, offset as u64, bytes);
                }
            }
            MemoryModel::ManagedDiscrete => {
                // Write to staging buffer via queue.write_buffer.
                if let (Some(queue), Some(staging)) = (&self.queue, &self.staging_buffer) {
                    queue.write_buffer(staging, offset as u64, bytes);
                }
            }
            MemoryModel::CpuOnly => {
                // Safety: we own the Vec and slot is bounds-checked.
                if let Some(backing) = &self.cpu_backing {
                    // Vec is behind a shared ref, but we guarantee non-overlapping slot writes.
                    let ptr = backing.as_ptr() as *mut u8;
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(offset), ACCOUNT_SLOT_SIZE);
                    }
                }
            }
        }

        // Track high-water mark.
        let _ = self.len.fetch_max(slot + 1, Ordering::Relaxed);
    }

    /// Read an account from a specific slot.
    ///
    /// On Metal: reads from unified memory via a staging copy (queue.write_buffer
    /// data must be flushed before it's visible to map_async).
    /// On discrete: reads from staging buffer (call `sync_from_gpu()` first if GPU modified data).
    /// On CPU-only: reads from the backing Vec.
    pub fn read_account(&self, slot: usize) -> GpuAccountRepr {
        assert!(slot < self.capacity, "slot {} out of range (capacity {})", slot, self.capacity);
        let offset = slot * ACCOUNT_SLOT_SIZE;
        let byte_len = ACCOUNT_SLOT_SIZE as u64;

        match self.memory_model {
            MemoryModel::UnifiedMetal => {
                // On Metal, queue.write_buffer stages data internally.
                // To read back, we need to copy from the main buffer to a
                // temporary read-back buffer via the command encoder, then map it.
                let device = self.device.as_ref().unwrap();
                let queue = self.queue.as_ref().unwrap();
                let src_buffer = self.gpu_buffer.as_ref().unwrap();

                // Submit any pending writes.
                queue.submit(None::<wgpu::CommandBuffer>);

                let readback = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("readback"),
                    size: byte_len,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("readback encoder"),
                });
                encoder.copy_buffer_to_buffer(src_buffer, offset as u64, &readback, 0, byte_len);
                queue.submit(Some(encoder.finish()));

                let slice = readback.slice(..);
                let (tx, rx) = std::sync::mpsc::channel();
                slice.map_async(wgpu::MapMode::Read, move |result| {
                    let _ = tx.send(result);
                });
                let _ = device.poll(wgpu::PollType::wait());

                if rx.recv().ok().and_then(|r| r.ok()).is_some() {
                    let view = slice.get_mapped_range();
                    let account: GpuAccountRepr = *bytemuck::from_bytes(&view);
                    drop(view);
                    readback.unmap();
                    account
                } else {
                    GpuAccountRepr::default()
                }
            }
            MemoryModel::ManagedDiscrete => {
                // On discrete GPUs, read from the staging buffer.
                // Caller should have called sync_from_gpu() first if GPU modified data.
                let device = self.device.as_ref().unwrap();
                let queue = self.queue.as_ref().unwrap();
                let staging = self.staging_buffer.as_ref().unwrap();

                // Flush any pending queue.write_buffer calls.
                queue.submit(None::<wgpu::CommandBuffer>);

                let readback = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("readback"),
                    size: byte_len,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("readback encoder"),
                });
                encoder.copy_buffer_to_buffer(staging, offset as u64, &readback, 0, byte_len);
                queue.submit(Some(encoder.finish()));

                let slice = readback.slice(..);
                let (tx, rx) = std::sync::mpsc::channel();
                slice.map_async(wgpu::MapMode::Read, move |result| {
                    let _ = tx.send(result);
                });
                let _ = device.poll(wgpu::PollType::wait());

                if rx.recv().ok().and_then(|r| r.ok()).is_some() {
                    let view = slice.get_mapped_range();
                    let account: GpuAccountRepr = *bytemuck::from_bytes(&view);
                    drop(view);
                    readback.unmap();
                    account
                } else {
                    GpuAccountRepr::default()
                }
            }
            MemoryModel::CpuOnly => {
                if let Some(backing) = &self.cpu_backing {
                    let src = &backing[offset..offset + ACCOUNT_SLOT_SIZE];
                    *bytemuck::from_bytes(src)
                } else {
                    GpuAccountRepr::default()
                }
            }
        }
    }

    /// Batch write multiple accounts into consecutive slots starting at `start_slot`.
    pub fn write_batch(&self, start_slot: usize, accounts: &[GpuAccountRepr]) {
        let end = start_slot + accounts.len();
        assert!(end <= self.capacity, "batch write exceeds capacity");

        let bytes = bytemuck::cast_slice(accounts);
        let offset = start_slot * ACCOUNT_SLOT_SIZE;

        match self.memory_model {
            MemoryModel::UnifiedMetal => {
                if let (Some(queue), Some(buffer)) = (&self.queue, &self.gpu_buffer) {
                    queue.write_buffer(buffer, offset as u64, bytes);
                }
            }
            MemoryModel::ManagedDiscrete => {
                if let (Some(queue), Some(staging)) = (&self.queue, &self.staging_buffer) {
                    queue.write_buffer(staging, offset as u64, bytes);
                }
            }
            MemoryModel::CpuOnly => {
                if let Some(backing) = &self.cpu_backing {
                    let ptr = backing.as_ptr() as *mut u8;
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(offset), bytes.len());
                    }
                }
            }
        }

        let _ = self.len.fetch_max(end, Ordering::Relaxed);
    }

    /// Flush staging buffer → device-local buffer.
    ///
    /// **No-op on Metal** (unified memory).
    /// **No-op on CPU-only**.
    /// On discrete GPUs, encodes a `copy_buffer_to_buffer` and submits.
    pub fn sync_to_gpu(&self) {
        if self.memory_model != MemoryModel::ManagedDiscrete {
            return;
        }
        if let (Some(device), Some(queue), Some(staging), Some(storage)) =
            (&self.device, &self.queue, &self.staging_buffer, &self.gpu_buffer)
        {
            let buf_size = (self.capacity * ACCOUNT_SLOT_SIZE) as u64;
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("GPU state sync → device"),
            });
            encoder.copy_buffer_to_buffer(staging, 0, storage, 0, buf_size);
            queue.submit(Some(encoder.finish()));
        }
    }

    /// Flush device-local buffer → staging buffer for CPU readback.
    ///
    /// **No-op on Metal** (unified memory).
    /// **No-op on CPU-only**.
    pub fn sync_from_gpu(&self) {
        if self.memory_model != MemoryModel::ManagedDiscrete {
            return;
        }
        if let (Some(device), Some(queue), Some(storage), Some(staging)) =
            (&self.device, &self.queue, &self.gpu_buffer, &self.staging_buffer)
        {
            let buf_size = (self.capacity * ACCOUNT_SLOT_SIZE) as u64;
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("GPU state sync → host"),
            });
            encoder.copy_buffer_to_buffer(storage, 0, staging, 0, buf_size);
            queue.submit(Some(encoder.finish()));
            let _ = device.poll(wgpu::PollType::wait());
        }
    }

    /// Return a reference to the GPU storage buffer (for binding in compute shaders).
    pub fn storage_buffer(&self) -> Option<&wgpu::Buffer> {
        self.gpu_buffer.as_ref()
    }

    /// Securely zero all buffer memory and drop GPU resources.
    ///
    /// Writes zeros over the entire mapped range to prevent sensitive account data
    /// (balances, nonces) from persisting in GPU/unified memory after shutdown.
    pub fn secure_shutdown(mut self) {
        let buf_size = self.capacity * ACCOUNT_SLOT_SIZE;
        let zeros = vec![0u8; buf_size.min(4 * 1024 * 1024)]; // 4MB chunks

        match self.memory_model {
            MemoryModel::UnifiedMetal => {
                if let (Some(queue), Some(buffer)) = (&self.queue, &self.gpu_buffer) {
                    let mut offset = 0u64;
                    while offset < buf_size as u64 {
                        let chunk = zeros.len().min((buf_size as u64 - offset) as usize);
                        queue.write_buffer(buffer, offset, &zeros[..chunk]);
                        offset += chunk as u64;
                    }
                    queue.submit(None::<wgpu::CommandBuffer>);
                }
            }
            MemoryModel::ManagedDiscrete => {
                if let (Some(queue), Some(staging)) = (&self.queue, &self.staging_buffer) {
                    let mut offset = 0u64;
                    while offset < buf_size as u64 {
                        let chunk = zeros.len().min((buf_size as u64 - offset) as usize);
                        queue.write_buffer(staging, offset, &zeros[..chunk]);
                        offset += chunk as u64;
                    }
                    self.sync_to_gpu();
                }
            }
            MemoryModel::CpuOnly => {
                if let Some(ref mut backing) = self.cpu_backing {
                    backing.fill(0);
                }
            }
        }

        info!("GPU state buffer securely zeroed and released");
        // Drop order: buffers, then device, then queue — wgpu handles cleanup.
    }
}

impl Drop for GpuAccountBuffer {
    fn drop(&mut self) {
        // Best-effort zeroing on drop (secure_shutdown is preferred).
        // We can't do async map here, but queue.write_buffer is synchronous-enough.
        if self.memory_model == MemoryModel::CpuOnly {
            if let Some(ref mut backing) = self.cpu_backing {
                backing.fill(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpu_account_repr_size() {
        assert_eq!(ACCOUNT_SLOT_SIZE, 128);
        assert_eq!(std::mem::size_of::<GpuAccountRepr>(), 128);
    }

    #[test]
    fn test_cpu_only_buffer() {
        let buf = GpuAccountBuffer::cpu_only(1000);
        assert_eq!(buf.memory_model(), MemoryModel::CpuOnly);
        assert_eq!(buf.capacity(), 1000);

        let acct = GpuAccountRepr {
            address: [42u8; 32],
            balance: 1_000_000,
            nonce: 7,
            code_hash: [0u8; 32],
            storage_root: [0u8; 32],
            staked_balance: 500,
            _padding: [0u8; 8],
        };

        buf.write_account(0, &acct);
        buf.write_account(999, &acct);

        let read_0 = buf.read_account(0);
        let read_999 = buf.read_account(999);

        assert_eq!(read_0.address, [42u8; 32]);
        assert_eq!(read_0.balance, 1_000_000);
        assert_eq!(read_0.nonce, 7);
        assert_eq!(read_0.staked_balance, 500);
        assert_eq!(read_999, read_0);
    }

    #[test]
    fn test_batch_write_cpu() {
        let buf = GpuAccountBuffer::cpu_only(100);
        let batch: Vec<GpuAccountRepr> = (0..10u8)
            .map(|i| {
                let mut acct = GpuAccountRepr::default();
                acct.address[0] = i;
                acct.balance = i as u64 * 100;
                acct
            })
            .collect();

        buf.write_batch(5, &batch);

        for i in 0..10u8 {
            let read = buf.read_account(5 + i as usize);
            assert_eq!(read.address[0], i);
            assert_eq!(read.balance, i as u64 * 100);
        }
    }

    #[test]
    fn test_gpu_buffer_allocation() {
        // This test will use real GPU if available, CPU fallback otherwise.
        let buf = GpuAccountBuffer::new(10_000).expect("should not fail");
        println!("Memory model: {:?}", buf.memory_model());
        assert!(buf.capacity() == 10_000);

        let acct = GpuAccountRepr {
            address: [1u8; 32],
            balance: 42,
            nonce: 1,
            code_hash: [0u8; 32],
            storage_root: [0u8; 32],
            staked_balance: 0,
            _padding: [0u8; 8],
        };

        buf.write_account(0, &acct);
        buf.sync_to_gpu(); // no-op on Metal/CPU

        let read = buf.read_account(0);
        assert_eq!(read.balance, 42);
        assert_eq!(read.nonce, 1);
        assert_eq!(read.address, [1u8; 32]);
    }

    #[test]
    fn test_secure_shutdown_cpu() {
        let buf = GpuAccountBuffer::cpu_only(100);
        let acct = GpuAccountRepr {
            address: [0xFF; 32],
            balance: u64::MAX,
            ..GpuAccountRepr::default()
        };
        buf.write_account(0, &acct);
        buf.secure_shutdown();
        // Buffer is consumed — no way to read after shutdown.
    }
}
