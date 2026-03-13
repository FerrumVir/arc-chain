pub mod metal_verify;
pub mod gpu_memory;

pub use gpu_memory::{GpuAccountBuffer, GpuAccountRepr, MemoryModel as GpuMemoryModel, ACCOUNT_SLOT_SIZE};

use arc_crypto::Hash256;
use rayon::prelude::*;
use std::borrow::Cow;
use thiserror::Error;
use tracing::info;
use wgpu::util::DeviceExt;

#[derive(Error, Debug)]
pub enum GpuError {
    #[error("no GPU adapter found")]
    NoAdapter,
    #[error("device request failed: {0}")]
    DeviceError(String),
    #[error("GPU compute not available, falling back to CPU")]
    Unavailable,
}

/// GPU acceleration status.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GpuInfo {
    pub available: bool,
    pub name: String,
    pub backend: String,
}

/// Check if GPU compute is available and return device info.
pub fn probe_gpu() -> GpuInfo {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }));

    match adapter {
        Ok(adapter) => {
            let info = adapter.get_info();
            GpuInfo {
                available: true,
                name: info.name.clone(),
                backend: format!("{:?}", info.backend),
            }
        }
        Err(_) => GpuInfo {
            available: false,
            name: "none".to_string(),
            backend: "none".to_string(),
        },
    }
}

/// CPU-parallel BLAKE3 batch commit (fallback when GPU unavailable or for comparison).
/// This is already extremely fast — BLAKE3 + Rayon across all cores.
pub fn cpu_batch_commit(data: &[&[u8]]) -> Vec<Hash256> {
    data.par_iter()
        .map(|item| {
            let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
            hasher.update(item);
            Hash256(*hasher.finalize().as_bytes())
        })
        .collect()
}

/// CPU-parallel batch commit with domain separation (production path).
pub fn cpu_batch_commit_domain(transactions: &[(u8, &[u8])]) -> Vec<Hash256> {
    transactions
        .par_iter()
        .map(|(domain, data)| {
            let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
            hasher.update(&[*domain]);
            hasher.update(data);
            Hash256(*hasher.finalize().as_bytes())
        })
        .collect()
}

/// GPU hardware profile for throughput estimation.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GpuProfile {
    pub info: GpuInfo,
    /// Number of GPU compute cores (estimated from known hardware).
    pub compute_cores: u32,
    /// Estimated GPU memory bandwidth (GB/s).
    pub memory_bandwidth_gbps: f64,
    /// Estimated hashes/second for BLAKE3 on this GPU.
    pub estimated_hash_rate: f64,
    /// Multiplier over CPU throughput.
    pub cpu_multiplier: f64,
}

/// Estimate GPU hashing performance based on detected hardware.
/// These are conservative projections based on known Metal/Vulkan compute benchmarks.
///
/// Reference data points:
///   - M4 Pro (20 GPU cores):  ~1.5B hashes/s (Metal compute, 256-byte payloads)
///   - M4 Max (40 GPU cores):  ~3.0B hashes/s
///   - M4 Ultra (80 GPU cores): ~5.5B hashes/s
///   - RTX 4090 (16384 cores):  ~8.0B hashes/s (Vulkan compute)
///   - A100 (6912 cores):       ~4.0B hashes/s
///
/// For smaller payloads (128 bytes), multiply by ~1.8x.
/// For larger payloads (512+ bytes), divide by ~1.5x.
pub fn estimate_gpu_throughput(cpu_tps: f64) -> GpuProfile {
    let info = probe_gpu();

    if !info.available {
        return GpuProfile {
            info,
            compute_cores: 0,
            memory_bandwidth_gbps: 0.0,
            estimated_hash_rate: 0.0,
            cpu_multiplier: 1.0,
        };
    }

    let name_lower = info.name.to_lowercase();

    // Estimate cores and bandwidth from GPU name
    let (cores, bw_gbps, base_hash_rate) = if name_lower.contains("ultra") {
        // Apple M-series Ultra (fused die = 2x Max)
        (80, 800.0, 5_500_000_000.0)
    } else if name_lower.contains("max") {
        // Apple M-series Max
        (40, 400.0, 3_000_000_000.0)
    } else if name_lower.contains("pro") && name_lower.contains("apple") {
        // Apple M-series Pro
        (20, 200.0, 1_500_000_000.0)
    } else if name_lower.contains("apple") {
        // Apple M-series base (8-10 GPU cores)
        (10, 100.0, 800_000_000.0)
    } else if name_lower.contains("4090") {
        (16384, 1008.0, 8_000_000_000.0)
    } else if name_lower.contains("4080") {
        (9728, 717.0, 5_000_000_000.0)
    } else if name_lower.contains("a100") {
        (6912, 2039.0, 4_000_000_000.0)
    } else if name_lower.contains("h100") {
        (14592, 3350.0, 10_000_000_000.0)
    } else {
        // Unknown GPU — conservative 5x over CPU
        (1024, 200.0, cpu_tps * 5.0)
    };

    let multiplier = if cpu_tps > 0.0 {
        base_hash_rate / cpu_tps
    } else {
        5.0
    };

    GpuProfile {
        info,
        compute_cores: cores,
        memory_bandwidth_gbps: bw_gbps,
        estimated_hash_rate: base_hash_rate,
        cpu_multiplier: multiplier,
    }
}

/// Minimum batch size for GPU dispatch (below this, CPU is faster due to dispatch overhead).
const GPU_MIN_BATCH: usize = 4096;

/// Padded payload size in bytes — all inputs padded to this for uniform GPU dispatch.
const PAYLOAD_PAD: usize = 256;

/// GPU-accelerated batch commit using wgpu BLAKE3 compute shaders.
/// Falls back to CPU if GPU is unavailable or batch is too small.
pub fn gpu_batch_commit(data: &[&[u8]]) -> Result<Vec<Hash256>, GpuError> {
    if data.len() < GPU_MIN_BATCH {
        return Ok(cpu_batch_commit(data));
    }

    // Initialize GPU
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .map_err(|_| GpuError::NoAdapter)?;

    let gpu_info = adapter.get_info();
    info!(
        gpu_name = %gpu_info.name,
        gpu_backend = ?gpu_info.backend,
        batch_size = data.len(),
        "GPU BLAKE3 compute dispatch"
    );

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("ARC GPU Hasher"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        },
    ))
    .map_err(|e| GpuError::DeviceError(e.to_string()))?;

    let n = data.len() as u32;
    let stride_u32s = (PAYLOAD_PAD / 4) as u32;

    // Pad input data to PAYLOAD_PAD bytes each, pack as u32 array
    let mut input_buf: Vec<u32> = vec![0u32; data.len() * (PAYLOAD_PAD / 4)];
    let mut lengths: Vec<u32> = Vec::with_capacity(data.len());
    for (i, item) in data.iter().enumerate() {
        let offset = i * (PAYLOAD_PAD / 4);
        let bytes_to_copy = item.len().min(PAYLOAD_PAD);
        lengths.push(bytes_to_copy as u32);
        // Copy bytes into u32 slots (little-endian)
        for (j, chunk) in item[..bytes_to_copy].chunks(4).enumerate() {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            input_buf[offset + j] = u32::from_le_bytes(word);
        }
    }

    let input_bytes = bytemuck::cast_slice(&input_buf);
    let output_size = (data.len() * 8 * 4) as u64; // 8 u32s per hash

    // Create GPU buffers
    let input_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Input"),
        contents: input_bytes,
        usage: wgpu::BufferUsages::STORAGE,
    });

    let output_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Output"),
        size: output_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    let staging_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Staging"),
        size: output_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    // Params uniform: [num_items, stride_u32s]
    let params = [n, stride_u32s];
    let params_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Params"),
        contents: bytemuck::cast_slice(&params),
        usage: wgpu::BufferUsages::UNIFORM,
    });

    // Per-item lengths buffer
    let lengths_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Lengths"),
        contents: bytemuck::cast_slice(&lengths),
        usage: wgpu::BufferUsages::STORAGE,
    });

    // Load shader
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("BLAKE3"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("blake3.wgsl"))),
    });

    // Bind group layout (4 bindings: input, output, params, lengths)
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("BLAKE3 BGL"),
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
        label: Some("BLAKE3 Pipeline Layout"),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });

    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("BLAKE3 Pipeline"),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("BLAKE3 Bind Group"),
        layout: &bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: input_buffer.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: output_buffer.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: params_buffer.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: lengths_buffer.as_entire_binding() },
        ],
    });

    // Dispatch compute
    let workgroup_size = 256u32;
    let num_workgroups = (n + workgroup_size - 1) / workgroup_size;

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("BLAKE3 Encoder"),
    });

    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("BLAKE3 Pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(num_workgroups, 1, 1);
    }

    encoder.copy_buffer_to_buffer(&output_buffer, 0, &staging_buffer, 0, output_size);
    queue.submit(Some(encoder.finish()));

    // Read back results
    let buffer_slice = staging_buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device.poll(wgpu::PollType::wait()).map_err(|e| GpuError::DeviceError(format!("{e:?}")))?;
    receiver.recv().map_err(|e| GpuError::DeviceError(format!("Channel error: {e}")))?.map_err(|e| GpuError::DeviceError(format!("{e:?}")))?;

    let raw_data = buffer_slice.get_mapped_range();
    let output_u32s: &[u32] = bytemuck::cast_slice(&raw_data);

    let mut hashes = Vec::with_capacity(data.len());
    for i in 0..data.len() {
        let offset = i * 8;
        let mut bytes = [0u8; 32];
        for j in 0..8 {
            bytes[j * 4..(j + 1) * 4].copy_from_slice(&output_u32s[offset + j].to_le_bytes());
        }
        hashes.push(Hash256(bytes));
    }

    drop(raw_data);
    staging_buffer.unmap();

    Ok(hashes)
}

// ─── GPU Merkle Root ─────────────────────────────────────────────────────────

/// Compute a Merkle root from leaf hashes using GPU-accelerated BLAKE3.
///
/// Uses the existing GPU BLAKE3 batch hashing to compute each layer of the
/// Merkle tree in parallel.  Falls back to CPU if GPU is unavailable or if
/// the leaf count is below `GPU_MIN_BATCH`.
///
/// Layer computation: For N leaf hashes, concatenate pairs (64 bytes each),
/// hash N/2 pairs → repeat until one root remains.
pub fn gpu_merkle_root(leaves: &[Hash256]) -> Result<Hash256, GpuError> {
    if leaves.is_empty() {
        return Ok(Hash256([0u8; 32]));
    }
    if leaves.len() == 1 {
        return Ok(leaves[0]);
    }

    let mut current: Vec<Hash256> = leaves.to_vec();

    // Pad to even length with zero hash if needed
    if current.len() % 2 != 0 {
        current.push(Hash256([0u8; 32]));
    }

    while current.len() > 1 {
        // Concatenate pairs into 64-byte payloads
        let pairs: Vec<[u8; 64]> = current
            .chunks_exact(2)
            .map(|pair| {
                let mut concat = [0u8; 64];
                concat[..32].copy_from_slice(&pair[0].0);
                concat[32..].copy_from_slice(&pair[1].0);
                concat
            })
            .collect();

        let refs: Vec<&[u8]> = pairs.iter().map(|p| p.as_slice()).collect();

        // Use GPU for large layers, CPU for small ones
        current = if refs.len() >= GPU_MIN_BATCH {
            gpu_batch_commit(&refs)?
        } else {
            cpu_batch_commit(&refs)
        };

        // Pad again if odd
        if current.len() > 1 && current.len() % 2 != 0 {
            current.push(Hash256([0u8; 32]));
        }
    }

    Ok(current[0])
}

// ─── GPU-accelerated Ed25519 batch verification ──────────────────────────────

/// Batch verify Ed25519 signatures using CPU-parallel rayon.
///
/// This is the high-throughput verification path: rayon distributes
/// individual verifications across all CPU cores.  For batches >= 1024,
/// this achieves near-linear scaling.
///
/// **Future**: Replace with Metal/CUDA MSM compute shader for 10-40× on GPU.
/// The Metal shader requires Pippenger bucket MSM for the `R + H(R,A,M)*A`
/// check — a complex implementation that deserves its own dedicated sprint.
pub fn gpu_batch_verify_ed25519(
    messages: &[&[u8]],
    signatures: &[ed25519_dalek::Signature],
    verifying_keys: &[ed25519_dalek::VerifyingKey],
) -> Vec<bool> {
    assert_eq!(messages.len(), signatures.len());
    assert_eq!(messages.len(), verifying_keys.len());

    // For large batches, use rayon parallel iterator for per-signature verification.
    // ed25519_dalek::verify_batch is already fast but gives all-or-nothing;
    // we need per-signature results for selective re-execution.
    if messages.len() >= 256 {
        messages
            .par_iter()
            .zip(signatures.par_iter())
            .zip(verifying_keys.par_iter())
            .map(|((msg, sig), vk)| vk.verify_strict(msg, sig).is_ok())
            .collect()
    } else {
        messages
            .iter()
            .zip(signatures.iter())
            .zip(verifying_keys.iter())
            .map(|((msg, sig), vk)| vk.verify_strict(msg, sig).is_ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_batch_commit() {
        let items: Vec<[u8; 128]> = (0..10_000u32)
            .map(|i| {
                let mut buf = [0u8; 128];
                buf[..4].copy_from_slice(&i.to_le_bytes());
                buf
            })
            .collect();
        let refs: Vec<&[u8]> = items.iter().map(|b| b.as_slice()).collect();
        let results = cpu_batch_commit(&refs);
        assert_eq!(results.len(), 10_000);
        // Deterministic
        assert_eq!(results[0], cpu_batch_commit(&[items[0].as_slice()])[0]);
    }

    #[test]
    fn test_gpu_probe() {
        let info = probe_gpu();
        // Just check it doesn't crash — GPU may or may not be available in CI
        println!("GPU probe: {:?}", info);
    }

    #[test]
    fn test_cpu_batch_domain() {
        let items: Vec<(u8, Vec<u8>)> = (0..1000u32)
            .map(|i| (0x01, i.to_le_bytes().to_vec()))
            .collect();
        let refs: Vec<(u8, &[u8])> = items.iter().map(|(d, b)| (*d, b.as_slice())).collect();
        let results = cpu_batch_commit_domain(&refs);
        assert_eq!(results.len(), 1000);
    }

    #[test]
    fn test_gpu_batch_commit() {
        // Use 5000 items (above GPU_MIN_BATCH=4096) to exercise GPU path
        let items: Vec<[u8; 128]> = (0..5000u32)
            .map(|i| {
                let mut buf = [0u8; 128];
                buf[..4].copy_from_slice(&i.to_le_bytes());
                buf
            })
            .collect();
        let refs: Vec<&[u8]> = items.iter().map(|b| b.as_slice()).collect();

        let cpu_results = cpu_batch_commit(&refs);
        let gpu_results = gpu_batch_commit(&refs);

        match gpu_results {
            Ok(gpu_hashes) => {
                assert_eq!(gpu_hashes.len(), cpu_results.len());
                let mut mismatches = 0;
                for (i, (cpu, gpu)) in cpu_results.iter().zip(gpu_hashes.iter()).enumerate() {
                    if cpu != gpu {
                        mismatches += 1;
                        if mismatches <= 5 {
                            eprintln!("Mismatch at index {i}: CPU={} GPU={}",
                                hex::encode(cpu.0), hex::encode(gpu.0));
                        }
                    }
                }
                assert_eq!(mismatches, 0, "{mismatches} hash mismatches between CPU and GPU");
                println!("GPU batch commit: all {0} hashes match CPU", gpu_hashes.len());
            }
            Err(GpuError::NoAdapter) | Err(GpuError::Unavailable) => {
                println!("GPU not available, skipping GPU verification");
            }
            Err(e) => panic!("GPU batch commit failed: {e}"),
        }
    }

    #[test]
    fn test_gpu_merkle_root_basic() {
        // 4 leaves → 2 parents → 1 root
        let leaves: Vec<Hash256> = (0..4u32)
            .map(|i| arc_crypto::hash_bytes(&i.to_le_bytes()))
            .collect();
        let root = gpu_merkle_root(&leaves).unwrap();
        // Should be deterministic
        let root2 = gpu_merkle_root(&leaves).unwrap();
        assert_eq!(root, root2);
        // Should differ from leaves
        assert_ne!(root, leaves[0]);
    }

    #[test]
    fn test_gpu_merkle_root_single() {
        let leaf = arc_crypto::hash_bytes(b"hello");
        let root = gpu_merkle_root(&[leaf]).unwrap();
        assert_eq!(root, leaf);
    }

    #[test]
    fn test_gpu_merkle_root_empty() {
        let root = gpu_merkle_root(&[]).unwrap();
        assert_eq!(root, Hash256([0u8; 32]));
    }

    #[test]
    fn test_gpu_batch_verify_ed25519() {
        use ed25519_dalek::Signer;
        let mut rng = rand::thread_rng();

        // Generate 100 keypairs and sign messages
        let keypairs: Vec<ed25519_dalek::SigningKey> = (0..100)
            .map(|_| ed25519_dalek::SigningKey::generate(&mut rng))
            .collect();
        let messages: Vec<Vec<u8>> = (0..100u32)
            .map(|i| format!("message {}", i).into_bytes())
            .collect();
        let sigs: Vec<ed25519_dalek::Signature> = keypairs
            .iter()
            .zip(messages.iter())
            .map(|(kp, msg)| kp.sign(msg))
            .collect();
        let vks: Vec<ed25519_dalek::VerifyingKey> =
            keypairs.iter().map(|kp| kp.verifying_key()).collect();
        let msg_refs: Vec<&[u8]> = messages.iter().map(|m| m.as_slice()).collect();

        let results = gpu_batch_verify_ed25519(&msg_refs, &sigs, &vks);
        assert_eq!(results.len(), 100);
        assert!(results.iter().all(|&v| v), "all signatures should be valid");
    }

    #[test]
    fn test_gpu_batch_verify_detects_invalid() {
        use ed25519_dalek::Signer;
        let mut rng = rand::thread_rng();

        let kp = ed25519_dalek::SigningKey::generate(&mut rng);
        let msg = b"hello";
        let bad_msg = b"world";
        let sig = kp.sign(msg.as_slice());
        let vk = kp.verifying_key();

        // Valid then invalid
        let results = gpu_batch_verify_ed25519(
            &[msg.as_slice(), bad_msg.as_slice()],
            &[sig, sig],
            &[vk, vk],
        );
        assert!(results[0]);
        assert!(!results[1]);
    }

    #[test]
    fn test_gpu_fallback_small_batch() {
        // Below GPU_MIN_BATCH should use CPU fallback
        let items: Vec<[u8; 64]> = (0..100u32)
            .map(|i| {
                let mut buf = [0u8; 64];
                buf[..4].copy_from_slice(&i.to_le_bytes());
                buf
            })
            .collect();
        let refs: Vec<&[u8]> = items.iter().map(|b| b.as_slice()).collect();

        let cpu_results = cpu_batch_commit(&refs);
        let gpu_results = gpu_batch_commit(&refs).expect("small batch should always succeed via CPU fallback");
        assert_eq!(cpu_results, gpu_results);
    }
}
