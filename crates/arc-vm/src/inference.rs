use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Layer {
    Dense {
        weights: Vec<Vec<f32>>,
        bias: Vec<f32>,
    },
    ReLU,
    Softmax,
    LayerNorm {
        gamma: Vec<f32>,
        beta: Vec<f32>,
        eps: f32,
    },
    Embedding {
        table: Vec<Vec<f32>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct NeuralNet {
    pub layers: Vec<Layer>,
    pub input_size: usize,
    pub output_size: usize,
}

const TAG_DENSE: u8 = 0;
const TAG_RELU: u8 = 1;
const TAG_SOFTMAX: u8 = 2;
const TAG_LAYER_NORM: u8 = 3;
const TAG_EMBEDDING: u8 = 4;

fn read_u32(data: &[u8], offset: &mut usize) -> Result<u32, String> {
    if *offset + 4 > data.len() {
        return Err(format!(
            "unexpected end of model data at offset {} (need 4 bytes, have {})",
            offset,
            data.len() - *offset
        ));
    }
    let val = u32::from_le_bytes([
        data[*offset],
        data[*offset + 1],
        data[*offset + 2],
        data[*offset + 3],
    ]);
    *offset += 4;
    Ok(val)
}

fn read_f32(data: &[u8], offset: &mut usize) -> Result<f32, String> {
    if *offset + 4 > data.len() {
        return Err(format!(
            "unexpected end of model data at offset {} (need 4 bytes for f32)",
            offset
        ));
    }
    let val = f32::from_le_bytes([
        data[*offset],
        data[*offset + 1],
        data[*offset + 2],
        data[*offset + 3],
    ]);
    *offset += 4;
    Ok(val)
}

fn write_u32(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn write_f32(buf: &mut Vec<u8>, val: f32) {
    buf.extend_from_slice(&val.to_le_bytes());
}

impl NeuralNet {
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() < 4 {
            return Err("model data too short (need at least 4 bytes for header)".to_string());
        }

        let mut offset: usize = 0;
        let num_layers = read_u32(data, &mut offset)? as usize;

        let mut layers = Vec::with_capacity(num_layers);
        for i in 0..num_layers {
            if offset >= data.len() {
                return Err(format!("unexpected end of data before layer {i}"));
            }
            let tag = data[offset];
            offset += 1;

            let layer = match tag {
                TAG_DENSE => {
                    let out_rows = read_u32(data, &mut offset)? as usize;
                    let in_cols = read_u32(data, &mut offset)? as usize;

                    let mut weights = Vec::with_capacity(out_rows);
                    for _ in 0..out_rows {
                        let mut row = Vec::with_capacity(in_cols);
                        for _ in 0..in_cols {
                            row.push(read_f32(data, &mut offset)?);
                        }
                        weights.push(row);
                    }

                    let mut bias = Vec::with_capacity(out_rows);
                    for _ in 0..out_rows {
                        bias.push(read_f32(data, &mut offset)?);
                    }

                    Layer::Dense { weights, bias }
                }
                TAG_RELU => Layer::ReLU,
                TAG_SOFTMAX => Layer::Softmax,
                TAG_LAYER_NORM => {
                    let size = read_u32(data, &mut offset)? as usize;
                    let mut gamma = Vec::with_capacity(size);
                    for _ in 0..size {
                        gamma.push(read_f32(data, &mut offset)?);
                    }
                    let mut beta = Vec::with_capacity(size);
                    for _ in 0..size {
                        beta.push(read_f32(data, &mut offset)?);
                    }
                    let eps = read_f32(data, &mut offset)?;
                    Layer::LayerNorm { gamma, beta, eps }
                }
                TAG_EMBEDDING => {
                    let vocab_size = read_u32(data, &mut offset)? as usize;
                    let embed_dim = read_u32(data, &mut offset)? as usize;
                    let mut table = Vec::with_capacity(vocab_size);
                    for _ in 0..vocab_size {
                        let mut row = Vec::with_capacity(embed_dim);
                        for _ in 0..embed_dim {
                            row.push(read_f32(data, &mut offset)?);
                        }
                        table.push(row);
                    }
                    Layer::Embedding { table }
                }
                other => return Err(format!("unknown layer tag {other} at layer {i}")),
            };
            layers.push(layer);
        }

        let input_size = Self::infer_input_size(&layers)?;
        let output_size = Self::infer_output_size(&layers)?;

        Ok(Self {
            layers,
            input_size,
            output_size,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        write_u32(&mut buf, self.layers.len() as u32);

        for layer in &self.layers {
            match layer {
                Layer::Dense { weights, bias } => {
                    buf.push(TAG_DENSE);
                    let out_rows = weights.len() as u32;
                    let in_cols = if weights.is_empty() {
                        0u32
                    } else {
                        weights[0].len() as u32
                    };
                    write_u32(&mut buf, out_rows);
                    write_u32(&mut buf, in_cols);
                    for row in weights {
                        for &v in row {
                            write_f32(&mut buf, v);
                        }
                    }
                    for &b in bias {
                        write_f32(&mut buf, b);
                    }
                }
                Layer::ReLU => buf.push(TAG_RELU),
                Layer::Softmax => buf.push(TAG_SOFTMAX),
                Layer::LayerNorm { gamma, beta, eps } => {
                    buf.push(TAG_LAYER_NORM);
                    write_u32(&mut buf, gamma.len() as u32);
                    for &g in gamma {
                        write_f32(&mut buf, g);
                    }
                    for &b in beta {
                        write_f32(&mut buf, b);
                    }
                    write_f32(&mut buf, *eps);
                }
                Layer::Embedding { table } => {
                    buf.push(TAG_EMBEDDING);
                    let vocab_size = table.len() as u32;
                    let embed_dim = if table.is_empty() {
                        0u32
                    } else {
                        table[0].len() as u32
                    };
                    write_u32(&mut buf, vocab_size);
                    write_u32(&mut buf, embed_dim);
                    for row in table {
                        for &v in row {
                            write_f32(&mut buf, v);
                        }
                    }
                }
            }
        }
        buf
    }

    pub fn forward(&self, input: &[f32]) -> Vec<f32> {
        let mut activations = input.to_vec();

        for layer in &self.layers {
            activations = match layer {
                Layer::Dense { weights, bias } => Self::forward_dense(weights, bias, &activations),
                Layer::ReLU => Self::forward_relu(&activations),
                Layer::Softmax => Self::forward_softmax(&activations),
                Layer::LayerNorm { gamma, beta, eps } => {
                    Self::forward_layer_norm(gamma, beta, *eps, &activations)
                }
                Layer::Embedding { table } => Self::forward_embedding(table, &activations),
            };
        }

        activations
    }

    fn forward_dense(weights: &[Vec<f32>], bias: &[f32], input: &[f32]) -> Vec<f32> {
        let out_size = weights.len();
        let mut output = Vec::with_capacity(out_size);
        for i in 0..out_size {
            let mut acc: f32 = bias[i];
            for (j, &x) in input.iter().enumerate() {
                if j < weights[i].len() {
                    acc += weights[i][j] * x;
                }
            }
            output.push(acc);
        }
        output
    }

    fn forward_relu(input: &[f32]) -> Vec<f32> {
        input
            .iter()
            .map(|&x| if x > 0.0 { x } else { 0.0 })
            .collect()
    }

    fn forward_softmax(input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        let max_val = input.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let exps: Vec<f32> = input.iter().map(|&x| (x - max_val).exp()).collect();
        let sum: f32 = exps.iter().sum();
        if sum == 0.0 {
            let n = input.len() as f32;
            return vec![1.0 / n; input.len()];
        }
        exps.iter().map(|&e| e / sum).collect()
    }

    fn forward_layer_norm(gamma: &[f32], beta: &[f32], eps: f32, input: &[f32]) -> Vec<f32> {
        let n = input.len() as f32;
        let mean: f32 = input.iter().sum::<f32>() / n;
        let var: f32 = input.iter().map(|&x| (x - mean) * (x - mean)).sum::<f32>() / n;
        let inv_std = 1.0 / (var + eps).sqrt();

        input
            .iter()
            .enumerate()
            .map(|(i, &x)| {
                let normed = (x - mean) * inv_std;
                let g = if i < gamma.len() { gamma[i] } else { 1.0 };
                let b = if i < beta.len() { beta[i] } else { 0.0 };
                normed * g + b
            })
            .collect()
    }

    fn forward_embedding(table: &[Vec<f32>], input: &[f32]) -> Vec<f32> {
        let mut output = Vec::new();
        for &idx_f in input {
            let idx = idx_f as usize;
            if idx < table.len() {
                output.extend_from_slice(&table[idx]);
            } else {
                let dim = if table.is_empty() { 0 } else { table[0].len() };
                output.extend(std::iter::repeat_n(0.0f32, dim));
            }
        }
        output
    }

    fn infer_input_size(layers: &[Layer]) -> Result<usize, String> {
        for layer in layers {
            match layer {
                Layer::Dense { weights, .. } => {
                    return Ok(if weights.is_empty() {
                        0
                    } else {
                        weights[0].len()
                    });
                }
                Layer::Embedding { table } => {
                    return Ok(table.len());
                }
                Layer::LayerNorm { gamma, .. } => return Ok(gamma.len()),
                _ => continue,
            }
        }
        Ok(0)
    }

    fn infer_output_size(layers: &[Layer]) -> Result<usize, String> {
        for layer in layers.iter().rev() {
            match layer {
                Layer::Dense { weights, .. } => return Ok(weights.len()),
                Layer::Embedding { table } => {
                    return Ok(if table.is_empty() { 0 } else { table[0].len() });
                }
                Layer::LayerNorm { gamma, .. } => return Ok(gamma.len()),
                _ => continue,
            }
        }
        Ok(0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelStatus {
    Loading,
    Ready,
    Busy,
    Unloaded,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    pub model_type: String,
    pub parameter_count: u64,
    pub quantization: String,
    pub max_context: u32,
}

#[derive(Debug, Clone)]
pub struct LoadedModel {
    pub id: [u8; 32],
    pub metadata: ModelInfo,
    pub status: ModelStatus,
    pub last_used: u64,
    neural_net: Option<NeuralNet>,
}

#[derive(Debug, Clone)]
pub struct InferenceConfig {
    pub max_loaded_models: usize,
    pub default_timeout_ms: u64,
    pub max_tokens: u32,
    pub temperature: f32,
}

#[derive(Debug, Clone)]
pub enum InferenceInput {
    Text(String),
    Tokens(Vec<u32>),
    Embedding(Vec<f32>),
    Image(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct InferenceParams {
    pub max_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub stop_sequences: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InferenceRequest {
    pub model_id: [u8; 32],
    pub input: InferenceInput,
    pub params: InferenceParams,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InferenceOutput {
    Text(String),
    Tokens(Vec<u32>),
    Embedding(Vec<f32>),
    Classification(Vec<(String, f64)>),
}

#[derive(Debug, Clone)]
pub struct InferenceResponse {
    pub model_id: [u8; 32],
    pub output: InferenceOutput,
    pub tokens_used: u64,
    pub compute_time_ms: u64,
    pub cost: u64,
}

#[derive(Debug, Clone, Default)]
pub struct InferenceStats {
    pub total_inferences: u64,
    pub total_tokens: u64,
    pub avg_latency_ms: u64,
    pub cache_hits: u64,
}

pub struct InferenceEngine {
    models: HashMap<[u8; 32], LoadedModel>,
    config: InferenceConfig,
    stats: InferenceStats,
    total_latency_ms: u64,
}

impl InferenceEngine {
    pub fn new(config: InferenceConfig) -> Self {
        Self {
            models: HashMap::new(),
            config,
            stats: InferenceStats::default(),
            total_latency_ms: 0,
        }
    }

    pub fn load_model(&mut self, id: [u8; 32], info: ModelInfo) -> Result<(), String> {
        self.load_model_inner(id, info, None)
    }

    pub fn load_model_with_weights(
        &mut self,
        id: [u8; 32],
        info: ModelInfo,
        model_data: &[u8],
    ) -> Result<(), String> {
        let net = if model_data.is_empty() {
            None
        } else {
            NeuralNet::from_bytes(model_data).ok()
        };
        self.load_model_inner(id, info, net)
    }

    fn load_model_inner(
        &mut self,
        id: [u8; 32],
        info: ModelInfo,
        neural_net: Option<NeuralNet>,
    ) -> Result<(), String> {
        if self.models.contains_key(&id) {
            return Err("Model is already loaded".to_string());
        }
        if self.models.len() >= self.config.max_loaded_models {
            return Err(format!(
                "Maximum loaded models ({}) reached",
                self.config.max_loaded_models
            ));
        }

        let model = LoadedModel {
            id,
            metadata: info,
            status: ModelStatus::Ready,
            last_used: 0,
            neural_net,
        };
        self.models.insert(id, model);
        Ok(())
    }

    pub fn unload_model(&mut self, id: &[u8; 32]) -> bool {
        if let Some(model) = self.models.get_mut(id) {
            model.status = ModelStatus::Unloaded;
            self.models.remove(id);
            true
        } else {
            false
        }
    }

    pub fn run_inference(
        &mut self,
        request: &InferenceRequest,
    ) -> Result<InferenceResponse, String> {
        let model = self
            .models
            .get_mut(&request.model_id)
            .ok_or_else(|| "Model not loaded".to_string())?;

        match &model.status {
            ModelStatus::Ready => {}
            ModelStatus::Busy => return Err("Model is busy".to_string()),
            ModelStatus::Unloaded => return Err("Model is unloaded".to_string()),
            ModelStatus::Loading => return Err("Model is still loading".to_string()),
            ModelStatus::Error(e) => return Err(format!("Model error: {}", e)),
        }

        model.status = ModelStatus::Busy;
        let model_name = model.metadata.name.clone();
        let neural_net_clone = model.neural_net.clone();

        let (output, tokens_used) = if let Some(ref net) = neural_net_clone {
            Self::run_real_inference(net, &request.input, &model_name)
        } else {
            Self::run_mock_inference(&request.input, &model_name)
        };

        let compute_time_ms = tokens_used * 2 + 10;
        let cost = tokens_used * 5;

        let model = self.models.get_mut(&request.model_id).unwrap();
        model.status = ModelStatus::Ready;
        model.last_used += 1;

        self.stats.total_inferences += 1;
        self.stats.total_tokens += tokens_used;
        self.total_latency_ms += compute_time_ms;
        self.stats.avg_latency_ms = self.total_latency_ms / self.stats.total_inferences;

        Ok(InferenceResponse {
            model_id: request.model_id,
            output,
            tokens_used,
            compute_time_ms,
            cost,
        })
    }

    fn run_real_inference(
        net: &NeuralNet,
        input: &InferenceInput,
        _model_name: &str,
    ) -> (InferenceOutput, u64) {
        match input {
            InferenceInput::Text(text) => {
                let char_indices: Vec<f32> =
                    text.chars().map(|c| (c as u32).min(65535) as f32).collect();
                let raw_output = net.forward(&char_indices);
                let decoded = Self::decode_output(&raw_output);
                let tokens = char_indices.len() as u64;
                (InferenceOutput::Text(decoded), tokens)
            }
            InferenceInput::Tokens(tokens) => {
                let float_tokens: Vec<f32> = tokens.iter().map(|&t| t as f32).collect();
                let raw_output = net.forward(&float_tokens);
                let out_tokens: Vec<u32> = raw_output.iter().map(|&v| v as u32).collect();
                let tok_count = tokens.len() as u64;
                (InferenceOutput::Tokens(out_tokens), tok_count)
            }
            InferenceInput::Embedding(emb) => {
                let raw_output = net.forward(emb);
                let tok_count = emb.len() as u64;
                (InferenceOutput::Embedding(raw_output), tok_count)
            }
            InferenceInput::Image(data) => {
                let float_data: Vec<f32> = data.iter().map(|&b| b as f32 / 255.0).collect();
                let raw_output = net.forward(&float_data);
                let classes = Self::output_to_classification(&raw_output);
                (InferenceOutput::Classification(classes), 10)
            }
        }
    }

    fn decode_output(output: &[f32]) -> String {
        if output.is_empty() {
            return String::new();
        }
        output
            .iter()
            .filter_map(|&v| {
                let idx = ((v.abs() * 100.0) as u32) % 65536;
                let cp = if idx < 32 { idx + 32 } else { idx };
                char::from_u32(cp)
            })
            .collect()
    }

    fn output_to_classification(output: &[f32]) -> Vec<(String, f64)> {
        if output.is_empty() {
            return vec![("unknown".to_string(), 1.0)];
        }
        let probs = NeuralNet::forward_softmax(output);
        probs
            .iter()
            .enumerate()
            .map(|(i, &p)| (format!("class_{}", i), p as f64))
            .collect()
    }

    fn run_mock_inference(input: &InferenceInput, model_name: &str) -> (InferenceOutput, u64) {
        match input {
            InferenceInput::Text(text) => {
                let reversed: String = text.chars().rev().collect();
                let result = format!("[{}] {}", model_name, reversed);
                let tokens = result.len() as u64 / 4 + 1;
                (InferenceOutput::Text(result), tokens)
            }
            InferenceInput::Tokens(tokens) => {
                (InferenceOutput::Tokens(tokens.clone()), tokens.len() as u64)
            }
            InferenceInput::Embedding(emb) => {
                let magnitude: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
                let normalized = if magnitude > 0.0 {
                    emb.iter().map(|x| x / magnitude).collect()
                } else {
                    emb.clone()
                };
                (InferenceOutput::Embedding(normalized), emb.len() as u64)
            }
            InferenceInput::Image(data) => {
                let confidence = (data.len() as f64 % 100.0) / 100.0;
                let classes = vec![
                    ("object".to_string(), confidence),
                    ("background".to_string(), 1.0 - confidence),
                ];
                (InferenceOutput::Classification(classes), 10)
            }
        }
    }

    pub fn get_model_status(&self, id: &[u8; 32]) -> Option<ModelStatus> {
        self.models.get(id).map(|m| m.status.clone())
    }

    pub fn loaded_models(&self) -> Vec<[u8; 32]> {
        self.models.keys().copied().collect()
    }

    pub fn stats(&self) -> &InferenceStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> InferenceConfig {
        InferenceConfig {
            max_loaded_models: 4,
            default_timeout_ms: 5000,
            max_tokens: 2048,
            temperature: 0.7,
        }
    }

    fn model_id(seed: u8) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[0] = seed;
        id
    }

    fn sample_model_info(name: &str) -> ModelInfo {
        ModelInfo {
            name: name.to_string(),
            model_type: "transformer".to_string(),
            parameter_count: 7_000_000_000,
            quantization: "Q4_K_M".to_string(),
            max_context: 4096,
        }
    }

    fn default_params() -> InferenceParams {
        InferenceParams {
            max_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            stop_sequences: vec![],
        }
    }

    #[test]
    fn test_load_model() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(1);
        assert!(
            engine
                .load_model(id, sample_model_info("test-model"))
                .is_ok()
        );
        assert_eq!(engine.loaded_models().len(), 1);
        assert_eq!(engine.get_model_status(&id), Some(ModelStatus::Ready));
    }

    #[test]
    fn test_load_duplicate_model_fails() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(2);
        engine.load_model(id, sample_model_info("m1")).unwrap();
        let result = engine.load_model(id, sample_model_info("m1-dup"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already loaded"));
    }

    #[test]
    fn test_load_exceeds_capacity() {
        let mut config = default_config();
        config.max_loaded_models = 2;
        let mut engine = InferenceEngine::new(config);

        engine
            .load_model(model_id(1), sample_model_info("m1"))
            .unwrap();
        engine
            .load_model(model_id(2), sample_model_info("m2"))
            .unwrap();
        let result = engine.load_model(model_id(3), sample_model_info("m3"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Maximum loaded models"));
    }

    #[test]
    fn test_unload_model() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(3);
        engine.load_model(id, sample_model_info("m")).unwrap();
        assert!(engine.unload_model(&id));
        assert!(engine.get_model_status(&id).is_none());
        assert!(!engine.unload_model(&id));
    }

    #[test]
    fn test_inference_text_input() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(4);
        engine
            .load_model(id, sample_model_info("gpt-mock"))
            .unwrap();

        let request = InferenceRequest {
            model_id: id,
            input: InferenceInput::Text("hello world".to_string()),
            params: default_params(),
        };

        let response = engine.run_inference(&request).unwrap();
        assert_eq!(response.model_id, id);
        if let InferenceOutput::Text(text) = &response.output {
            assert!(text.contains("dlrow olleh"));
            assert!(text.contains("[gpt-mock]"));
        } else {
            panic!("Expected Text output");
        }
        assert!(response.tokens_used > 0);
        assert!(response.cost > 0);
    }

    #[test]
    fn test_inference_tokens_input() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(5);
        engine
            .load_model(id, sample_model_info("tok-model"))
            .unwrap();

        let tokens = vec![100, 200, 300];
        let request = InferenceRequest {
            model_id: id,
            input: InferenceInput::Tokens(tokens.clone()),
            params: default_params(),
        };

        let response = engine.run_inference(&request).unwrap();
        assert_eq!(response.output, InferenceOutput::Tokens(tokens));
        assert_eq!(response.tokens_used, 3);
    }

    #[test]
    fn test_inference_embedding_input() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(6);
        engine
            .load_model(id, sample_model_info("emb-model"))
            .unwrap();

        let emb = vec![3.0f32, 4.0];
        let request = InferenceRequest {
            model_id: id,
            input: InferenceInput::Embedding(emb),
            params: default_params(),
        };

        let response = engine.run_inference(&request).unwrap();
        if let InferenceOutput::Embedding(norm) = &response.output {
            assert!((norm[0] - 0.6).abs() < 0.001);
            assert!((norm[1] - 0.8).abs() < 0.001);
        } else {
            panic!("Expected Embedding output");
        }
    }

    #[test]
    fn test_inference_image_input() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(7);
        engine.load_model(id, sample_model_info("vision")).unwrap();

        let request = InferenceRequest {
            model_id: id,
            input: InferenceInput::Image(vec![0u8; 50]),
            params: default_params(),
        };

        let response = engine.run_inference(&request).unwrap();
        if let InferenceOutput::Classification(classes) = &response.output {
            assert_eq!(classes.len(), 2);
            assert_eq!(classes[0].0, "object");
            assert_eq!(classes[1].0, "background");
            assert!((classes[0].1 + classes[1].1 - 1.0).abs() < 0.001);
        } else {
            panic!("Expected Classification output");
        }
    }

    #[test]
    fn test_inference_model_not_loaded() {
        let mut engine = InferenceEngine::new(default_config());
        let request = InferenceRequest {
            model_id: model_id(99),
            input: InferenceInput::Text("test".to_string()),
            params: default_params(),
        };
        let result = engine.run_inference(&request);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not loaded"));
    }

    #[test]
    fn test_stats_accumulate() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(8);
        engine
            .load_model(id, sample_model_info("stats-test"))
            .unwrap();

        assert_eq!(engine.stats().total_inferences, 0);
        assert_eq!(engine.stats().total_tokens, 0);

        let request = InferenceRequest {
            model_id: id,
            input: InferenceInput::Tokens(vec![1, 2, 3]),
            params: default_params(),
        };

        engine.run_inference(&request).unwrap();
        assert_eq!(engine.stats().total_inferences, 1);
        assert_eq!(engine.stats().total_tokens, 3);

        engine.run_inference(&request).unwrap();
        assert_eq!(engine.stats().total_inferences, 2);
        assert_eq!(engine.stats().total_tokens, 6);
        assert!(engine.stats().avg_latency_ms > 0);
    }

    #[test]
    fn test_model_status_returns_ready_after_inference() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(9);
        engine.load_model(id, sample_model_info("m")).unwrap();

        let request = InferenceRequest {
            model_id: id,
            input: InferenceInput::Text("test".to_string()),
            params: default_params(),
        };

        engine.run_inference(&request).unwrap();
        assert_eq!(engine.get_model_status(&id), Some(ModelStatus::Ready));
    }

    #[test]
    fn test_dense_layer_forward() {
        let net = NeuralNet {
            layers: vec![Layer::Dense {
                weights: vec![vec![1.0, 0.0, -1.0], vec![0.0, 1.0, 2.0]],
                bias: vec![0.5, -0.5],
            }],
            input_size: 3,
            output_size: 2,
        };

        let output = net.forward(&[1.0, 2.0, 3.0]);
        assert_eq!(output.len(), 2);
        assert!((output[0] - (-1.5)).abs() < 1e-6, "got {}", output[0]);
        assert!((output[1] - 7.5).abs() < 1e-6, "got {}", output[1]);
    }

    #[test]
    fn test_relu_activation() {
        let net = NeuralNet {
            layers: vec![Layer::ReLU],
            input_size: 0,
            output_size: 0,
        };
        let output = net.forward(&[-3.0, -0.1, 0.0, 0.5, 2.0]);
        assert_eq!(output, vec![0.0, 0.0, 0.0, 0.5, 2.0]);
    }

    #[test]
    fn test_softmax_normalization() {
        let net = NeuralNet {
            layers: vec![Layer::Softmax],
            input_size: 0,
            output_size: 0,
        };
        let output = net.forward(&[1.0, 2.0, 3.0, 4.0]);

        let sum: f32 = output.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "softmax sum should be 1.0, got {}",
            sum
        );

        for &v in &output {
            assert!(v > 0.0, "softmax outputs must be positive");
        }

        for i in 1..output.len() {
            assert!(
                output[i] >= output[i - 1],
                "softmax should preserve order: {} < {}",
                output[i],
                output[i - 1]
            );
        }
    }

    #[test]
    fn test_layernorm() {
        let net = NeuralNet {
            layers: vec![Layer::LayerNorm {
                gamma: vec![1.0, 1.0, 1.0, 1.0],
                beta: vec![0.0, 0.0, 0.0, 0.0],
                eps: 1e-5,
            }],
            input_size: 4,
            output_size: 4,
        };

        let output = net.forward(&[2.0, 4.0, 6.0, 8.0]);
        assert_eq!(output.len(), 4);

        let mean: f32 = output.iter().sum::<f32>() / output.len() as f32;
        assert!(
            mean.abs() < 1e-5,
            "layernorm output mean should be ~0, got {}",
            mean
        );

        let var: f32 =
            output.iter().map(|&x| (x - mean) * (x - mean)).sum::<f32>() / output.len() as f32;
        assert!(
            (var - 1.0).abs() < 1e-4,
            "layernorm output variance should be ~1, got {}",
            var
        );

        let net2 = NeuralNet {
            layers: vec![Layer::LayerNorm {
                gamma: vec![2.0, 2.0, 2.0, 2.0],
                beta: vec![1.0, 1.0, 1.0, 1.0],
                eps: 1e-5,
            }],
            input_size: 4,
            output_size: 4,
        };
        let output2 = net2.forward(&[2.0, 4.0, 6.0, 8.0]);
        let mean2: f32 = output2.iter().sum::<f32>() / output2.len() as f32;
        assert!(
            (mean2 - 1.0).abs() < 1e-4,
            "layernorm with beta=1 output mean should be ~1, got {}",
            mean2
        );
    }

    #[test]
    fn test_small_network_forward() {
        let net = NeuralNet {
            layers: vec![
                Layer::Dense {
                    weights: vec![vec![1.0, -1.0, 0.0], vec![0.0, 1.0, 1.0]],
                    bias: vec![0.0, 0.0],
                },
                Layer::ReLU,
                Layer::Dense {
                    weights: vec![vec![1.0, 1.0]],
                    bias: vec![0.1],
                },
            ],
            input_size: 3,
            output_size: 1,
        };

        let output = net.forward(&[1.0, 2.0, 3.0]);
        assert_eq!(output.len(), 1);
        assert!(
            (output[0] - 5.1).abs() < 1e-5,
            "expected 5.1, got {}",
            output[0]
        );
    }

    #[test]
    fn test_model_serialization_roundtrip() {
        let original = NeuralNet {
            layers: vec![
                Layer::Embedding {
                    table: vec![
                        vec![0.1, 0.2, 0.3],
                        vec![0.4, 0.5, 0.6],
                        vec![0.7, 0.8, 0.9],
                    ],
                },
                Layer::LayerNorm {
                    gamma: vec![1.0, 1.0, 1.0],
                    beta: vec![0.0, 0.0, 0.0],
                    eps: 1e-5,
                },
                Layer::Dense {
                    weights: vec![vec![1.0, 0.5, -0.5], vec![0.0, 1.0, 0.0]],
                    bias: vec![0.1, -0.1],
                },
                Layer::ReLU,
                Layer::Softmax,
            ],
            input_size: 3,
            output_size: 2,
        };

        let bytes = original.to_bytes();
        assert!(!bytes.is_empty());

        let restored = NeuralNet::from_bytes(&bytes).expect("deserialization should succeed");

        assert_eq!(original.layers.len(), restored.layers.len());
        assert_eq!(original.input_size, restored.input_size);
        assert_eq!(original.output_size, restored.output_size);

        let test_input = vec![0.0, 1.0, 2.0];
        let out_orig = original.forward(&test_input);
        let out_restored = restored.forward(&test_input);
        assert_eq!(out_orig.len(), out_restored.len());
        for (a, b) in out_orig.iter().zip(out_restored.iter()) {
            assert!((a - b).abs() < 1e-7, "output mismatch: {} vs {}", a, b);
        }
    }

    #[test]
    fn test_inference_engine_with_real_model() {
        let net = NeuralNet {
            layers: vec![
                Layer::Dense {
                    weights: vec![
                        vec![0.5, -0.3, 0.1, 0.2],
                        vec![-0.1, 0.4, 0.3, -0.2],
                        vec![0.2, 0.1, -0.4, 0.5],
                    ],
                    bias: vec![0.0, 0.0, 0.0],
                },
                Layer::ReLU,
                Layer::Dense {
                    weights: vec![vec![1.0, -1.0, 0.5], vec![-0.5, 1.0, 0.3]],
                    bias: vec![0.0, 0.0],
                },
                Layer::Softmax,
            ],
            input_size: 4,
            output_size: 2,
        };

        let model_bytes = net.to_bytes();

        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(20);
        engine
            .load_model_with_weights(id, sample_model_info("real-clf"), &model_bytes)
            .unwrap();

        let request = InferenceRequest {
            model_id: id,
            input: InferenceInput::Embedding(vec![1.0, 0.5, -0.5, 0.2]),
            params: default_params(),
        };

        let response = engine.run_inference(&request).unwrap();
        if let InferenceOutput::Embedding(out) = &response.output {
            let sum: f32 = out.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-5,
                "real model softmax sum should be 1.0, got {}",
                sum
            );
            assert_eq!(out.len(), 2);
        } else {
            panic!("Expected Embedding output from real model");
        }

        assert_eq!(engine.get_model_status(&id), Some(ModelStatus::Ready));
        assert_eq!(engine.stats().total_inferences, 1);
    }

    #[test]
    fn test_load_model_with_empty_weights_falls_back_to_mock() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(21);
        engine
            .load_model_with_weights(id, sample_model_info("mock-fallback"), &[])
            .unwrap();

        let request = InferenceRequest {
            model_id: id,
            input: InferenceInput::Text("abc".to_string()),
            params: default_params(),
        };

        let response = engine.run_inference(&request).unwrap();
        if let InferenceOutput::Text(text) = &response.output {
            assert!(text.contains("cba"), "mock fallback should reverse text");
            assert!(
                text.contains("[mock-fallback]"),
                "mock fallback should contain model name"
            );
        } else {
            panic!("Expected Text output from mock fallback");
        }
    }

    #[test]
    fn test_load_model_with_corrupt_weights_falls_back_to_mock() {
        let mut engine = InferenceEngine::new(default_config());
        let id = model_id(22);
        let garbage = vec![0xFF, 0xFE, 0xAB];
        engine
            .load_model_with_weights(id, sample_model_info("corrupt"), &garbage)
            .unwrap();

        let request = InferenceRequest {
            model_id: id,
            input: InferenceInput::Text("xyz".to_string()),
            params: default_params(),
        };

        let response = engine.run_inference(&request).unwrap();
        if let InferenceOutput::Text(text) = &response.output {
            assert!(
                text.contains("zyx"),
                "corrupt weights should fall back to mock"
            );
        } else {
            panic!("Expected Text output from corrupt-weight fallback");
        }
    }

    #[test]
    fn test_embedding_layer_forward() {
        let net = NeuralNet {
            layers: vec![Layer::Embedding {
                table: vec![vec![10.0, 20.0], vec![30.0, 40.0], vec![50.0, 60.0]],
            }],
            input_size: 3,
            output_size: 2,
        };

        let output = net.forward(&[2.0, 0.0, 1.0]);
        assert_eq!(
            output,
            vec![50.0, 60.0, 10.0, 20.0, 30.0, 40.0],
            "embedding lookup should concatenate rows"
        );
    }

    #[test]
    fn test_forward_determinism() {
        let net = NeuralNet {
            layers: vec![
                Layer::Dense {
                    weights: vec![vec![0.123, -0.456, 0.789], vec![0.321, 0.654, -0.987]],
                    bias: vec![0.01, -0.02],
                },
                Layer::ReLU,
                Layer::Softmax,
            ],
            input_size: 3,
            output_size: 2,
        };

        let input = vec![1.0f32, -0.5, 0.25];
        let reference = net.forward(&input);

        for _ in 0..100 {
            let output = net.forward(&input);
            assert_eq!(
                output, reference,
                "forward pass must be deterministic across runs"
            );
        }
    }

    #[test]
    fn test_from_bytes_errors_on_truncated_data() {
        assert!(NeuralNet::from_bytes(&[]).is_err());
        assert!(NeuralNet::from_bytes(&[0, 0]).is_err());

        let mut buf = Vec::new();
        write_u32(&mut buf, 1);
        assert!(NeuralNet::from_bytes(&buf).is_err());

        buf.push(TAG_DENSE);
        write_u32(&mut buf, 2);
        write_u32(&mut buf, 2);
        assert!(NeuralNet::from_bytes(&buf).is_err());
    }
}
