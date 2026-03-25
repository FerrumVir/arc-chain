//! GPU-accelerated Ed25519 batch signature verification via Apple Metal.
//!
//! Ed25519 verification is the #1 CPU bottleneck (79.6% of wall time in
//! benchmarks).  Metal GPU can parallelize the scalar multiplication step
//! (`R + H(R,A,M)*A`) across thousands of cores.
//!
//! This module provides:
//! - Metal GPU availability detection (Apple Silicon)
//! - Automatic GPU/CPU path selection based on batch size
//! - Per-signature validity results (not all-or-nothing)
//! - Throughput statistics for benchmarking
//!
//! # Architecture
//!
//! ```text
//!  batch_verify(tasks)
//!       |
//!       +-- len >= min_batch_for_gpu? ──yes──> batch_verify_gpu()
//!       |                                         |
//!       +-- no ──> batch_verify_cpu()        [wgpu compute shader]
//!                       |                    [Ed25519 WGSL verification]
//!                  [sequential ed25519_dalek] [CPU fallback on GPU error]
//! ```
//!
//! # Usage
//!
//! ```rust,no_run
//! use arc_gpu::metal_verify::{MetalVerifier, VerifyTask};
//!
//! let mut verifier = MetalVerifier::new();
//! println!("GPU available: {}", verifier.is_gpu_available());
//!
//! let tasks: Vec<VerifyTask> = vec![]; // populated from transactions
//! let result = verifier.batch_verify(&tasks);
//! println!("{} of {} valid, took {}us", result.valid, result.total, result.elapsed_us);
//! ```
//
// Add to lib.rs: pub mod metal_verify;

use curve25519_dalek::scalar::Scalar;
use dashmap::DashMap;
use ed25519_dalek::VerifyingKey;
use rayon::prelude::*;
use sha2::{Sha512, Digest};
use parking_lot::Mutex;
use std::borrow::Cow;
use std::sync::OnceLock;
use std::time::Instant;
use wgpu::util::DeviceExt;

// ── Types ────────────────────────────────────────────────────────────────────

/// Result of a batch GPU verification.
#[derive(Debug)]
pub struct GpuVerifyResult {
    /// Total number of signatures in the batch.
    pub total: usize,
    /// Number of valid signatures.
    pub valid: usize,
    /// Indices of signatures that failed verification.
    pub invalid_indices: Vec<usize>,
    /// Wall-clock time for the verification in microseconds.
    pub elapsed_us: u64,
    /// Whether the GPU path was used (vs CPU fallback).
    pub used_gpu: bool,
}

/// A single signature verification task.
#[derive(Clone)]
pub struct VerifyTask {
    /// The message bytes that were signed.
    pub message: Vec<u8>,
    /// The 32-byte Ed25519 public key (compressed Edwards point).
    pub public_key: [u8; 32],
    /// The 64-byte Ed25519 signature (R || S).
    pub signature: [u8; 64],
}

/// GPU verification engine with automatic Metal/CPU path selection.
///
/// Maintains running performance statistics to enable throughput comparison
/// between GPU and CPU paths.
pub struct MetalVerifier {
    /// Whether a Metal-capable GPU was detected.
    gpu_available: bool,
    /// Minimum batch size to dispatch to GPU. Below this threshold CPU is
    /// faster due to GPU dispatch overhead (PCIe/shared-memory copy + shader
    /// launch latency). Default: 1024.
    min_batch_for_gpu: usize,
    /// Maximum batch size per GPU dispatch. Prevents exhausting GPU memory on
    /// extremely large batches. Default: 1_000_000.
    max_batch_size: usize,
    /// Accumulated performance statistics.
    stats: VerifyStats,
}

/// Accumulated performance statistics for GPU and CPU verification paths.
#[derive(Debug, Default, Clone)]
pub struct VerifyStats {
    /// Total number of individual signatures verified (both paths).
    pub total_verified: u64,
    /// Number of signatures verified via GPU path.
    pub gpu_verified: u64,
    /// Number of signatures verified via CPU path.
    pub cpu_verified: u64,
    /// Number of GPU batch dispatches.
    pub gpu_batches: u64,
    /// Number of CPU batch dispatches.
    pub cpu_batches: u64,
    /// Running average GPU throughput (signatures/second).
    pub avg_gpu_throughput: f64,
    /// Running average CPU throughput (signatures/second).
    pub avg_cpu_throughput: f64,
}

// ── Implementation ───────────────────────────────────────────────────────────

impl MetalVerifier {
    /// Probe for Metal GPU availability and create a new verifier.
    ///
    /// On Apple Silicon Macs (`aarch64` + `macos`), Metal is available and the
    /// GPU path will be used for large batches. On all other platforms, the
    /// verifier falls back to CPU-only mode.
    pub fn new() -> Self {
        let gpu_available = Self::detect_metal_gpu();
        MetalVerifier {
            gpu_available,
            min_batch_for_gpu: 1024,
            max_batch_size: 1_000_000,
            stats: VerifyStats::default(),
        }
    }

    /// Returns `true` if a Metal-capable GPU was detected.
    pub fn is_gpu_available(&self) -> bool {
        self.gpu_available
    }

    /// Batch verify signatures, automatically selecting GPU or CPU path.
    ///
    /// - If `tasks.len() >= min_batch_for_gpu` and GPU is available, uses GPU.
    /// - Otherwise falls back to sequential CPU verification.
    ///
    /// Returns per-signature validity with indices of any invalid signatures.
    pub fn batch_verify(&mut self, tasks: &[VerifyTask]) -> GpuVerifyResult {
        if tasks.is_empty() {
            return GpuVerifyResult {
                total: 0,
                valid: 0,
                invalid_indices: vec![],
                elapsed_us: 0,
                used_gpu: false,
            };
        }

        if self.gpu_available && tasks.len() >= self.min_batch_for_gpu {
            // GPU path — unwrap is safe because we checked gpu_available
            self.batch_verify_gpu(tasks)
                .unwrap_or_else(|_| self.batch_verify_cpu(tasks))
        } else {
            self.batch_verify_cpu(tasks)
        }
    }

    /// Force CPU-only verification (useful for benchmarking comparison).
    ///
    /// Verifies each signature sequentially using `ed25519_dalek::verify_strict`.
    pub fn batch_verify_cpu(&mut self, tasks: &[VerifyTask]) -> GpuVerifyResult {
        let start = Instant::now();

        let results: Vec<bool> = tasks
            .iter()
            .map(|task| verify_single(task))
            .collect();

        let elapsed_us = start.elapsed().as_micros() as u64;
        let (valid, invalid_indices) = tally_results(&results);

        // Update stats
        let count = tasks.len() as u64;
        self.stats.total_verified += count;
        self.stats.cpu_verified += count;
        self.stats.cpu_batches += 1;
        if elapsed_us > 0 {
            let throughput = (count as f64) / (elapsed_us as f64 / 1_000_000.0);
            // Exponential moving average (alpha = 0.3)
            self.stats.avg_cpu_throughput = if self.stats.cpu_batches == 1 {
                throughput
            } else {
                self.stats.avg_cpu_throughput * 0.7 + throughput * 0.3
            };
        }

        GpuVerifyResult {
            total: tasks.len(),
            valid,
            invalid_indices,
            elapsed_us,
            used_gpu: false,
        }
    }

    /// Force GPU path for verification via wgpu compute shader.
    ///
    /// Returns `Err` if GPU is not available. For batches larger than
    /// `max_batch_size`, processes in chunks.
    ///
    /// # Implementation
    ///
    /// 1. CPU pre-computes k = SHA-512(R || A || M) mod l for each signature
    /// 2. GPU WGSL shader decompresses points, performs scalar multiplications,
    ///    and checks [S]B == R + [k]A in parallel across all GPU cores
    /// 3. Falls back to rayon CPU parallelism if GPU dispatch fails
    pub fn batch_verify_gpu(&mut self, tasks: &[VerifyTask]) -> Result<GpuVerifyResult, String> {
        if !self.gpu_available {
            return Err("Metal GPU not available on this platform".to_string());
        }

        let start = Instant::now();

        // Process in chunks if batch exceeds GPU memory limit
        let results: Vec<bool> = if tasks.len() <= self.max_batch_size {
            gpu_parallel_verify(tasks)
        } else {
            tasks
                .chunks(self.max_batch_size)
                .flat_map(|chunk| gpu_parallel_verify(chunk))
                .collect()
        };

        let elapsed_us = start.elapsed().as_micros() as u64;
        let (valid, invalid_indices) = tally_results(&results);

        // Update stats
        let count = tasks.len() as u64;
        self.stats.total_verified += count;
        self.stats.gpu_verified += count;
        self.stats.gpu_batches += 1;
        if elapsed_us > 0 {
            let throughput = (count as f64) / (elapsed_us as f64 / 1_000_000.0);
            self.stats.avg_gpu_throughput = if self.stats.gpu_batches == 1 {
                throughput
            } else {
                self.stats.avg_gpu_throughput * 0.7 + throughput * 0.3
            };
        }

        Ok(GpuVerifyResult {
            total: tasks.len(),
            valid,
            invalid_indices,
            elapsed_us,
            used_gpu: true,
        })
    }

    /// Submit GPU verification asynchronously and return a future.
    ///
    /// CPU precomputes SHA-512 scalars, then submits GPU work and returns
    /// immediately. The caller can do other work (e.g., execute previous
    /// block's transactions) while the GPU computes. Call `future.wait()`
    /// to collect results.
    ///
    /// This takes `&self` (not `&mut self`) because stats are not updated
    /// until `wait()` returns. The caller should track stats externally.
    pub fn batch_verify_gpu_async(&self, tasks: &[VerifyTask]) -> Result<GpuVerifyFuture, String> {
        if !self.gpu_available {
            return Err("Metal GPU not available on this platform".to_string());
        }

        // CPU precompute SHA-512 scalars (fast: ~4ms for 100K)
        let packed: Vec<[u8; 128]> = tasks
            .par_iter()
            .map(|task| {
                let k = compute_k_scalar(&task.signature, &task.public_key, &task.message);
                let mut buf = [0u8; 128];
                buf[0..32].copy_from_slice(&task.signature[0..32]);
                buf[32..64].copy_from_slice(&task.signature[32..64]);
                buf[64..96].copy_from_slice(&k);
                buf[96..128].copy_from_slice(&task.public_key);
                buf
            })
            .collect();

        dispatch_ed25519_verify_async(&packed)
    }

    /// Get accumulated performance statistics.
    pub fn stats(&self) -> &VerifyStats {
        &self.stats
    }

    /// Reset all performance statistics to zero.
    pub fn reset_stats(&mut self) {
        self.stats = VerifyStats::default();
    }

    /// Set the minimum batch size for GPU dispatch.
    ///
    /// Batches smaller than this threshold will always use the CPU path,
    /// since GPU dispatch overhead dominates for small N.
    pub fn set_min_batch(&mut self, min: usize) {
        self.min_batch_for_gpu = min;
    }

    /// Construct a `Vec<VerifyTask>` from parallel slices of raw data.
    ///
    /// All three slices must have the same length, otherwise this panics.
    pub fn prepare_batch(
        messages: &[&[u8]],
        public_keys: &[[u8; 32]],
        signatures: &[[u8; 64]],
    ) -> Vec<VerifyTask> {
        assert_eq!(
            messages.len(),
            public_keys.len(),
            "messages and public_keys must have the same length"
        );
        assert_eq!(
            messages.len(),
            signatures.len(),
            "messages and signatures must have the same length"
        );

        messages
            .iter()
            .zip(public_keys.iter())
            .zip(signatures.iter())
            .map(|((msg, pk), sig)| VerifyTask {
                message: msg.to_vec(),
                public_key: *pk,
                signature: *sig,
            })
            .collect()
    }

    // ── Private helpers ──────────────────────────────────────────────────

    /// Detect Metal GPU availability at runtime.
    ///
    /// Metal is available on macOS with Apple Silicon (aarch64).
    /// Intel Macs also have Metal but with significantly less compute
    /// throughput — we target Apple Silicon only for now.
    fn detect_metal_gpu() -> bool {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            true
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            false
        }
    }
}

impl Default for MetalVerifier {
    fn default() -> Self {
        Self::new()
    }
}

// ── Free functions ───────────────────────────────────────────────────────────

/// Verify a single Ed25519 signature using `ed25519_dalek::verify_strict`.
///
/// Returns `true` if the signature is valid, `false` otherwise.
/// This never panics — invalid keys or signatures simply return `false`.
fn verify_single(task: &VerifyTask) -> bool {
    let vk = match VerifyingKey::from_bytes(&task.public_key) {
        Ok(vk) => vk,
        Err(_) => return false,
    };
    let sig = ed25519_dalek::Signature::from_bytes(&task.signature);
    vk.verify_strict(&task.message, &sig).is_ok()
}

/// Pre-compute the Ed25519 verification scalar k = SHA-512(R || A || M) mod l.
///
/// This is the CPU-side precomputation for the hybrid CPU+GPU approach.
/// SHA-512 requires 64-bit arithmetic which WGSL doesn't natively support,
/// so we compute k on CPU and send the reduced 32-byte scalar to GPU.
fn compute_k_scalar(signature: &[u8; 64], public_key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut hasher = Sha512::new();
    hasher.update(&signature[0..32]); // R
    hasher.update(public_key);         // A
    hasher.update(message);            // M
    let hash: [u8; 64] = hasher.finalize().into();
    Scalar::from_bytes_mod_order_wide(&hash).to_bytes()
}

// ── Cached GPU Context ──────────────────────────────────────────────────────

/// Pre-allocated GPU buffer pool for zero-alloc dispatch.
/// Avoids creating 4 buffers + 1 bind group per dispatch (the #1 bottleneck).
struct BufferPool {
    input_buffer: wgpu::Buffer,
    output_buffer: wgpu::Buffer,
    staging_buffer: wgpu::Buffer,
    params_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// Maximum number of signatures this pool can handle.
    capacity: usize,
}

const DEFAULT_POOL_CAPACITY: usize = 65_536;

fn create_buffer_pool(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    base_table_buffer: &wgpu::Buffer,
    capacity: usize,
) -> BufferPool {
    let input_size = (capacity * 128) as u64; // 32 u32s × 4 bytes per sig
    let output_size = (capacity * 4) as u64;  // 1 u32 per sig

    let input_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Ed25519 Input Pool"),
        size: input_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let output_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Ed25519 Output Pool"),
        size: output_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    let staging_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Ed25519 Staging Pool"),
        size: output_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Ed25519 Params Pool"),
        size: 4,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("Ed25519 Pool Bind Group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: input_buffer.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: output_buffer.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: params_buffer.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: base_table_buffer.as_entire_binding() },
        ],
    });

    BufferPool { input_buffer, output_buffer, staging_buffer, params_buffer, bind_group, capacity }
}

/// Cached wgpu device, queue, pipeline, and bind group layout.
/// Initialized once on first use, reused across all dispatch calls.
struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    /// Native MSL pipeline (Metal only, uses hardware u64 arithmetic).
    /// Preferred over WGSL pipeline when available.
    msl_pipeline: Option<wgpu::ComputePipeline>,
    bind_group_layout: wgpu::BindGroupLayout,
    base_table_buffer: wgpu::Buffer,
    /// Pre-allocated buffer pool — eliminates per-dispatch Metal allocations.
    pool: Mutex<BufferPool>,
}

/// Precomputed base point table: 16 entries × 30 u32s each (x[10], y[10], t[10]).
/// B_TABLE[i] = i * B in affine extended coordinates (z=1).
/// Generated and verified against curve25519-dalek in `generate_base_point_table` test.
fn base_point_table_data() -> [u32; 480] {
    [
        // 0*B (identity)
        0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000,
        0x0000001, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000,
        0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000, 0x0000000,
        // 1*B
        0x325D51A, 0x18B5823, 0x0F6592A, 0x104A92D, 0x1A4B31D, 0x1D6DC5C, 0x27118FE, 0x07FD814, 0x13CD6E5, 0x085A4DB,
        0x2666658, 0x1999999, 0x0CCCCCC, 0x1333333, 0x1999999, 0x0666666, 0x3333333, 0x0CCCCCC, 0x2666666, 0x1999999,
        0x1B7DDA3, 0x1A2ACE9, 0x25EADBB, 0x003BA8A, 0x083C27E, 0x0ABE37D, 0x1274732, 0x0CCACDD, 0x0FD78B7, 0x19E1D7C,
        // 2*B
        0x043CE0E, 0x168538A, 0x08BF078, 0x028AEBD, 0x0203639, 0x033E7AC, 0x21DBE8C, 0x08D87A0, 0x0C9F5A0, 0x0DAACE1,
        0x2F8A3C9, 0x1D1AB9A, 0x22AC1CB, 0x08B21C2, 0x25CE43D, 0x1A21F56, 0x12F7464, 0x13843B4, 0x3309232, 0x0898337,
        0x169B401, 0x08FD55B, 0x08056E3, 0x04E0FB9, 0x175E6B3, 0x103B413, 0x2AF8439, 0x11B83BC, 0x050B2F6, 0x092629E,
        // 3*B
        0x3F8E25C, 0x09217F4, 0x110D58C, 0x0CC0B12, 0x18D0E60, 0x0DAC83A, 0x2573A1F, 0x1E923FE, 0x0A22928, 0x19EBA71,
        0x0F5B4D4, 0x0DA121E, 0x0608058, 0x0BB3920, 0x27C5BB0, 0x0269EF7, 0x350C730, 0x1357424, 0x1177EE6, 0x0499EC7,
        0x0B3A41A, 0x0423E9E, 0x38959BF, 0x05FD8B7, 0x1709CD6, 0x1B91527, 0x39BC1D6, 0x0A21D7D, 0x1CB1DD9, 0x0A93409,
        // 4*B
        0x0C9F870, 0x0A995F1, 0x2A8E927, 0x00C9E70, 0x069CE7B, 0x03520F9, 0x2EA5C3D, 0x028D064, 0x1B56CFF, 0x080F6A3,
        0x232112F, 0x02AD872, 0x1FE1BE7, 0x1975178, 0x133C8A0, 0x0D5716C, 0x35C42C0, 0x0BC28E1, 0x27CB159, 0x11F43A0,
        0x28A1358, 0x1C8BD9C, 0x0F94BF1, 0x0F5B6E8, 0x1C6578C, 0x07C2960, 0x25D3563, 0x0C18E42, 0x18D8732, 0x089E0F3,
        // 5*B
        0x22EF233, 0x027300C, 0x034B228, 0x1C9F0DF, 0x170A067, 0x12DA5DE, 0x3BE7BE8, 0x10F7F9D, 0x3EADE35, 0x127F69C,
        0x276C8ED, 0x087E0F5, 0x16BA21A, 0x0544A18, 0x0C4A0BB, 0x1924666, 0x2370A44, 0x1CDFC05, 0x3298FEA, 0x17D2096,
        0x3E801D0, 0x0542F3C, 0x0D7EC83, 0x0280049, 0x2BEE53A, 0x136C7F9, 0x0752843, 0x1A9862C, 0x2C9C593, 0x1D17158,
        // 6*B
        0x1CBF23D, 0x09D069F, 0x084EF07, 0x01363DA, 0x2879666, 0x10A29BE, 0x356606E, 0x0038C55, 0x3A7A456, 0x1325E5E,
        0x1497EF4, 0x09EB43E, 0x16C183A, 0x034A26B, 0x3E505F0, 0x14F7D77, 0x384D3FE, 0x11423B6, 0x3C2886D, 0x015378F,
        0x04EF1D2, 0x1B5B22D, 0x11378F9, 0x0A3E3EE, 0x02EBE48, 0x1E7DCC4, 0x2AD4CA7, 0x009F050, 0x1109AA6, 0x0F0CBBF,
        // 7*B
        0x10E4107, 0x16606BD, 0x1D2AB0A, 0x19DDF8E, 0x20FA027, 0x11D8107, 0x1F70CA5, 0x1A9DD3C, 0x05FCF4B, 0x0515A1A,
        0x34062B8, 0x1312D67, 0x247A258, 0x037BD5F, 0x3C220AD, 0x136AD41, 0x332346E, 0x0A5F0F9, 0x232B47D, 0x0C7158F,
        0x187ED1B, 0x1515595, 0x09C8217, 0x150F4D5, 0x14A518C, 0x1D5BAB4, 0x016E2C4, 0x1C3737D, 0x311D165, 0x04679DE,
        // 8*B
        0x0A584C8, 0x1FF6F02, 0x1732770, 0x1DC034C, 0x3ACEB19, 0x04ECF93, 0x316AE7C, 0x036C850, 0x1F97D77, 0x19D0B85,
        0x037B9B4, 0x1D6EA7F, 0x09263C5, 0x1E310F7, 0x205E0F3, 0x08AF38F, 0x2B784B3, 0x06F2DD5, 0x00C9E57, 0x0874C18,
        0x01A51BF, 0x1BD06B5, 0x17EADF7, 0x169A261, 0x117C339, 0x0B3D993, 0x0614D9C, 0x15C22C1, 0x2CEDF7E, 0x0B13D67,
        // 9*B
        0x185715C, 0x008C194, 0x0529A7C, 0x0E17270, 0x21B6039, 0x19422B8, 0x19B7037, 0x02CA37E, 0x30C8007, 0x0D5F325,
        0x122F1C0, 0x1912115, 0x08618E9, 0x0991B72, 0x07DE240, 0x0F2D2FD, 0x37E74AB, 0x1BE9657, 0x02C2DD0, 0x1FCF48F,
        0x0B2F465, 0x0CE1BE2, 0x24FCB87, 0x107BB81, 0x05ECF52, 0x147CD74, 0x33B56FB, 0x17F3B6C, 0x08EA87C, 0x171C3F1,
        // 10*B
        0x077F94F, 0x147C892, 0x3028892, 0x076C1B7, 0x081FA39, 0x0BC8677, 0x05B0769, 0x1AEA88E, 0x3E30CA6, 0x180B1E5,
        0x2E87B2C, 0x01D2C1A, 0x1087751, 0x029EF07, 0x095DA9A, 0x098BFCF, 0x226AA64, 0x08EF91C, 0x2A7A1B2, 0x18DFFF2,
        0x35BC63E, 0x1D32540, 0x0A4A87C, 0x1A59DC9, 0x1D3D6A7, 0x1B4577F, 0x3E88184, 0x0525117, 0x3266735, 0x0DB817C,
        // 11*B
        0x07CF3CB, 0x1F4B048, 0x2AA5FE5, 0x1962C9E, 0x34E0696, 0x0712438, 0x383C6EB, 0x082F6D9, 0x31154BE, 0x05394A2,
        0x2033713, 0x1CB70DA, 0x31A8611, 0x0E1E4E2, 0x0496164, 0x0FF0FCE, 0x3BE71A0, 0x172EB4D, 0x313F21A, 0x0B64208,
        0x35BEED4, 0x173CBB6, 0x106FD70, 0x0EF0C7E, 0x1640007, 0x016ADBF, 0x1535F79, 0x1144739, 0x25800F2, 0x16B9A95,
        // 12*B
        0x0F0902D, 0x0BF999E, 0x36855CC, 0x11C2E09, 0x0CA56FC, 0x0A249DB, 0x3B87006, 0x1A6ABD9, 0x3E016E5, 0x11C6785,
        0x22DE4F9, 0x0A0770B, 0x2CCE67A, 0x1A2939C, 0x15921FA, 0x17825B8, 0x2B62731, 0x0045CE3, 0x208BCE8, 0x101C339,
        0x24B1CCB, 0x130C4A3, 0x2757ABD, 0x02462D8, 0x03C7BC3, 0x1CD006F, 0x3E9D418, 0x114240C, 0x203A485, 0x0ACD138,
        // 13*B
        0x3C05FED, 0x0381CED, 0x2F706F0, 0x1446915, 0x2210F8F, 0x02D304F, 0x1D6F814, 0x0D99B66, 0x20D5F36, 0x041D09F,
        0x2401F80, 0x1F86BBA, 0x0E470FD, 0x19144D1, 0x0DD033E, 0x0DA89B8, 0x3301169, 0x16E8F08, 0x0DED538, 0x04B6EC0,
        0x0DE2F53, 0x137337A, 0x1DF8A45, 0x08F5974, 0x16C52A9, 0x1562338, 0x06EB0E6, 0x08C18B8, 0x3917BE6, 0x104A01A,
        // 14*B
        0x18515B9, 0x19CD4ED, 0x0655471, 0x0C1F1CC, 0x3637B9B, 0x0CABB15, 0x23D44AE, 0x155E091, 0x02F5884, 0x0817CED,
        0x19C2839, 0x07F6622, 0x0CB906D, 0x140CDB1, 0x29D2263, 0x1B702BF, 0x168BD65, 0x17022B7, 0x2F50C4C, 0x1394095,
        0x271C2E0, 0x0CF4B28, 0x0BC1122, 0x1FF3322, 0x1C79D0D, 0x0BFE479, 0x3055F9E, 0x1C1F1D4, 0x1463A87, 0x1990CB4,
        // 15*B
        0x2A18DC1, 0x05FCF99, 0x139720C, 0x1A3B80C, 0x3658C4D, 0x120419E, 0x1637CAE, 0x086BB8B, 0x2AEC2EC, 0x13C58B7,
        0x12E5CDF, 0x153312B, 0x354328D, 0x0C50CD4, 0x2B396BC, 0x0D29331, 0x17B80B2, 0x05EAA2C, 0x2D04FF2, 0x04B2FEC,
        0x092BF29, 0x15FCCD7, 0x0BEA281, 0x07F2057, 0x3E98EDA, 0x1760DE2, 0x27E9120, 0x0923564, 0x0E36B77, 0x178CFC0,
    ]
}

/// Global cached GPU context — avoids recreating Instance/Adapter/Device/Pipeline per call.
static GPU_CONTEXT: OnceLock<Option<GpuContext>> = OnceLock::new();

fn get_or_init_gpu() -> Option<&'static GpuContext> {
    GPU_CONTEXT.get_or_init(|| {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        })).ok()?;

        let gpu_info = adapter.get_info();
        let has_msl = adapter.features().contains(wgpu::Features::MSL_SHADER_PASSTHROUGH);
        tracing::info!(
            gpu_name = %gpu_info.name,
            gpu_backend = ?gpu_info.backend,
            msl_passthrough = has_msl,
            "GPU Ed25519 context initialized"
        );

        // Request MSL_SHADER_PASSTHROUGH if available (enables native Metal shader)
        let required_features = if has_msl {
            wgpu::Features::MSL_SHADER_PASSTHROUGH
        } else {
            wgpu::Features::empty()
        };

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("ARC Ed25519 Verifier"),
                required_features,
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            },
        ))
        .ok()?;

        // WGSL pipeline (always available, fallback)
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Ed25519 Verify WGSL"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("ed25519_verify.wgsl"))),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Ed25519 BGL"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Ed25519 Pipeline Layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("Ed25519 WGSL Pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        // Native MSL pipeline (Metal only — uses hardware u64 arithmetic)
        let msl_pipeline = if has_msl {
            let msl_source = include_str!("ed25519_verify.metal");
            let msl_shader = unsafe {
                device.create_shader_module_passthrough(
                    wgpu::ShaderModuleDescriptorPassthrough::Msl(
                        wgpu::ShaderModuleDescriptorMsl {
                            entry_point: "ed25519_verify_main".to_string(),
                            label: Some("Ed25519 Verify MSL"),
                            num_workgroups: (64, 1, 1), // workgroup size, not dispatch count
                            source: Cow::Borrowed(msl_source),
                        },
                    ),
                )
            };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("Ed25519 MSL Pipeline"),
                    layout: Some(&pipeline_layout),
                    module: &msl_shader,
                    entry_point: Some("ed25519_verify_main"),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })) {
                Ok(p) => {
                    tracing::info!("Native MSL Ed25519 pipeline created (hardware u64)");
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!("MSL pipeline creation failed, using WGSL fallback: {:?}", e);
                    None
                }
            }
        } else {
            None
        };

        let table_data = base_point_table_data();
        let base_table_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Ed25519 Base Table"),
            contents: bytemuck::cast_slice(&table_data),
            usage: wgpu::BufferUsages::STORAGE,
        });

        let pool = create_buffer_pool(&device, &bind_group_layout, &base_table_buffer, DEFAULT_POOL_CAPACITY);
        tracing::info!(capacity = DEFAULT_POOL_CAPACITY, "Buffer pool pre-allocated");

        Some(GpuContext {
            device, queue, pipeline, msl_pipeline, bind_group_layout, base_table_buffer,
            pool: Mutex::new(pool),
        })
    }).as_ref()
}

/// Per-phase timing breakdown from a GPU dispatch.
#[derive(Debug, Default)]
struct DispatchTiming {
    /// Time to pack u32s and write to GPU buffers.
    write_us: u64,
    /// Time for GPU compute (encode + submit + poll).
    compute_us: u64,
    /// Time to map staging buffer and read results.
    readback_us: u64,
}

/// Dispatch Ed25519 verification to the GPU via cached wgpu compute pipeline.
///
/// Each entry in `packed` is 128 bytes:
///   [0..32]   R (compressed signature point)
///   [32..64]  S (signature scalar)
///   [64..96]  k (pre-computed SHA-512 scalar, reduced mod l)
///   [96..128] A (compressed public key)
///
/// Returns one u32 per signature: 1=valid, 0=invalid.
/// Uses pre-allocated buffer pool for zero-alloc dispatch.
fn dispatch_ed25519_verify(packed: &[[u8; 128]]) -> Result<Vec<u32>, String> {
    dispatch_ed25519_verify_timed(packed, None)
}

fn dispatch_ed25519_verify_timed(
    packed: &[[u8; 128]],
    timing: Option<&mut DispatchTiming>,
) -> Result<Vec<u32>, String> {
    let n = packed.len();
    if n == 0 {
        return Ok(vec![]);
    }

    let ctx = get_or_init_gpu()
        .ok_or_else(|| "GPU context initialization failed".to_string())?;

    // Phase A: Pack + write to GPU buffers
    let t_write = Instant::now();

    let mut input_u32s: Vec<u32> = Vec::with_capacity(n * 32);
    for p in packed {
        for chunk in p.chunks_exact(4) {
            input_u32s.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
    }

    let input_bytes: &[u8] = bytemuck::cast_slice(&input_u32s);
    let output_size = (n * 4) as u64;

    // Lock buffer pool (held through dispatch — GPU serializes anyway)
    let mut pool = ctx.pool.lock();

    // Grow pool if batch exceeds capacity
    if n > pool.capacity {
        let new_cap = n.next_power_of_two();
        tracing::info!(old = pool.capacity, new = new_cap, "Growing buffer pool");
        *pool = create_buffer_pool(&ctx.device, &ctx.bind_group_layout, &ctx.base_table_buffer, new_cap);
    }

    // Write data into pre-allocated buffers (zero Metal allocations)
    ctx.queue.write_buffer(&pool.input_buffer, 0, input_bytes);
    let params = [n as u32];
    ctx.queue.write_buffer(&pool.params_buffer, 0, bytemuck::cast_slice(&params));

    let write_elapsed = t_write.elapsed();

    // Phase B: GPU compute (encode + submit + poll)
    let t_compute = Instant::now();

    let workgroup_size = 64u32;
    let num_workgroups = (n as u32 + workgroup_size - 1) / workgroup_size;

    let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("Ed25519 Encoder"),
    });

    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("Ed25519 Pass"),
            timestamp_writes: None,
        });
        let active_pipeline = ctx.msl_pipeline.as_ref().unwrap_or(&ctx.pipeline);
        pass.set_pipeline(active_pipeline);
        pass.set_bind_group(0, &pool.bind_group, &[]);
        pass.dispatch_workgroups(num_workgroups, 1, 1);
    }

    encoder.copy_buffer_to_buffer(&pool.output_buffer, 0, &pool.staging_buffer, 0, output_size);
    ctx.queue.submit(Some(encoder.finish()));
    ctx.device.poll(wgpu::PollType::wait()).map_err(|e| format!("GPU poll error: {e:?}"))?;

    let compute_elapsed = t_compute.elapsed();

    // Phase C: Readback (map + copy)
    let t_readback = Instant::now();

    let buffer_slice = pool.staging_buffer.slice(..output_size);
    let (sender, receiver) = std::sync::mpsc::channel();
    buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    ctx.device.poll(wgpu::PollType::wait()).map_err(|e| format!("GPU poll error: {e:?}"))?;
    receiver
        .recv()
        .map_err(|e| format!("Channel error: {e}"))?
        .map_err(|e| format!("Map error: {e:?}"))?;

    let raw_data = buffer_slice.get_mapped_range();
    let output_u32s: &[u32] = bytemuck::cast_slice(&raw_data);
    let results: Vec<u32> = output_u32s.to_vec();

    drop(raw_data);
    pool.staging_buffer.unmap();

    let readback_elapsed = t_readback.elapsed();

    if let Some(t) = timing {
        t.write_us = write_elapsed.as_micros() as u64;
        t.compute_us = compute_elapsed.as_micros() as u64;
        t.readback_us = readback_elapsed.as_micros() as u64;
    }

    Ok(results)
}

// ── Async GPU Dispatch ──────────────────────────────────────────────────────

/// A future representing in-flight GPU signature verification.
///
/// Created by `MetalVerifier::batch_verify_gpu_async()`. The GPU compute
/// runs concurrently — call `wait()` to block until results are ready.
/// This frees CPU cores for execution while the GPU verifies signatures.
pub struct GpuVerifyFuture {
    staging_buffer: wgpu::Buffer,
    count: usize,
    start: Instant,
}

impl GpuVerifyFuture {
    /// Block until GPU results are ready and return verification results.
    pub fn wait(self) -> Result<GpuVerifyResult, String> {
        let ctx = get_or_init_gpu()
            .ok_or_else(|| "GPU context lost".to_string())?;

        let output_size = (self.count * 4) as u64;
        let buffer_slice = self.staging_buffer.slice(..output_size);
        let (sender, receiver) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        ctx.device.poll(wgpu::PollType::wait()).map_err(|e| format!("GPU poll error: {e:?}"))?;
        receiver
            .recv()
            .map_err(|e| format!("Channel error: {e}"))?
            .map_err(|e| format!("Map error: {e:?}"))?;

        let raw_data = buffer_slice.get_mapped_range();
        let output_u32s: &[u32] = bytemuck::cast_slice(&raw_data);
        let results: Vec<bool> = output_u32s.iter().map(|&v| v == 1).collect();
        drop(raw_data);
        self.staging_buffer.unmap();

        let elapsed_us = self.start.elapsed().as_micros() as u64;
        let (valid, invalid_indices) = tally_results(&results);

        Ok(GpuVerifyResult {
            total: self.count,
            valid,
            invalid_indices,
            elapsed_us,
            used_gpu: true,
        })
    }

    /// Number of signatures being verified.
    pub fn count(&self) -> usize {
        self.count
    }
}

/// Submit GPU verification asynchronously.
///
/// Writes data to pool buffers, encodes compute + copy commands, submits to
/// GPU queue, then returns immediately. The pool mutex is only held during
/// write+encode+submit, NOT during GPU compute. A per-dispatch staging buffer
/// is created so multiple async dispatches can be in-flight concurrently.
fn dispatch_ed25519_verify_async(packed: &[[u8; 128]]) -> Result<GpuVerifyFuture, String> {
    let n = packed.len();
    if n == 0 {
        return Err("Empty batch for async dispatch".to_string());
    }

    let ctx = get_or_init_gpu()
        .ok_or_else(|| "GPU context initialization failed".to_string())?;

    let start = Instant::now();

    // Pack input data
    let mut input_u32s: Vec<u32> = Vec::with_capacity(n * 32);
    for p in packed {
        for chunk in p.chunks_exact(4) {
            input_u32s.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
    }
    let input_bytes: &[u8] = bytemuck::cast_slice(&input_u32s);
    let output_size = (n * 4) as u64;

    // Per-dispatch staging buffer (small: n*4 bytes, allows concurrent futures)
    let staging_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Ed25519 Async Staging"),
        size: output_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    // Lock pool only for write + encode + submit
    {
        let mut pool = ctx.pool.lock();

        if n > pool.capacity {
            let new_cap = n.next_power_of_two();
            *pool = create_buffer_pool(&ctx.device, &ctx.bind_group_layout, &ctx.base_table_buffer, new_cap);
        }

        ctx.queue.write_buffer(&pool.input_buffer, 0, input_bytes);
        let params = [n as u32];
        ctx.queue.write_buffer(&pool.params_buffer, 0, bytemuck::cast_slice(&params));

        let workgroup_size = 64u32;
        let num_workgroups = (n as u32 + workgroup_size - 1) / workgroup_size;

        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Ed25519 Async Encoder"),
        });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("Ed25519 Async Pass"),
                timestamp_writes: None,
            });
            let active_pipeline = ctx.msl_pipeline.as_ref().unwrap_or(&ctx.pipeline);
            pass.set_pipeline(active_pipeline);
            pass.set_bind_group(0, &pool.bind_group, &[]);
            pass.dispatch_workgroups(num_workgroups, 1, 1);
        }

        // Copy from pool's output buffer to per-dispatch staging buffer
        encoder.copy_buffer_to_buffer(&pool.output_buffer, 0, &staging_buffer, 0, output_size);
        ctx.queue.submit(Some(encoder.finish()));

        // Pool unlocked here — GPU has captured all commands
    }

    Ok(GpuVerifyFuture { staging_buffer, count: n, start })
}

// ── Pre-verification Cache ──────────────────────────────────────────────────

/// Signature pre-verification cache for amortizing verification across
/// the inter-block interval.
///
/// Instead of verifying all signatures at block time (blocking the pipeline),
/// the cache allows background verification as transactions arrive in the
/// mempool. At block time, the pipeline checks the cache first and only
/// verifies uncached signatures.
///
/// Thread-safe: all methods are `&self` and can be called from any thread.
pub struct SigVerifyCache {
    /// Cache: tx_hash → signature validity.
    results: DashMap<[u8; 32], bool>,
}

impl SigVerifyCache {
    pub fn new() -> Self {
        Self { results: DashMap::new() }
    }

    /// Pre-verify a batch of transactions using GPU (or CPU fallback).
    ///
    /// Results are cached immediately and available via `lookup()`.
    /// Call this from a background thread after mempool insertion.
    pub fn pre_verify(&self, tasks: &[VerifyTask], tx_hashes: &[[u8; 32]]) {
        if tasks.is_empty() { return; }
        assert_eq!(tasks.len(), tx_hashes.len());

        let results = gpu_parallel_verify(tasks);

        for (i, valid) in results.into_iter().enumerate() {
            self.results.insert(tx_hashes[i], valid);
        }
    }

    /// Pre-verify a batch asynchronously via GPU.
    ///
    /// Returns a `GpuVerifyFuture` that must be waited on to cache results.
    /// Use `cache_from_future()` to store results after `wait()`.
    pub fn pre_verify_async(
        &self,
        tasks: &[VerifyTask],
    ) -> Result<GpuVerifyFuture, String> {
        let verifier = MetalVerifier::new();
        verifier.batch_verify_gpu_async(tasks)
    }

    /// Store results from a completed `GpuVerifyFuture` into the cache.
    pub fn cache_from_result(
        &self,
        result: &GpuVerifyResult,
        tx_hashes: &[[u8; 32]],
    ) {
        assert_eq!(result.total, tx_hashes.len());
        let invalid_set: std::collections::HashSet<usize> =
            result.invalid_indices.iter().copied().collect();
        for (i, hash) in tx_hashes.iter().enumerate() {
            self.results.insert(*hash, !invalid_set.contains(&i));
        }
    }

    /// Look up a cached verification result.
    /// Returns `Some(true)` if verified valid, `Some(false)` if invalid,
    /// `None` if not yet verified.
    pub fn lookup(&self, tx_hash: &[u8; 32]) -> Option<bool> {
        self.results.get(tx_hash).map(|v| *v)
    }

    /// Remove a cached result (after block inclusion).
    pub fn remove(&self, tx_hash: &[u8; 32]) {
        self.results.remove(tx_hash);
    }

    /// Number of cached results.
    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    /// Clear all cached results.
    pub fn clear(&self) {
        self.results.clear();
    }
}

impl Default for SigVerifyCache {
    fn default() -> Self {
        Self::new()
    }
}

/// GPU-accelerated Ed25519 signature verification via wgpu compute shader.
///
/// 1. CPU: pre-computes k = SHA-512(R || A || M) mod l for each signature (rayon parallel)
/// 2. GPU: dispatches WGSL Ed25519 verification shader (point decompression + scalar multiply)
/// 3. Falls back to CPU-parallel verification if GPU dispatch fails
fn gpu_parallel_verify(tasks: &[VerifyTask]) -> Vec<bool> {
    // Step 1: CPU pre-compute SHA-512 scalars in parallel
    let packed: Vec<[u8; 128]> = tasks
        .par_iter()
        .map(|task| {
            let k = compute_k_scalar(&task.signature, &task.public_key, &task.message);
            let mut buf = [0u8; 128];
            buf[0..32].copy_from_slice(&task.signature[0..32]);   // R
            buf[32..64].copy_from_slice(&task.signature[32..64]); // S
            buf[64..96].copy_from_slice(&k);                       // k (reduced mod l)
            buf[96..128].copy_from_slice(&task.public_key);        // A
            buf
        })
        .collect();

    // Step 2: Dispatch to GPU (convert u32 results to bool: 1=valid, anything else=invalid)
    match dispatch_ed25519_verify(&packed) {
        Ok(raw) => raw.iter().map(|&v| v == 1).collect(),
        Err(e) => {
            tracing::warn!("GPU Ed25519 dispatch failed ({}), falling back to CPU", e);
            tasks.par_iter().map(|task| verify_single(task)).collect()
        }
    }
}

/// Count valid signatures and collect indices of invalid ones.
fn tally_results(results: &[bool]) -> (usize, Vec<usize>) {
    let mut valid = 0usize;
    let mut invalid_indices = Vec::new();

    for (i, &is_valid) in results.iter().enumerate() {
        if is_valid {
            valid += 1;
        } else {
            invalid_indices.push(i);
        }
    }

    (valid, invalid_indices)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// Helper: generate `n` valid (keypair, message, signature) tuples.
    fn generate_valid_tasks(n: usize) -> Vec<VerifyTask> {
        let mut rng = rand::thread_rng();
        (0..n)
            .map(|i| {
                let sk = SigningKey::generate(&mut rng);
                let msg = format!("test message {i}").into_bytes();
                let sig = sk.sign(&msg);
                VerifyTask {
                    message: msg,
                    public_key: sk.verifying_key().to_bytes(),
                    signature: sig.to_bytes(),
                }
            })
            .collect()
    }

    /// Helper: generate a single task with an invalid signature.
    fn generate_invalid_task() -> VerifyTask {
        let mut rng = rand::thread_rng();
        let sk = SigningKey::generate(&mut rng);
        let msg = b"correct message".to_vec();
        // Sign the correct message, then swap the message to make it invalid
        let sig = sk.sign(msg.as_slice());
        VerifyTask {
            message: b"wrong message".to_vec(),
            public_key: sk.verifying_key().to_bytes(),
            signature: sig.to_bytes(),
        }
    }

    #[test]
    fn test_metal_verifier_creation() {
        let verifier = MetalVerifier::new();
        // On Apple Silicon Mac this is true, on other platforms false.
        // Either way, the verifier should construct without panicking.
        println!("Metal GPU available: {}", verifier.is_gpu_available());

        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(verifier.is_gpu_available(), "Apple Silicon should detect Metal");

        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        assert!(!verifier.is_gpu_available(), "Non-Apple-Silicon should not detect Metal");
    }

    #[test]
    fn test_batch_verify_all_valid() {
        let mut verifier = MetalVerifier::new();
        let tasks = generate_valid_tasks(100);

        let result = verifier.batch_verify(&tasks);

        assert_eq!(result.total, 100);
        assert_eq!(result.valid, 100);
        assert!(result.invalid_indices.is_empty());
        assert!(result.elapsed_us > 0 || result.total == 0);
    }

    #[test]
    fn test_batch_verify_with_invalid() {
        let mut verifier = MetalVerifier::new();
        let mut tasks = generate_valid_tasks(10);

        // Insert an invalid signature at index 3
        tasks[3] = generate_invalid_task();

        let result = verifier.batch_verify(&tasks);

        assert_eq!(result.total, 10);
        assert_eq!(result.valid, 9);
        assert_eq!(result.invalid_indices, vec![3]);
    }

    #[test]
    fn test_batch_verify_empty() {
        let mut verifier = MetalVerifier::new();
        let tasks: Vec<VerifyTask> = vec![];

        let result = verifier.batch_verify(&tasks);

        assert_eq!(result.total, 0);
        assert_eq!(result.valid, 0);
        assert!(result.invalid_indices.is_empty());
        assert_eq!(result.elapsed_us, 0);
        assert!(!result.used_gpu);
    }

    #[test]
    fn test_cpu_fallback() {
        let mut verifier = MetalVerifier::new();
        let tasks = generate_valid_tasks(50);

        // CPU path
        let cpu_result = verifier.batch_verify_cpu(&tasks);

        // Auto path (50 < 1024, so this should also use CPU)
        let auto_result = verifier.batch_verify(&tasks);

        assert_eq!(cpu_result.total, auto_result.total);
        assert_eq!(cpu_result.valid, auto_result.valid);
        assert_eq!(cpu_result.invalid_indices, auto_result.invalid_indices);
        assert!(!cpu_result.used_gpu);
        assert!(!auto_result.used_gpu);
    }

    #[test]
    fn test_prepare_batch() {
        let mut rng = rand::thread_rng();

        let sk1 = SigningKey::generate(&mut rng);
        let sk2 = SigningKey::generate(&mut rng);

        let msg1 = b"hello";
        let msg2 = b"world";

        let sig1 = sk1.sign(msg1.as_slice());
        let sig2 = sk2.sign(msg2.as_slice());

        let messages: Vec<&[u8]> = vec![msg1.as_slice(), msg2.as_slice()];
        let public_keys = [sk1.verifying_key().to_bytes(), sk2.verifying_key().to_bytes()];
        let signatures = [sig1.to_bytes(), sig2.to_bytes()];

        let tasks = MetalVerifier::prepare_batch(&messages, &public_keys, &signatures);

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].message, b"hello");
        assert_eq!(tasks[1].message, b"world");
        assert_eq!(tasks[0].public_key, sk1.verifying_key().to_bytes());
        assert_eq!(tasks[1].public_key, sk2.verifying_key().to_bytes());
        assert_eq!(tasks[0].signature, sig1.to_bytes());
        assert_eq!(tasks[1].signature, sig2.to_bytes());

        // Verify the prepared tasks actually pass verification
        let mut verifier = MetalVerifier::new();
        let result = verifier.batch_verify(&tasks);
        assert_eq!(result.valid, 2);
    }

    #[test]
    fn test_stats_tracking() {
        let mut verifier = MetalVerifier::new();
        let tasks = generate_valid_tasks(20);

        // Initial stats should be zero
        assert_eq!(verifier.stats().total_verified, 0);
        assert_eq!(verifier.stats().cpu_batches, 0);

        // Verify a batch (will use CPU since 20 < 1024)
        verifier.batch_verify(&tasks);

        assert_eq!(verifier.stats().total_verified, 20);
        assert_eq!(verifier.stats().cpu_verified, 20);
        assert_eq!(verifier.stats().cpu_batches, 1);
        assert_eq!(verifier.stats().gpu_verified, 0);
        assert!(verifier.stats().avg_cpu_throughput > 0.0);

        // Verify another batch
        verifier.batch_verify(&tasks);

        assert_eq!(verifier.stats().total_verified, 40);
        assert_eq!(verifier.stats().cpu_batches, 2);

        // Reset
        verifier.reset_stats();
        assert_eq!(verifier.stats().total_verified, 0);
        assert_eq!(verifier.stats().cpu_batches, 0);
        assert_eq!(verifier.stats().avg_cpu_throughput, 0.0);
    }

    #[test]
    fn test_large_batch() {
        let mut verifier = MetalVerifier::new();
        let mut tasks = generate_valid_tasks(10_000);

        // Sprinkle in 5 invalid signatures at known positions
        let invalid_positions = vec![42, 999, 3333, 7777, 9999];
        for &pos in &invalid_positions {
            tasks[pos] = generate_invalid_task();
        }

        let result = verifier.batch_verify(&tasks);

        assert_eq!(result.total, 10_000);
        assert_eq!(result.valid, 10_000 - invalid_positions.len());
        assert_eq!(result.invalid_indices.len(), invalid_positions.len());

        // The invalid indices should match our injected positions
        for &pos in &invalid_positions {
            assert!(
                result.invalid_indices.contains(&pos),
                "expected index {pos} in invalid_indices, got {:?}",
                result.invalid_indices
            );
        }

        // On Apple Silicon, large batch should use GPU path
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(result.used_gpu, "10K batch should use GPU on Apple Silicon");
    }

    #[test]
    fn test_min_batch_threshold() {
        let mut verifier = MetalVerifier::new();

        // Small batch (below default threshold of 1024) => CPU
        let small_tasks = generate_valid_tasks(100);
        let result = verifier.batch_verify(&small_tasks);
        assert!(!result.used_gpu, "small batch should use CPU");

        // Large batch (above threshold) => GPU if available
        let large_tasks = generate_valid_tasks(2000);
        let result = verifier.batch_verify(&large_tasks);
        if verifier.is_gpu_available() {
            assert!(result.used_gpu, "large batch should use GPU when available");
        } else {
            assert!(!result.used_gpu, "large batch uses CPU when GPU unavailable");
        }

        // Change threshold and verify behavior changes
        verifier.set_min_batch(5000);
        let result = verifier.batch_verify(&large_tasks);
        assert!(
            !result.used_gpu,
            "2000 tasks should use CPU with min_batch=5000"
        );
    }

    #[test]
    fn test_gpu_path_matches_cpu_path() {
        // Ensure both paths produce identical results
        let mut verifier = MetalVerifier::new();
        let mut tasks = generate_valid_tasks(200);
        tasks[50] = generate_invalid_task();
        tasks[150] = generate_invalid_task();

        let cpu_result = verifier.batch_verify_cpu(&tasks);

        // GPU path (if available) should produce the same results
        if verifier.is_gpu_available() {
            let gpu_result = verifier.batch_verify_gpu(&tasks).expect("GPU should succeed");
            assert_eq!(cpu_result.total, gpu_result.total);
            assert_eq!(cpu_result.valid, gpu_result.valid);
            assert_eq!(cpu_result.invalid_indices, gpu_result.invalid_indices);
        }
    }

    #[test]
    fn test_async_gpu_verify() {
        let verifier = MetalVerifier::new();
        if !verifier.is_gpu_available() {
            println!("Skipping async test — no GPU");
            return;
        }

        let mut tasks = generate_valid_tasks(2000);
        tasks[42] = generate_invalid_task();
        tasks[999] = generate_invalid_task();

        // Submit async GPU verification
        let future = verifier.batch_verify_gpu_async(&tasks)
            .expect("async dispatch should succeed");

        assert_eq!(future.count(), 2000);

        // Simulate CPU work while GPU computes
        let cpu_work_start = Instant::now();
        let _busy: u64 = (0..1_000_000u64).map(|x| x.wrapping_mul(x)).sum();
        let cpu_work_time = cpu_work_start.elapsed();

        // Collect GPU results
        let result = future.wait().expect("GPU wait should succeed");

        assert_eq!(result.total, 2000);
        assert_eq!(result.valid, 1998);
        assert!(result.invalid_indices.contains(&42));
        assert!(result.invalid_indices.contains(&999));
        assert!(result.used_gpu);

        println!("Async GPU verify: {} sigs in {}us, CPU work overlap: {}us",
            result.total, result.elapsed_us, cpu_work_time.as_micros());
    }

    #[test]
    fn test_async_matches_sync() {
        let verifier = MetalVerifier::new();
        if !verifier.is_gpu_available() {
            return;
        }

        let mut tasks = generate_valid_tasks(5000);
        for i in (0..50).map(|x| x * 100) { tasks[i] = generate_invalid_task(); }

        // Sync path
        let sync_results = gpu_parallel_verify(&tasks);

        // Async path
        let future = verifier.batch_verify_gpu_async(&tasks).unwrap();
        let async_result = future.wait().unwrap();
        let async_bools: Vec<bool> = (0..tasks.len())
            .map(|i| !async_result.invalid_indices.contains(&i))
            .collect();

        // Compare
        let mismatches: Vec<usize> = sync_results.iter().zip(async_bools.iter())
            .enumerate()
            .filter(|(_, (s, a))| s != a)
            .map(|(i, _)| i)
            .collect();

        assert!(mismatches.is_empty(), "Async and sync must match. Mismatches at: {:?}", mismatches);
    }

    #[test]
    fn test_sig_verify_cache() {
        let cache = SigVerifyCache::new();
        assert!(cache.is_empty());

        let mut tasks = generate_valid_tasks(100);
        tasks[10] = generate_invalid_task();
        tasks[50] = generate_invalid_task();

        let hashes: Vec<[u8; 32]> = tasks.iter().enumerate()
            .map(|(i, _)| {
                let mut h = [0u8; 32];
                h[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                h
            })
            .collect();

        // Pre-verify
        cache.pre_verify(&tasks, &hashes);

        assert_eq!(cache.len(), 100);

        // Check valid signatures
        assert_eq!(cache.lookup(&hashes[0]), Some(true));
        assert_eq!(cache.lookup(&hashes[99]), Some(true));

        // Check invalid signatures
        assert_eq!(cache.lookup(&hashes[10]), Some(false));
        assert_eq!(cache.lookup(&hashes[50]), Some(false));

        // Check non-existent
        let fake_hash = [0xFF; 32];
        assert_eq!(cache.lookup(&fake_hash), None);

        // Remove and verify gone
        cache.remove(&hashes[0]);
        assert_eq!(cache.lookup(&hashes[0]), None);
        assert_eq!(cache.len(), 99);

        // Clear
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn test_sig_verify_cache_async() {
        let cache = SigVerifyCache::new();
        let verifier = MetalVerifier::new();
        if !verifier.is_gpu_available() {
            println!("Skipping cache async test — no GPU");
            return;
        }

        let mut tasks = generate_valid_tasks(2000);
        tasks[100] = generate_invalid_task();

        let hashes: Vec<[u8; 32]> = tasks.iter().enumerate()
            .map(|(i, _)| {
                let mut h = [0u8; 32];
                h[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                h
            })
            .collect();

        // Async pre-verify
        let future = cache.pre_verify_async(&tasks).expect("async should work");
        let result = future.wait().expect("wait should succeed");

        // Cache results
        cache.cache_from_result(&result, &hashes);

        assert_eq!(cache.len(), 2000);
        assert_eq!(cache.lookup(&hashes[0]), Some(true));
        assert_eq!(cache.lookup(&hashes[100]), Some(false));
    }

    #[test]
    fn test_verify_single_invalid_key() {
        // A task with garbage public key bytes should return false, not panic
        let task = VerifyTask {
            message: b"hello".to_vec(),
            public_key: [0xFF; 32], // invalid Edwards point
            signature: [0xAA; 64],
        };
        assert!(!verify_single(&task));
    }

    #[test]
    fn test_default_trait() {
        // MetalVerifier should implement Default via new()
        let verifier = MetalVerifier::default();
        assert_eq!(verifier.stats().total_verified, 0);
    }

    #[test]
    fn test_gpu_diagnostic_codes() {
        // Diagnostic test: GPU arithmetic tests
        // idx 0: fe_sq(1) limb 0 — expect 1
        // idx 1: fe_sq([0,1,0,...]) limb 2 — expect 2 (odd-odd doubling test)
        // idx 2: ge_frombytes identity check — 0=identity, 99=not identity
        // idx 3: y^2 limb 0 from R decompression
        // idx 4: vx2 check result — 100=first pass, 200=sqrt(-1) pass, 300=both fail
        let tasks = generate_valid_tasks(12);

        // CPU-side: compute decompression intermediates for first R
        let r_bytes = &tasks[0].signature[0..32];

        // Manual 10-limb conversion (same as shader)
        let mut y_u32s = [0u32; 8];
        for i in 0..8 {
            y_u32s[i] = u32::from_le_bytes([
                r_bytes[i*4], r_bytes[i*4+1], r_bytes[i*4+2], r_bytes[i*4+3]
            ]);
        }
        y_u32s[7] &= 0x7FFFFFFF;

        let mut y = [0u32; 10];
        y[0] = y_u32s[0] & 0x3FFFFFF;
        y[1] = ((y_u32s[0] >> 26) | (y_u32s[1] << 6)) & 0x1FFFFFF;
        y[2] = ((y_u32s[1] >> 19) | (y_u32s[2] << 13)) & 0x3FFFFFF;
        y[3] = ((y_u32s[2] >> 13) | (y_u32s[3] << 19)) & 0x1FFFFFF;
        y[4] = (y_u32s[3] >> 6) & 0x3FFFFFF;
        y[5] = y_u32s[4] & 0x1FFFFFF;
        y[6] = ((y_u32s[4] >> 25) | (y_u32s[5] << 7)) & 0x3FFFFFF;
        y[7] = ((y_u32s[5] >> 19) | (y_u32s[6] << 13)) & 0x1FFFFFF;
        y[8] = ((y_u32s[6] >> 12) | (y_u32s[7] << 20)) & 0x3FFFFFF;
        y[9] = (y_u32s[7] >> 6) & 0x1FFFFFF;

        // CPU decompression intermediates
        let y2 = cpu_fe_mul(&y, &y);
        let u = cpu_fe_sub(&y2, &[1,0,0,0,0,0,0,0,0,0]);
        // d constant (from the shader)
        let d_const: [u32; 10] = [
            0x35978A3, 0x0D37284, 0x3156EBD, 0x06A0A0E, 0x001C029,
            0x179E898, 0x3A03CBB, 0x1CE7198, 0x2E2B6FF, 0x1480DB3
        ];
        let dy2 = cpu_fe_mul(&d_const, &y2);
        let v = cpu_fe_add(&dy2, &[1,0,0,0,0,0,0,0,0,0]);
        let v2 = cpu_fe_mul(&v, &v);
        let v3 = cpu_fe_mul(&v2, &v);
        let v6 = cpu_fe_mul(&v3, &v3);
        let v7 = cpu_fe_mul(&v6, &v);
        let uv7 = cpu_fe_mul(&u, &v7);

        // CPU fe_pow2523 to compare
        let pow_cpu = cpu_fe_pow2523(&uv7);
        let uv3 = cpu_fe_mul(&u, &v3);
        let x_cpu = cpu_fe_mul(&uv3, &pow_cpu);
        let x2_cpu = cpu_fe_mul(&x_cpu, &x_cpu);
        let vx2_cpu = cpu_fe_mul(&v, &x2_cpu);

        println!("CPU intermediates [limb 0]:");
        println!("  y^2[0]={} u[0]={} dy2[0]={} v[0]={} uv7[0]={}",
            y2[0], u[0], dy2[0], v[0], uv7[0]);
        println!("  pow[0]={} x[0]={} vx2[0]={}", pow_cpu[0], x_cpu[0], vx2_cpu[0]);

        let packed: Vec<[u8; 128]> = tasks
            .iter()
            .map(|task| {
                let k = compute_k_scalar(&task.signature, &task.public_key, &task.message);
                let mut buf = [0u8; 128];
                buf[0..32].copy_from_slice(&task.signature[0..32]);
                buf[32..64].copy_from_slice(&task.signature[32..64]);
                buf[64..96].copy_from_slice(&k);
                buf[96..128].copy_from_slice(&task.public_key);
                buf
            })
            .collect();

        match dispatch_ed25519_verify(&packed) {
            Ok(raw) => {
                println!("\nGPU diagnostic results:");
                println!("  [0] fe_sq(1)[0]  = {} (expect 1)", raw[0]);
                println!("  [1] fe_sq(e1)[2] = {} (expect 2)", raw[1]);
                println!("  [2] R decomp     = {} (0=identity, 99=valid)", raw[2]);
                println!("  [3] y^2[0]  GPU={} CPU={} {}", raw[3], y2[0],
                    if raw[3] == y2[0] { "MATCH" } else { "MISMATCH!" });
                println!("  [4] u[0]    GPU={} CPU={} {}", raw[4], u[0],
                    if raw[4] == u[0] { "MATCH" } else { "MISMATCH!" });
                println!("  [5] dy2[0]  GPU={} CPU={} {}", raw[5], dy2[0],
                    if raw[5] == dy2[0] { "MATCH" } else { "MISMATCH!" });
                println!("  [6] v[0]    GPU={} CPU={} {}", raw[6], v[0],
                    if raw[6] == v[0] { "MATCH" } else { "MISMATCH!" });
                println!("  [7] uv7[0]  GPU={} CPU={} {}", raw[7], uv7[0],
                    if raw[7] == uv7[0] { "MATCH" } else { "MISMATCH!" });
                println!("  [8] pow[0]  GPU={}", raw[8]);
                println!("  [9] x[0]    GPU={}", raw[9]);
                println!("  [10] vx2[0] GPU={}", raw[10]);
                println!("  [11] check  GPU={} (100=pass, 200=sqrtm1, 300=fail)", raw[11]);
            }
            Err(e) => {
                println!("GPU dispatch failed: {}", e);
            }
        }
    }

    fn cpu_fe_add(a: &[u32; 10], b: &[u32; 10]) -> [u32; 10] {
        let mut r = [0u32; 10];
        for i in 0..10 { r[i] = a[i] + b[i]; }
        r
    }

    fn cpu_fe_sub(a: &[u32; 10], b: &[u32; 10]) -> [u32; 10] {
        let bias: [u32; 10] = [
            0x7FFFFDAu32, 0x3FFFFFEu32, 0x7FFFFFEu32, 0x3FFFFFEu32, 0x7FFFFFEu32,
            0x3FFFFFEu32, 0x7FFFFFEu32, 0x3FFFFFEu32, 0x7FFFFFEu32, 0x3FFFFFEu32
        ];
        let mut r = [0u32; 10];
        for i in 0..10 {
            r[i] = a[i].wrapping_add(bias[i]).wrapping_sub(b[i]);
        }
        cpu_fe_reduce(&mut r);
        r
    }

    fn cpu_fe_reduce(r: &mut [u32; 10]) {
        for _ in 0..2 {
            for i in 0..9 {
                if i % 2 == 0 {
                    let carry = r[i] >> 26;
                    r[i] &= 0x3FFFFFF;
                    r[i+1] += carry;
                } else {
                    let carry = r[i] >> 25;
                    r[i] &= 0x1FFFFFF;
                    r[i+1] += carry;
                }
            }
            let carry9 = r[9] >> 25;
            r[9] &= 0x1FFFFFF;
            r[0] += carry9 * 19;
        }
    }

    fn cpu_fe_sq_n(a: &[u32; 10], n: u32) -> [u32; 10] {
        let mut r = *a;
        for _ in 0..n {
            r = cpu_fe_mul(&r, &r);
        }
        r
    }

    fn cpu_fe_pow2523(z: &[u32; 10]) -> [u32; 10] {
        let z2 = cpu_fe_mul(z, z);
        let z9 = cpu_fe_mul(&cpu_fe_sq_n(&z2, 2), z);
        let z11 = cpu_fe_mul(&z9, &z2);
        let z_5_0 = cpu_fe_mul(&cpu_fe_mul(&z11, &z11), &z9);
        let z_10_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_5_0, 5), &z_5_0);
        let z_20_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_10_0, 10), &z_10_0);
        let z_40_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_20_0, 20), &z_20_0);
        let z_50_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_40_0, 10), &z_10_0);
        let z_100_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_50_0, 50), &z_50_0);
        let z_200_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_100_0, 100), &z_100_0);
        let z_250_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_200_0, 50), &z_50_0);
        cpu_fe_mul(&cpu_fe_sq_n(&z_250_0, 2), z)
    }

    /// CPU implementation of 10-limb field multiplication with odd-odd doubling
    fn cpu_fe_mul(a: &[u32; 10], b: &[u32; 10]) -> [u32; 10] {
        let mut b19 = [0u32; 10];
        let mut a2 = [0u32; 10];
        for i in 0..10 {
            b19[i] = b[i].wrapping_mul(19);
            a2[i] = a[i].wrapping_mul(2);
        }

        let mut h = [0u64; 10];

        for i in 0..10 {
            let i_odd = i % 2 == 1;
            for j in 0..10 {
                let k = i + j;
                let both_odd = i_odd && (j % 2 == 1);
                let ai = if both_odd { a2[i] } else { a[i] };
                let bj = if k >= 10 { b19[j] } else { b[j] };
                let idx = k % 10;
                h[idx] += ai as u64 * bj as u64;
            }
        }

        // Extract limbs with carry propagation (stay in u64)
        let mut r64 = [0u64; 10];
        let mut carry = 0u64;
        for i in 0..10 {
            let sum = h[i] + carry;
            if i % 2 == 0 {
                r64[i] = sum & 0x3FFFFFF;
                carry = sum >> 26;
            } else {
                r64[i] = sum & 0x1FFFFFF;
                carry = sum >> 25;
            }
        }

        // Final carry wraps with factor 19 — MUST stay in u64 to avoid truncation
        r64[0] += carry * 19;

        // One more carry propagation round (still u64 to handle overflow)
        for i in 0..9 {
            if i % 2 == 0 {
                let c = r64[i] >> 26;
                r64[i] &= 0x3FFFFFF;
                r64[i+1] += c;
            } else {
                let c = r64[i] >> 25;
                r64[i] &= 0x1FFFFFF;
                r64[i+1] += c;
            }
        }
        let c = r64[9] >> 25;
        r64[9] &= 0x1FFFFFF;
        r64[0] += c * 19;

        // Convert to u32 (all values now fit in their limb widths)
        let mut r = [0u32; 10];
        for i in 0..10 { r[i] = r64[i] as u32; }
        r
    }

    fn cpu_fe_invert(z: &[u32; 10]) -> [u32; 10] {
        let z2 = cpu_fe_mul(z, z);
        let z9 = cpu_fe_mul(&cpu_fe_sq_n(&z2, 2), z);
        let z11 = cpu_fe_mul(&z9, &z2);
        let z_5_0 = cpu_fe_mul(&cpu_fe_mul(&z11, &z11), &z9);
        let z_10_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_5_0, 5), &z_5_0);
        let z_20_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_10_0, 10), &z_10_0);
        let z_40_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_20_0, 20), &z_20_0);
        let z_50_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_40_0, 10), &z_10_0);
        let z_100_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_50_0, 50), &z_50_0);
        let z_200_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_100_0, 100), &z_100_0);
        let z_250_0 = cpu_fe_mul(&cpu_fe_sq_n(&z_200_0, 50), &z_50_0);
        cpu_fe_mul(&cpu_fe_sq_n(&z_250_0, 5), &z11)
    }

    fn cpu_fe_tobytes(f: &[u32; 10]) -> [u8; 32] {
        let h = cpu_fe_freeze(f);
        let mut out = [0u32; 8];
        out[0] = h[0] | (h[1] << 26);
        out[1] = (h[1] >> 6) | (h[2] << 19);
        out[2] = (h[2] >> 13) | (h[3] << 13);
        out[3] = (h[3] >> 19) | (h[4] << 6);
        out[4] = h[5] | (h[6] << 25);
        out[5] = (h[6] >> 7) | (h[7] << 19);
        out[6] = (h[7] >> 13) | (h[8] << 12);
        out[7] = (h[8] >> 20) | (h[9] << 6);
        let mut bytes = [0u8; 32];
        for i in 0..8 {
            let b = out[i].to_le_bytes();
            bytes[i*4..i*4+4].copy_from_slice(&b);
        }
        bytes
    }

    #[test]
    fn test_cpu_fe_invert() {
        // Test: 2 * 2^(p-2) == 1 mod p
        let two = [2u32, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let two_inv = cpu_fe_invert(&two);
        let product = cpu_fe_mul(&two, &two_inv);
        let canonical = cpu_fe_freeze(&product);
        println!("2 * 2^(p-2) = {:?}", canonical);
        assert_eq!(canonical, [1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            "2 * inverse(2) should be 1");

        // Also test with a larger value: 42
        let val = [42u32, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let val_inv = cpu_fe_invert(&val);
        let product2 = cpu_fe_mul(&val, &val_inv);
        let canonical2 = cpu_fe_freeze(&product2);
        println!("42 * 42^(p-2) = {:?}", canonical2);
        assert_eq!(canonical2, [1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            "42 * inverse(42) should be 1");

        // Test fe_tobytes round-trip
        let y_bytes: [u8; 32] = [
            0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
        ];
        let mut y_u32s = [0u32; 8];
        for i in 0..8 {
            y_u32s[i] = u32::from_le_bytes([
                y_bytes[i*4], y_bytes[i*4+1], y_bytes[i*4+2], y_bytes[i*4+3]
            ]);
        }
        y_u32s[7] &= 0x7FFFFFFF;
        let mut y = [0u32; 10];
        y[0] = y_u32s[0] & 0x3FFFFFF;
        y[1] = ((y_u32s[0] >> 26) | (y_u32s[1] << 6)) & 0x1FFFFFF;
        y[2] = ((y_u32s[1] >> 19) | (y_u32s[2] << 13)) & 0x3FFFFFF;
        y[3] = ((y_u32s[2] >> 13) | (y_u32s[3] << 19)) & 0x1FFFFFF;
        y[4] = (y_u32s[3] >> 6) & 0x3FFFFFF;
        y[5] = y_u32s[4] & 0x1FFFFFF;
        y[6] = ((y_u32s[4] >> 25) | (y_u32s[5] << 7)) & 0x3FFFFFF;
        y[7] = ((y_u32s[5] >> 19) | (y_u32s[6] << 13)) & 0x1FFFFFF;
        y[8] = ((y_u32s[6] >> 12) | (y_u32s[7] << 20)) & 0x3FFFFFF;
        y[9] = (y_u32s[7] >> 6) & 0x1FFFFFF;

        let roundtrip = cpu_fe_tobytes(&y);
        println!("y round-trip: {:?}", roundtrip);
        assert_eq!(roundtrip, y_bytes, "fe_tobytes should round-trip to original bytes");
    }

    #[test]
    fn test_cpu_basepoint_decompress() {
        // Verify the CPU 10-limb arithmetic by decompressing the Ed25519 base point
        // Known: y = 4/5 mod p, x = 15112221...
        // Compressed base point: y bytes with sign bit
        let by_bytes: [u8; 32] = [
            0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
        ];

        let mut y_u32s = [0u32; 8];
        for i in 0..8 {
            y_u32s[i] = u32::from_le_bytes([
                by_bytes[i*4], by_bytes[i*4+1], by_bytes[i*4+2], by_bytes[i*4+3]
            ]);
        }
        let _x_sign = (y_u32s[7] >> 31) & 1;
        y_u32s[7] &= 0x7FFFFFFF;

        let mut y = [0u32; 10];
        y[0] = y_u32s[0] & 0x3FFFFFF;
        y[1] = ((y_u32s[0] >> 26) | (y_u32s[1] << 6)) & 0x1FFFFFF;
        y[2] = ((y_u32s[1] >> 19) | (y_u32s[2] << 13)) & 0x3FFFFFF;
        y[3] = ((y_u32s[2] >> 13) | (y_u32s[3] << 19)) & 0x1FFFFFF;
        y[4] = (y_u32s[3] >> 6) & 0x3FFFFFF;
        y[5] = y_u32s[4] & 0x1FFFFFF;
        y[6] = ((y_u32s[4] >> 25) | (y_u32s[5] << 7)) & 0x3FFFFFF;
        y[7] = ((y_u32s[5] >> 19) | (y_u32s[6] << 13)) & 0x1FFFFFF;
        y[8] = ((y_u32s[6] >> 12) | (y_u32s[7] << 20)) & 0x3FFFFFF;
        y[9] = (y_u32s[7] >> 6) & 0x1FFFFFF;

        println!("Base point y limbs: {:?}", y);

        let d_const: [u32; 10] = [
            0x35978A3, 0x0D37284, 0x3156EBD, 0x06A0A0E, 0x001C029,
            0x179E898, 0x3A03CBB, 0x1CE7198, 0x2E2B6FF, 0x1480DB3
        ];

        let y2 = cpu_fe_mul(&y, &y);
        let one = [1u32,0,0,0,0,0,0,0,0,0];
        let u = cpu_fe_sub(&y2, &one);
        let dy2 = cpu_fe_mul(&d_const, &y2);
        let v = cpu_fe_add(&dy2, &one);

        println!("u = {:?}", u);
        println!("v = {:?}", v);

        let v2 = cpu_fe_mul(&v, &v);
        let v3 = cpu_fe_mul(&v2, &v);
        let v6 = cpu_fe_mul(&v3, &v3);
        let v7 = cpu_fe_mul(&v6, &v);
        let uv7 = cpu_fe_mul(&u, &v7);
        let pow = cpu_fe_pow2523(&uv7);
        let uv3 = cpu_fe_mul(&u, &v3);
        let x = cpu_fe_mul(&uv3, &pow);

        println!("x_candidate = {:?}", x);

        // Check: v * x^2 should equal u or -u
        let x2 = cpu_fe_mul(&x, &x);
        let vx2 = cpu_fe_mul(&v, &x2);
        println!("vx2 = {:?}", vx2);
        println!("u   = {:?}", u);

        // Check if they're equal
        let diff = cpu_fe_sub(&vx2, &u);
        println!("vx2 - u = {:?}", diff);
        let is_zero = cpu_fe_is_zero(&diff);
        println!("vx2 == u? {}", is_zero);

        if !is_zero {
            // Check if vx2 == -u
            let neg_u = cpu_fe_neg(&u);
            let diff2 = cpu_fe_sub(&vx2, &neg_u);
            println!("vx2 + u = {:?}", diff2);
            let is_neg = cpu_fe_is_zero(&diff2);
            println!("vx2 == -u? {}", is_neg);
        }
    }

    fn cpu_fe_neg(a: &[u32; 10]) -> [u32; 10] {
        cpu_fe_sub(&[0u32;10], a)
    }

    fn cpu_fe_is_zero(a: &[u32; 10]) -> bool {
        let f = cpu_fe_freeze(a);
        f.iter().all(|&x| x == 0)
    }

    fn cpu_fe_freeze(a: &[u32; 10]) -> [u32; 10] {
        let mut h = *a;
        cpu_fe_reduce(&mut h);

        let mut q = (h[0].wrapping_add(19)) >> 26;
        q = (h[1].wrapping_add(q)) >> 25;
        q = (h[2].wrapping_add(q)) >> 26;
        q = (h[3].wrapping_add(q)) >> 25;
        q = (h[4].wrapping_add(q)) >> 26;
        q = (h[5].wrapping_add(q)) >> 25;
        q = (h[6].wrapping_add(q)) >> 26;
        q = (h[7].wrapping_add(q)) >> 25;
        q = (h[8].wrapping_add(q)) >> 26;
        q = (h[9].wrapping_add(q)) >> 25;

        h[0] = h[0].wrapping_add(19 * q);

        let mut carry;
        carry = h[0] >> 26; h[0] &= 0x3FFFFFF; h[1] += carry;
        carry = h[1] >> 25; h[1] &= 0x1FFFFFF; h[2] += carry;
        carry = h[2] >> 26; h[2] &= 0x3FFFFFF; h[3] += carry;
        carry = h[3] >> 25; h[3] &= 0x1FFFFFF; h[4] += carry;
        carry = h[4] >> 26; h[4] &= 0x3FFFFFF; h[5] += carry;
        carry = h[5] >> 25; h[5] &= 0x1FFFFFF; h[6] += carry;
        carry = h[6] >> 26; h[6] &= 0x3FFFFFF; h[7] += carry;
        carry = h[7] >> 25; h[7] &= 0x1FFFFFF; h[8] += carry;
        carry = h[8] >> 26; h[8] &= 0x3FFFFFF; h[9] += carry;
        h[9] &= 0x1FFFFFF;

        h
    }

    #[test]
    fn print_ed25519_constants() {
        // Compute the correct 10-limb representations of Ed25519 constants
        // for the WGSL shader.

        fn bytes_to_10limb(bytes: &[u8]) -> [u32; 10] {
            let mut b = [0u32; 8];
            for i in 0..8 {
                b[i] = u32::from_le_bytes([bytes[i*4], bytes[i*4+1], bytes[i*4+2], bytes[i*4+3]]);
            }
            let mut f = [0u32; 10];
            f[0] = b[0] & 0x3FFFFFF;
            f[1] = ((b[0] >> 26) | (b[1] << 6)) & 0x1FFFFFF;
            f[2] = ((b[1] >> 19) | (b[2] << 13)) & 0x3FFFFFF;
            f[3] = ((b[2] >> 13) | (b[3] << 19)) & 0x1FFFFFF;
            f[4] = (b[3] >> 6) & 0x3FFFFFF;
            f[5] = b[4] & 0x1FFFFFF;
            f[6] = ((b[4] >> 25) | (b[5] << 7)) & 0x3FFFFFF;
            f[7] = ((b[5] >> 19) | (b[6] << 13)) & 0x1FFFFFF;
            f[8] = ((b[6] >> 12) | (b[7] << 20)) & 0x3FFFFFF;
            f[9] = (b[7] >> 6) & 0x1FFFFFF;
            f
        }

        fn print_limbs(name: &str, limbs: &[u32; 10]) {
            println!("let {} = array<u32, 10>(", name);
            println!("    0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u,",
                limbs[0], limbs[1], limbs[2], limbs[3], limbs[4]);
            println!("    0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u",
                limbs[5], limbs[6], limbs[7], limbs[8], limbs[9]);
            println!(");");
        }

        // Base point y = 4/5 mod p
        // Compressed: 5866666666666666666666666666666666666666666666666666666666666666
        let by_bytes: [u8; 32] = [
            0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
        ];
        let by = bytes_to_10limb(&by_bytes);
        print_limbs("by", &by);

        // Base point x (positive) from RFC 8032
        // x = 15112221349535807912866137220509078750507884956996801397042474682556759602985
        // hex (big-endian): 216936D3CD6E53FEC0A4E231FDD6DC5C692CC7609525A7B2C9562D608F25D51A
        let bx_bytes: [u8; 32] = [
            0x1A, 0xD5, 0x25, 0x8F, 0x60, 0x2D, 0x56, 0xC9,
            0xB2, 0xA7, 0x25, 0x95, 0x60, 0xC7, 0x2C, 0x69,
            0x5C, 0xDC, 0xD6, 0xFD, 0x31, 0xE2, 0xA4, 0xC0,
            0xFE, 0x53, 0x6E, 0xCD, 0xD3, 0x36, 0x69, 0x21,
        ];
        let bx = bytes_to_10limb(&bx_bytes);
        print_limbs("bx", &bx);

        // d = -121665/121666 mod p
        // hex (big-endian): 52036cee2b6ffe738cc740797779e89800700a4d4141d8ab75eb4dca135978a3
        let d_bytes: [u8; 32] = [
            0xa3, 0x78, 0x59, 0x13, 0xca, 0x4d, 0xeb, 0x75,
            0xab, 0xd8, 0x41, 0x41, 0x4d, 0x0a, 0x70, 0x00,
            0x98, 0xe8, 0x79, 0x77, 0x79, 0x40, 0xc7, 0x8c,
            0x73, 0xfe, 0x6f, 0x2b, 0xee, 0x6c, 0x03, 0x52,
        ];
        let d = bytes_to_10limb(&d_bytes);
        print_limbs("d", &d);

        // 2d
        // We compute 2d by doubling each limb and carrying
        let mut d2 = [0u32; 10];
        for i in 0..10 {
            d2[i] = d[i] * 2;
        }
        // Carry propagation
        for _ in 0..2 {
            for i in 0..9 {
                if i % 2 == 0 {
                    let carry = d2[i] >> 26;
                    d2[i] &= 0x3FFFFFF;
                    d2[i+1] += carry;
                } else {
                    let carry = d2[i] >> 25;
                    d2[i] &= 0x1FFFFFF;
                    d2[i+1] += carry;
                }
            }
            let carry9 = d2[9] >> 25;
            d2[9] &= 0x1FFFFFF;
            d2[0] += carry9 * 19;
        }
        print_limbs("d2", &d2);

        // sqrt(-1) mod p = 2^((p-1)/4) mod p
        // hex (big-endian): 2b8324804fc1df0b2b4d00993dfbd7a72f431806ad2fe478c4ee1b274a0ea0b0
        let sqrtm1_bytes: [u8; 32] = [
            0xb0, 0xa0, 0x0e, 0x4a, 0x27, 0x1b, 0xee, 0xc4,
            0x78, 0xe4, 0x2f, 0xad, 0x06, 0x18, 0x43, 0x2f,
            0xa7, 0xd7, 0xfb, 0x3d, 0x99, 0x00, 0x4d, 0x2b,
            0x0b, 0xdf, 0xc1, 0x4f, 0x80, 0x24, 0x83, 0x2b,
        ];
        let sqrtm1 = bytes_to_10limb(&sqrtm1_bytes);
        print_limbs("sqrtm1", &sqrtm1);
    }

    // CPU-side extended point operations (mirrors WGSL ge_add / ge_double)
    type CpuGePoint = ([u32; 10], [u32; 10], [u32; 10], [u32; 10]); // (x, y, z, t)

    fn cpu_ge_zero() -> CpuGePoint {
        ([0;10], [1,0,0,0,0,0,0,0,0,0], [1,0,0,0,0,0,0,0,0,0], [0;10])
    }

    fn cpu_fe_2d() -> [u32; 10] {
        [0x2B2F159, 0x1A6E509, 0x22ADD7A, 0x0D4141D, 0x0038052,
         0x0F3D130, 0x3407977, 0x19CE331, 0x1C56DFF, 0x0901B67]
    }

    fn cpu_ge_add(p: &CpuGePoint, q: &CpuGePoint) -> CpuGePoint {
        let a = cpu_fe_mul(&cpu_fe_sub(&p.1, &p.0), &cpu_fe_sub(&q.1, &q.0));
        let b = cpu_fe_mul(&cpu_fe_add(&p.1, &p.0), &cpu_fe_add(&q.1, &q.0));
        let c = cpu_fe_mul(&cpu_fe_mul(&p.3, &q.3), &cpu_fe_2d());
        let d = cpu_fe_mul(&p.2, &cpu_fe_add(&q.2, &q.2));

        let e = cpu_fe_sub(&b, &a);
        let f = cpu_fe_sub(&d, &c);
        let g = cpu_fe_add(&d, &c);
        let h = cpu_fe_add(&b, &a);

        (cpu_fe_mul(&e, &f), cpu_fe_mul(&g, &h),
         cpu_fe_mul(&f, &g), cpu_fe_mul(&e, &h))
    }

    fn cpu_ge_double(p: &CpuGePoint) -> CpuGePoint {
        let a = cpu_fe_mul(&p.0, &p.0);
        let b = cpu_fe_mul(&p.1, &p.1);
        let c_inner = cpu_fe_mul(&p.2, &p.2);
        let c = cpu_fe_add(&c_inner, &c_inner);
        let d = cpu_fe_neg(&a);

        let e = cpu_fe_sub(&cpu_fe_mul(&cpu_fe_add(&p.0, &p.1), &cpu_fe_add(&p.0, &p.1)), &cpu_fe_add(&a, &b));
        let g = cpu_fe_add(&d, &b);
        let f = cpu_fe_sub(&g, &c);
        let h = cpu_fe_sub(&d, &b);

        (cpu_fe_mul(&e, &f), cpu_fe_mul(&g, &h),
         cpu_fe_mul(&f, &g), cpu_fe_mul(&e, &h))
    }

    fn cpu_ge_to_affine(p: &CpuGePoint) -> CpuGePoint {
        let z_inv = cpu_fe_invert(&p.2);
        let x = cpu_fe_mul(&p.0, &z_inv);
        let y = cpu_fe_mul(&p.1, &z_inv);
        let t = cpu_fe_mul(&x, &y);
        (x, y, [1,0,0,0,0,0,0,0,0,0], t)
    }

    fn cpu_ge_basepoint() -> CpuGePoint {
        let bx: [u32; 10] = [
            0x325D51A, 0x18B5823, 0x0F6592A, 0x104A92D, 0x1A4B31D,
            0x1D6DC5C, 0x27118FE, 0x07FD814, 0x13CD6E5, 0x085A4DB
        ];
        let by: [u32; 10] = [
            0x2666658, 0x1999999, 0x0CCCCCC, 0x1333333, 0x1999999,
            0x0666666, 0x3333333, 0x0CCCCCC, 0x2666666, 0x1999999
        ];
        let bz: [u32; 10] = [1,0,0,0,0,0,0,0,0,0];
        let bt = cpu_fe_mul(&bx, &by);
        (bx, by, bz, bt)
    }

    #[test]
    fn generate_base_point_table() {
        // Generate 16 precomputed base point multiples for the 4-bit windowed
        // scalar multiplication table: B_TABLE[i] = i * B for i in 0..16
        // Each point is in affine extended coordinates (x, y, z=1, t=xy)
        let bp = cpu_ge_basepoint();

        // Verify basepoint is on curve using curve25519-dalek
        use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;
        use curve25519_dalek::Scalar;
        let dalek_bp_compressed = ED25519_BASEPOINT_POINT.compress();
        let our_by_bytes = cpu_fe_tobytes(&bp.1);
        assert_eq!(our_by_bytes, dalek_bp_compressed.as_bytes()[..32],
            "Base point y should match curve25519-dalek");

        let mut table: Vec<CpuGePoint> = Vec::with_capacity(16);
        // 0 * B = identity
        table.push(cpu_ge_zero());
        // 1 * B
        table.push(bp);
        // i * B = (i-1) * B + B for i >= 2
        for i in 2..16 {
            let prev = &table[i - 1];
            let next = cpu_ge_add(prev, &bp);
            table.push(next);
        }

        // Convert all to affine and verify against curve25519-dalek
        let mut affine_table: Vec<CpuGePoint> = Vec::with_capacity(16);
        for i in 0..16 {
            let aff = if i == 0 { cpu_ge_zero() } else { cpu_ge_to_affine(&table[i]) };
            // Verify against dalek for non-zero points
            if i > 0 {
                let dalek_point = Scalar::from(i as u64) * ED25519_BASEPOINT_POINT;
                let dalek_compressed = dalek_point.compress();
                let our_y_bytes = cpu_fe_tobytes(&aff.1);
                // Compressed = y with sign bit of x in top bit
                let our_x_bytes = cpu_fe_tobytes(&aff.0);
                let x_sign = our_x_bytes[0] & 1;
                let mut expected_compressed = our_y_bytes;
                expected_compressed[31] |= x_sign << 7;
                assert_eq!(expected_compressed, dalek_compressed.as_bytes()[..32],
                    "{}*B should match curve25519-dalek", i);
            }
            affine_table.push(aff);
        }

        // Print WGSL constant table
        println!("\n// Precomputed base point table: B_TABLE[i] = i * B (affine extended)");
        println!("// Generated from curve25519-dalek-verified CPU arithmetic");
        for i in 0..16 {
            let (x, y, _, t) = &affine_table[i];
            let xf = cpu_fe_freeze(x);
            let yf = cpu_fe_freeze(y);
            let tf = cpu_fe_freeze(t);
            println!("\n// {}*B", i);
            println!("const B_TABLE_{}_X = array<u32, 10>(\n    0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u,\n    0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u\n);",
                i, xf[0], xf[1], xf[2], xf[3], xf[4], xf[5], xf[6], xf[7], xf[8], xf[9]);
            println!("const B_TABLE_{}_Y = array<u32, 10>(\n    0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u,\n    0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u\n);",
                i, yf[0], yf[1], yf[2], yf[3], yf[4], yf[5], yf[6], yf[7], yf[8], yf[9]);
            println!("const B_TABLE_{}_T = array<u32, 10>(\n    0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u,\n    0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u, 0x{:07X}u\n);",
                i, tf[0], tf[1], tf[2], tf[3], tf[4], tf[5], tf[6], tf[7], tf[8], tf[9]);
        }
    }

    #[test]
    fn bench_gpu_vs_cpu_ed25519() {
        let batch_sizes = [1_000, 5_000, 10_000, 25_000];

        // Detect which GPU pipeline is active
        let gpu_label = match get_or_init_gpu() {
            Some(ctx) if ctx.msl_pipeline.is_some() => "GPU(MSL-u64)",
            Some(_) => "GPU(WGSL-u32)",
            None => "GPU(none)",
        };

        println!("\nEd25519 Verification Throughput Comparison (buffer pool + wg=256):");
        println!("{:-<100}", "");
        println!("{:>7}  {:>14}  {:>14}  {:>14}  {:>10}  {:>10}  {:>12}",
            "batch", "CPU-seq", "CPU-par(rayon)", gpu_label, "GPU/seq", "GPU/par", "GPU time");
        println!("{:-<100}", "");

        for &n in &batch_sizes {
            let mut tasks = generate_valid_tasks(n);
            // Sprinkle in ~1% invalid
            let num_invalid = std::cmp::max(1, n / 100);
            for i in 0..num_invalid {
                if i * 100 < n {
                    tasks[i * 100] = generate_invalid_task();
                }
            }

            // CPU sequential (skip for large batches — too slow)
            let (cpu_vps, cpu_results) = if n <= 10_000 {
                let cpu_start = Instant::now();
                let results: Vec<bool> = tasks.iter().map(|t| verify_single(t)).collect();
                let elapsed = cpu_start.elapsed();
                (n as f64 / elapsed.as_secs_f64(), Some(results))
            } else {
                (0.0, None)
            };

            // CPU parallel (rayon)
            let par_start = Instant::now();
            let par_results: Vec<bool> = tasks.par_iter().map(|t| verify_single(t)).collect();
            let par_elapsed = par_start.elapsed();
            let par_vps = n as f64 / par_elapsed.as_secs_f64();

            // GPU path with per-phase timing
            let t0 = Instant::now();

            // Phase 1: CPU SHA-512 scalar precomputation
            let packed: Vec<[u8; 128]> = tasks
                .par_iter()
                .map(|task| {
                    let k = compute_k_scalar(&task.signature, &task.public_key, &task.message);
                    let mut buf = [0u8; 128];
                    buf[0..32].copy_from_slice(&task.signature[0..32]);
                    buf[32..64].copy_from_slice(&task.signature[32..64]);
                    buf[64..96].copy_from_slice(&k);
                    buf[96..128].copy_from_slice(&task.public_key);
                    buf
                })
                .collect();
            let t_precompute = t0.elapsed();

            // Phase 2: GPU dispatch with per-phase timing
            let t1 = Instant::now();
            let mut timing = DispatchTiming::default();
            let gpu_raw = dispatch_ed25519_verify_timed(&packed, Some(&mut timing))
                .expect("GPU dispatch should succeed");
            let t_dispatch = t1.elapsed();

            let gpu_results: Vec<bool> = gpu_raw.iter().map(|&v| v == 1).collect();
            let gpu_elapsed = t0.elapsed();
            let gpu_vps = n as f64 / gpu_elapsed.as_secs_f64();

            // Verify results match
            let mismatch_gpu_par = par_results.iter().zip(gpu_results.iter())
                .filter(|(c, g)| c != g).count();
            if let Some(ref cpu_res) = cpu_results {
                let mismatch = cpu_res.iter().zip(gpu_results.iter())
                    .filter(|(c, g)| c != g).count();
                assert_eq!(mismatch, 0, "GPU and CPU-seq must match for batch {}", n);
            }

            if cpu_vps > 0.0 {
                println!("{:>7}  {:>10.0} v/s  {:>10.0} v/s  {:>10.0} v/s  {:>9.2}x  {:>9.2}x  {:>8.1}ms",
                    n, cpu_vps, par_vps, gpu_vps,
                    gpu_vps / cpu_vps, gpu_vps / par_vps,
                    gpu_elapsed.as_secs_f64() * 1000.0);
            } else {
                println!("{:>7}  {:>14}  {:>10.0} v/s  {:>10.0} v/s  {:>10}  {:>9.2}x  {:>8.1}ms",
                    n, "—", par_vps, gpu_vps,
                    "—", gpu_vps / par_vps,
                    gpu_elapsed.as_secs_f64() * 1000.0);
            }
            println!("         precompute={:.1}ms  write={:.1}ms  compute={:.1}ms  readback={:.1}ms",
                t_precompute.as_secs_f64() * 1000.0,
                timing.write_us as f64 / 1000.0,
                timing.compute_us as f64 / 1000.0,
                timing.readback_us as f64 / 1000.0);

            assert_eq!(mismatch_gpu_par, 0, "GPU and rayon must match for batch {}", n);
        }
        println!("{:-<100}", "");
        println!("Note: GPU = SHA-512 precompute (rayon) + dispatch (buffer pool, zero alloc).");
        if gpu_label.contains("MSL") {
            println!("      Native Metal shader with hardware u64 + workgroup_size=256.");
        }
    }
}
