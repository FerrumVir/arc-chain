//! Canonical coordinator state without resident projection matrices or a
//! local generation fallback. Lazy reads are checked against chunk hashes
//! captured in the same pass that verifies the qualified artifact commitment.
//! The arithmetic and generation loop are shared with the resident executor.

use crate::cached_integer_model::{BackendGenerationError, CachedIntegerModel, ModelConfig};
use crate::tensor_parallel::{ProjectionBackend, TensorKey, TensorParallelError};
use arc_crypto::Hash256;

/// Local canonical rows used for challenges, spot checks and duplicate
/// verification. This is not a generation backend or a fallback policy.
pub trait CanonicalRowSource: Send + Sync {
    fn config(&self) -> &ModelConfig;
    fn canonical_execution_profile(&self) -> Option<&'static str>;
    fn projection_shape(&self, layer: Option<usize>, tensor: TensorKey) -> Option<(usize, usize)>;
    fn projection_rows(
        &self,
        layer: Option<usize>,
        tensor: TensorKey,
        start: usize,
        end: usize,
        input: &[i64],
    ) -> Result<Vec<i64>, TensorParallelError>;
}

pub(crate) trait CanonicalForwardSource: CanonicalRowSource {
    fn embedding(&self, token: u32) -> Result<Vec<i64>, TensorParallelError>;
    fn norms(&self, layer: usize) -> Result<(&[i64], &[i64]), TensorParallelError>;
    fn final_norm(&self) -> &[i64];
}

impl CanonicalRowSource for CachedIntegerModel {
    fn config(&self) -> &ModelConfig {
        &self.config
    }
    fn canonical_execution_profile(&self) -> Option<&'static str> {
        self.canonical_execution_profile()
    }
    fn projection_shape(&self, layer: Option<usize>, tensor: TensorKey) -> Option<(usize, usize)> {
        crate::tensor_parallel::model_projection_weights(self, layer, tensor)
            .map(|w| (w.n_rows, w.n_cols))
    }
    fn projection_rows(
        &self,
        layer: Option<usize>,
        tensor: TensorKey,
        start: usize,
        end: usize,
        input: &[i64],
    ) -> Result<Vec<i64>, TensorParallelError> {
        let weights = crate::tensor_parallel::model_projection_weights(self, layer, tensor)
            .ok_or(TensorParallelError::WrongIdentity)?;
        crate::tensor_parallel::rows_of(weights, start, end, input)
    }
}
impl CanonicalForwardSource for CachedIntegerModel {
    fn embedding(&self, token: u32) -> Result<Vec<i64>, TensorParallelError> {
        let start = (token as usize)
            .checked_mul(self.config.d_model)
            .ok_or(TensorParallelError::Bounds)?;
        self.embedding_q16
            .get(start..start + self.config.d_model)
            .map(<[i64]>::to_vec)
            .ok_or(TensorParallelError::WrongShape)
    }
    fn norms(&self, layer: usize) -> Result<(&[i64], &[i64]), TensorParallelError> {
        let layer = self
            .layers
            .get(layer)
            .ok_or(TensorParallelError::WrongShape)?;
        Ok((&layer.attn_norm, &layer.ffn_norm))
    }
    fn final_norm(&self) -> &[i64] {
        &self.final_norm
    }
}

/// Norms and RoPE stay resident; even embeddings are read one row at a time.
/// Construction validates every required projection's shape and block layout.
/// It never loads a whole layer, embedding matrix, or output head.
pub struct LowResidencyModel {
    #[cfg(feature = "candle")]
    artifact: Hash256,
    config: ModelConfig,
    norms: Vec<(Vec<i64>, Vec<i64>)>,
    final_norm: Vec<i64>,
    #[cfg(feature = "candle")]
    source: std::sync::Mutex<GgufRows>,
}

impl LowResidencyModel {
    pub fn config(&self) -> &ModelConfig {
        &self.config
    }
    pub fn try_generate_v2_with_backend(
        &self,
        request: Hash256,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
        backend: &impl ProjectionBackend,
    ) -> Result<(Vec<u32>, Hash256), BackendGenerationError> {
        crate::cached_integer_model::generate_canonical_with_backend(
            self, request, prompt, max_tokens, eos_tokens, backend,
        )
    }
    pub fn forward_one_token_with_backend(
        &self,
        token: u32,
        cache: &mut crate::cached_integer_model::KVCache,
        call: Hash256,
        backend: &impl ProjectionBackend,
    ) -> Result<Vec<i64>, TensorParallelError> {
        crate::cached_integer_model::forward_canonical_with_backend(
            self, token, cache, call, backend,
        )
    }
    /// Export one projection for an existing bounded stdio worker. At most
    /// one matrix is prepared, never a layer or the whole model. The legacy
    /// row-file writer has one equally sized serialization scratch buffer.
    #[cfg(feature = "candle")]
    pub fn export_rows(
        &self,
        path: &std::path::Path,
        assignment: &crate::tensor_parallel::RowAssignment,
    ) -> Result<(), TensorParallelError> {
        use crate::cached_integer_model::I8Weights;
        if assignment.artifact_id != self.artifact
            || Some(assignment.execution_profile.as_str()) != self.canonical_execution_profile()
        {
            return Err(TensorParallelError::WrongIdentity);
        }
        let (rows, cols) = self
            .projection_shape(assignment.layer, assignment.tensor)
            .ok_or(TensorParallelError::WrongIdentity)?;
        let (start, end) = (assignment.row_start, assignment.row_end);
        if start >= end || end > rows {
            return Err(TensorParallelError::WrongShape);
        }
        let bytes = (end - start)
            .checked_mul(cols + 8)
            .ok_or(TensorParallelError::Bounds)?;
        if bytes > crate::tensor_parallel::MAX_CANONICAL_ROW_FILE_BYTES {
            return Err(TensorParallelError::Bounds);
        }
        let mut source = self
            .source
            .lock()
            .map_err(|_| TensorParallelError::Worker("GGUF row reader poisoned".into()))?;
        let name = source.tensor_name(assignment.layer, assignment.tensor);
        let mut weights = I8Weights {
            data: Vec::with_capacity((end - start) * cols),
            scales: Vec::with_capacity(end - start),
            n_rows: end - start,
            n_cols: cols,
        };
        for row in start..end {
            let row = canonical_source_row(row, assignment.tensor, self.config.d_head);
            let f = source.row(&name, row)?;
            let prepared = I8Weights::quantize_f32(&f, 1, cols);
            weights.data.extend(prepared.data);
            weights.scales.extend(prepared.scales);
        }
        crate::tensor_parallel::export_canonical_row_file(path, assignment, &weights)
    }

    /// Norms and RoPE only; excludes bounded GGUF metadata, hash index, one
    /// 64 KiB verified read buffer, activations and the separately capped KV.
    pub fn resident_state_bytes(&self) -> usize {
        (self
            .norms
            .iter()
            .map(|(a, b)| a.len() + b.len())
            .sum::<usize>()
            + self.final_norm.len()
            + self.config.rope_cos.len()
            + self.config.rope_sin.len())
            * 8
    }
}

impl CanonicalRowSource for LowResidencyModel {
    fn config(&self) -> &ModelConfig {
        &self.config
    }
    fn canonical_execution_profile(&self) -> Option<&'static str> {
        Some(crate::cached_integer_model::GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE)
    }
    fn projection_shape(&self, layer: Option<usize>, tensor: TensorKey) -> Option<(usize, usize)> {
        projection_shape(&self.config, layer, tensor)
    }
    fn projection_rows(
        &self,
        layer: Option<usize>,
        tensor: TensorKey,
        start: usize,
        end: usize,
        input: &[i64],
    ) -> Result<Vec<i64>, TensorParallelError> {
        let (rows, cols) = self
            .projection_shape(layer, tensor)
            .ok_or(TensorParallelError::WrongIdentity)?;
        if start >= end || end > rows || input.len() != cols {
            return Err(TensorParallelError::WrongShape);
        }
        #[cfg(feature = "candle")]
        {
            use crate::cached_integer_model::{I8Weights, matmul_i8_canonical_rows};
            let mut source = self
                .source
                .lock()
                .map_err(|_| TensorParallelError::Worker("GGUF row reader poisoned".into()))?;
            let name = source.tensor_name(layer, tensor);
            let mut output = Vec::with_capacity(end - start);
            // Bound scratch to one source row even for a wide duplicate or
            // challenge. Quantization is per row, exactly as the full loader.
            for row in start..end {
                let source_row = canonical_source_row(row, tensor, self.config.d_head);
                let f = source.row(&name, source_row)?;
                let weights = I8Weights::quantize_f32(&f, 1, cols);
                let mut value = [0i64];
                matmul_i8_canonical_rows(&weights, input, &mut value)
                    .map_err(TensorParallelError::Worker)?;
                output.push(value[0]);
            }
            Ok(output)
        }
        #[cfg(not(feature = "candle"))]
        Err(TensorParallelError::Worker(
            "candle feature not enabled".into(),
        ))
    }
}

impl CanonicalForwardSource for LowResidencyModel {
    fn embedding(&self, token: u32) -> Result<Vec<i64>, TensorParallelError> {
        if token as usize >= self.config.vocab_size {
            return Err(TensorParallelError::WrongShape);
        }
        #[cfg(feature = "candle")]
        {
            let mut source = self
                .source
                .lock()
                .map_err(|_| TensorParallelError::Worker("GGUF row reader poisoned".into()))?;
            Ok(source
                .row("token_embd.weight", token as usize)?
                .into_iter()
                .map(|x| (x as f64 * crate::integer_lut::ONE as f64).round() as i64)
                .collect())
        }
        #[cfg(not(feature = "candle"))]
        Err(TensorParallelError::Worker(
            "candle feature not enabled".into(),
        ))
    }
    fn norms(&self, layer: usize) -> Result<(&[i64], &[i64]), TensorParallelError> {
        let (a, b) = self
            .norms
            .get(layer)
            .ok_or(TensorParallelError::WrongShape)?;
        Ok((a, b))
    }
    fn final_norm(&self) -> &[i64] {
        &self.final_norm
    }
}

#[cfg(feature = "candle")]
fn canonical_source_row(row: usize, tensor: TensorKey, d_head: usize) -> usize {
    match tensor {
        TensorKey::Wq | TensorKey::Wk => {
            let head = row / d_head;
            let within = row % d_head;
            let half = d_head / 2;
            head * d_head + (within % half) * 2 + within / half
        }
        _ => row,
    }
}

fn projection_shape(
    config: &ModelConfig,
    layer: Option<usize>,
    tensor: TensorKey,
) -> Option<(usize, usize)> {
    if tensor == TensorKey::LmHead {
        return layer
            .is_none()
            .then_some((config.vocab_size, config.d_model));
    }
    if layer.is_none_or(|l| l >= config.n_layers) {
        return None;
    }
    Some(match tensor {
        TensorKey::Wq | TensorKey::Wo => (config.d_model, config.d_model),
        TensorKey::Wk | TensorKey::Wv => (config.d_kv, config.d_model),
        TensorKey::WGate | TensorKey::WUp => (config.d_ff, config.d_model),
        TensorKey::WDown => (config.d_model, config.d_ff),
        TensorKey::LmHead => unreachable!(),
    })
}

/// Blocks are hashed in the *same pass* as the whole artifact. Every later
/// read verifies a complete block before returning any bytes from it. Holding
/// the descriptor also prevents path replacement from redirecting lazy reads.
#[cfg(any(feature = "candle", test))]
struct VerifiedArtifact {
    file: std::fs::File,
    len: u64,
    chunks: Vec<blake3::Hash>,
    position: u64,
    cached: Option<u64>,
    buffer: Vec<u8>,
}
#[cfg(any(feature = "candle", test))]
impl VerifiedArtifact {
    const CHUNK: usize = 64 * 1024;
    fn open(path: &std::path::Path, expected: Hash256) -> std::io::Result<Self> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::other("artifact is not a regular file"));
        }
        let len = metadata.len();
        let mut buffer = vec![0; Self::CHUNK];
        let mut chunks = Vec::new();
        let mut whole = blake3::Hasher::new();
        let mut remaining = len;
        while remaining > 0 {
            let size = remaining.min(Self::CHUNK as u64) as usize;
            file.read_exact(&mut buffer[..size])?;
            whole.update(&buffer[..size]);
            chunks.push(blake3::hash(&buffer[..size]));
            remaining -= size as u64;
        }
        if Hash256(*whole.finalize().as_bytes()) != expected || file.metadata()?.len() != len {
            return Err(std::io::Error::other(
                "artifact bytes do not match the pinned model hash",
            ));
        }
        Ok(Self {
            file,
            len,
            chunks,
            position: 0,
            cached: None,
            buffer,
        })
    }
}
#[cfg(any(feature = "candle", test))]
impl std::io::Read for VerifiedArtifact {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Seek, SeekFrom};
        if out.is_empty() || self.position >= self.len {
            return Ok(0);
        }
        let index = self.position / Self::CHUNK as u64;
        let start = index * Self::CHUNK as u64;
        let size = (self.len - start).min(Self::CHUNK as u64) as usize;
        if self.cached != Some(index) {
            self.cached = None;
            self.file.seek(SeekFrom::Start(start))?;
            self.file.read_exact(&mut self.buffer[..size])?;
            if blake3::hash(&self.buffer[..size]) != self.chunks[index as usize] {
                return Err(std::io::Error::other(
                    "qualified artifact changed after verification",
                ));
            }
            self.cached = Some(index);
        }
        let within = (self.position - start) as usize;
        let count = out.len().min(size - within);
        out[..count].copy_from_slice(&self.buffer[within..within + count]);
        self.position += count as u64;
        Ok(count)
    }
}
#[cfg(any(feature = "candle", test))]
impl std::io::Seek for VerifiedArtifact {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        let position = match from {
            SeekFrom::Start(p) => i128::from(p),
            SeekFrom::End(p) => i128::from(self.len) + i128::from(p),
            SeekFrom::Current(p) => i128::from(self.position) + i128::from(p),
        };
        self.position =
            u64::try_from(position).map_err(|_| std::io::Error::other("invalid artifact seek"))?;
        Ok(self.position)
    }
}

#[cfg(feature = "candle")]
struct GgufRows {
    file: VerifiedArtifact,
    content: candle_core::quantized::gguf_file::Content,
}
#[cfg(feature = "candle")]
impl GgufRows {
    fn tensor_name(&self, layer: Option<usize>, tensor: TensorKey) -> String {
        let suffix = match tensor {
            TensorKey::Wq => "attn_q.weight",
            TensorKey::Wk => "attn_k.weight",
            TensorKey::Wv => "attn_v.weight",
            TensorKey::Wo => "attn_output.weight",
            TensorKey::WGate => "ffn_gate.weight",
            TensorKey::WUp => "ffn_up.weight",
            TensorKey::WDown => "ffn_down.weight",
            TensorKey::LmHead => {
                return if self.content.tensor_infos.contains_key("output.weight") {
                    "output.weight"
                } else {
                    "token_embd.weight"
                }
                .into();
            }
        };
        format!("blk.{}.{suffix}", layer.unwrap_or(usize::MAX))
    }
    fn validate_tensor(&self, name: &str, shape: &[usize]) -> Result<(), TensorParallelError> {
        let info = self
            .content
            .tensor_infos
            .get(name)
            .ok_or(TensorParallelError::WrongIdentity)?;
        let block = info.ggml_dtype.block_size();
        if info.shape.dims() != shape
            || shape.is_empty()
            || shape.last().is_none_or(|cols| !cols.is_multiple_of(block))
        {
            return Err(TensorParallelError::WrongShape);
        }
        let bytes = shape
            .iter()
            .try_fold(1usize, |n, dim| n.checked_mul(*dim))
            .and_then(|n| n.checked_div(block))
            .and_then(|n| n.checked_mul(info.ggml_dtype.type_size()))
            .ok_or(TensorParallelError::Bounds)?;
        let end = self
            .content
            .tensor_data_offset
            .checked_add(info.offset)
            .and_then(|n| n.checked_add(bytes as u64))
            .ok_or(TensorParallelError::Bounds)?;
        if end > self.file.len {
            return Err(TensorParallelError::WrongShape);
        }
        Ok(())
    }
    fn row(&mut self, name: &str, row: usize) -> Result<Vec<f32>, TensorParallelError> {
        use candle_core::{Device, quantized::gguf_file::TensorInfo};
        let info = self
            .content
            .tensor_infos
            .get(name)
            .ok_or(TensorParallelError::WrongIdentity)?;
        let dims = info.shape.dims();
        if dims.len() != 2 || row >= dims[0] {
            return Err(TensorParallelError::WrongShape);
        }
        let cols = dims[1];
        if cols > crate::tensor_parallel::MAX_ROW_INPUT_ELEMENTS
            || !cols.is_multiple_of(info.ggml_dtype.block_size())
        {
            return Err(TensorParallelError::Bounds);
        }
        let stride = cols / info.ggml_dtype.block_size() * info.ggml_dtype.type_size();
        let offset = info
            .offset
            .checked_add((row.checked_mul(stride).ok_or(TensorParallelError::Bounds)?) as u64)
            .ok_or(TensorParallelError::Bounds)?;
        let row_info = TensorInfo {
            ggml_dtype: info.ggml_dtype,
            shape: vec![1, cols].into(),
            offset,
        };
        let result = row_info
            .read(
                &mut self.file,
                self.content.tensor_data_offset,
                &Device::Cpu,
            )
            .and_then(|q| q.dequantize(&Device::Cpu))
            .and_then(|t| t.flatten_all())
            .and_then(|t| t.to_vec1::<f32>());
        result.map_err(|e| TensorParallelError::Worker(format!("GGUF row {name}/{row}: {e}")))
    }
    fn norm(&mut self, name: &str, width: usize) -> Result<Vec<i64>, TensorParallelError> {
        self.validate_tensor(name, &[width])?;
        let values = self
            .content
            .tensor(&mut self.file, name, &candle_core::Device::Cpu)
            .and_then(|q| q.dequantize(&candle_core::Device::Cpu))
            .and_then(|t| t.flatten_all())
            .and_then(|t| t.to_vec1::<f32>())
            .map_err(|e| TensorParallelError::Worker(format!("GGUF norm {name}: {e}")))?;
        Ok(values
            .into_iter()
            .map(|x| (x * crate::integer_lut::ONE as f32).round() as i64)
            .collect())
    }
}

#[cfg(feature = "candle")]
pub fn load_low_residency_model(
    path: &std::path::Path,
    artifact: Hash256,
) -> Result<LowResidencyModel, crate::InferenceError> {
    use candle_core::quantized::gguf_file::{Content, Value};
    let refuse = |e: String| crate::InferenceError::Runtime(e);
    let mut file = VerifiedArtifact::open(path, artifact).map_err(|e| refuse(e.to_string()))?;
    let content = Content::read(&mut file).map_err(|e| refuse(e.to_string()))?;
    if !matches!(content.metadata.get("general.architecture"), Some(Value::String(s)) if s == "llama")
    {
        return Err(refuse(
            "low-residency canonical profile requires general.architecture=llama".into(),
        ));
    }
    // Enforce the coordinator's resident-state bound *before* the shared
    // parser allocates RoPE tables. A qualified but unsuitable graph must
    // refuse without first allocating a large prepared state.
    let metadata_u64 = |name: &str| -> Option<u64> {
        match content.metadata.get(name) {
            Some(Value::U32(n)) => Some(u64::from(*n)),
            Some(Value::U64(n)) => Some(*n),
            Some(Value::I32(n)) => u64::try_from(*n).ok(),
            _ => None,
        }
    };
    let layers = metadata_u64("llama.block_count").unwrap_or(0);
    let width = metadata_u64("llama.embedding_length").unwrap_or(0);
    let heads = metadata_u64("llama.attention.head_count").unwrap_or(0);
    let state_bytes = layers
        .checked_mul(2)
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(width))
        .and_then(|n| n.checked_mul(8))
        .zip(
            width
                .checked_div(heads)
                .and_then(|n| n.checked_mul(4096 * 8)),
        )
        .and_then(|(norms, rope)| norms.checked_add(rope));
    if layers == 0
        || layers > 256
        || width == 0
        || heads == 0
        || state_bytes.is_none_or(|n| n > 64 * 1024 * 1024)
    {
        return Err(refuse("low-residency norms/RoPE exceed the 64 MiB prepared-state bound or have invalid dimensions".into()));
    }
    let (mut config, _) = crate::cached_integer_model::config_from_gguf(&content)?;
    if !config.d_head.is_multiple_of(2)
        || config.n_layers > 256
        || [config.d_model, config.d_kv, config.d_ff, config.vocab_size]
            .iter()
            .any(|&n| n > crate::tensor_parallel::MAX_ROW_INPUT_ELEMENTS)
    {
        return Err(refuse(
            "low-residency model dimensions exceed bounded canonical row layout".into(),
        ));
    }
    let mut source = GgufRows { file, content };
    let checked = (|| -> Result<_, TensorParallelError> {
        source.validate_tensor("token_embd.weight", &[config.vocab_size, config.d_model])?;
        let mut norms = Vec::with_capacity(config.n_layers);
        for layer in 0..config.n_layers {
            for tensor in [
                TensorKey::Wq,
                TensorKey::Wk,
                TensorKey::Wv,
                TensorKey::Wo,
                TensorKey::WGate,
                TensorKey::WUp,
                TensorKey::WDown,
            ] {
                let (rows, cols) = projection_shape(&config, Some(layer), tensor)
                    .ok_or(TensorParallelError::WrongIdentity)?;
                source.validate_tensor(&source.tensor_name(Some(layer), tensor), &[rows, cols])?;
            }
            norms.push((
                source.norm(&format!("blk.{layer}.attn_norm.weight"), config.d_model)?,
                source.norm(&format!("blk.{layer}.ffn_norm.weight"), config.d_model)?,
            ));
        }
        source.validate_tensor(
            &source.tensor_name(None, TensorKey::LmHead),
            &[config.vocab_size, config.d_model],
        )?;
        let final_norm = source.norm("output_norm.weight", config.d_model)?;
        Ok((norms, final_norm))
    })()
    .map_err(|e| refuse(format!("low-residency GGUF: {e}")))?;
    config.arithmetic_profile =
        crate::cached_integer_model::ArithmeticProfile::GgufInterleavedRowsV1;
    Ok(LowResidencyModel {
        artifact,
        config,
        norms: checked.0,
        final_norm: checked.1,
        source: std::sync::Mutex::new(source),
    })
}
#[cfg(not(feature = "candle"))]
pub fn load_low_residency_model(
    _path: &std::path::Path,
    _artifact: Hash256,
) -> Result<LowResidencyModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime(
        "candle feature not enabled".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    fn temporary(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "arc-low-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
    #[test]
    fn verified_lazy_reads_bind_the_hash_pass_and_reject_later_changes() {
        let path = temporary("chunks");
        let bytes: Vec<u8> = (0..VerifiedArtifact::CHUNK * 3 + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&path, &bytes).unwrap();
        assert!(VerifiedArtifact::open(&path, Hash256([1; 32])).is_err());
        let mut pinned = VerifiedArtifact::open(&path, arc_crypto::hash_bytes(&bytes)).unwrap();
        pinned
            .seek(SeekFrom::Start((VerifiedArtifact::CHUNK - 3) as u64))
            .unwrap();
        let mut across = [0; 12];
        pinned.read_exact(&mut across).unwrap();
        assert_eq!(
            &across,
            &bytes[VerifiedArtifact::CHUNK - 3..VerifiedArtifact::CHUNK + 9]
        );
        let mut changed = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        changed
            .seek(SeekFrom::Start((VerifiedArtifact::CHUNK * 2) as u64))
            .unwrap();
        changed.write_all(&[255]).unwrap();
        pinned
            .seek(SeekFrom::Start((VerifiedArtifact::CHUNK * 2) as u64))
            .unwrap();
        assert!(
            pinned
                .read_exact(&mut across)
                .unwrap_err()
                .to_string()
                .contains("changed after verification")
        );
        drop(changed);
        drop(pinned);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(feature = "candle")]
    fn tiny_gguf(
        dtype: candle_core::quantized::GgmlDType,
        omit: Option<&str>,
    ) -> std::path::PathBuf {
        use candle_core::{
            Device, Tensor,
            quantized::{GgmlDType, QTensor, gguf_file},
        };
        use gguf_file::Value;
        let metadata = [
            ("general.architecture", Value::String("llama".into())),
            ("llama.block_count", Value::U32(2)),
            ("llama.embedding_length", Value::U32(256)),
            ("llama.attention.head_count", Value::U32(4)),
            ("llama.attention.head_count_kv", Value::U32(2)),
            ("llama.feed_forward_length", Value::U32(256)),
            ("tokenizer.ggml.bos_token_id", Value::U32(1)),
            ("tokenizer.ggml.eos_token_id", Value::U32(2)),
        ];
        let mut tensors = Vec::new();
        let mut add = |name: String, shape: Vec<usize>| {
            if Some(name.as_str()) == omit {
                return;
            }
            let count = shape.iter().product::<usize>();
            let values: Vec<f32> = (0..count)
                .map(|i| {
                    if shape.len() == 1 {
                        1.0 + (i % 7) as f32 / 64.0
                    } else {
                        (((i * 13 + name.len()) % 97) as f32 - 48.0) / 1024.0
                    }
                })
                .collect();
            let t = Tensor::from_vec(values, shape.as_slice(), &Device::Cpu).unwrap();
            tensors.push((
                name,
                QTensor::quantize(
                    &t,
                    if shape.len() == 1 {
                        GgmlDType::F32
                    } else {
                        dtype
                    },
                )
                .unwrap(),
            ));
        };
        add("token_embd.weight".into(), vec![32, 256]);
        add("output.weight".into(), vec![32, 256]);
        add("output_norm.weight".into(), vec![256]);
        for layer in 0..2 {
            for (name, rows, cols) in [
                ("attn_q", 256, 256),
                ("attn_k", 128, 256),
                ("attn_v", 128, 256),
                ("attn_output", 256, 256),
                ("ffn_gate", 256, 256),
                ("ffn_up", 256, 256),
                ("ffn_down", 256, 256),
            ] {
                add(format!("blk.{layer}.{name}.weight"), vec![rows, cols]);
            }
            add(format!("blk.{layer}.attn_norm.weight"), vec![256]);
            add(format!("blk.{layer}.ffn_norm.weight"), vec![256]);
        }
        let path = temporary("fixture.gguf");
        let mut file = std::fs::File::create(&path).unwrap();
        let metadata: Vec<_> = metadata.iter().map(|(k, v)| (*k, v)).collect();
        let tensors: Vec<_> = tensors.iter().map(|(k, v)| (k.as_str(), v)).collect();
        gguf_file::write(&mut file, &metadata, &tensors).unwrap();
        path
    }
    #[cfg(feature = "candle")]
    struct SourceBackend<'a>(&'a dyn CanonicalRowSource);
    #[cfg(feature = "candle")]
    impl ProjectionBackend for SourceBackend<'_> {
        fn project_rows(
            &self,
            _: Hash256,
            layer: Option<usize>,
            tensor: TensorKey,
            input: &[i64],
            rows: usize,
        ) -> Result<Vec<i64>, TensorParallelError> {
            self.0.projection_rows(layer, tensor, 0, rows, input)
        }
    }
    #[cfg(feature = "candle")]
    #[test]
    fn bounded_gguf_rows_and_forward_match_full_canonical_across_storage_formats() {
        use crate::cached_integer_model::{
            KVCache, load_cached_model_canonical_i8_interleaved_rope,
        };
        use candle_core::quantized::GgmlDType;
        // Includes block quantizations used by Q4_K artifacts, GQA, Q/K row
        // permutation, embeddings, norms, output-head rows and full generation.
        for dtype in [
            GgmlDType::F32,
            GgmlDType::Q4_0,
            GgmlDType::Q4K,
            GgmlDType::Q6K,
            GgmlDType::Q8_0,
        ] {
            let path = tiny_gguf(dtype, None);
            let hash = arc_crypto::hash_bytes(&std::fs::read(&path).unwrap());
            let mut full =
                load_cached_model_canonical_i8_interleaved_rope(path.to_str().unwrap()).unwrap();
            let mut low = load_low_residency_model(&path, hash).unwrap();
            assert_eq!(
                full.canonical_execution_profile(),
                low.canonical_execution_profile()
            );
            assert_eq!(full.config.rope_cos, low.config.rope_cos);
            assert_eq!(full.config.rope_sin, low.config.rope_sin);
            for token in [0, 1, 31] {
                assert_eq!(
                    full.embedding(token).unwrap(),
                    low.embedding(token).unwrap()
                );
            }
            let keys = (0..2)
                .flat_map(|l| {
                    [
                        TensorKey::Wq,
                        TensorKey::Wk,
                        TensorKey::Wv,
                        TensorKey::Wo,
                        TensorKey::WGate,
                        TensorKey::WUp,
                        TensorKey::WDown,
                    ]
                    .into_iter()
                    .map(move |t| (Some(l), t))
                })
                .chain(std::iter::once((None, TensorKey::LmHead)));
            for (layer, tensor) in keys {
                let (rows, cols) = low.projection_shape(layer, tensor).unwrap();
                let input: Vec<i64> = (0..cols).map(|i| ((i % 13) as i64 - 6) * 12345).collect();
                assert_eq!(
                    full.projection_rows(layer, tensor, 0, rows, &input)
                        .unwrap(),
                    low.projection_rows(layer, tensor, 0, rows, &input).unwrap(),
                    "{dtype:?}/{layer:?}/{tensor:?}"
                );
            }
            let mut a = KVCache::new(2);
            let mut b = KVCache::new(2);
            let backend = SourceBackend(&full);
            for (pos, token) in [1, 4, 7, 3].into_iter().enumerate() {
                let call = Hash256([pos as u8; 32]);
                let expected = full
                    .forward_one_token_canonical_i8_with_backend(token, &mut a, call, &backend)
                    .unwrap();
                let actual = low
                    .forward_one_token_with_backend(token, &mut b, call, &backend)
                    .unwrap();
                assert_eq!(actual, expected);
                assert_eq!(a.seq_len, b.seq_len);
                assert_eq!(a.k_data, b.k_data);
                assert_eq!(a.v_data, b.v_data);
            }
            // Exact context boundary (BOS+2 prompt+3 generated) and plus one.
            full.config.max_seq = 6;
            low.config.max_seq = 6;
            let expected = full.try_generate_v2(&[4, 7], 3, &[]).unwrap();
            let actual = low
                .try_generate_v2_with_backend(
                    Hash256([9; 32]),
                    &[4, 7],
                    3,
                    &[],
                    &SourceBackend(&low),
                )
                .unwrap();
            assert_eq!(actual, expected);
            assert!(matches!(
                low.try_generate_v2_with_backend(
                    Hash256([9; 32]),
                    &[4, 7],
                    4,
                    &[],
                    &SourceBackend(&low)
                ),
                Err(BackendGenerationError::Generation(_))
            ));
            assert!(low.embedding(32).is_err());
            // Exported rows are byte-identical to the qualified full loader's
            // format, including permuted Q/K and the layerless output head.
            for (layer, tensor) in [
                (Some(0), TensorKey::Wq),
                (Some(0), TensorKey::Wk),
                (None, TensorKey::LmHead),
            ] {
                let assignment = crate::tensor_parallel::RowAssignment {
                    artifact_id: hash,
                    execution_profile: low.canonical_execution_profile().unwrap().into(),
                    layer,
                    tensor,
                    row_start: 0,
                    row_end: low.projection_shape(layer, tensor).unwrap().0,
                    worker_id: "fixture".into(),
                };
                let a = temporary("full.arcrow");
                let b = temporary("low.arcrow");
                crate::tensor_parallel::export_verified_model_rows(&a, &full, hash, &assignment)
                    .unwrap();
                low.export_rows(&b, &assignment).unwrap();
                assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
                std::fs::remove_file(a).unwrap();
                std::fs::remove_file(b).unwrap();
            }
            drop(low);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[cfg(feature = "candle")]
    #[test]
    fn low_residency_requires_every_projection_and_norm_but_accepts_tied_output() {
        use candle_core::quantized::GgmlDType;
        for omit in ["blk.1.ffn_down.weight", "blk.0.attn_norm.weight"] {
            let path = tiny_gguf(GgmlDType::F32, Some(omit));
            let hash = arc_crypto::hash_bytes(&std::fs::read(&path).unwrap());
            assert!(load_low_residency_model(&path, hash).is_err());
            std::fs::remove_file(path).unwrap();
        }
        let path = tiny_gguf(GgmlDType::F32, Some("output.weight"));
        let hash = arc_crypto::hash_bytes(&std::fs::read(&path).unwrap());
        let low = load_low_residency_model(&path, hash).unwrap();
        let full = crate::cached_integer_model::load_cached_model_canonical_i8_interleaved_rope(
            path.to_str().unwrap(),
        )
        .unwrap();
        let input = vec![1234; 256];
        assert_eq!(
            low.projection_rows(None, TensorKey::LmHead, 0, 32, &input)
                .unwrap(),
            full.projection_rows(None, TensorKey::LmHead, 0, 32, &input)
                .unwrap()
        );
        drop(low);
        std::fs::remove_file(path).unwrap();
    }
}
