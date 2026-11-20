use crate::cached_integer_model::{
    CachedIntegerModel, CachedLayer, I8Weights, KVCache, ModelConfig,
};
use crate::integer_lut::*;
use std::collections::HashMap;
#[cfg(feature = "candle")]
use tracing::debug;
use tracing::info;

#[derive(Debug, Clone)]
pub struct MemoryTierConfig {
    pub vram_bytes: u64,
    pub ram_bytes: u64,
    pub ssd_bytes: u64,
    pub ssd_bandwidth: u64,
    pub vram_layer_capacity: usize,
    pub ram_layer_capacity: usize,
}

impl MemoryTierConfig {
    pub fn detect(bytes_per_layer: u64) -> Self {
        let ram_bytes = Self::estimate_available_ram();
        let ssd_bytes = Self::estimate_available_ssd();
        let vram_bytes = Self::estimate_available_vram();

        let ssd_bandwidth = 3_500_000_000u64;

        let vram_layer_capacity = if bytes_per_layer > 0 {
            ((vram_bytes as f64 * 0.8) / bytes_per_layer as f64) as usize
        } else {
            0
        };

        let ram_layer_capacity = if bytes_per_layer > 0 {
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
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("sysctl")
                .args(["-n", "hw.memsize"])
                .output()
                .ok();
            if let Some(out) = output
                && let Ok(s) = String::from_utf8(out.stdout)
                && let Ok(bytes) = s.trim().parse::<u64>()
            {
                return bytes;
            }
            16 * 1024 * 1024 * 1024
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
        500 * 1024 * 1024 * 1024
    }

    fn estimate_available_vram() -> u64 {
        0
    }

    pub fn assign_tiers(&self, n_layers: usize) -> Vec<MemoryTier> {
        let mut tiers = Vec::with_capacity(n_layers);
        for i in 0..n_layers {
            if i < self.vram_layer_capacity {
                tiers.push(MemoryTier::Vram);
            } else if i < self.vram_layer_capacity + self.ram_layer_capacity {
                tiers.push(MemoryTier::Ram);
            } else {
                tiers.push(MemoryTier::Ssd);
            }
        }
        tiers
    }

    pub fn estimate_ssd_latency_ms(&self, ssd_layers: usize, bytes_per_layer: u64) -> f64 {
        if self.ssd_bandwidth == 0 || ssd_layers == 0 {
            return 0.0;
        }
        let total_bytes = ssd_layers as u64 * bytes_per_layer;
        (total_bytes as f64 / self.ssd_bandwidth as f64) * 1000.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    Vram,
    Ram,
    Ssd,
}

pub struct StreamingLayerCache {
    cache: HashMap<usize, CachedLayer>,
    #[allow(dead_code)]
    lru_order: Vec<usize>,
    capacity: usize,
    #[allow(dead_code)]
    config: ModelConfig,
    #[allow(dead_code)]
    gguf_path: String,
    pub evictions: u64,
    pub disk_loads: u64,
}

impl StreamingLayerCache {
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

    #[cfg(feature = "candle")]
    pub fn get_layer(&mut self, layer_idx: usize) -> Result<&CachedLayer, crate::InferenceError> {
        if self.cache.contains_key(&layer_idx) {
            self.lru_order.retain(|&x| x != layer_idx);
            self.lru_order.push(layer_idx);
            return Ok(self.cache.get(&layer_idx).unwrap());
        }

        self.load_layer_from_disk(layer_idx)?;
        Ok(self.cache.get(&layer_idx).unwrap())
    }

    #[cfg(feature = "candle")]
    fn load_layer_from_disk(&mut self, layer_idx: usize) -> Result<(), crate::InferenceError> {
        use candle_core::Device;
        use candle_core::quantized::gguf_file;

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

        let extract_i8 = |reader: &mut std::fs::File,
                          name: &str,
                          rows: usize,
                          cols: usize|
         -> Result<I8Weights, crate::InferenceError> {
            let qt = content
                .tensor(reader, name, &device)
                .map_err(|e| crate::InferenceError::Runtime(format!("{name}: {e}")))?;
            let deq = qt
                .dequantize(&device)
                .map_err(|e| crate::InferenceError::Runtime(format!("dequant {name}: {e}")))?;
            let f = deq
                .flatten_all()
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
            wk: extract_i8(
                &mut reader,
                &format!("{prefix}.attn_k.weight"),
                self.config.d_kv,
                d,
            )?,
            wv: extract_i8(
                &mut reader,
                &format!("{prefix}.attn_v.weight"),
                self.config.d_kv,
                d,
            )?,
            wo: extract_i8(&mut reader, &format!("{prefix}.attn_output.weight"), d, d)?,
            w_gate: extract_i8(
                &mut reader,
                &format!("{prefix}.ffn_gate.weight"),
                self.config.d_ff,
                d,
            )?,
            w_up: extract_i8(
                &mut reader,
                &format!("{prefix}.ffn_up.weight"),
                self.config.d_ff,
                d,
            )?,
            w_down: extract_i8(
                &mut reader,
                &format!("{prefix}.ffn_down.weight"),
                d,
                self.config.d_ff,
            )?,
            attn_norm: extract_norm(&mut reader, &format!("{prefix}.attn_norm.weight"), d),
            ffn_norm: extract_norm(&mut reader, &format!("{prefix}.ffn_norm.weight"), d),
        };

        self.cache.insert(layer_idx, layer);
        self.lru_order.push(layer_idx);
        self.disk_loads += 1;
        debug!(
            layer = layer_idx,
            cached = self.cache.len(),
            "StreamingLayerCache: loaded layer from disk"
        );

        Ok(())
    }

    #[cfg(not(feature = "candle"))]
    pub fn get_layer(&mut self, _layer_idx: usize) -> Result<&CachedLayer, crate::InferenceError> {
        Err(crate::InferenceError::Runtime(
            "candle feature required for streaming".into(),
        ))
    }

    pub fn cached_count(&self) -> usize {
        self.cache.len()
    }

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

    pub fn is_cached(&self, layer_idx: usize) -> bool {
        self.cache.contains_key(&layer_idx)
    }
}

#[derive(Debug, Clone)]
pub struct SpeculativeResult {
    pub accepted_tokens: Vec<u32>,
    pub proposed: usize,
    pub accepted: usize,
    pub acceptance_rate: f64,
    pub draft_ms: u64,
    pub verify_ms: u64,
}

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

    for &tok in prompt_tokens {
        draft.forward_one_token(tok, &mut draft_cache);
        target.forward_one_token(tok, &mut target_cache);
    }

    while accepted_tokens.len() < max_tokens as usize {
        let t0 = std::time::Instant::now();
        let mut draft_tokens: Vec<u32> = Vec::with_capacity(speculation_depth);
        let mut draft_input = *all_tokens.last().unwrap_or(&1);

        let _draft_seq_before = draft_cache.seq_len;

        for _ in 0..speculation_depth {
            let logits = draft.forward_one_token(draft_input, &mut draft_cache);
            if logits.is_empty() {
                break;
            }
            let tok = crate::integer_lut::argmax_i64(&logits) as u32;
            draft_tokens.push(tok);
            draft_input = tok;
            if draft.config.eos_tokens.contains(&tok) {
                break;
            }
        }
        draft_ms += t0.elapsed().as_millis() as u64;

        if draft_tokens.is_empty() {
            break;
        }
        total_proposed += draft_tokens.len();

        let t1 = std::time::Instant::now();
        let mut accepted_this_round = 0usize;
        let mut rejected = false;

        for &draft_tok in draft_tokens.iter() {
            let verify_input = *all_tokens.last().unwrap_or(&1);
            let target_logits = target.forward_one_token(verify_input, &mut target_cache);

            if target_logits.is_empty() {
                break;
            }
            let target_tok = crate::integer_lut::argmax_i64(&target_logits) as u32;

            if target_tok == draft_tok {
                accepted_tokens.push(target_tok);
                all_tokens.push(target_tok);
                accepted_this_round += 1;

                if target.config.eos_tokens.contains(&target_tok) {
                    break;
                }
                if accepted_tokens.len() >= max_tokens as usize {
                    break;
                }
            } else {
                accepted_tokens.push(target_tok);
                all_tokens.push(target_tok);
                accepted_this_round += 1;
                rejected = true;

                draft_cache = KVCache::new(draft.config.n_layers);
                for &t in &all_tokens {
                    draft.forward_one_token(t, &mut draft_cache);
                }
                break;
            }
        }
        verify_ms += t1.elapsed().as_millis() as u64;
        total_accepted += accepted_this_round;

        if !rejected && accepted_this_round < draft_tokens.len() {
            draft_cache = KVCache::new(draft.config.n_layers);
            for &t in &all_tokens {
                draft.forward_one_token(t, &mut draft_cache);
            }
        }

        if accepted_tokens
            .last()
            .map(|t| target.config.eos_tokens.contains(t))
            .unwrap_or(false)
        {
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

#[derive(Debug, Clone)]
pub struct ExpertSelection {
    pub expert_indices: Vec<u32>,
    pub weights: Vec<i64>,
}

pub fn select_experts(gate_weights: &I8Weights, hidden: &[i64], top_k: usize) -> ExpertSelection {
    let num_experts = gate_weights.n_rows;
    let d = gate_weights.n_cols;

    let mut logits = vec![0i64; num_experts];
    for (row, logit) in logits.iter_mut().enumerate() {
        let mut acc: i64 = 0;
        let scale = gate_weights.scales[row];
        for (col, &h) in hidden[..d].iter().enumerate() {
            acc += (gate_weights.data[row * d + col] as i64) * h;
        }
        *logit = ((acc / 127) * scale) >> FRAC_BITS;
    }

    let mut indexed: Vec<(usize, i64)> = logits.iter().enumerate().map(|(i, &v)| (i, v)).collect();
    indexed.sort_by_key(|e| std::cmp::Reverse(e.1));
    indexed.truncate(top_k);

    let selected_logits: Vec<i64> = indexed.iter().map(|(_, v)| *v).collect();
    let weights = crate::integer_lut::softmax_i64(&selected_logits);

    ExpertSelection {
        expert_indices: indexed.iter().map(|(i, _)| *i as u32).collect(),
        weights,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cached_integer_model::compute_rope_tables;

    fn make_test_config(n_layers: usize, d_model: usize) -> ModelConfig {
        let d_head = 64;
        let n_heads = d_model / d_head;
        let (rope_cos, rope_sin) = compute_rope_tables(d_head, 256, 10000.0);
        ModelConfig {
            n_layers,
            d_model,
            n_heads,
            n_kv_heads: n_heads,
            d_head,
            d_kv: d_model,
            d_ff: d_model * 4,
            vocab_size: 64,
            attn_scale: (ONE as f64 / (d_head as f64).sqrt()).round() as i64,
            rope_cos,
            rope_sin,
            max_seq: 256,
            eos_tokens: vec![2],
            bos_token: 1,
            chat_template: String::new(),
            arithmetic_profile: crate::cached_integer_model::ArithmeticProfile::LegacySplitHalfV0,
        }
    }

    fn make_random_i8_weights(rows: usize, cols: usize, seed: u64) -> I8Weights {
        let mut data = vec![0i8; rows * cols];
        let mut rng = seed;
        for v in data.iter_mut() {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *v = ((rng >> 56) as i8).clamp(-5, 5);
        }
        let scales = vec![ONE / 10; rows];
        I8Weights {
            data,
            scales,
            n_rows: rows,
            n_cols: cols,
        }
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
        let tiers = config.assign_tiers(80);
        assert_eq!(tiers.iter().filter(|&&t| t == MemoryTier::Vram).count(), 0);
        assert_eq!(tiers.iter().filter(|&&t| t == MemoryTier::Ram).count(), 8);
        assert_eq!(tiers.iter().filter(|&&t| t == MemoryTier::Ssd).count(), 72);
    }

    #[allow(clippy::float_cmp)]
    #[test]
    fn test_ssd_latency_estimate() {
        let config = MemoryTierConfig {
            vram_bytes: 0,
            ram_bytes: 16 * 1024 * 1024 * 1024,
            ssd_bytes: 500 * 1024 * 1024 * 1024,
            ssd_bandwidth: 3_500_000_000,
            vram_layer_capacity: 0,
            ram_layer_capacity: 8,
        };
        let latency = config.estimate_ssd_latency_ms(10, 500_000_000);
        assert!(latency > 1400.0 && latency < 1500.0, "latency={}", latency);
        assert_eq!(config.estimate_ssd_latency_ms(0, 500_000_000), 0.0);
    }

    #[test]
    fn test_streaming_cache_insert_and_evict() {
        let config = make_test_config(32, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;
        let mut cache = StreamingLayerCache::new(config, "/dev/null".to_string(), 3);

        cache.insert_layer(0, make_random_layer(d, d_ff, 100));
        cache.insert_layer(1, make_random_layer(d, d_ff, 200));
        cache.insert_layer(2, make_random_layer(d, d_ff, 300));
        assert_eq!(cache.cached_count(), 3);
        assert!(cache.is_cached(0));
        assert!(cache.is_cached(1));
        assert!(cache.is_cached(2));
        assert_eq!(cache.evictions, 0);

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

        cache.insert_layer(0, make_random_layer(d, d_ff, 100));
        cache.insert_layer(3, make_random_layer(d, d_ff, 400));
        assert!(
            cache.is_cached(0),
            "layer 0 was recently accessed, should survive"
        );
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

    #[test]
    fn test_expert_selection_basic() {
        let gate = I8Weights {
            data: vec![10, 0, 0, 0, 0, 10, 0, 0, 0, 0, 10, 0, 0, 0, 0, 10],
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
        assert!(
            sel1.expert_indices != sel2.expert_indices || sel1.weights != sel2.weights,
            "Different inputs should produce different expert selections"
        );
    }

    #[allow(clippy::float_cmp)]
    #[test]
    fn test_speculative_decode_identical_models() {
        let config = make_test_config(2, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;

        let mut layers = Vec::new();
        for i in 0..2 {
            layers.push(make_random_layer(d, d_ff, 1000 + i as u64));
        }

        let mut rng = 42u64;
        let embedding_q16: Vec<i64> = (0..config.vocab_size * d)
            .map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((rng >> 48) as i64) * 16
            })
            .collect();

        let model = CachedIntegerModel {
            config: config.clone(),
            embedding_q16: embedding_q16.clone(),
            embedding_i8: I8Weights::empty(),
            layers,
            final_norm: vec![ONE; d],
            output_weight: make_random_i8_weights(config.vocab_size, d, 9999),
            vocab: (0..config.vocab_size)
                .map(|i| format!("tok_{}", i))
                .collect(),
            q4_layers: None,
            q4_output: None,
            i16_layers: None,
            i16_output: None,
            block_i8_layers: None,
            block_i8_output: None,
            ternary_layers: None,
            ternary_output: None,
            ternary_hybrid_layers: None,
            ternary_hybrid_output: None,
        };

        let result = speculative_decode(&model, &model, &[1, 5, 10], 8, 4);
        assert!(
            !result.accepted_tokens.is_empty(),
            "Should produce at least 1 token"
        );
        assert!(
            result.accepted_tokens.len() <= 8,
            "Should respect max_tokens"
        );
        assert_eq!(
            result.acceptance_rate, 1.0,
            "Identical models should have 100% acceptance rate, got {}",
            result.acceptance_rate
        );
    }

    #[test]
    fn test_speculative_decode_different_models() {
        let config = make_test_config(2, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;

        let mut rng = 42u64;
        let embedding_q16: Vec<i64> = (0..config.vocab_size * d)
            .map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((rng >> 48) as i64) * 16
            })
            .collect();

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
                vocab: (0..config.vocab_size)
                    .map(|i| format!("tok_{}", i))
                    .collect(),
                q4_layers: None,
                q4_output: None,
                i16_layers: None,
                i16_output: None,
                block_i8_layers: None,
                block_i8_output: None,
                ternary_layers: None,
                ternary_output: None,
                ternary_hybrid_layers: None,
                ternary_hybrid_output: None,
            }
        };

        let draft = make_model(1000);
        let target = make_model(2000);

        let result = speculative_decode(&draft, &target, &[1, 5], 6, 3);
        assert!(
            !result.accepted_tokens.is_empty(),
            "Should produce at least 1 token"
        );
        assert!(
            result.accepted_tokens.len() <= 6,
            "Should respect max_tokens"
        );
        assert!(result.proposed > 0, "Draft should propose tokens");
        assert!(result.accepted > 0, "Should accept at least some tokens");
    }

    #[test]
    fn test_speculative_decode_deterministic() {
        let config = make_test_config(2, 128);
        let d = config.d_model;
        let d_ff = config.d_ff;

        let mut rng = 42u64;
        let embedding_q16: Vec<i64> = (0..config.vocab_size * d)
            .map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((rng >> 48) as i64) * 16
            })
            .collect();

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
                vocab: (0..config.vocab_size)
                    .map(|i| format!("tok_{}", i))
                    .collect(),
                q4_layers: None,
                q4_output: None,
                i16_layers: None,
                i16_output: None,
                block_i8_layers: None,
                block_i8_output: None,
                ternary_layers: None,
                ternary_output: None,
                ternary_hybrid_layers: None,
                ternary_hybrid_output: None,
            }
        };

        let draft = make_model(3000);
        let target = make_model(4000);

        let r1 = speculative_decode(&draft, &target, &[1, 3], 5, 3);
        let r2 = speculative_decode(&draft, &target, &[1, 3], 5, 3);

        assert_eq!(
            r1.accepted_tokens, r2.accepted_tokens,
            "Speculative decode must be deterministic"
        );
        assert_eq!(r1.proposed, r2.proposed);
        assert_eq!(r1.accepted, r2.accepted);
    }
}
