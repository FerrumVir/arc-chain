use arc_crypto::Hash256;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardAssignment {
    pub node_address: Hash256,
    pub start_layer: u32,
    pub end_layer: u32,
    pub expert_indices: Vec<u32>,
    pub socket_addr: String,
    pub gpu_tier: u8,
    pub available_memory: u64,
}

pub struct ShardRegistry {
    models: DashMap<Hash256, Vec<ShardAssignment>>,
    node_shards: DashMap<Hash256, Vec<(Hash256, ShardAssignment)>>,
}

impl Default for ShardRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ShardRegistry {
    pub fn new() -> Self {
        Self {
            models: DashMap::new(),
            node_shards: DashMap::new(),
        }
    }

    pub fn register_shard(&self, model_id: Hash256, shard: ShardAssignment) {
        let mut model_shards = self.models.entry(model_id).or_default();
        model_shards.retain(|s| s.node_address != shard.node_address);
        model_shards.push(shard.clone());
        model_shards.sort_by_key(|s| s.start_layer);

        let mut node_entries = self.node_shards.entry(shard.node_address).or_default();
        node_entries.retain(|(mid, _)| *mid != model_id);
        node_entries.push((model_id, shard));
    }

    pub fn remove_node(&self, node_address: &Hash256) {
        if let Some((_, node_entries)) = self.node_shards.remove(node_address) {
            for (model_id, _) in &node_entries {
                if let Some(mut model_shards) = self.models.get_mut(model_id) {
                    model_shards.retain(|s| s.node_address != *node_address);
                }
            }
        }
    }

    pub fn get_pipeline(&self, model_id: &Hash256) -> Option<Vec<ShardAssignment>> {
        self.models.get(model_id).map(|v| v.clone())
    }

    pub fn next_shard_holder(
        &self,
        model_id: &Hash256,
        after_layer: u32,
    ) -> Option<ShardAssignment> {
        self.models.get(model_id).and_then(|shards| {
            shards
                .iter()
                .find(|s| s.start_layer >= after_layer)
                .cloned()
        })
    }

    pub fn is_model_fully_covered(&self, model_id: &Hash256, total_layers: u32) -> bool {
        if let Some(shards) = self.models.get(model_id) {
            if shards.is_empty() {
                return false;
            }
            let mut covered_to = 0u32;
            for shard in shards.iter() {
                if shard.start_layer > covered_to {
                    return false;
                }
                covered_to = covered_to.max(shard.end_layer);
            }
            covered_to >= total_layers
        } else {
            false
        }
    }

    pub fn get_node_shards(&self, node_address: &Hash256) -> Vec<(Hash256, ShardAssignment)> {
        self.node_shards
            .get(node_address)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub fn fully_covered_models(&self) -> Vec<Hash256> {
        self.models
            .iter()
            .filter_map(|entry| {
                let model_id = *entry.key();
                let shards = entry.value();
                if shards.is_empty() {
                    return None;
                }
                let last = shards.last().unwrap().end_layer;
                if self.is_model_fully_covered(&model_id, last) {
                    Some(model_id)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn total_shard_nodes(&self) -> usize {
        self.node_shards.len()
    }
}

pub fn compute_shard_plan(
    _model_id: Hash256,
    total_layers: u32,
    _total_params_b: f64,
    nodes: &[NodeCapability],
) -> Vec<ShardAssignment> {
    if nodes.is_empty() {
        return Vec::new();
    }

    let total_weight: f64 = nodes
        .iter()
        .map(|n| {
            let mem_weight = n.available_memory as f64;
            let gpu_bonus = if n.gpu_tier > 0 { 1.5 } else { 1.0 };
            mem_weight * gpu_bonus
        })
        .sum();

    let mut assignments = Vec::new();
    let mut current_layer = 0u32;

    for (i, node) in nodes.iter().enumerate() {
        let weight = node.available_memory as f64 * if node.gpu_tier > 0 { 1.5 } else { 1.0 };
        let fraction = weight / total_weight;

        let layer_count = if i == nodes.len() - 1 {
            total_layers - current_layer
        } else {
            ((total_layers as f64 * fraction).round() as u32).max(1)
        };

        let end_layer = (current_layer + layer_count).min(total_layers);

        assignments.push(ShardAssignment {
            node_address: node.address,
            start_layer: current_layer,
            end_layer,
            expert_indices: Vec::new(),
            socket_addr: node.socket_addr.clone(),
            gpu_tier: node.gpu_tier,
            available_memory: node.available_memory,
        });

        current_layer = end_layer;
        if current_layer >= total_layers {
            break;
        }
    }

    assignments
}

pub fn compute_expert_shard_plan(
    _model_id: Hash256,
    total_layers: u32,
    num_experts: u32,
    _top_k: u32,
    nodes: &[NodeCapability],
) -> Vec<ShardAssignment> {
    if nodes.is_empty() || num_experts == 0 {
        return Vec::new();
    }

    let experts_per_node = (num_experts as f64 / nodes.len() as f64).ceil() as u32;

    let mut assignments = Vec::new();
    let mut expert_idx = 0u32;

    for node in nodes {
        let start_expert = expert_idx;
        let end_expert = (expert_idx + experts_per_node).min(num_experts);
        let expert_indices: Vec<u32> = (start_expert..end_expert).collect();

        if !expert_indices.is_empty() {
            assignments.push(ShardAssignment {
                node_address: node.address,
                start_layer: 0,
                end_layer: total_layers,
                expert_indices,
                socket_addr: node.socket_addr.clone(),
                gpu_tier: node.gpu_tier,
                available_memory: node.available_memory,
            });
        }

        expert_idx = end_expert;
        if expert_idx >= num_experts {
            break;
        }
    }

    assignments
}

#[derive(Debug, Clone)]
pub struct NodeCapability {
    pub address: Hash256,
    pub socket_addr: String,
    pub gpu_tier: u8,
    pub available_memory: u64,
}

pub fn serialize_activations(activations: &[i64]) -> (Vec<u8>, Hash256) {
    let bytes: Vec<u8> = activations.iter().flat_map(|v| v.to_le_bytes()).collect();
    let hash = arc_crypto::hash_bytes(&bytes);
    (bytes, hash)
}

pub fn deserialize_activations(
    bytes: &[u8],
    expected_hash: &Hash256,
) -> Result<Vec<i64>, InferenceShardError> {
    let actual_hash = arc_crypto::hash_bytes(bytes);
    if actual_hash != *expected_hash {
        return Err(InferenceShardError::ActivationIntegrityFailed {
            expected: *expected_hash,
            actual: actual_hash,
        });
    }

    if !bytes.len().is_multiple_of(8) {
        return Err(InferenceShardError::InvalidActivationSize { size: bytes.len() });
    }

    let activations: Vec<i64> = bytes
        .chunks_exact(8)
        .map(|chunk| i64::from_le_bytes(chunk.try_into().unwrap()))
        .collect();

    Ok(activations)
}

#[derive(Debug, Clone)]
pub struct PipelineState {
    pub request_id: Hash256,
    pub model_id: Hash256,
    pub current_shard_idx: usize,
    pub total_shards: usize,
    pub generated_tokens: Vec<u32>,
    pub max_tokens: u32,
    pub prompt_tokens: Vec<u32>,
    pub started_at: std::time::Instant,
}

pub struct PipelineCoordinator {
    active_requests: DashMap<Hash256, PipelineState>,
    pub registry: Arc<ShardRegistry>,
    results: DashMap<Hash256, Vec<u32>>,
}

impl PipelineCoordinator {
    pub fn new(registry: Arc<ShardRegistry>) -> Self {
        Self {
            active_requests: DashMap::new(),
            registry,
            results: DashMap::new(),
        }
    }

    pub fn start_inference(
        &self,
        request_id: Hash256,
        model_id: Hash256,
        prompt_tokens: Vec<u32>,
        max_tokens: u32,
    ) -> Result<ShardAssignment, InferenceShardError> {
        let pipeline = self
            .registry
            .get_pipeline(&model_id)
            .ok_or(InferenceShardError::NoPipeline { model_id })?;

        if pipeline.is_empty() {
            return Err(InferenceShardError::NoPipeline { model_id });
        }

        let state = PipelineState {
            request_id,
            model_id,
            current_shard_idx: 0,
            total_shards: pipeline.len(),
            generated_tokens: Vec::new(),
            max_tokens,
            prompt_tokens,
            started_at: std::time::Instant::now(),
        };

        self.active_requests.insert(request_id, state);
        Ok(pipeline[0].clone())
    }

    pub fn on_shard_result(&self, request_id: &Hash256, token_id: u32) -> PipelineAction {
        let mut state = match self.active_requests.get_mut(request_id) {
            Some(s) => s,
            None => return PipelineAction::RequestNotFound,
        };

        state.generated_tokens.push(token_id);

        if state.generated_tokens.len() as u32 >= state.max_tokens || token_id == 2 {
            let tokens = state.generated_tokens.clone();
            let req_id = *request_id;
            drop(state);
            self.active_requests.remove(&req_id);
            self.results.insert(req_id, tokens.clone());
            return PipelineAction::Complete { tokens };
        }

        state.current_shard_idx = 0;
        let model_id = state.model_id;
        drop(state);

        match self.registry.get_pipeline(&model_id) {
            Some(pipeline) if !pipeline.is_empty() => PipelineAction::NextToken {
                first_shard: pipeline[0].clone(),
            },
            _ => PipelineAction::PipelineBroken,
        }
    }

    pub fn get_result(&self, request_id: &Hash256) -> Option<Vec<u32>> {
        self.results.get(request_id).map(|v| v.clone())
    }

    pub fn evict_stale(&self, timeout_secs: u64) {
        let cutoff = std::time::Duration::from_secs(timeout_secs);
        let stale: Vec<Hash256> = self
            .active_requests
            .iter()
            .filter(|e| e.value().started_at.elapsed() > cutoff)
            .map(|e| *e.key())
            .collect();
        for id in stale {
            self.active_requests.remove(&id);
        }
    }

    pub fn active_count(&self) -> usize {
        self.active_requests.len()
    }
}

#[derive(Debug)]
pub enum PipelineAction {
    Complete { tokens: Vec<u32> },
    NextToken { first_shard: ShardAssignment },
    PipelineBroken,
    RequestNotFound,
}

use super::cached_integer_model::{
    CachedIntegerModel, KVCache, QuantizedInput, apply_rope, layernorm, matmul_fast_preq, silu_i64,
};
use super::integer_lut::{FRAC_BITS, ONE, argmax_i64, integer_exp};

pub fn forward_shard_layers(
    model: &CachedIntegerModel,
    hidden_input: &[i64],
    cache: &mut KVCache,
    start_layer: usize,
    end_layer: usize,
    token_position: usize,
) -> Vec<i64> {
    let cfg = &model.config;
    let d = cfg.d_model;

    let mut hidden = hidden_input.to_vec();

    let mut q = vec![0i64; d];
    let mut k_buf = vec![0i64; cfg.d_kv];
    let mut v_buf = vec![0i64; cfg.d_kv];
    let mut attn_out = vec![0i64; d];
    let mut projected = vec![0i64; d];
    let mut gate = vec![0i64; cfg.d_ff];
    let mut up = vec![0i64; cfg.d_ff];
    let mut ff_out = vec![0i64; d];

    let end = end_layer.min(model.layers.len());

    for layer_idx in start_layer..end {
        let layer = &model.layers[layer_idx];
        let q4_layer = model.q4_layers.as_ref().map(|ql| &ql[layer_idx]);

        macro_rules! dispatch_matmul {
            ($q4w:expr, $i8w:expr, $inq:expr, $raw:expr, $in_sz:expr, $out:expr) => {{
                if let Some(q4w) = $q4w {
                    super::cached_integer_model::matmul_q4_full(q4w, $raw, $out);
                } else {
                    matmul_fast_preq($i8w, $inq, $raw, $in_sz, $out);
                }
            }};
        }

        let normed = layernorm(&hidden, &layer.attn_norm);
        let normed_q = QuantizedInput::from_i64(&normed);

        dispatch_matmul!(
            q4_layer.map(|l| &l.wq),
            &layer.wq,
            &normed_q,
            &normed,
            d,
            &mut q
        );
        dispatch_matmul!(
            q4_layer.map(|l| &l.wk),
            &layer.wk,
            &normed_q,
            &normed,
            d,
            &mut k_buf
        );
        dispatch_matmul!(
            q4_layer.map(|l| &l.wv),
            &layer.wv,
            &normed_q,
            &normed,
            d,
            &mut v_buf
        );

        for h in 0..cfg.n_heads {
            apply_rope(
                &mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                token_position,
                cfg.d_head,
                &cfg.rope_cos,
                &cfg.rope_sin,
            );
        }
        for h in 0..cfg.n_kv_heads {
            apply_rope(
                &mut k_buf[h * cfg.d_head..(h + 1) * cfg.d_head],
                token_position,
                cfg.d_head,
                &cfg.rope_cos,
                &cfg.rope_sin,
            );
        }

        cache.push_k(layer_idx, &k_buf);
        cache.push_v(layer_idx, &v_buf);

        let full_seq = token_position + 1;
        let head_results: Vec<Vec<i64>> = (0..cfg.n_heads)
            .map(|h| {
                let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                let dh = cfg.d_head;
                let q_head = &q[h * dh..(h + 1) * dh];

                let mut running_max: i64 = i64::MIN / 2;
                let mut running_sum: i64 = 0;
                let mut out = vec![0i64; dh];

                for j in 0..full_seq {
                    let k_off = j * cfg.d_kv + kv_h * dh;
                    let mut dot: i64 = 0;
                    for (dd, &qv) in q_head.iter().enumerate() {
                        dot += qv * cache.k_data[layer_idx][k_off + dd];
                    }
                    let score = ((dot >> FRAC_BITS) * cfg.attn_scale) >> FRAC_BITS;

                    if score > running_max {
                        let diff = running_max - score;
                        let correction = integer_exp(diff);
                        running_sum = (running_sum * correction) >> FRAC_BITS;
                        for o in out.iter_mut() {
                            *o = (*o * correction) >> FRAC_BITS;
                        }
                        running_max = score;
                    }

                    let w = integer_exp(score - running_max);
                    running_sum += w;

                    let v_off = j * cfg.d_kv + kv_h * dh;
                    for (dd, o) in out.iter_mut().enumerate() {
                        *o += (w * cache.v_data[layer_idx][v_off + dd]) >> FRAC_BITS;
                    }
                }

                if running_sum > 0 {
                    for o in out.iter_mut() {
                        *o = (*o * ONE) / running_sum;
                    }
                }
                out
            })
            .collect();

        for val in attn_out.iter_mut() {
            *val = 0;
        }
        for (h, head_out) in head_results.iter().enumerate() {
            attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(head_out);
        }

        let attn_out_q = QuantizedInput::from_i64(&attn_out);
        dispatch_matmul!(
            q4_layer.map(|l| &l.wo),
            &layer.wo,
            &attn_out_q,
            &attn_out,
            d,
            &mut projected
        );
        for i in 0..d {
            hidden[i] += projected[i];
        }

        let normed_ff = layernorm(&hidden, &layer.ffn_norm);
        let normed_ff_q = QuantizedInput::from_i64(&normed_ff);

        dispatch_matmul!(
            q4_layer.map(|l| &l.w_gate),
            &layer.w_gate,
            &normed_ff_q,
            &normed_ff,
            d,
            &mut gate
        );
        dispatch_matmul!(
            q4_layer.map(|l| &l.w_up),
            &layer.w_up,
            &normed_ff_q,
            &normed_ff,
            d,
            &mut up
        );

        for j in 0..cfg.d_ff {
            gate[j] = (silu_i64(gate[j]) * up[j]) >> FRAC_BITS;
        }

        let gate_q = QuantizedInput::from_i64(&gate);
        dispatch_matmul!(
            q4_layer.map(|l| &l.w_down),
            &layer.w_down,
            &gate_q,
            &gate,
            cfg.d_ff,
            &mut ff_out
        );
        for i in 0..d {
            hidden[i] += ff_out[i];
        }
    }

    hidden
}

pub fn forward_final(model: &CachedIntegerModel, hidden: &[i64]) -> (u32, Hash256) {
    let cfg = &model.config;
    let normed = layernorm(hidden, &model.final_norm);

    let logits = if let Some(q4_out) = &model.q4_output {
        let mut logits = vec![0i64; cfg.vocab_size];
        super::cached_integer_model::matmul_q4_full(q4_out, &normed, &mut logits);
        logits
    } else {
        super::cached_integer_model::matmul_fast(
            &model.output_weight,
            &normed,
            cfg.d_model,
            cfg.vocab_size,
        )
    };

    let token_id = argmax_i64(&logits) as u32;
    let logits_bytes: Vec<u8> = logits.iter().flat_map(|v| v.to_le_bytes()).collect();
    let logits_hash = arc_crypto::hash_bytes(&logits_bytes);

    (token_id, logits_hash)
}

pub struct DistributedCache {
    entries: DashMap<Hash256, CacheEntry>,
    max_entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub output_tokens: Vec<u32>,
    pub output_hash: Hash256,
    pub model_id: Hash256,
    pub hit_count: u64,
    pub created_at_secs: u64,
}

impl DistributedCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: DashMap::new(),
            max_entries,
        }
    }

    pub fn cache_key(model_id: &Hash256, input_tokens: &[u32]) -> Hash256 {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&model_id.0);
        for &tok in input_tokens {
            hasher.update(&tok.to_le_bytes());
        }
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn get(&self, key: &Hash256) -> Option<Vec<u32>> {
        self.entries.get_mut(key).map(|mut e| {
            e.hit_count += 1;
            e.output_tokens.clone()
        })
    }

    pub fn insert(&self, key: Hash256, entry: CacheEntry) {
        if self.entries.len() >= self.max_entries {
            self.evict_lru();
        }
        self.entries.insert(key, entry);
    }

    fn evict_lru(&self) {
        if let Some(min_entry) = self.entries.iter().min_by_key(|e| e.value().hit_count) {
            let key = *min_entry.key();
            drop(min_entry);
            self.entries.remove(&key);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn capacity(&self) -> usize {
        self.max_entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, key: &Hash256) -> bool {
        self.entries.contains_key(key)
    }

    pub fn total_hits(&self) -> u64 {
        self.entries.iter().map(|e| e.value().hit_count).sum()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InferenceShardError {
    #[error("no pipeline registered for model {model_id}")]
    NoPipeline { model_id: Hash256 },

    #[error("activation integrity check failed: expected {expected}, got {actual}")]
    ActivationIntegrityFailed { expected: Hash256, actual: Hash256 },

    #[error("invalid activation size: {size} bytes (must be multiple of 8)")]
    InvalidActivationSize { size: usize },

    #[error("shard execution timeout")]
    Timeout,

    #[error("shard node offline: {node}")]
    NodeOffline { node: Hash256 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(seed: &[u8]) -> Hash256 {
        arc_crypto::hash_bytes(seed)
    }

    #[test]
    fn test_shard_registry_basic() {
        let registry = ShardRegistry::new();
        let model_id = test_address(b"model-1");
        let node_a = test_address(b"node-a");
        let node_b = test_address(b"node-b");

        registry.register_shard(
            model_id,
            ShardAssignment {
                node_address: node_a,
                start_layer: 0,
                end_layer: 16,
                expert_indices: vec![],
                socket_addr: "1.2.3.4:9091".into(),
                gpu_tier: 1,
                available_memory: 16_000_000_000,
            },
        );

        registry.register_shard(
            model_id,
            ShardAssignment {
                node_address: node_b,
                start_layer: 16,
                end_layer: 32,
                expert_indices: vec![],
                socket_addr: "5.6.7.8:9091".into(),
                gpu_tier: 2,
                available_memory: 64_000_000_000,
            },
        );

        assert!(registry.is_model_fully_covered(&model_id, 32));
        assert!(!registry.is_model_fully_covered(&model_id, 64));

        let pipeline = registry.get_pipeline(&model_id).unwrap();
        assert_eq!(pipeline.len(), 2);
        assert_eq!(pipeline[0].start_layer, 0);
        assert_eq!(pipeline[1].start_layer, 16);
    }

    #[test]
    fn test_shard_registry_remove_node() {
        let registry = ShardRegistry::new();
        let model_id = test_address(b"model-1");
        let node_a = test_address(b"node-a");

        registry.register_shard(
            model_id,
            ShardAssignment {
                node_address: node_a,
                start_layer: 0,
                end_layer: 32,
                expert_indices: vec![],
                socket_addr: "1.2.3.4:9091".into(),
                gpu_tier: 1,
                available_memory: 16_000_000_000,
            },
        );

        assert_eq!(registry.total_shard_nodes(), 1);
        registry.remove_node(&node_a);
        assert_eq!(registry.total_shard_nodes(), 0);
    }

    #[test]
    fn test_compute_shard_plan() {
        let model_id = test_address(b"model-670b");
        let nodes = vec![
            NodeCapability {
                address: test_address(b"n1"),
                socket_addr: "1.1.1.1:9091".into(),
                gpu_tier: 2,
                available_memory: 64_000_000_000,
            },
            NodeCapability {
                address: test_address(b"n2"),
                socket_addr: "2.2.2.2:9091".into(),
                gpu_tier: 0,
                available_memory: 32_000_000_000,
            },
            NodeCapability {
                address: test_address(b"n3"),
                socket_addr: "3.3.3.3:9091".into(),
                gpu_tier: 1,
                available_memory: 48_000_000_000,
            },
        ];

        let plan = compute_shard_plan(model_id, 80, 670.0, &nodes);
        assert_eq!(plan.len(), 3);

        assert_eq!(plan[0].start_layer, 0);
        assert_eq!(plan.last().unwrap().end_layer, 80);
        for i in 1..plan.len() {
            assert_eq!(plan[i].start_layer, plan[i - 1].end_layer);
        }

        assert!(plan[0].end_layer - plan[0].start_layer >= plan[1].end_layer - plan[1].start_layer);
    }

    #[test]
    fn test_expert_shard_plan() {
        let model_id = test_address(b"moe-model");
        let nodes = vec![
            NodeCapability {
                address: test_address(b"e1"),
                socket_addr: "1.1.1.1:9091".into(),
                gpu_tier: 1,
                available_memory: 32_000_000_000,
            },
            NodeCapability {
                address: test_address(b"e2"),
                socket_addr: "2.2.2.2:9091".into(),
                gpu_tier: 1,
                available_memory: 32_000_000_000,
            },
        ];

        let plan = compute_expert_shard_plan(model_id, 32, 8, 2, &nodes);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].expert_indices, vec![0, 1, 2, 3]);
        assert_eq!(plan[1].expert_indices, vec![4, 5, 6, 7]);
    }

    #[test]
    fn test_activation_serialization_roundtrip() {
        let activations: Vec<i64> = (0..4096).map(|i| (i * 1000 - 2048000) as i64).collect();
        let (bytes, hash) = serialize_activations(&activations);
        let recovered = deserialize_activations(&bytes, &hash).unwrap();
        assert_eq!(activations, recovered);
    }

    #[test]
    fn test_activation_integrity_check() {
        let activations: Vec<i64> = vec![1, 2, 3, 4];
        let (mut bytes, hash) = serialize_activations(&activations);
        bytes[0] ^= 0xFF;
        let result = deserialize_activations(&bytes, &hash);
        assert!(result.is_err());
    }

    #[test]
    fn test_distributed_cache() {
        let cache = DistributedCache::new(100);
        let model_id = test_address(b"model-1");
        let key = DistributedCache::cache_key(&model_id, &[1, 2, 3]);

        assert!(cache.get(&key).is_none());

        cache.insert(
            key,
            CacheEntry {
                output_tokens: vec![10, 20, 30],
                output_hash: test_address(b"hash"),
                model_id,
                hit_count: 0,
                created_at_secs: 0,
            },
        );

        assert_eq!(cache.get(&key), Some(vec![10, 20, 30]));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_pipeline_coordinator() {
        let registry = Arc::new(ShardRegistry::new());
        let model_id = test_address(b"model-1");
        let node_a = test_address(b"node-a");

        registry.register_shard(
            model_id,
            ShardAssignment {
                node_address: node_a,
                start_layer: 0,
                end_layer: 32,
                expert_indices: vec![],
                socket_addr: "1.2.3.4:9091".into(),
                gpu_tier: 1,
                available_memory: 16_000_000_000,
            },
        );

        let coord = PipelineCoordinator::new(registry);
        let req_id = test_address(b"req-1");

        let first_shard = coord
            .start_inference(req_id, model_id, vec![1, 2, 3], 5)
            .unwrap();
        assert_eq!(first_shard.start_layer, 0);
        assert_eq!(coord.active_count(), 1);

        match coord.on_shard_result(&req_id, 100) {
            PipelineAction::NextToken { .. } => {}
            other => panic!("Expected NextToken, got {:?}", other),
        }

        match coord.on_shard_result(&req_id, 2) {
            PipelineAction::Complete { tokens } => {
                assert_eq!(tokens, vec![100, 2]);
            }
            other => panic!("Expected Complete, got {:?}", other),
        }

        assert_eq!(coord.active_count(), 0);
    }
}
