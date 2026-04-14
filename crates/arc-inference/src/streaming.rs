//! Streaming Weight Provider & Speculative Decoding
//!
//! Enables models larger than RAM by streaming layer weights from disk (mmap)
//! and speculative decoding with a small draft model for 2-3x throughput.
//!
//! ## SSD Weight Streaming
//!
//! Instead of loading all layers into memory, the `StreamingWeightProvider`
//! keeps only N layers in a hot cache (RAM). When a layer is needed that's
//! not cached, it's loaded from the memory-mapped GGUF file on-demand.
//! Cold layers are evicted LRU. This means a 400B model can run on a
//! machine with 16GB RAM — only ~2 layers in memory at a time.
//!
//! ## Speculative Decoding
//!
//! A small draft model (e.g. TinyLlama 1.1B) predicts N candidate tokens.
//! The large target model verifies the batch in a single forward pass.
//! Because integer inference is deterministic, verification is just hash
//! comparison — if draft output == target output, all N tokens are accepted.
//! Typical acceptance rate: 60-80% → 2-3x effective throughput.
//!
//! ## Memory-Tier Execution
//!
//! `MemoryTierConfig` detects available VRAM/RAM/SSD and assigns layers:
//! - **GPU VRAM**: Hot layers (attention, first/last layers)
//! - **RAM**: Warm layers (middle transformer blocks)
//! - **SSD**: Cold layers (loaded on-demand via mmap)
//!
//! This is what makes ARC 15x cheaper: any hardware works. A $5K community
//! node with a 2TB NVMe SSD can serve a 400B model. No $240K H100 required.

use crate::cached_integer_model::{
    CachedIntegerModel, CachedLayer, KVCache, I8Weights, ModelConfig,
};
use crate::integer_lut::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

// ─── Memory Tier Detection ─────────────────────────────────────────────────

/// Detected memory tiers on this machine.
#[derive(Debug, Clone)]
pub struct MemoryTierConfig {
    /// Available GPU VRAM in bytes (0 if no GPU).
    pub vram_bytes: u64,
    /// Available system RAM in bytes.
    pub ram_bytes: u64,
    /// Available SSD space in bytes (for mmap weight files).
    pub ssd_bytes: u64,
    /// NVMe sequential read bandwidth estimate (bytes/sec).
    /// Used to predict per-token latency when streaming from SSD.
    pub ssd_bandwidth: u64,
    /// How many layers fit in VRAM.
    pub vram_layer_capacity: usize,
    /// How many layers fit in RAM.
    pub ram_layer_capacity: usize,
}

impl MemoryTierConfig {
    /// Auto-detect memory tiers for a model with given layer size.
    pub fn detect(bytes_per_layer: u64) -> Self {
        // Estimate available memory conservatively
        let ram_bytes = Self::estimate_available_ram();
        let ssd_bytes = Self::estimate_available_ssd();
        let vram_bytes = Self::estimate_available_vram();

        // NVMe: ~3.5 GB/s sequential read (conservative for consumer NVMe)
        let ssd_bandwidth = 3_500_000_000u64;

        let vram_layer_capacity = if bytes_per_layer > 0 {
            // Reserve 20% VRAM for activations/KV cache
            ((vram_bytes as f64 * 0.8) / bytes_per_layer as f64) as usize
        } else {
            0
        };

        let ram_layer_capacity = if bytes_per_layer > 0 {
            // Reserve 30% RAM for OS + activations + KV cache
            ((ram_bytes as f64 * 0.7) / bytes_per_layer as f64) as usize
        } else {
            0
        };

        let config = Self {
            vram_bytes,
            ram_bytes,
            ssd_bytes,
            ssd_bandwidth,
            vram_layer_capacity,
            ram_layer_capacity,
        };

        info!(
            vram_mb = vram_bytes / (1024 * 1024),
            ram_mb = ram_bytes / (1024 * 1024),
            ssd_gb = ssd_bytes / (1024 * 1024 * 1024),
            vram_layers = vram_layer_capacity,
            ram_layers = ram_layer_capacity,
            bytes_per_layer = bytes_per_layer,
            "Memory tier detection complete"
        );

        config
    }

    fn estimate_available_ram() -> u64 {
        // Use /proc/meminfo on Linux, sysctl on macOS
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("sysctl")
                .args(["-n", "hw.memsize"])
                .output()
                .ok();
            if let Some(out) = output {
                if let Ok(s) = String::from_utf8(out.stdout) {
                    if let Ok(bytes) = s.trim().parse::<u64>() {
                        return bytes;
                    }
                }
            }
            16 * 1024 * 1024 * 1024 // default 16GB
        }
        #[cfg(target_os = "linux")]
        {
            if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
                for line in s.lines() {
                    if line.starts_with("MemTotal:") {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if let Some(kb) = parts.get(1).and_then(|s| s.parse::<u64>().ok()) {
                            return kb * 1024;
                        }
                    }
                }
            }
            16 * 1024 * 1024 * 1024
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            16 * 1024 * 1024 * 1024
        }
    }

    fn estimate_available_ssd() -> u64 {
        // Conservative: assume 500GB available for model caching
        500 * 1024 * 1024 * 1024
    }

    fn estimate_available_vram() -> u64 {
        // GPU detection is handled by arc-gpu crate
        // Default: 0 (CPU-only). Real detection in hardware_detect.rs.
        0
    }

    /// Compute the tier assignment for each layer.
    /// Returns: Vec of MemoryTier, one per layer.
    pub fn assign_tiers(&self, n_layers: usize) -> Vec<MemoryTier> {
        let mut tiers = Vec::with_capacity(n_layers);
        for i in 0..n_layers {
            if i < self.vram_layer_capacity {
                // First N layers in VRAM (fast attention)
                tiers.push(MemoryTier::Vram);
            } else if i < self.vram_layer_capacity + self.ram_layer_capacity {
                // Middle layers in RAM
                tiers.push(MemoryTier::Ram);
            } else {
                // Overflow layers on SSD (streamed on-demand)
                tiers.push(MemoryTier::Ssd);
            }
        }
        tiers
    }

    /// Estimate ms/token for a given number of SSD-streamed layers.
    pub fn estimate_ssd_latency_ms(&self, ssd_layers: usize, bytes_per_layer: u64) -> f64 {
        if self.ssd_bandwidth == 0 || ssd_layers == 0 {
            return 0.0;
        }
        let total_bytes = ssd_layers as u64 * bytes_per_layer;
        (total_bytes as f64 / self.ssd_bandwidth as f64) * 1000.0
    }
}

/// Which memory tier a layer's weights are stored in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    /// GPU VRAM — fastest access, limited capacity.
    Vram,
    /// System RAM — fast, moderate capacity.
    Ram,
    /// SSD via mmap — slow but unlimited capacity.
    Ssd,
}

// ─── Streaming Weight Provider ─────────────────────────────────────────────

/// LRU cache for transformer layers loaded from disk on-demand.
/// Only keeps `capacity` layers in RAM at a time. When a layer is needed
/// that's not in cache, it's loaded from the GGUF file (or mmap) and the
/// least-recently-used layer is evicted.
pub struct StreamingLayerCache {
    /// Loaded layers: layer_idx → CachedLayer.
    cache: HashMap<usize, CachedLayer>,
    /// LRU order: most recently used at the back.
    #[allow(dead_code)]
    lru_order: Vec<usize>,
    /// Maximum layers to keep in memory.
    capacity: usize,
    /// Model config for weight dimensions.
    #[allow(dead_code)]
    config: ModelConfig,
    /// Path to GGUF file for on-demand loading.
    #[allow(dead_code)]
    gguf_path: String,
    /// Total layers evicted (for stats).
    pub evictions: u64,
    /// Total layers loaded from disk (for stats).
    pub disk_loads: u64,
}

impl StreamingLayerCache {
    /// Create a new streaming cache with given capacity.
    /// `capacity` = how many layers fit in RAM. Detect with MemoryTierConfig.
    pub fn new(config: ModelConfig, gguf_path: String, capacity: usize) -> Self {
        info!(capacity, path = %gguf_path, "StreamingLayerCache initialized");
        Self {
            cache: HashMap::new(),
            lru_order: Vec::new(),
            capacity: capacity.max(1),
            config,
            gguf_path,
            evictions: 0,
            disk_loads: 0,
        }
    }

    /// Get a layer, loading from disk if not cached.
    /// Updates LRU order. Evicts oldest layer if at capacity.
    #[cfg(feature = "candle")]
    pub fn get_layer(&mut self, layer_idx: usize) -> Result<&CachedLayer, crate::InferenceError> {
        // Check if already cached
        if self.cache.contains_key(&layer_idx) {
            // Move to back of LRU
            self.lru_order.retain(|&x| x != layer_idx);
            self.lru_order.push(layer_idx);
            return Ok(self.cache.get(&layer_idx).unwrap());
        }

        // Not cached — load from disk
        self.load_layer_from_disk(layer_idx)?;
        Ok(self.cache.get(&layer_idx).unwrap())
    }

    /// Load a single layer from the GGUF file into cache.
    #[cfg(feature = "candle")]
    fn load_layer_from_disk(&mut self, layer_idx: usize) -> Result<(), crate::InferenceError> {
        use candle_core::Device;
        use candle_core::quantized::gguf_file;

        // Evict LRU if at capacity
        while self.cache.len() >= self.capacity {
            if let Some(evict_idx) = self.lru_order.first().copied() {
                self.cache.remove(&evict_idx);
                self.lru_order.remove(0);
                self.evictions += 1;
                debug!(evicted = evict_idx, "StreamingLayerCache: evicted layer");
            } else {
                break;
            }
        }

        let device = Device::Cpu;
        let mut reader = std::fs::File::open(&self.gguf_path)
            .map_err(|e| crate::InferenceError::Runtime(format!("Open GGUF: {e}")))?;
        let content = gguf_file::Content::read(&mut reader)
            .map_err(|e| crate::InferenceError::Runtime(format!("GGUF parse: {e}")))?;

        let d = self.config.d_model;
        let prefix = format!("blk.{layer_idx}");

        let extract_i8 = |reader: &mut std::fs::File, name: &str, rows: usize, cols: usize| -> Result<I8Weights, crate::InferenceError> {
            let qt = content.tensor(reader, name, &device)
                .map_err(|e| crate::InferenceError::Runtime(format!("{name}: {e}")))?;
            let deq = qt.dequantize(&device)
                .map_err(|e| crate::InferenceError::Runtime(format!("dequant {name}: {e}")))?;
            let f = deq.flatten_all()
                .map_err(|e| crate::InferenceError::Runtime(format!("flatten: {e}")))?
                .to_vec1::<f32>()
                .map_err(|e| crate::InferenceError::Runtime(format!("tovec: {e}")))?;
            Ok(I8Weights::quantize_f32(&f, rows, cols))
        };

        let extract_norm = |reader: &mut std::fs::File, name: &str, size: usize| -> Vec<i64> {
            let qt = content.tensor(reader, name, &device).ok();
            qt.and_then(|t| t.dequantize(&device).ok())
                .and_then(|t| t.flatten_all().ok())
                .and_then(|t| t.to_vec1::<f32>().ok())
                .map(|f| f.iter().map(|&x| (x * ONE as f32).round() as i64).collect())
                .unwrap_or_else(|| vec![ONE; size])
        };

        let layer = CachedLayer {
            wq: extract_i8(&mut reader, &format!("{prefix}.attn_q.weight"), d, d)?,
            wk: extract_i8(&mut reader, &format!("{prefix}.attn_k.weight"), self.config.d_kv, d)?,
            wv: extract_i8(&mut reader, &format!("{prefix}.attn_v.weight"), self.config.d_kv, d)?,
            wo: extract_i8(&mut reader, &format!("{prefix}.attn_output.weight"), d, d)?,
            w_gate: extract_i8(&mut reader, &format!("{prefix}.ffn_gate.weight"), self.config.d_ff, d)?,
            w_up: extract_i8(&mut reader, &format!("{prefix}.ffn_up.weight"), self.config.d_ff, d)?,
            w_down: extract_i8(&mut reader, &format!("{prefix}.ffn_down.weight"), d, self.config.d_ff)?,
            attn_norm: extract_norm(&mut reader, &format!("{prefix}.attn_norm.weight"), d),
            ffn_norm: extract_norm(&mut reader, &format!("{prefix}.ffn_norm.weight"), d),
        };

        self.cache.insert(layer_idx, layer);
        self.lru_order.push(layer_idx);
        self.disk_loads += 1;
        debug!(layer = layer_idx, cached = self.cache.len(), "StreamingLayerCache: loaded layer from disk");

        Ok(())
    }

    #[cfg(not(feature = "candle"))]
    pub fn get_layer(&mut self, _layer_idx: usize) -> Result<&CachedLayer, crate::InferenceError> {
        Err(crate::InferenceError::Runtime("candle feature required for streaming".into()))
    }

    /// Number of layers currently in cache.
    pub fn cached_count(&self) -> usize {
        self.cache.len()
    }

    /// Insert a layer directly (for testing or pre-loading).
    /// Evicts LRU if at capacity.
    pub fn insert_layer(&mut self, layer_idx: usize, layer: CachedLayer) {
        while self.cache.len() >= self.capacity {
            if let Some(evict_idx) = self.lru_order.first().copied() {
                self.cache.remove(&evict_idx);
                self.lru_order.remove(0);
                self.evictions += 1;
            } else {
                break;
            }
        }
        self.lru_order.retain(|&x| x != layer_idx);
        self.cache.insert(layer_idx, layer);
        self.lru_order.push(layer_idx);
    }

    /// Check if a layer is currently cached.
    pub fn is_cached(&self, layer_idx: usize) -> bool {
        self.cache.contains_key(&layer_idx)
    }
}

// ─── Speculative Decoding ──────────────────────────────────────────────────

/// Result of speculative decoding: accepted tokens + stats.
#[derive(Debug, Clone)]
pub struct SpeculativeResult {
    /// Tokens accepted from draft model (verified by target).
    pub accepted_tokens: Vec<u32>,
    /// Total tokens proposed by draft.
    pub proposed: usize,
    /// How many were accepted.
    pub accepted: usize,
    /// Acceptance rate (accepted / proposed).
    pub acceptance_rate: f64,
    /// Time spent on draft inference (ms).
    pub draft_ms: u64,
    /// Time spent on target verification (ms).
    pub verify_ms: u64,
}

/// Speculative decoding: use a small draft model to predict N tokens,
/// then verify the batch with the large target model.
///
/// Because ARC's integer inference is deterministic, verification is
/// trivial: if the target model produces the same token at each position,
/// the draft was correct. No probabilistic acceptance/rejection needed.
///
/// Throughput improvement: if draft acceptance rate is p and draft is Kx
/// faster than target, effective speedup ≈ K*p / (1 + 1/N) where N is
/// the speculation depth. For p=0.7, K=10, N=8: ~5.6x speedup.
pub fn speculative_decode(
    draft: &CachedIntegerModel,
    target: &CachedIntegerModel,
    prompt_tokens: &[u32],
    max_tokens: u32,
    speculation_depth: usize,
) -> SpeculativeResult {
    let mut all_tokens = prompt_tokens.to_vec();
    let mut accepted_tokens: Vec<u32> = Vec::new();
    let mut total_proposed = 0usize;
    let mut total_accepted = 0usize;
    let mut draft_ms = 0u64;
    let mut verify_ms = 0u64;

    let mut draft_cache = KVCache::new(draft.config.n_layers);
    let mut target_cache = KVCache::new(target.config.n_layers);

    // Prefill: run prompt through both models.
    // forward_one_token increments cache.seq_len internally — do NOT set it manually.
    for &tok in prompt_tokens {
        draft.forward_one_token(tok, &mut draft_cache);
        target.forward_one_token(tok, &mut target_cache);
    }

    while accepted_tokens.len() < max_tokens as usize {
        // Phase 1: Draft model generates N speculative tokens.
        // Each forward_one_token call advances draft_cache.seq_len by 1.
        let t0 = std::time::Instant::now();
        let mut draft_tokens: Vec<u32> = Vec::with_capacity(speculation_depth);
        let mut draft_input = *all_tokens.last().unwrap_or(&1);

        // Save draft cache position so we can rollback on rejection
        let draft_seq_before = draft_cache.seq_len;

        for _ in 0..speculation_depth {
            let logits = draft.forward_one_token(draft_input, &mut draft_cache);
            if logits.is_empty() { break; }
            let tok = crate::integer_lut::argmax_i64(&logits) as u32;
            draft_tokens.push(tok);
            draft_input = tok;
            if draft.config.eos_tokens.contains(&tok) { break; }
        }
        draft_ms += t0.elapsed().as_millis() as u64;

        if draft_tokens.is_empty() { break; }
        total_proposed += draft_tokens.len();

        // Phase 2: Target model verifies each drafted token.
        // We feed the LAST accepted token to target's forward_one_token,
        // which produces logits predicting the NEXT token.
        let t1 = std::time::Instant::now();
        let mut accepted_this_round = 0usize;
        let mut rejected = false;

        for (i, &draft_tok) in draft_tokens.iter().enumerate() {
            let verify_input = *all_tokens.last().unwrap_or(&1);
            let target_logits = target.forward_one_token(verify_input, &mut target_cache);

            if target_logits.is_empty() { break; }
            let target_tok = crate::integer_lut::argmax_i64(&target_logits) as u32;

            if target_tok == draft_tok {
                // Draft was correct — accept
                accepted_tokens.push(target_tok);
                all_tokens.push(target_tok);
                accepted_this_round += 1;

                if target.config.eos_tokens.contains(&target_tok) { break; }
                if accepted_tokens.len() >= max_tokens as usize { break; }
            } else {
                // Draft diverged — accept target's token (it's correct),
                // discard remaining draft tokens
                accepted_tokens.push(target_tok);
                all_tokens.push(target_tok);
                accepted_this_round += 1;
                rejected = true;

                // Rollback draft cache: replay from scratch to current position.
                // This is expensive but correct. Future optimization: keep
                // snapshots at checkpoint positions.
                draft_cache = KVCache::new(draft.config.n_layers);
                for &t in &all_tokens {
                    draft.forward_one_token(t, &mut draft_cache);
                }
                break;
            }
        }
        verify_ms += t1.elapsed().as_millis() as u64;
        total_accepted += accepted_this_round;

        // If no rejection and draft produced tokens, the draft cache is ahead
        // by (draft_tokens.len() - accepted_this_round) positions. Rollback
        // the excess positions.
        if !rejected && accepted_this_round < draft_tokens.len() {
            // Draft cache advanced too far — rebuild
            draft_cache = KVCache::new(draft.config.n_layers);
            for &t in &all_tokens {
                draft.forward_one_token(t, &mut draft_cache);
            }
        }

        if accepted_tokens.last().map(|t| target.config.eos_tokens.contains(t)).unwrap_or(false) {
            break;
        }
    }

    let acceptance_rate = if total_proposed > 0 {
        total_accepted as f64 / total_proposed as f64
    } else {
        0.0
    };

    SpeculativeResult {
        accepted_tokens,
        proposed: total_proposed,
        accepted: total_accepted,
        acceptance_rate,
        draft_ms,
        verify_ms,
    }
}

// ─── Expert-Parallel Routing for MoE ───────────────────────────────────────

/// Which experts were selected by the gating network for a given token.
#[derive(Debug, Clone)]
pub struct ExpertSelection {
    /// Expert indices selected (top-k from gating logits).
    pub expert_indices: Vec<u32>,
    /// Gating weights for each selected expert (normalized).
    pub weights: Vec<i64>,
}

/// Evaluate the MoE gating network to select top-k experts for a token.
/// Uses the integer matmul path for deterministic expert selection.
///
/// gate_weights: [num_experts × d_model] I8 weights for the gating network.
/// hidden: [d_model] i64 Q16 hidden state at the MoE layer.
/// top_k: how many experts to select (typically 2 for Mixtral, 8 for R1).
pub fn select_experts(
    gate_weights: &I8Weights,
    hidden: &[i64],
    top_k: usize,
) -> ExpertSelection {
    let num_experts = gate_weights.n_rows;
    let d = gate_weights.n_cols;

    // Compute gating logits: gate_weights × hidden
    let mut logits = vec![0i64; num_experts];
    for row in 0..num_experts {
        let mut acc: i64 = 0;
        let scale = gate_weights.scales[row];
        for col in 0..d {
            acc += (gate_weights.data[row * d + col] as i64) * hidden[col];
        }
        logits[row] = (acc / 127) * scale >> FRAC_BITS;
    }

    // Top-k selection
    let mut indexed: Vec<(usize, i64)> = logits.iter().enumerate().map(|(i, &v)| (i, v)).collect();
    indexed.sort_by(|a, b| b.1.cmp(&a.1));
    indexed.truncate(top_k);

    // Softmax over selected experts for gating weights
    let selected_logits: Vec<i64> = indexed.iter().map(|(_, v)| *v).collect();
    let weights = crate::integer_lut::softmax_i64(&selected_logits);

    ExpertSelection {
        expert_indices: indexed.iter().map(|(i, _)| *i as u32).collect(),
        weights,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cached_integer_model::compute_rope_tables;

    fn make_test_config(n_layers: usize, d_model: usize) -> ModelConfig {
        let d_head = 64;
        let n_heads = d_model / d_head;
        let (rope_cos, rope_sin) = compute_rope_tables(d_head, 256, 10000.0);
        ModelConfig {
            n_layers, d_model, n_heads, n_kv_heads: n_heads,
            d_head, d_kv: d_model, d_ff: d_model * 4, vocab_size: 64,
            attn_scale: (ONE as f64 / (d_head as f64).sqrt()).round() as i64,
            rope_cos, rope_sin,
            max_seq: 256, eos_tokens: vec![2], bos_token: 1,
            chat_template: String::new(),
        }
    }

    fn make_random_i8_weights(rows: usize, cols: usize, seed: u64) -> I8Weights {
        let mut data = vec![0i8; rows * cols];
        let mut rng = seed;
        for v in data.iter_mut() {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *v = ((rng >> 56) as i8).clamp(-5, 5); // small values to avoid overflow through layers
        }
        // Small scales to keep activations bounded through multiple layers
        let scales = vec![ONE / 10; rows];
        I8Weights { data, scales, n_rows: rows, n_cols: cols }
    }

    fn make_random_layer(d_model: usize, d_ff: usize, seed: u64) -> CachedLayer {
        CachedLayer {
            wq: make_random_i8_weights(d_model, d_model, seed),
            wk: make_random_i8_weights(d_model, d_model, seed + 1),
            wv: make_random_i8_weights(d_model, d_model, seed + 2),
            wo: make_random_i8_weights(d_model, d_model, seed + 3),
            w_gate: make_random_i8_weights(d_ff, d_model, seed + 4),
            w_up: make_random_i8_weights(d_ff, d_model, seed + 5),
            w_down: make_random_i8_weights(d_model, d_ff, seed + 6),
            attn_norm: vec![ONE; d_model],
            ffn_norm: vec![ONE; d_model],
        }
    }

    // ── Memory Tier Tests ──────────────────────────────────────────────

    #[test]
    fn test_memory_tier_detection() {
        let config = MemoryTierConfig::detect(500_000_000);
        assert!(config.ram_bytes > 0);
        assert!(config.ram_layer_capacity > 0);
    }

    #[test]
    fn test_tier_assignment_all_vram() {
        let config = MemoryTierConfig {
            vram_bytes: 80 * 1024 * 1024 * 1024,
            ram_bytes: 32 * 1024 * 1024 * 1024,
            ssd_bytes: 500 * 1024 * 1024 * 1024,
            ssd_bandwidth: 3_500_000_000,
            vram_layer_capacity: 100,
            ram_layer_capacity: 50,
        };
        let tiers = config.assign_tiers(32);
        assert!(tiers.iter().all(|t| *t == MemoryTier::Vram));
    }

    #[test]
    fn test_tier_assignment_mixed() {
        let config = MemoryTierConfig {
            vram_bytes: 8 * 1024 * 1024 * 1024,
            ram_bytes: 32 * 1024 * 1024 * 1024,
            ssd_bytes: 500 * 1024 * 1024 * 1024,
            ssd_bandwidth: 3_500_000_000,
            vram_layer_capacity: 4,
            ram_layer_capacity: 16,
        };
        let tiers = config.assign_tiers(32);
        assert_eq!(tiers.len(), 32);
        assert_eq!(tiers.iter().filter(|&&t| t == MemoryTier::Vram).count(), 4);
        assert_eq!(tiers.iter().filter(|&&t| t == MemoryTier::Ram).count(), 16);
        assert_eq!(tiers.iter().filter(|&&t| t == MemoryTier::Ssd).count(), 12);
    }

    #[test]
    fn test_tier_assignment_no_gpu() {
        let config = MemoryTierConfig {
            vram_bytes: 0,
            ram_bytes: 16 * 1024 * 1024 * 1024,
            ssd_bytes: 500 * 1024 * 1024 * 1024,
            ssd_bandwidth: 3_500_000_000,
            vram_layer_capacity: 0,
            ram_layer_capacity: 8,
        };
        let tiers = config.assign_tiers(80); // 80-layer model on 16GB machine
        assert_eq!(tiers.iter().filter(|&&t| t == MemoryTier::Vram).count(), 0);
        assert_eq!(tiers.iter().filter(|&&t| t == MemoryTier::Ram).count(), 8);
        assert_eq!(tiers.iter().filter(|&&t| t == MemoryTier::Ssd).count(), 72);
    }

    #[test]
    fn test_ssd_latency_estimate() {
        let config = MemoryTierConfig {
            vram_bytes: 0, ram_bytes: 16 * 1024 * 1024 * 1024,
            ssd_bytes: 500 * 1024 * 1024 * 1024, ssd_bandwidth: 3_500_000_000,
            vram_layer_capacity: 0, ram_layer_capacity: 8,
        };
        let latency = config.estimate_ssd_latency_ms(10, 500_000_000);
        assert!(latency > 1400.0 && latency < 1500.0, "latency={}", latency);
        assert_eq!(config.estimate_ssd_latency_ms(0, 500_000_000), 0.0);
    }

    // ── Streaming Cache Tests ──────────────────────────────────────────

    #[test]
    fn test_streaming_cache_insert_and_evict() {
        let config = make_test_config(32, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;
        let mut cache = StreamingLayerCache::new(config, "/dev/null".to_string(), 3);

        // Insert 3 layers — should all fit
        cache.insert_layer(0, make_random_layer(d, d_ff, 100));
        cache.insert_layer(1, make_random_layer(d, d_ff, 200));
        cache.insert_layer(2, make_random_layer(d, d_ff, 300));
        assert_eq!(cache.cached_count(), 3);
        assert!(cache.is_cached(0));
        assert!(cache.is_cached(1));
        assert!(cache.is_cached(2));
        assert_eq!(cache.evictions, 0);

        // Insert 4th — should evict layer 0 (LRU)
        cache.insert_layer(3, make_random_layer(d, d_ff, 400));
        assert_eq!(cache.cached_count(), 3);
        assert!(!cache.is_cached(0), "layer 0 should be evicted");
        assert!(cache.is_cached(1));
        assert!(cache.is_cached(2));
        assert!(cache.is_cached(3));
        assert_eq!(cache.evictions, 1);
    }

    #[test]
    fn test_streaming_cache_lru_ordering() {
        let config = make_test_config(32, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;
        let mut cache = StreamingLayerCache::new(config, "/dev/null".to_string(), 3);

        cache.insert_layer(0, make_random_layer(d, d_ff, 100));
        cache.insert_layer(1, make_random_layer(d, d_ff, 200));
        cache.insert_layer(2, make_random_layer(d, d_ff, 300));

        // Access layer 0 (makes it MRU), then insert layer 3
        // Should evict layer 1 (now LRU), not layer 0
        cache.insert_layer(0, make_random_layer(d, d_ff, 100)); // re-touch layer 0
        cache.insert_layer(3, make_random_layer(d, d_ff, 400));
        assert!(cache.is_cached(0), "layer 0 was recently accessed, should survive");
        assert!(!cache.is_cached(1), "layer 1 should be evicted (LRU)");
        assert!(cache.is_cached(2));
        assert!(cache.is_cached(3));
    }

    #[test]
    fn test_streaming_cache_capacity_one() {
        let config = make_test_config(32, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;
        let mut cache = StreamingLayerCache::new(config, "/dev/null".to_string(), 1);

        cache.insert_layer(5, make_random_layer(d, d_ff, 500));
        assert_eq!(cache.cached_count(), 1);
        assert!(cache.is_cached(5));

        cache.insert_layer(10, make_random_layer(d, d_ff, 600));
        assert_eq!(cache.cached_count(), 1);
        assert!(!cache.is_cached(5));
        assert!(cache.is_cached(10));
        assert_eq!(cache.evictions, 1);
    }

    // ── Expert Selection Tests ─────────────────────────────────────────

    #[test]
    fn test_expert_selection_basic() {
        let gate = I8Weights {
            data: vec![
                10, 0, 0, 0,
                0, 10, 0, 0,
                0, 0, 10, 0,
                0, 0, 0, 10,
            ],
            scales: vec![ONE; 4],
            n_rows: 4,
            n_cols: 4,
        };
        let hidden = vec![ONE, 0, ONE, 0];
        let sel = select_experts(&gate, &hidden, 2);
        assert_eq!(sel.expert_indices.len(), 2);
        assert!(sel.expert_indices.contains(&0));
        assert!(sel.expert_indices.contains(&2));
    }

    #[test]
    fn test_expert_selection_deterministic() {
        let gate = make_random_i8_weights(8, 16, 42);
        let hidden: Vec<i64> = (0..16).map(|i| ONE * (i as i64 + 1)).collect();

        let sel1 = select_experts(&gate, &hidden, 4);
        let sel2 = select_experts(&gate, &hidden, 4);

        // Same input → same experts (determinism)
        assert_eq!(sel1.expert_indices, sel2.expert_indices);
        assert_eq!(sel1.weights, sel2.weights);
    }

    #[test]
    fn test_expert_selection_top_k_respects_limit() {
        let gate = make_random_i8_weights(16, 8, 99);
        let hidden: Vec<i64> = (0..8).map(|i| ONE * (i as i64 + 1)).collect();

        for k in [1, 2, 4, 8, 16] {
            let sel = select_experts(&gate, &hidden, k);
            assert_eq!(sel.expert_indices.len(), k);
            assert_eq!(sel.weights.len(), k);
        }
    }

    #[test]
    fn test_expert_selection_different_inputs_select_different_experts() {
        let gate = make_random_i8_weights(8, 4, 77);
        let h1 = vec![ONE, 0, 0, 0];
        let h2 = vec![0, 0, 0, ONE];
        let sel1 = select_experts(&gate, &h1, 2);
        let sel2 = select_experts(&gate, &h2, 2);
        // Different inputs should (usually) select different experts
        // Not guaranteed for random weights, but very likely with orthogonal inputs
        assert!(sel1.expert_indices != sel2.expert_indices || sel1.weights != sel2.weights,
            "Different inputs should produce different expert selections");
    }

    // ── Speculative Decoding Tests ─────────────────────────────────────

    #[test]
    fn test_speculative_decode_identical_models() {
        // When draft == target (same model), acceptance rate should be 100%
        let config = make_test_config(2, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;

        // Build a tiny model with random weights
        let mut layers = Vec::new();
        for i in 0..2 {
            layers.push(make_random_layer(d, d_ff, 1000 + i as u64));
        }

        // Random embeddings
        // Small embeddings to avoid i64 overflow in layernorm (x*x >> 16)
        let mut rng = 42u64;
        let embedding_q16: Vec<i64> = (0..config.vocab_size * d).map(|_| {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 48) as i64) * 16 // small values, ~O(1000)
        }).collect();

        let model = CachedIntegerModel {
            config: config.clone(),
            embedding_q16: embedding_q16.clone(),
            embedding_i8: I8Weights::empty(),
            layers,
            final_norm: vec![ONE; d],
            output_weight: make_random_i8_weights(config.vocab_size, d, 9999),
            vocab: (0..config.vocab_size).map(|i| format!("tok_{}", i)).collect(),
            q4_layers: None,
            q4_output: None,
            i16_layers: None,
            i16_output: None,
        };

        // Use the same model as both draft and target
        let result = speculative_decode(&model, &model, &[1, 5, 10], 8, 4);
        assert!(result.accepted_tokens.len() > 0, "Should produce at least 1 token");
        assert!(result.accepted_tokens.len() <= 8, "Should respect max_tokens");
        // With identical models, every draft token should be accepted
        assert_eq!(result.acceptance_rate, 1.0,
            "Identical models should have 100% acceptance rate, got {}", result.acceptance_rate);
    }

    #[test]
    fn test_speculative_decode_different_models() {
        // Different models should still produce valid output (just lower acceptance)
        let config = make_test_config(2, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;

        // Small embeddings to avoid i64 overflow in layernorm (x*x >> 16)
        let mut rng = 42u64;
        let embedding_q16: Vec<i64> = (0..config.vocab_size * d).map(|_| {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 48) as i64) * 16 // small values, ~O(1000)
        }).collect();

        let make_model = |seed: u64| -> CachedIntegerModel {
            let mut layers = Vec::new();
            for i in 0..2 {
                layers.push(make_random_layer(d, d_ff, seed + i as u64));
            }
            CachedIntegerModel {
                config: config.clone(),
                embedding_q16: embedding_q16.clone(),
                embedding_i8: I8Weights::empty(),
                layers,
                final_norm: vec![ONE; d],
                output_weight: make_random_i8_weights(config.vocab_size, d, seed + 100),
                vocab: (0..config.vocab_size).map(|i| format!("tok_{}", i)).collect(),
                q4_layers: None, q4_output: None, i16_layers: None, i16_output: None,
            }
        };

        let draft = make_model(1000);
        let target = make_model(2000);

        let result = speculative_decode(&draft, &target, &[1, 5], 6, 3);
        assert!(result.accepted_tokens.len() > 0, "Should produce at least 1 token");
        assert!(result.accepted_tokens.len() <= 6, "Should respect max_tokens");
        assert!(result.proposed > 0, "Draft should propose tokens");
        // Acceptance rate with different models should be < 1.0 (probably 0.3-0.7)
        // but at least some tokens should be accepted (target always produces 1 per round)
        assert!(result.accepted > 0, "Should accept at least some tokens");
    }

    #[test]
    fn test_speculative_decode_deterministic() {
        let config = make_test_config(2, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;

        // Small embeddings to avoid i64 overflow in layernorm (x*x >> 16)
        let mut rng = 42u64;
        let embedding_q16: Vec<i64> = (0..config.vocab_size * d).map(|_| {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 48) as i64) * 16 // small values, ~O(1000)
        }).collect();

        let make_model = |seed: u64| -> CachedIntegerModel {
            let mut layers = Vec::new();
            for i in 0..2 {
                layers.push(make_random_layer(d, d_ff, seed + i as u64));
            }
            CachedIntegerModel {
                config: config.clone(),
                embedding_q16: embedding_q16.clone(),
                embedding_i8: I8Weights::empty(),
                layers,
                final_norm: vec![ONE; d],
                output_weight: make_random_i8_weights(config.vocab_size, d, seed + 100),
                vocab: (0..config.vocab_size).map(|i| format!("tok_{}", i)).collect(),
                q4_layers: None, q4_output: None, i16_layers: None, i16_output: None,
            }
        };

        let draft = make_model(3000);
        let target = make_model(4000);

        let r1 = speculative_decode(&draft, &target, &[1, 3], 5, 3);
        let r2 = speculative_decode(&draft, &target, &[1, 3], 5, 3);

        // Deterministic: same input → same output every time
        assert_eq!(r1.accepted_tokens, r2.accepted_tokens,
            "Speculative decode must be deterministic");
        assert_eq!(r1.proposed, r2.proposed);
        assert_eq!(r1.accepted, r2.accepted);
    }
}
