use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use arc_crypto::Hash256;

use crate::cached_integer_model::{CachedIntegerModel, CachedLayer};
use crate::gguf_meta::MetaError;
use crate::model_artifact::{ModelArtifactCommitment, ModelArtifactLoadError};

#[cfg(feature = "candle")]
use std::collections::BTreeMap;
#[cfg(feature = "candle")]
use std::io::{BufReader, Cursor, Read};

#[cfg(feature = "candle")]
use candle_core::quantized::gguf_file;

#[cfg(feature = "candle")]
use crate::cached_integer_model::{ArithmeticProfile, ModelConfig, config_from_gguf};
#[cfg(feature = "candle")]
use crate::gguf_meta::{MetaValue, TensorEntry, inventory_digest, read_header, supported_adapter};

pub const MAX_N_LAYERS: usize = 1_024;
pub const MAX_D_MODEL: usize = 65_536;
pub const MAX_D_FF: usize = 262_144;
pub const MAX_D_HEAD: usize = 1_024;
pub const MAX_VOCAB_SIZE: usize = 1_000_000;
pub const CANONICAL_MAX_SEQ: usize = 4_096;
pub const MAX_ROPE_TABLE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_GGUF_HEADER_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_METADATA_ARRAY_ELEMENTS: u64 = 4_000_000;
pub const MAX_EOS_TOKENS: u64 = 64;

pub const I8_WEIGHT_BYTES_PER_ELEMENT: u64 = 1;
pub const SCALE_BYTES_PER_ROW: u64 = 8;
pub const WEIGHT_STRUCT_OVERHEAD_BYTES: u64 = 16;
pub const EMBEDDING_Q16_BYTES_PER_ELEMENT: u64 = 8;
pub const FIRST_EMBEDDING_BYTES_PER_ELEMENT: u64 =
    EMBEDDING_Q16_BYTES_PER_ELEMENT + I8_WEIGHT_BYTES_PER_ELEMENT;
pub const NORM_WEIGHT_BYTES_PER_ELEMENT: u64 = 8;
pub const ROPE_ENTRY_BYTES: u64 = 8;
pub const EXECUTION_SCRATCH_SCALAR_BYTES: u64 = 8;
pub const MAX_TENSOR_LOAD_BYTES_PER_ELEMENT: u64 = 24;
pub const FIXED_OVERHEAD_MARGIN_BYTES: u64 = 16 * 1024 * 1024;
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, thiserror::Error)]
pub enum ShardAdmissionError {
    #[error("canonical shard metadata verification cancelled")]
    Cancelled,

    #[error("candle feature not enabled: canonical shard GGUF inspection requires candle")]
    CandleFeatureDisabled,

    #[error("failed to open or read GGUF {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("GGUF header exceeds maximum inspection limit of {limit_bytes} bytes")]
    HeaderTooLarge { limit_bytes: u64 },

    #[error("bounded GGUF header inspection failed: {0}")]
    Header(#[from] MetaError),

    #[error("unsupported GGUF architecture: expected \"llama\", found {found}")]
    UnsupportedArchitecture { found: String },

    #[error("invalid GGUF metadata {key}: {reason}")]
    InvalidMetadata { key: String, reason: String },

    #[error("GGUF geometry exceeds safety ceiling: {detail}")]
    DimensionCeilingExceeded { detail: String },

    #[error("malformed GGUF tensor geometry for {tensor}: {reason}")]
    MalformedTensorGeometry { tensor: String, reason: String },

    #[error("missing required GGUF tensor: {tensor}")]
    MissingTensor { tensor: String },

    #[error("GGUF content parse failed: {0}")]
    GgufParse(String),

    #[error("GGUF canonical config validation failed: {0}")]
    Config(String),

    #[error("pinned model artifact verification failed: {0}")]
    ArtifactVerification(String),

    #[error(
        "Invalid canonical shard range: [{start_layer}, {end_layer}) {reason} (n_layers={n_layers})"
    )]
    InvalidRange {
        start_layer: usize,
        end_layer: usize,
        n_layers: usize,
        reason: String,
    },

    #[error("checked integer arithmetic overflow while computing {context}")]
    ArithmeticOverflow { context: &'static str },

    #[error("invalid admission budget for {field}: must be greater than zero")]
    InvalidBudget { field: &'static str },

    #[error(
        "canonical shard [{start_layer}, {end_layer}) exceeds conservative working-set payload \
         budget: peak_loading_budget_bytes={peak_loading_budget_bytes} \
         (resident_budget_bytes={resident_budget_bytes}, \
         reserved_kv_cache_bytes={reserved_kv_cache_bytes}) exceeds \
         max_working_set_bytes={max_working_set_bytes} by {shortfall_bytes} bytes{role_note}; \
         assign a smaller layer range or provision more RAM"
    )]
    BudgetExceeded {
        start_layer: usize,
        end_layer: usize,
        resident_budget_bytes: u64,
        peak_loading_budget_bytes: u64,
        reserved_kv_cache_bytes: u64,
        max_working_set_bytes: u64,
        shortfall_bytes: u64,
        role_note: &'static str,
    },
}

impl ShardAdmissionError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactTrustBoundary {
    UnverifiedHeaderOnly,
    VerifiedBlake3Commitment { model_id: Hash256, size_bytes: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShardLayerRange {
    pub start_layer: usize,
    pub end_layer: usize,
}

impl ShardLayerRange {
    pub const fn new(start_layer: usize, end_layer: usize) -> Self {
        Self {
            start_layer,
            end_layer,
        }
    }

    pub const fn owned_layers(&self) -> usize {
        self.end_layer.saturating_sub(self.start_layer)
    }

    pub const fn is_first(&self) -> bool {
        self.start_layer == 0
    }

    pub const fn is_last(&self, n_layers: usize) -> bool {
        self.end_layer == n_layers
    }
}

impl From<std::ops::Range<usize>> for ShardLayerRange {
    fn from(range: std::ops::Range<usize>) -> Self {
        Self::new(range.start, range.end)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardAdmissionEstimate {
    pub range: ShardLayerRange,
    pub owned_layers: usize,
    pub is_first_shard: bool,
    pub is_last_shard: bool,
    pub layer_weights_bytes: u64,
    pub layer_norms_bytes: u64,
    pub first_embedding_bytes: u64,
    pub output_head_and_norm_bytes: u64,
    pub rope_tables_bytes: u64,
    pub vocab_and_metadata_storage_bytes: u64,
    pub placeholder_struct_overhead_bytes: u64,
    pub execution_scratch_bytes: u64,
    pub fixed_overhead_margin_bytes: u64,
    pub reserved_kv_cache_bytes: u64,
    pub resident_payload_bytes: u64,
    pub resident_budget_bytes: u64,
    pub max_loaded_tensor_elements: u64,
    pub worst_tensor_load_transient_bytes: u64,
    pub loader_temporary_overhead_bytes: u64,
    pub peak_loading_transient_bytes: u64,
    pub peak_loading_budget_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalShardMetadata {
    architecture: String,
    canonical_profile: &'static str,
    n_layers: usize,
    d_model: usize,
    n_heads: usize,
    n_kv_heads: usize,
    d_ff: usize,
    d_kv: usize,
    d_head: usize,
    vocab_size: usize,
    max_seq: usize,
    bos_token: u32,
    eos_tokens: Vec<u32>,
    attn_scale: i64,
    has_tied_output_head: bool,
    header_bytes: u64,
    header_storage_budget_bytes: u64,
    header_inventory_digest: [u8; 32],
    vocab_entries: usize,
    vocab_string_payload_bytes: u64,
    chat_template_bytes: u64,
    trust_boundary: ArtifactTrustBoundary,
}

impl CanonicalShardMetadata {
    #[cfg(feature = "candle")]
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ShardAdmissionError> {
        let path = path.as_ref();
        let file = std::fs::File::open(path).map_err(|source| ShardAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_reader_with_limit(BufReader::new(file), MAX_GGUF_HEADER_BYTES)
    }

    #[cfg(feature = "candle")]
    fn from_path_cancellable(
        path: impl AsRef<Path>,
        cancel: &AtomicBool,
    ) -> Result<Self, ShardAdmissionError> {
        if cancel.load(Ordering::Acquire) {
            return Err(ShardAdmissionError::Cancelled);
        }
        let path = path.as_ref();
        let file = std::fs::File::open(path).map_err(|source| ShardAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_reader_with_limit_cancellable(
            BufReader::new(file),
            MAX_GGUF_HEADER_BYTES,
            Some(cancel),
        )
    }

    #[cfg(feature = "candle")]
    fn from_reader_with_limit<R: Read>(
        reader: R,
        max_header_bytes: u64,
    ) -> Result<Self, ShardAdmissionError> {
        Self::from_reader_with_limit_cancellable(reader, max_header_bytes, None)
    }

    #[cfg(feature = "candle")]
    fn from_reader_with_limit_cancellable<R: Read>(
        reader: R,
        max_header_bytes: u64,
        cancel: Option<&AtomicBool>,
    ) -> Result<Self, ShardAdmissionError> {
        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(ShardAdmissionError::Cancelled);
        }
        let ParsedGgufHeader {
            metadata: meta,
            tensors,
            captured_header,
        } = read_bounded_header_with_limit_cancellable(reader, max_header_bytes, cancel)?;
        let header_bytes = captured_header.len_u64()?;
        let header = validate_header_metadata_and_tensors(&meta, &tensors)?;
        let header_inventory_digest = inventory_digest(&tensors);
        let array_elements = meta.values().try_fold(0u64, |sum, value| {
            let count = match value {
                MetaValue::Array { len } => *len,
                _ => 0,
            };
            checked_add(sum, count, "metadata array elements")
        })?;
        let value_slots = checked_mul(
            array_elements,
            2 * std::mem::size_of::<gguf_file::Value>() as u64,
            "metadata array value storage",
        )?;
        let map_storage = checked_mul(
            usize_to_u64(meta.len() + tensors.len(), "header map entries")?,
            512,
            "header map allocation margin",
        )?;
        let header_storage_budget_bytes = checked_add(
            checked_mul(2, header_bytes, "metadata string storage")?,
            checked_add(value_slots, map_storage, "header containers")?,
            "metadata heap storage",
        )?;
        drop((meta, tensors));

        let content = {
            let mut cursor = Cursor::new(captured_header.as_slice());
            let content = gguf_file::Content::read(&mut cursor)
                .map_err(|error| ShardAdmissionError::GgufParse(error.to_string()))?;
            if cursor.position() != header_bytes {
                return Err(ShardAdmissionError::GgufParse(format!(
                    "GGUF header byte span mismatch between bounded header parser ({header_bytes}) \
                     and candle Content::read ({})",
                    cursor.position()
                )));
            }
            content
        };
        drop(captured_header);

        validate_candle_content(&content, &header)?;

        let (config, vocab) = config_from_gguf(&content)
            .map_err(|error| ShardAdmissionError::Config(error.to_string()))?;
        drop(content);
        validate_config_matches_header(&config, &header)?;

        let mut vocab_string_payload_bytes = 0u64;
        for token in &vocab {
            let token_bytes = usize_to_u64(token.len(), "vocab token length")?;
            vocab_string_payload_bytes = checked_add(
                vocab_string_payload_bytes,
                token_bytes,
                "vocab string bytes",
            )?;
        }

        Ok(Self {
            architecture: "llama".to_string(),
            canonical_profile: header.canonical_profile,
            n_layers: config.n_layers,
            d_model: config.d_model,
            n_heads: config.n_heads,
            n_kv_heads: config.n_kv_heads,
            d_ff: config.d_ff,
            d_kv: config.d_kv,
            d_head: config.d_head,
            vocab_size: config.vocab_size,
            max_seq: config.max_seq,
            bos_token: config.bos_token,
            eos_tokens: config.eos_tokens,
            attn_scale: config.attn_scale,
            has_tied_output_head: header.has_tied_output_head,
            header_bytes,
            header_storage_budget_bytes,
            header_inventory_digest,
            vocab_entries: vocab.len(),
            vocab_string_payload_bytes,
            chat_template_bytes: header.chat_template_bytes,
            trust_boundary: ArtifactTrustBoundary::UnverifiedHeaderOnly,
        })
    }

    #[cfg(not(feature = "candle"))]
    pub fn from_path(_path: impl AsRef<Path>) -> Result<Self, ShardAdmissionError> {
        Err(ShardAdmissionError::CandleFeatureDisabled)
    }

    #[cfg(not(feature = "candle"))]
    fn from_path_cancellable(
        _path: impl AsRef<Path>,
        cancel: &AtomicBool,
    ) -> Result<Self, ShardAdmissionError> {
        if cancel.load(Ordering::Acquire) {
            return Err(ShardAdmissionError::Cancelled);
        }
        Err(ShardAdmissionError::CandleFeatureDisabled)
    }

    pub fn from_committed_artifact(
        commitment: &ModelArtifactCommitment,
    ) -> Result<Self, ShardAdmissionError> {
        Self::from_committed_artifact_cancellable(commitment, &NEVER_CANCELLED)
    }

    pub fn from_committed_artifact_cancellable(
        commitment: &ModelArtifactCommitment,
        cancel: &AtomicBool,
    ) -> Result<Self, ShardAdmissionError> {
        Self::from_committed_artifact_cancellable_with_hook(commitment, cancel, || {})
    }

    fn from_committed_artifact_cancellable_with_hook(
        commitment: &ModelArtifactCommitment,
        cancel: &AtomicBool,
        after_header: impl FnOnce(),
    ) -> Result<Self, ShardAdmissionError> {
        if cancel.load(Ordering::Acquire) {
            return Err(ShardAdmissionError::Cancelled);
        }
        let mut metadata = Self::from_path_cancellable(commitment.path(), cancel)?;
        after_header();
        commitment
            .verify_unchanged_cancellable(cancel)
            .map_err(|error| match error {
                ModelArtifactLoadError::Cancelled => ShardAdmissionError::Cancelled,
                ModelArtifactLoadError::Inference(error) => {
                    ShardAdmissionError::ArtifactVerification(error.to_string())
                }
            })?;
        if cancel.load(Ordering::Acquire) {
            return Err(ShardAdmissionError::Cancelled);
        }
        metadata.trust_boundary = ArtifactTrustBoundary::VerifiedBlake3Commitment {
            model_id: commitment.model_id(),
            size_bytes: commitment.size_bytes(),
        };
        Ok(metadata)
    }

    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    pub fn canonical_profile(&self) -> &'static str {
        self.canonical_profile
    }

    pub fn n_layers(&self) -> usize {
        self.n_layers
    }

    pub fn d_model(&self) -> usize {
        self.d_model
    }

    pub fn n_heads(&self) -> usize {
        self.n_heads
    }

    pub fn n_kv_heads(&self) -> usize {
        self.n_kv_heads
    }

    pub fn d_ff(&self) -> usize {
        self.d_ff
    }

    pub fn d_kv(&self) -> usize {
        self.d_kv
    }

    pub fn d_head(&self) -> usize {
        self.d_head
    }

    pub fn vocab_size(&self) -> usize {
        self.vocab_size
    }

    pub fn max_seq(&self) -> usize {
        self.max_seq
    }

    pub fn bos_token(&self) -> u32 {
        self.bos_token
    }

    pub fn eos_tokens(&self) -> &[u32] {
        &self.eos_tokens
    }

    pub fn attn_scale(&self) -> i64 {
        self.attn_scale
    }

    pub fn has_tied_output_head(&self) -> bool {
        self.has_tied_output_head
    }

    pub fn header_bytes(&self) -> u64 {
        self.header_bytes
    }

    pub fn header_inventory_digest(&self) -> [u8; 32] {
        self.header_inventory_digest
    }

    pub fn vocab_entries(&self) -> usize {
        self.vocab_entries
    }

    pub fn vocab_string_payload_bytes(&self) -> u64 {
        self.vocab_string_payload_bytes
    }

    pub fn chat_template_bytes(&self) -> u64 {
        self.chat_template_bytes
    }

    pub fn trust_boundary(&self) -> ArtifactTrustBoundary {
        self.trust_boundary
    }

    pub fn max_shard_layers(&self) -> usize {
        self.n_layers.div_ceil(2)
    }

    pub fn validate_range(
        &self,
        range: impl Into<ShardLayerRange>,
    ) -> Result<ShardLayerRange, ShardAdmissionError> {
        let range = range.into();
        if range.start_layer >= range.end_layer {
            return Err(ShardAdmissionError::InvalidRange {
                start_layer: range.start_layer,
                end_layer: range.end_layer,
                n_layers: self.n_layers,
                reason: "is empty or reversed".into(),
            });
        }
        if range.end_layer > self.n_layers {
            return Err(ShardAdmissionError::InvalidRange {
                start_layer: range.start_layer,
                end_layer: range.end_layer,
                n_layers: self.n_layers,
                reason: "is out of bounds".into(),
            });
        }
        let owned_layers = range.end_layer - range.start_layer;
        if owned_layers == self.n_layers {
            return Err(ShardAdmissionError::InvalidRange {
                start_layer: range.start_layer,
                end_layer: range.end_layer,
                n_layers: self.n_layers,
                reason: "covers full model".into(),
            });
        }
        let max_owned_layers = self.max_shard_layers();
        if owned_layers > max_owned_layers {
            return Err(ShardAdmissionError::InvalidRange {
                start_layer: range.start_layer,
                end_layer: range.end_layer,
                n_layers: self.n_layers,
                reason: format!(
                    "owns {owned_layers} layers, exceeding ceil(n_layers/2)={max_owned_layers}"
                ),
            });
        }
        Ok(range)
    }

    pub fn estimate_range(
        &self,
        range: impl Into<ShardLayerRange>,
        reserved_kv_cache_bytes: u64,
    ) -> Result<ShardAdmissionEstimate, ShardAdmissionError> {
        let range = self.validate_range(range)?;
        let owned_layers = range.owned_layers();
        let is_first_shard = range.is_first();
        let is_last_shard = range.is_last(self.n_layers);

        let nl = usize_to_u64(self.n_layers, "n_layers")?;
        let owned = usize_to_u64(owned_layers, "owned_layers")?;
        let d = usize_to_u64(self.d_model, "d_model")?;
        let d_kv = usize_to_u64(self.d_kv, "d_kv")?;
        let d_ff = usize_to_u64(self.d_ff, "d_ff")?;
        let d_head = usize_to_u64(self.d_head, "d_head")?;
        let n_heads = usize_to_u64(self.n_heads, "n_heads")?;
        let v = usize_to_u64(self.vocab_size, "vocab_size")?;
        let max_seq = usize_to_u64(self.max_seq, "max_seq")?;

        let wq_wo = checked_mul(2, i8_matrix_bytes(d, d)?, "attn_q/attn_output weights")?;
        let wk_wv = checked_mul(2, i8_matrix_bytes(d_kv, d)?, "attn_k/attn_v weights")?;
        let wg_wu = checked_mul(2, i8_matrix_bytes(d_ff, d)?, "ffn_gate/ffn_up weights")?;
        let wd = i8_matrix_bytes(d, d_ff)?;
        let per_layer_weights = checked_add(
            checked_add(wq_wo, wk_wv, "attention layer weights")?,
            checked_add(wg_wu, wd, "ffn layer weights")?,
            "per-layer weights",
        )?;
        let layer_weights_bytes = checked_mul(per_layer_weights, owned, "assigned layer weights")?;

        let single_norm_bytes = checked_mul(d, NORM_WEIGHT_BYTES_PER_ELEMENT, "single norm bytes")?;
        let per_layer_norms = checked_mul(2, single_norm_bytes, "per-layer norm bytes")?;
        let layer_norms_bytes = checked_mul(per_layer_norms, owned, "assigned layer norms")?;

        let vocab_d_elements = checked_mul(v, d, "vocab * d_model elements")?;
        let first_embedding_bytes = if is_first_shard {
            let payload = checked_mul(
                vocab_d_elements,
                FIRST_EMBEDDING_BYTES_PER_ELEMENT,
                "first embedding q16+i8 payload",
            )?;
            let scales = checked_mul(v, SCALE_BYTES_PER_ROW, "first embedding scales")?;
            checked_add(
                checked_add(payload, scales, "first embedding payload+scales")?,
                WEIGHT_STRUCT_OVERHEAD_BYTES,
                "first embedding struct overhead",
            )?
        } else {
            0
        };

        let output_head_and_norm_bytes = if is_last_shard {
            let output_head = i8_matrix_bytes(v, d)?;
            checked_add(output_head, single_norm_bytes, "output head + final norm")?
        } else {
            0
        };

        let rope_half_head = d_head / 2;
        let rope_entries = checked_mul(max_seq, rope_half_head, "rope table entries")?;
        let rope_single_table = checked_mul(rope_entries, ROPE_ENTRY_BYTES, "single rope table")?;
        let rope_tables_bytes = checked_mul(2, rope_single_table, "rope cos+sin tables")?;

        let vocab_slots = checked_mul(
            usize_to_u64(self.vocab_entries, "vocab_entries")?,
            usize_to_u64(std::mem::size_of::<String>(), "string struct size")?,
            "vocab vec slots",
        )?;
        let eos_storage = checked_mul(
            usize_to_u64(self.eos_tokens.len(), "eos_tokens len")?,
            usize_to_u64(std::mem::size_of::<u32>(), "eos token size")?,
            "eos tokens storage",
        )?;
        let vocab_and_metadata_storage_bytes = checked_add(
            checked_add(
                vocab_slots,
                self.vocab_string_payload_bytes,
                "vocab storage",
            )?,
            checked_add(
                self.chat_template_bytes,
                eos_storage,
                "metadata string storage",
            )?,
            "vocab and metadata storage",
        )?;

        let cached_layer_size =
            usize_to_u64(std::mem::size_of::<CachedLayer>(), "cached layer size")?;
        let model_struct_size = usize_to_u64(
            std::mem::size_of::<CachedIntegerModel>(),
            "cached model struct size",
        )?;
        let layer_vec_overhead = checked_mul(nl, cached_layer_size, "layer placeholder vec")?;
        let placeholder_struct_overhead_bytes = checked_add(
            layer_vec_overhead,
            model_struct_size,
            "placeholder struct overhead",
        )?;

        let hidden_scratch = checked_mul(
            checked_mul(8, d, "hidden scratch scalars")?,
            EXECUTION_SCRATCH_SCALAR_BYTES,
            "hidden scratch bytes",
        )?;
        let kv_scratch = checked_mul(
            checked_mul(2, d_kv, "kv scratch scalars")?,
            EXECUTION_SCRATCH_SCALAR_BYTES,
            "kv scratch bytes",
        )?;
        let ffn_scratch = checked_mul(
            checked_mul(3, d_ff, "ffn scratch scalars")?,
            EXECUTION_SCRATCH_SCALAR_BYTES,
            "ffn scratch bytes",
        )?;
        let attn_scratch = checked_mul(
            checked_mul(
                2,
                checked_mul(n_heads, max_seq, "attention scores")?,
                "attn buffers",
            )?,
            EXECUTION_SCRATCH_SCALAR_BYTES,
            "attention scratch bytes",
        )?;
        let logits_scratch = if is_last_shard {
            checked_mul(
                checked_mul(2, v, "logits scratch scalars")?,
                EXECUTION_SCRATCH_SCALAR_BYTES,
                "logits scratch bytes",
            )?
        } else {
            0
        };
        let execution_scratch_bytes = checked_add(
            checked_add(
                checked_add(hidden_scratch, kv_scratch, "hidden+kv scratch")?,
                checked_add(ffn_scratch, attn_scratch, "ffn+attn scratch")?,
                "layer execution scratch",
            )?,
            logits_scratch,
            "total execution scratch",
        )?;

        let fixed_overhead_margin_bytes = FIXED_OVERHEAD_MARGIN_BYTES;
        let weights_and_embeddings = checked_add(
            checked_add(layer_weights_bytes, layer_norms_bytes, "layer payload")?,
            checked_add(
                first_embedding_bytes,
                output_head_and_norm_bytes,
                "edge tensor payload",
            )?,
            "weights and embeddings payload",
        )?;
        let shared_and_runtime = checked_add(
            checked_add(
                rope_tables_bytes,
                vocab_and_metadata_storage_bytes,
                "rope+vocab payload",
            )?,
            checked_add(
                placeholder_struct_overhead_bytes,
                checked_add(
                    execution_scratch_bytes,
                    fixed_overhead_margin_bytes,
                    "scratch+margin",
                )?,
                "runtime overhead payload",
            )?,
            "shared and runtime payload",
        )?;
        let resident_payload_bytes = checked_add(
            weights_and_embeddings,
            shared_and_runtime,
            "resident payload",
        )?;
        let resident_budget_bytes = checked_add(
            resident_payload_bytes,
            reserved_kv_cache_bytes,
            "resident payload + reserved kv cache",
        )?;

        let dd_elements = checked_mul(d, d, "d_model * d_model elements")?;
        let dff_d_elements = checked_mul(d_ff, d, "d_ff * d_model elements")?;
        let mut max_loaded_tensor_elements = dd_elements.max(dff_d_elements);
        if is_first_shard || is_last_shard {
            max_loaded_tensor_elements = max_loaded_tensor_elements.max(vocab_d_elements);
        }
        let worst_tensor_load_transient_bytes = checked_mul(
            max_loaded_tensor_elements,
            MAX_TENSOR_LOAD_BYTES_PER_ELEMENT,
            "worst tensor load transient",
        )?;
        let temp_optional_layer_vecs = checked_mul(
            2,
            layer_vec_overhead,
            "temporary i16/q4 placeholder layer vecs",
        )?;
        let header_load_transient = self.header_storage_budget_bytes;
        let loader_temporary_overhead_bytes = checked_add(
            temp_optional_layer_vecs,
            header_load_transient,
            "loader temporary overhead",
        )?;
        let peak_loading_transient_bytes = checked_add(
            worst_tensor_load_transient_bytes,
            loader_temporary_overhead_bytes,
            "peak loading transient",
        )?;
        let peak_loading_budget_bytes = checked_add(
            resident_budget_bytes,
            peak_loading_transient_bytes,
            "peak loading budget",
        )?;

        Ok(ShardAdmissionEstimate {
            range,
            owned_layers,
            is_first_shard,
            is_last_shard,
            layer_weights_bytes,
            layer_norms_bytes,
            first_embedding_bytes,
            output_head_and_norm_bytes,
            rope_tables_bytes,
            vocab_and_metadata_storage_bytes,
            placeholder_struct_overhead_bytes,
            execution_scratch_bytes,
            fixed_overhead_margin_bytes,
            reserved_kv_cache_bytes,
            resident_payload_bytes,
            resident_budget_bytes,
            max_loaded_tensor_elements,
            worst_tensor_load_transient_bytes,
            loader_temporary_overhead_bytes,
            peak_loading_transient_bytes,
            peak_loading_budget_bytes,
        })
    }

    pub fn admit(
        &self,
        range: impl Into<ShardLayerRange>,
        max_working_set_bytes: u64,
        max_kv_cache_bytes: u64,
    ) -> Result<ShardAdmissionEstimate, ShardAdmissionError> {
        if max_working_set_bytes == 0 {
            return Err(ShardAdmissionError::InvalidBudget {
                field: "max_working_set_bytes",
            });
        }
        if max_kv_cache_bytes == 0 {
            return Err(ShardAdmissionError::InvalidBudget {
                field: "max_kv_cache_bytes",
            });
        }
        let estimate = self.estimate_range(range, max_kv_cache_bytes)?;
        if estimate.peak_loading_budget_bytes > max_working_set_bytes {
            let role_note = if estimate.is_first_shard && estimate.is_last_shard {
                " (includes first-shard Q16+I8 embeddings and last-shard output head + final norm)"
            } else if estimate.is_first_shard {
                " (includes first-shard Q16+I8 embeddings)"
            } else if estimate.is_last_shard {
                " (includes last-shard output head + final norm)"
            } else {
                ""
            };
            return Err(ShardAdmissionError::BudgetExceeded {
                start_layer: estimate.range.start_layer,
                end_layer: estimate.range.end_layer,
                resident_budget_bytes: estimate.resident_budget_bytes,
                peak_loading_budget_bytes: estimate.peak_loading_budget_bytes,
                reserved_kv_cache_bytes: estimate.reserved_kv_cache_bytes,
                max_working_set_bytes,
                shortfall_bytes: estimate.peak_loading_budget_bytes - max_working_set_bytes,
                role_note,
            });
        }
        Ok(estimate)
    }
}

fn checked_add(a: u64, b: u64, context: &'static str) -> Result<u64, ShardAdmissionError> {
    a.checked_add(b)
        .ok_or(ShardAdmissionError::ArithmeticOverflow { context })
}

fn checked_mul(a: u64, b: u64, context: &'static str) -> Result<u64, ShardAdmissionError> {
    a.checked_mul(b)
        .ok_or(ShardAdmissionError::ArithmeticOverflow { context })
}

fn usize_to_u64(value: usize, context: &'static str) -> Result<u64, ShardAdmissionError> {
    u64::try_from(value).map_err(|_| ShardAdmissionError::ArithmeticOverflow { context })
}

fn i8_matrix_bytes(rows: u64, cols: u64) -> Result<u64, ShardAdmissionError> {
    let elements = checked_mul(rows, cols, "i8 matrix elements")?;
    let data_bytes = checked_mul(elements, I8_WEIGHT_BYTES_PER_ELEMENT, "i8 matrix data")?;
    let scale_bytes = checked_mul(rows, SCALE_BYTES_PER_ROW, "i8 matrix scales")?;
    checked_add(
        checked_add(data_bytes, scale_bytes, "i8 matrix data+scales")?,
        WEIGHT_STRUCT_OVERHEAD_BYTES,
        "i8 matrix struct overhead",
    )
}

#[cfg(feature = "candle")]
struct HeaderGeometry {
    canonical_profile: &'static str,
    n_layers: usize,
    d_model: usize,
    n_heads: usize,
    n_kv_heads: usize,
    d_ff: usize,
    d_head: usize,
    d_kv: usize,
    vocab_size: usize,
    has_tied_output_head: bool,
    chat_template_bytes: u64,
}

#[cfg(feature = "candle")]
struct CapturedGgufHeader {
    bytes: Vec<u8>,
}

#[cfg(feature = "candle")]
impl CapturedGgufHeader {
    fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    fn len_u64(&self) -> Result<u64, ShardAdmissionError> {
        usize_to_u64(self.bytes.len(), "captured gguf header length")
    }
}

#[cfg(feature = "candle")]
struct BoundedHeaderRecorder<'a, R> {
    inner: R,
    captured: Vec<u8>,
    limit_bytes: u64,
    limit_exceeded: bool,
    cancel: Option<&'a AtomicBool>,
    cancelled: bool,
}

#[cfg(feature = "candle")]
impl<'a, R: Read> BoundedHeaderRecorder<'a, R> {
    fn new(inner: R, limit_bytes: u64, cancel: Option<&'a AtomicBool>) -> Self {
        Self {
            inner,
            captured: Vec::new(),
            limit_bytes,
            limit_exceeded: false,
            cancel,
            cancelled: false,
        }
    }

    fn into_parts(self) -> (CapturedGgufHeader, R) {
        (
            CapturedGgufHeader {
                bytes: self.captured,
            },
            self.inner,
        )
    }
}

#[cfg(feature = "candle")]
impl<R: Read> Read for BoundedHeaderRecorder<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            self.cancelled = true;
            return Err(std::io::Error::other(
                "canonical shard header inspection cancelled",
            ));
        }
        let remaining = self.limit_bytes.saturating_sub(self.captured.len() as u64);
        if remaining == 0 {
            if buf.is_empty() {
                return Ok(0);
            }
            let mut probe = [0u8; 1];
            let n = self.inner.read(&mut probe)?;
            if n > 0 {
                self.limit_exceeded = true;
            }
            return Ok(0);
        }
        let max_read = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let n = self.inner.read(&mut buf[..max_read])?;
        if self.cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            self.cancelled = true;
            return Err(std::io::Error::other(
                "canonical shard header inspection cancelled",
            ));
        }
        self.captured.extend_from_slice(&buf[..n]);
        Ok(n)
    }
}

#[cfg(feature = "candle")]
struct ParsedGgufHeader {
    metadata: BTreeMap<String, MetaValue>,
    tensors: Vec<TensorEntry>,
    captured_header: CapturedGgufHeader,
}

#[cfg(all(test, feature = "candle"))]
fn read_bounded_header_with_limit<R: Read>(
    reader: R,
    max_header_bytes: u64,
) -> Result<ParsedGgufHeader, ShardAdmissionError> {
    read_bounded_header_with_limit_cancellable(reader, max_header_bytes, None)
}

#[cfg(feature = "candle")]
fn read_bounded_header_with_limit_cancellable<R: Read>(
    reader: R,
    max_header_bytes: u64,
    cancel: Option<&AtomicBool>,
) -> Result<ParsedGgufHeader, ShardAdmissionError> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err(ShardAdmissionError::Cancelled);
    }
    let mut recorder = BoundedHeaderRecorder::new(reader, max_header_bytes, cancel);
    let (meta, tensors) = match read_header(&mut recorder) {
        Ok(parsed) => parsed,
        Err(source) => {
            if recorder.cancelled {
                return Err(ShardAdmissionError::Cancelled);
            }
            if recorder.limit_exceeded {
                return Err(ShardAdmissionError::HeaderTooLarge {
                    limit_bytes: max_header_bytes,
                });
            }
            return Err(ShardAdmissionError::Header(source));
        }
    };
    let (captured, _) = recorder.into_parts();
    if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err(ShardAdmissionError::Cancelled);
    }
    if captured.len_u64()? > max_header_bytes {
        return Err(ShardAdmissionError::HeaderTooLarge {
            limit_bytes: max_header_bytes,
        });
    }
    Ok(ParsedGgufHeader {
        metadata: meta,
        tensors,
        captured_header: captured,
    })
}

#[cfg(feature = "candle")]
fn is_finite_positive_f64_bits(bits: u64) -> bool {
    let sign = bits >> 63;
    let exponent = (bits >> 52) & 0x7FF;
    let magnitude = bits & 0x7FFF_FFFF_FFFF_FFFF;
    sign == 0 && exponent != 0x7FF && magnitude != 0
}

#[cfg(feature = "candle")]
fn parse_positive_u32_dim(
    meta: &BTreeMap<String, MetaValue>,
    key: &str,
    required: bool,
) -> Result<Option<usize>, ShardAdmissionError> {
    let Some(value) = meta.get(key) else {
        if required {
            return Err(ShardAdmissionError::InvalidMetadata {
                key: key.to_string(),
                reason: "missing required dimension".into(),
            });
        }
        return Ok(None);
    };
    let raw = match value {
        MetaValue::Uint(v) if *v >= 1 && *v <= u32::MAX as u64 => *v,
        MetaValue::Int(v) if *v >= 1 && *v <= i32::MAX as i64 => *v as u64,
        _ => {
            return Err(ShardAdmissionError::InvalidMetadata {
                key: key.to_string(),
                reason: "must be a positive 32-bit integer".into(),
            });
        }
    };
    let dim = usize::try_from(raw).map_err(|_| ShardAdmissionError::ArithmeticOverflow {
        context: "metadata dimension conversion",
    })?;
    Ok(Some(dim))
}

#[cfg(feature = "candle")]
fn layer_tensor_specs(
    d_model: usize,
    d_kv: usize,
    d_ff: usize,
) -> [(&'static str, usize, Option<usize>); 9] {
    [
        ("attn_q.weight", d_model, Some(d_model)),
        ("attn_k.weight", d_kv, Some(d_model)),
        ("attn_v.weight", d_kv, Some(d_model)),
        ("attn_output.weight", d_model, Some(d_model)),
        ("ffn_gate.weight", d_ff, Some(d_model)),
        ("ffn_up.weight", d_ff, Some(d_model)),
        ("ffn_down.weight", d_model, Some(d_ff)),
        ("attn_norm.weight", d_model, None),
        ("ffn_norm.weight", d_model, None),
    ]
}

#[cfg(feature = "candle")]
fn check_raw_tensor_dims(
    tensor_map: &BTreeMap<&str, &TensorEntry>,
    name: &str,
    expected_raw: &[u64],
) -> Result<(), ShardAdmissionError> {
    let Some(entry) = tensor_map.get(name) else {
        return Err(ShardAdmissionError::MissingTensor {
            tensor: name.to_string(),
        });
    };
    if entry.dims.as_slice() != expected_raw {
        return Err(ShardAdmissionError::MalformedTensorGeometry {
            tensor: name.to_string(),
            reason: format!(
                "expected raw GGUF dims {:?}, found {:?}",
                expected_raw, entry.dims
            ),
        });
    }
    Ok(())
}

#[cfg(feature = "candle")]
fn validate_header_metadata_and_tensors(
    meta: &BTreeMap<String, MetaValue>,
    tensors: &[TensorEntry],
) -> Result<HeaderGeometry, ShardAdmissionError> {
    match meta.get("general.architecture") {
        Some(MetaValue::Str(arch)) if arch == "llama" => {}
        Some(MetaValue::Str(arch)) => {
            return Err(ShardAdmissionError::UnsupportedArchitecture {
                found: arch.clone(),
            });
        }
        Some(_) => {
            return Err(ShardAdmissionError::UnsupportedArchitecture {
                found: "<non-string>".into(),
            });
        }
        None => {
            return Err(ShardAdmissionError::UnsupportedArchitecture {
                found: "<missing>".into(),
            });
        }
    }
    let canonical_profile =
        supported_adapter("llama").ok_or_else(|| ShardAdmissionError::UnsupportedArchitecture {
            found: "llama".into(),
        })?;

    let mut total_array_elements = 0u64;
    for value in meta.values() {
        if let MetaValue::Array { len } = value {
            total_array_elements =
                checked_add(total_array_elements, *len, "metadata array elements")?;
            if total_array_elements > MAX_METADATA_ARRAY_ELEMENTS {
                return Err(ShardAdmissionError::DimensionCeilingExceeded {
                    detail: format!(
                        "total metadata array elements {total_array_elements} exceed cap {MAX_METADATA_ARRAY_ELEMENTS}"
                    ),
                });
            }
        }
    }

    if let Some(alignment) = meta.get("general.alignment") {
        match alignment {
            MetaValue::Uint(v) if *v >= 1 && *v <= u32::MAX as u64 && v.is_power_of_two() => {}
            _ => {
                return Err(ShardAdmissionError::InvalidMetadata {
                    key: "general.alignment".into(),
                    reason: "must be a positive power-of-two u32".into(),
                });
            }
        }
    }
    if let Some(tokens) = meta.get("tokenizer.ggml.tokens") {
        match tokens {
            MetaValue::Array { len } if *len <= MAX_VOCAB_SIZE as u64 => {}
            MetaValue::Array { len } => {
                return Err(ShardAdmissionError::DimensionCeilingExceeded {
                    detail: format!(
                        "tokenizer.ggml.tokens length {len} exceeds vocab cap {MAX_VOCAB_SIZE}"
                    ),
                });
            }
            _ => {
                return Err(ShardAdmissionError::InvalidMetadata {
                    key: "tokenizer.ggml.tokens".into(),
                    reason: "must be a string array".into(),
                });
            }
        }
    }
    if let Some(bos) = meta.get("tokenizer.ggml.bos_token_id") {
        match bos {
            MetaValue::Uint(v) if *v <= u32::MAX as u64 => {}
            _ => {
                return Err(ShardAdmissionError::InvalidMetadata {
                    key: "tokenizer.ggml.bos_token_id".into(),
                    reason: "must be an unsigned 32-bit token id".into(),
                });
            }
        }
    }
    if let Some(eos) = meta.get("tokenizer.ggml.eos_token_id") {
        match eos {
            MetaValue::Uint(v) if *v <= u32::MAX as u64 => {}
            MetaValue::Array { len } if *len >= 1 && *len <= MAX_EOS_TOKENS => {}
            _ => {
                return Err(ShardAdmissionError::InvalidMetadata {
                    key: "tokenizer.ggml.eos_token_id".into(),
                    reason: format!(
                        "must be an unsigned 32-bit token id or non-empty array of at most {MAX_EOS_TOKENS} ids"
                    ),
                });
            }
        }
    }
    if let Some(rope_base) = meta.get("llama.rope.freq_base") {
        match rope_base {
            MetaValue::Float(v) if is_finite_positive_f64_bits(v.to_bits()) => {}
            _ => {
                return Err(ShardAdmissionError::InvalidMetadata {
                    key: "llama.rope.freq_base".into(),
                    reason: "must be a finite positive floating-point scalar".into(),
                });
            }
        }
    }
    let chat_template_bytes = match meta.get("tokenizer.chat_template") {
        None => 0u64,
        Some(MetaValue::Str(s)) => usize_to_u64(s.len(), "chat_template length")?,
        Some(_) => {
            return Err(ShardAdmissionError::InvalidMetadata {
                key: "tokenizer.chat_template".into(),
                reason: "must be a string".into(),
            });
        }
    };

    let n_layers = parse_positive_u32_dim(meta, "llama.block_count", true)?.ok_or_else(|| {
        ShardAdmissionError::InvalidMetadata {
            key: "llama.block_count".into(),
            reason: "missing".into(),
        }
    })?;
    let d_model =
        parse_positive_u32_dim(meta, "llama.embedding_length", true)?.ok_or_else(|| {
            ShardAdmissionError::InvalidMetadata {
                key: "llama.embedding_length".into(),
                reason: "missing".into(),
            }
        })?;
    let n_heads =
        parse_positive_u32_dim(meta, "llama.attention.head_count", true)?.ok_or_else(|| {
            ShardAdmissionError::InvalidMetadata {
                key: "llama.attention.head_count".into(),
                reason: "missing".into(),
            }
        })?;
    let n_kv_heads =
        parse_positive_u32_dim(meta, "llama.attention.head_count_kv", false)?.unwrap_or(n_heads);
    let d_ff =
        parse_positive_u32_dim(meta, "llama.feed_forward_length", true)?.ok_or_else(|| {
            ShardAdmissionError::InvalidMetadata {
                key: "llama.feed_forward_length".into(),
                reason: "missing".into(),
            }
        })?;

    if n_layers > MAX_N_LAYERS {
        return Err(ShardAdmissionError::DimensionCeilingExceeded {
            detail: format!("n_layers={n_layers} exceeds ceiling {MAX_N_LAYERS}"),
        });
    }
    if d_model > MAX_D_MODEL {
        return Err(ShardAdmissionError::DimensionCeilingExceeded {
            detail: format!("d_model={d_model} exceeds ceiling {MAX_D_MODEL}"),
        });
    }
    if d_ff > MAX_D_FF {
        return Err(ShardAdmissionError::DimensionCeilingExceeded {
            detail: format!("d_ff={d_ff} exceeds ceiling {MAX_D_FF}"),
        });
    }
    if !d_model.is_multiple_of(n_heads) {
        return Err(ShardAdmissionError::InvalidMetadata {
            key: "llama.attention.head_count".into(),
            reason: format!("d_model={d_model} is not divisible by n_heads={n_heads}"),
        });
    }
    if !n_heads.is_multiple_of(n_kv_heads) {
        return Err(ShardAdmissionError::InvalidMetadata {
            key: "llama.attention.head_count_kv".into(),
            reason: format!("n_heads={n_heads} is not divisible by n_kv_heads={n_kv_heads}"),
        });
    }
    let d_head = d_model / n_heads;
    if d_head < 2 || !d_head.is_multiple_of(2) {
        return Err(ShardAdmissionError::InvalidMetadata {
            key: "llama.attention.head_count".into(),
            reason: format!("d_head={d_head} must be even and at least 2 for RoPE"),
        });
    }
    if d_head > MAX_D_HEAD {
        return Err(ShardAdmissionError::DimensionCeilingExceeded {
            detail: format!("d_head={d_head} exceeds practical RoPE ceiling {MAX_D_HEAD}"),
        });
    }
    let d_kv = d_head
        .checked_mul(n_kv_heads)
        .ok_or(ShardAdmissionError::ArithmeticOverflow { context: "d_kv" })?;
    if d_kv > d_model {
        return Err(ShardAdmissionError::InvalidMetadata {
            key: "llama.attention.head_count_kv".into(),
            reason: format!("d_kv={d_kv} exceeds d_model={d_model}"),
        });
    }
    let rope_bytes = checked_mul(
        checked_mul(
            usize_to_u64(CANONICAL_MAX_SEQ, "max_seq")?,
            usize_to_u64(d_head / 2, "d_head/2")?,
            "pre-alloc rope entries",
        )?,
        checked_mul(2, ROPE_ENTRY_BYTES, "rope pair bytes")?,
        "pre-alloc rope table bytes",
    )?;
    if rope_bytes > MAX_ROPE_TABLE_BYTES {
        return Err(ShardAdmissionError::DimensionCeilingExceeded {
            detail: format!(
                "RoPE tables require {rope_bytes} bytes, exceeding pre-allocation cap {MAX_ROPE_TABLE_BYTES}"
            ),
        });
    }

    let mut tensor_map = BTreeMap::new();
    let mut blk_tensor_count = 0usize;
    for entry in tensors {
        if entry.dims.is_empty() || entry.dims.contains(&0) {
            return Err(ShardAdmissionError::MalformedTensorGeometry {
                tensor: entry.name.clone(),
                reason: format!("non-positive tensor dims {:?}", entry.dims),
            });
        }
        let mut elements = 1u64;
        for &dim in &entry.dims {
            elements = checked_mul(elements, dim, "header tensor element count")?;
        }
        let _ = elements;
        if entry.name.ends_with(".bias") {
            return Err(ShardAdmissionError::MalformedTensorGeometry {
                tensor: entry.name.clone(),
                reason: "bias tensors are unsupported in canonical Llama profile".into(),
            });
        }
        if entry.name.starts_with("blk.") {
            blk_tensor_count = blk_tensor_count.saturating_add(1);
        }
        tensor_map.insert(entry.name.as_str(), entry);
    }

    let d_model_u64 = usize_to_u64(d_model, "d_model")?;
    let d_kv_u64 = usize_to_u64(d_kv, "d_kv")?;
    let d_ff_u64 = usize_to_u64(d_ff, "d_ff")?;

    let embd =
        tensor_map
            .get("token_embd.weight")
            .ok_or_else(|| ShardAdmissionError::MissingTensor {
                tensor: "token_embd.weight".into(),
            })?;
    if embd.dims.len() != 2 || embd.dims[0] != d_model_u64 {
        return Err(ShardAdmissionError::MalformedTensorGeometry {
            tensor: "token_embd.weight".into(),
            reason: format!(
                "expected 2D raw GGUF dims [d_model={d_model_u64}, vocab_size] \
                 (matching candle [vocab_size, d_model]), found {:?}",
                embd.dims
            ),
        });
    }
    let vocab_u64 = embd.dims[1];
    if vocab_u64 == 0 || vocab_u64 > MAX_VOCAB_SIZE as u64 {
        return Err(ShardAdmissionError::DimensionCeilingExceeded {
            detail: format!("vocab_size={vocab_u64} outside 1..={MAX_VOCAB_SIZE}"),
        });
    }
    let vocab_size =
        usize::try_from(vocab_u64).map_err(|_| ShardAdmissionError::ArithmeticOverflow {
            context: "vocab_size",
        })?;

    let has_tied_output_head = if tensor_map.contains_key("output.weight") {
        check_raw_tensor_dims(&tensor_map, "output.weight", &[d_model_u64, vocab_u64])?;
        false
    } else {
        true
    };
    check_raw_tensor_dims(&tensor_map, "output_norm.weight", &[d_model_u64])?;

    let specs = layer_tensor_specs(d_model, d_kv, d_ff);
    for layer in 0..n_layers {
        for (suffix, rows, opt_cols) in specs {
            let name = format!("blk.{layer}.{suffix}");
            match opt_cols {
                Some(cols) => {
                    let c = if cols == d_model {
                        d_model_u64
                    } else if cols == d_ff {
                        d_ff_u64
                    } else {
                        usize_to_u64(cols, "tensor cols")?
                    };
                    let r = if rows == d_model {
                        d_model_u64
                    } else if rows == d_kv {
                        d_kv_u64
                    } else if rows == d_ff {
                        d_ff_u64
                    } else {
                        usize_to_u64(rows, "tensor rows")?
                    };
                    check_raw_tensor_dims(&tensor_map, &name, &[c, r])?;
                }
                None => {
                    check_raw_tensor_dims(&tensor_map, &name, &[d_model_u64])?;
                }
            }
        }
    }

    let expected_blk_tensors =
        n_layers
            .checked_mul(specs.len())
            .ok_or(ShardAdmissionError::ArithmeticOverflow {
                context: "expected block tensor count",
            })?;
    if blk_tensor_count != expected_blk_tensors {
        let unexpected = tensors
            .iter()
            .find(|entry| {
                if !entry.name.starts_with("blk.") {
                    return false;
                }
                let mut parts = entry.name.splitn(3, '.');
                let Some("blk") = parts.next() else {
                    return true;
                };
                let Some(layer_idx) = parts.next().and_then(|s| s.parse::<usize>().ok()) else {
                    return true;
                };
                let Some(suffix) = parts.next() else {
                    return true;
                };
                layer_idx >= n_layers || !specs.iter().any(|(known, _, _)| *known == suffix)
            })
            .map(|entry| entry.name.clone())
            .unwrap_or_else(|| "blk.<unknown>".into());
        return Err(ShardAdmissionError::MalformedTensorGeometry {
            tensor: unexpected,
            reason: format!(
                "unexpected block tensor for n_layers={n_layers} (expected {expected_blk_tensors} block tensors, found {blk_tensor_count})"
            ),
        });
    }

    Ok(HeaderGeometry {
        canonical_profile,
        n_layers,
        d_model,
        n_heads,
        n_kv_heads,
        d_ff,
        d_head,
        d_kv,
        vocab_size,
        has_tied_output_head,
        chat_template_bytes,
    })
}

#[cfg(feature = "candle")]
fn validate_candle_dim_type(
    content: &gguf_file::Content,
    key: &str,
    required: bool,
) -> Result<(), ShardAdmissionError> {
    match content.metadata.get(key) {
        None if !required => Ok(()),
        Some(gguf_file::Value::U32(v)) if *v > 0 => Ok(()),
        Some(gguf_file::Value::U64(v)) if *v > 0 && *v <= u32::MAX as u64 => Ok(()),
        Some(gguf_file::Value::I32(v)) if *v > 0 => Ok(()),
        _ => Err(ShardAdmissionError::InvalidMetadata {
            key: key.to_string(),
            reason: "unsupported GGUF scalar type for canonical config_from_gguf".into(),
        }),
    }
}

#[cfg(feature = "candle")]
fn check_candle_tensor_dims(
    content: &gguf_file::Content,
    name: &str,
    expected: &[usize],
) -> Result<(), ShardAdmissionError> {
    let Some(info) = content.tensor_infos.get(name) else {
        return Err(ShardAdmissionError::MissingTensor {
            tensor: name.to_string(),
        });
    };
    if info.shape.dims() != expected {
        return Err(ShardAdmissionError::MalformedTensorGeometry {
            tensor: name.to_string(),
            reason: format!(
                "expected candle tensor shape {:?}, found {:?}",
                expected,
                info.shape.dims()
            ),
        });
    }
    Ok(())
}

#[cfg(feature = "candle")]
fn validate_candle_content(
    content: &gguf_file::Content,
    header: &HeaderGeometry,
) -> Result<(), ShardAdmissionError> {
    validate_candle_dim_type(content, "llama.block_count", true)?;
    validate_candle_dim_type(content, "llama.embedding_length", true)?;
    validate_candle_dim_type(content, "llama.attention.head_count", true)?;
    validate_candle_dim_type(content, "llama.attention.head_count_kv", false)?;
    validate_candle_dim_type(content, "llama.feed_forward_length", true)?;

    if let Some(bos) = content.metadata.get("tokenizer.ggml.bos_token_id") {
        match bos {
            gguf_file::Value::U32(_) => {}
            gguf_file::Value::U64(v) if *v <= u32::MAX as u64 => {}
            _ => {
                return Err(ShardAdmissionError::InvalidMetadata {
                    key: "tokenizer.ggml.bos_token_id".into(),
                    reason: "expected U32 or U64<=u32::MAX for config_from_gguf".into(),
                });
            }
        }
    }
    if let Some(eos) = content.metadata.get("tokenizer.ggml.eos_token_id") {
        let valid = match eos {
            gguf_file::Value::U32(_) => true,
            gguf_file::Value::U64(v) => *v <= u32::MAX as u64,
            gguf_file::Value::Array(arr) => {
                !arr.is_empty()
                    && arr.iter().all(|item| match item {
                        gguf_file::Value::U32(_) => true,
                        gguf_file::Value::U64(v) => *v <= u32::MAX as u64,
                        _ => false,
                    })
            }
            _ => false,
        };
        if !valid {
            return Err(ShardAdmissionError::InvalidMetadata {
                key: "tokenizer.ggml.eos_token_id".into(),
                reason: "expected U32, U64<=u32::MAX, or non-empty array of them".into(),
            });
        }
    }
    if let Some(tokens) = content.metadata.get("tokenizer.ggml.tokens") {
        match tokens {
            gguf_file::Value::Array(arr)
                if arr.iter().all(|v| matches!(v, gguf_file::Value::String(_))) => {}
            _ => {
                return Err(ShardAdmissionError::InvalidMetadata {
                    key: "tokenizer.ggml.tokens".into(),
                    reason: "expected array of strings".into(),
                });
            }
        }
    }

    check_candle_tensor_dims(
        content,
        "token_embd.weight",
        &[header.vocab_size, header.d_model],
    )?;
    if !header.has_tied_output_head {
        check_candle_tensor_dims(
            content,
            "output.weight",
            &[header.vocab_size, header.d_model],
        )?;
    }
    check_candle_tensor_dims(content, "output_norm.weight", &[header.d_model])?;

    let specs = layer_tensor_specs(header.d_model, header.d_kv, header.d_ff);
    for layer in 0..header.n_layers {
        for (suffix, rows, opt_cols) in specs {
            let name = format!("blk.{layer}.{suffix}");
            match opt_cols {
                Some(cols) => check_candle_tensor_dims(content, &name, &[rows, cols])?,
                None => check_candle_tensor_dims(content, &name, &[rows])?,
            }
        }
    }
    Ok(())
}

#[cfg(feature = "candle")]
fn validate_config_matches_header(
    config: &ModelConfig,
    header: &HeaderGeometry,
) -> Result<(), ShardAdmissionError> {
    let chat_template_bytes =
        usize_to_u64(config.chat_template.len(), "config chat_template length")?;
    if config.n_layers != header.n_layers
        || config.d_model != header.d_model
        || config.n_heads != header.n_heads
        || config.n_kv_heads != header.n_kv_heads
        || config.d_ff != header.d_ff
        || config.d_head != header.d_head
        || config.d_kv != header.d_kv
        || config.vocab_size != header.vocab_size
        || config.max_seq != CANONICAL_MAX_SEQ
        || chat_template_bytes != header.chat_template_bytes
        || config.arithmetic_profile != ArithmeticProfile::LegacySplitHalfV0
    {
        return Err(ShardAdmissionError::Config(
            "config_from_gguf diverged from bounded GGUF header geometry or canonical profile"
                .into(),
        ));
    }
    let expected_rope_len = config.max_seq.checked_mul(config.d_head / 2).ok_or(
        ShardAdmissionError::ArithmeticOverflow {
            context: "rope table length",
        },
    )?;
    if config.rope_cos.len() != expected_rope_len || config.rope_sin.len() != expected_rope_len {
        return Err(ShardAdmissionError::Config(format!(
            "unexpected RoPE table length: cos={}, sin={}, expected={expected_rope_len}",
            config.rope_cos.len(),
            config.rope_sin.len()
        )));
    }
    Ok(())
}

#[cfg(all(test, feature = "candle"))]
mod candle_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    use candle_core::quantized::{GgmlDType, QTensor, gguf_file};
    use candle_core::{Device, Tensor};

    use crate::cached_integer_model::load_cached_model_shard_canonical_i8;

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    struct TempGguf(PathBuf);

    impl Drop for TempGguf {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[derive(Clone)]
    enum RawKv {
        U32(u32),
        U64(u64),
        Str(String),
        ArrayU32(Vec<u32>),
        ArrayStr(Vec<String>),
    }

    #[derive(Clone)]
    struct HeaderFixture {
        kvs: Vec<(String, RawKv)>,
        tensors: Vec<(String, Vec<u64>, u32, u64)>,
    }

    impl HeaderFixture {
        fn llama(
            n_layers: usize,
            d_model: usize,
            n_heads: usize,
            n_kv_heads: usize,
            d_ff: usize,
            vocab_size: usize,
            include_output_weight: bool,
        ) -> Self {
            let d_kv = (d_model / n_heads) * n_kv_heads;
            let tokens: Vec<String> = (0..vocab_size.min(64))
                .map(|idx| format!("tok_{idx}"))
                .collect();
            let kvs = vec![
                ("general.architecture".into(), RawKv::Str("llama".into())),
                ("llama.block_count".into(), RawKv::U32(n_layers as u32)),
                ("llama.embedding_length".into(), RawKv::U32(d_model as u32)),
                (
                    "llama.attention.head_count".into(),
                    RawKv::U32(n_heads as u32),
                ),
                (
                    "llama.attention.head_count_kv".into(),
                    RawKv::U32(n_kv_heads as u32),
                ),
                ("llama.feed_forward_length".into(), RawKv::U32(d_ff as u32)),
                ("tokenizer.ggml.bos_token_id".into(), RawKv::U32(1)),
                (
                    "tokenizer.ggml.eos_token_id".into(),
                    RawKv::ArrayU32(vec![2, 128_009]),
                ),
                ("tokenizer.ggml.tokens".into(), RawKv::ArrayStr(tokens)),
            ];
            let mut tensors = vec![(
                "token_embd.weight".into(),
                vec![d_model as u64, vocab_size as u64],
                0,
                0,
            )];
            if include_output_weight {
                tensors.push((
                    "output.weight".into(),
                    vec![d_model as u64, vocab_size as u64],
                    0,
                    0,
                ));
            }
            tensors.push(("output_norm.weight".into(), vec![d_model as u64], 0, 0));
            for l in 0..n_layers {
                let p = format!("blk.{l}");
                tensors.push((
                    format!("{p}.attn_q.weight"),
                    vec![d_model as u64, d_model as u64],
                    0,
                    0,
                ));
                tensors.push((
                    format!("{p}.attn_k.weight"),
                    vec![d_model as u64, d_kv as u64],
                    0,
                    0,
                ));
                tensors.push((
                    format!("{p}.attn_v.weight"),
                    vec![d_model as u64, d_kv as u64],
                    0,
                    0,
                ));
                tensors.push((
                    format!("{p}.attn_output.weight"),
                    vec![d_model as u64, d_model as u64],
                    0,
                    0,
                ));
                tensors.push((
                    format!("{p}.ffn_gate.weight"),
                    vec![d_model as u64, d_ff as u64],
                    0,
                    0,
                ));
                tensors.push((
                    format!("{p}.ffn_up.weight"),
                    vec![d_model as u64, d_ff as u64],
                    0,
                    0,
                ));
                tensors.push((
                    format!("{p}.ffn_down.weight"),
                    vec![d_ff as u64, d_model as u64],
                    0,
                    0,
                ));
                tensors.push((format!("{p}.attn_norm.weight"), vec![d_model as u64], 0, 0));
                tensors.push((format!("{p}.ffn_norm.weight"), vec![d_model as u64], 0, 0));
            }
            Self { kvs, tensors }
        }

        fn to_bytes(&self) -> Vec<u8> {
            fn push_str(buf: &mut Vec<u8>, s: &str) {
                buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            let mut buf = Vec::new();
            buf.extend_from_slice(b"GGUF");
            buf.extend_from_slice(&3u32.to_le_bytes());
            buf.extend_from_slice(&(self.tensors.len() as u64).to_le_bytes());
            buf.extend_from_slice(&(self.kvs.len() as u64).to_le_bytes());
            for (key, value) in &self.kvs {
                push_str(&mut buf, key);
                match value {
                    RawKv::U32(v) => {
                        buf.extend_from_slice(&4u32.to_le_bytes());
                        buf.extend_from_slice(&v.to_le_bytes());
                    }
                    RawKv::U64(v) => {
                        buf.extend_from_slice(&10u32.to_le_bytes());
                        buf.extend_from_slice(&v.to_le_bytes());
                    }
                    RawKv::Str(s) => {
                        buf.extend_from_slice(&8u32.to_le_bytes());
                        push_str(&mut buf, s);
                    }
                    RawKv::ArrayU32(xs) => {
                        buf.extend_from_slice(&9u32.to_le_bytes());
                        buf.extend_from_slice(&4u32.to_le_bytes());
                        buf.extend_from_slice(&(xs.len() as u64).to_le_bytes());
                        for x in xs {
                            buf.extend_from_slice(&x.to_le_bytes());
                        }
                    }
                    RawKv::ArrayStr(xs) => {
                        buf.extend_from_slice(&9u32.to_le_bytes());
                        buf.extend_from_slice(&8u32.to_le_bytes());
                        buf.extend_from_slice(&(xs.len() as u64).to_le_bytes());
                        for s in xs {
                            push_str(&mut buf, s);
                        }
                    }
                }
            }
            for (name, dims, ggml_type, offset) in &self.tensors {
                push_str(&mut buf, name);
                buf.extend_from_slice(&(dims.len() as u32).to_le_bytes());
                for dim in dims {
                    buf.extend_from_slice(&dim.to_le_bytes());
                }
                buf.extend_from_slice(&ggml_type.to_le_bytes());
                buf.extend_from_slice(&offset.to_le_bytes());
            }
            buf
        }

        fn write_temp(&self, tag: &str) -> TempGguf {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "arc-shard-adm-{}-{id}-{tag}.gguf",
                std::process::id()
            ));
            std::fs::write(&path, self.to_bytes()).expect("write header-only GGUF");
            TempGguf(path)
        }
    }

    #[test]
    fn header_only_gguf_without_weights_inspects_and_admits_partial_ranges() {
        let fixture = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true).write_temp("hdr_only");
        let meta = CanonicalShardMetadata::from_path(&fixture.0).expect("inspect header-only GGUF");

        assert_eq!(meta.architecture(), "llama");
        assert_eq!(meta.n_layers(), 4);
        assert_eq!(meta.d_model(), 16);
        assert_eq!(meta.n_heads(), 4);
        assert_eq!(meta.n_kv_heads(), 2);
        assert_eq!(meta.d_ff(), 28);
        assert_eq!(meta.d_head(), 4);
        assert_eq!(meta.d_kv(), 8);
        assert_eq!(meta.vocab_size(), 24);
        assert_eq!(meta.max_seq(), CANONICAL_MAX_SEQ);
        assert_eq!(meta.bos_token(), 1);
        assert_eq!(meta.eos_tokens(), &[2, 128_009]);
        assert!(!meta.has_tied_output_head());
        assert_eq!(
            meta.trust_boundary(),
            ArtifactTrustBoundary::UnverifiedHeaderOnly
        );

        assert!(
            load_cached_model_shard_canonical_i8(fixture.0.to_str().unwrap(), 0, 2).is_err(),
            "header-only fixture has no weight bytes"
        );

        let est = meta
            .estimate_range(0..2, 4_096)
            .expect("estimate first partial shard");
        assert_eq!(est.reserved_kv_cache_bytes, 4_096);
        let admitted = meta
            .admit(0..2, est.peak_loading_budget_bytes, 4_096)
            .expect("exact cap admits");
        assert_eq!(admitted, est);

        let tied = HeaderFixture::llama(4, 16, 4, 2, 28, 24, false).write_temp("tied_head");
        let tied_meta = CanonicalShardMetadata::from_path(&tied.0).expect("tied head valid");
        assert!(tied_meta.has_tied_output_head());

        let commitment = ModelArtifactCommitment::from_path(&fixture.0).expect("commit artifact");
        let verified = CanonicalShardMetadata::from_committed_artifact(&commitment)
            .expect("committed inspection");
        assert_eq!(
            verified.trust_boundary(),
            ArtifactTrustBoundary::VerifiedBlake3Commitment {
                model_id: commitment.model_id(),
                size_bytes: commitment.size_bytes(),
            }
        );

        let mut mutated = std::fs::read(&fixture.0).expect("read fixture");
        mutated.push(0xAA);
        std::fs::write(&fixture.0, mutated).expect("mutate fixture");
        assert!(matches!(
            CanonicalShardMetadata::from_committed_artifact(&commitment),
            Err(ShardAdmissionError::ArtifactVerification(_))
        ));
    }

    #[test]
    fn real_candle_written_non_square_gguf_matches_canonical_loader() {
        let (n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size) = (4, 16, 4, 2, 28, 24);
        let d_kv = (d_model / n_heads) * n_kv_heads;
        let vocab_vals: Vec<gguf_file::Value> = (0..vocab_size)
            .map(|t| gguf_file::Value::String(format!("t_{t}")))
            .collect();
        let metadata = [
            (
                "general.architecture",
                gguf_file::Value::String("llama".into()),
            ),
            ("llama.block_count", gguf_file::Value::U32(n_layers as u32)),
            (
                "llama.embedding_length",
                gguf_file::Value::U32(d_model as u32),
            ),
            (
                "llama.attention.head_count",
                gguf_file::Value::U32(n_heads as u32),
            ),
            (
                "llama.attention.head_count_kv",
                gguf_file::Value::U32(n_kv_heads as u32),
            ),
            (
                "llama.feed_forward_length",
                gguf_file::Value::U32(d_ff as u32),
            ),
            ("tokenizer.ggml.bos_token_id", gguf_file::Value::U32(1)),
            ("tokenizer.ggml.eos_token_id", gguf_file::Value::U32(2)),
            ("tokenizer.ggml.tokens", gguf_file::Value::Array(vocab_vals)),
        ];
        let mk = |name: String, shape: &[usize]| -> (String, QTensor) {
            let count: usize = shape.iter().product();
            let t = Tensor::from_vec(vec![0.125f32; count], shape, &Device::Cpu).expect("tensor");
            (
                name,
                QTensor::quantize(&t, GgmlDType::F32).expect("qtensor"),
            )
        };
        let mut tensors = vec![
            mk("token_embd.weight".into(), &[vocab_size, d_model]),
            mk("output.weight".into(), &[vocab_size, d_model]),
            mk("output_norm.weight".into(), &[d_model]),
        ];
        for l in 0..n_layers {
            let p = format!("blk.{l}");
            tensors.push(mk(format!("{p}.attn_q.weight"), &[d_model, d_model]));
            tensors.push(mk(format!("{p}.attn_k.weight"), &[d_kv, d_model]));
            tensors.push(mk(format!("{p}.attn_v.weight"), &[d_kv, d_model]));
            tensors.push(mk(format!("{p}.attn_output.weight"), &[d_model, d_model]));
            tensors.push(mk(format!("{p}.ffn_gate.weight"), &[d_ff, d_model]));
            tensors.push(mk(format!("{p}.ffn_up.weight"), &[d_ff, d_model]));
            tensors.push(mk(format!("{p}.ffn_down.weight"), &[d_model, d_ff]));
            tensors.push(mk(format!("{p}.attn_norm.weight"), &[d_model]));
            tensors.push(mk(format!("{p}.ffn_norm.weight"), &[d_model]));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let file_guard = TempGguf(std::env::temp_dir().join(format!(
            "arc-shard-adm-{}-{id}-candle.gguf",
            std::process::id()
        )));
        let mut file = std::fs::File::create(&file_guard.0).expect("create GGUF");
        let meta_refs: Vec<(&str, &gguf_file::Value)> =
            metadata.iter().map(|(k, v)| (*k, v)).collect();
        let tensor_refs: Vec<(&str, &QTensor)> =
            tensors.iter().map(|(n, t)| (n.as_str(), t)).collect();
        gguf_file::write(&mut file, &meta_refs, &tensor_refs).expect("write GGUF");

        let meta = CanonicalShardMetadata::from_path(&file_guard.0).expect("inspect candle GGUF");
        let shard0 = load_cached_model_shard_canonical_i8(file_guard.0.to_str().unwrap(), 0, 2)
            .expect("load shard0");
        let shard1 = load_cached_model_shard_canonical_i8(file_guard.0.to_str().unwrap(), 2, 4)
            .expect("load shard1");

        assert_eq!(meta.n_layers(), shard0.config.n_layers);
        assert_eq!(meta.d_model(), shard0.config.d_model);
        assert_eq!(meta.n_heads(), shard0.config.n_heads);
        assert_eq!(meta.n_kv_heads(), shard0.config.n_kv_heads);
        assert_eq!(meta.d_ff(), shard0.config.d_ff);
        assert_eq!(meta.d_kv(), shard0.config.d_kv);
        assert_eq!(meta.d_head(), shard0.config.d_head);
        assert_eq!(meta.vocab_size(), shard0.config.vocab_size);
        assert_eq!(meta.max_seq(), shard0.config.max_seq);
        assert_eq!(meta.bos_token(), shard0.config.bos_token);
        assert_eq!(meta.eos_tokens(), shard0.config.eos_tokens.as_slice());
        assert_eq!(meta.attn_scale(), shard0.config.attn_scale);

        let actual_bytes = |m: &CachedIntegerModel| -> u64 {
            let emb = m.embedding_q16.len() * 8
                + if m.embedding_i8.data.is_empty() {
                    0
                } else {
                    m.embedding_i8.memory_bytes()
                };
            let out = if m.output_weight.data.is_empty() {
                0
            } else {
                m.output_weight.memory_bytes()
            } + m.final_norm.len() * 8;
            let layers: usize = m
                .layers
                .iter()
                .filter(|l| l.is_loaded())
                .map(|l| {
                    l.wq.memory_bytes()
                        + l.wk.memory_bytes()
                        + l.wv.memory_bytes()
                        + l.wo.memory_bytes()
                        + l.w_gate.memory_bytes()
                        + l.w_up.memory_bytes()
                        + l.w_down.memory_bytes()
                        + l.attn_norm.len() * 8
                        + l.ffn_norm.len() * 8
                })
                .sum();
            let rope = (m.config.rope_cos.len() + m.config.rope_sin.len()) * 8;
            (emb + out + layers + rope) as u64
        };

        let est0 = meta.estimate_range(0..2, 0).expect("est0");
        let est1 = meta.estimate_range(2..4, 0).expect("est1");
        assert!(est0.resident_payload_bytes > actual_bytes(&shard0));
        assert!(est1.resident_payload_bytes > actual_bytes(&shard1));
    }

    #[test]
    fn rejects_malformed_geometry_ceilings_coercions_and_non_llama() {
        let mut non_llama = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        non_llama.kvs[0].1 = RawKv::Str("qwen2".into());
        let f = non_llama.write_temp("non_llama");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::UnsupportedArchitecture { .. })
        ));

        let mut swapped_emb = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        swapped_emb.tensors[0].1 = vec![24, 16];
        let f = swapped_emb.write_temp("swapped_emb");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::MalformedTensorGeometry { ref tensor, .. })
                if tensor == "token_embd.weight"
        ));

        let mut missing_emb = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        missing_emb.tensors.retain(|t| t.0 != "token_embd.weight");
        let f = missing_emb.write_temp("missing_emb");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::MissingTensor { ref tensor })
                if tensor == "token_embd.weight"
        ));

        let mut swapped_down = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        for t in &mut swapped_down.tensors {
            if t.0 == "blk.0.ffn_down.weight" {
                t.1 = vec![16, 28];
            }
        }
        let f = swapped_down.write_temp("swapped_down");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::MalformedTensorGeometry { ref tensor, .. })
                if tensor == "blk.0.ffn_down.weight"
        ));

        let mut missing_norm = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        missing_norm
            .tensors
            .retain(|t| t.0 != "blk.0.ffn_norm.weight");
        let f = missing_norm.write_temp("missing_norm");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::MissingTensor { ref tensor })
                if tensor == "blk.0.ffn_norm.weight"
        ));

        let mut bias_tensor = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        bias_tensor
            .tensors
            .push(("blk.0.attn_q.bias".into(), vec![16], 0, 0));
        let f = bias_tensor.write_temp("bias");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::MalformedTensorGeometry { ref tensor, .. })
                if tensor == "blk.0.attn_q.bias"
        ));

        let mut extra_blk = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        extra_blk
            .tensors
            .push(("blk.4.attn_q.weight".into(), vec![16, 16], 0, 0));
        let f = extra_blk.write_temp("extra_blk");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::MalformedTensorGeometry { ref tensor, .. })
                if tensor == "blk.4.attn_q.weight"
        ));

        let mut huge_rope = HeaderFixture::llama(2, 16, 4, 2, 28, 24, true);
        huge_rope.kvs[2].1 = RawKv::U32(4_096);
        huge_rope.kvs[3].1 = RawKv::U32(2);
        huge_rope.kvs[4].1 = RawKv::U32(2);
        let f = huge_rope.write_temp("huge_rope");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::DimensionCeilingExceeded { .. })
        ));

        let mut over_layers = HeaderFixture::llama(2, 16, 4, 2, 28, 24, true);
        over_layers.kvs[1].1 = RawKv::U32(1_025);
        let f = over_layers.write_temp("over_layers");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::DimensionCeilingExceeded { .. })
        ));

        let mut u64_trunc = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        u64_trunc.kvs[1].1 = RawKv::U64((1u64 << 32) + 4);
        let f = u64_trunc.write_temp("u64_trunc");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::InvalidMetadata { ref key, .. })
                if key == "llama.block_count"
        ));

        let mut zero_align = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        zero_align
            .kvs
            .push(("general.alignment".into(), RawKv::U32(0)));
        let f = zero_align.write_temp("zero_align");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::InvalidMetadata { ref key, .. })
                if key == "general.alignment"
        ));
    }

    #[test]
    fn metadata_array_heap_storage_is_included_in_loading_budget() {
        let baseline = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        let base_file = baseline.write_temp("metadata-base");
        let base = CanonicalShardMetadata::from_path(&base_file.0).unwrap();
        let mut enlarged = baseline;
        enlarged
            .kvs
            .push(("test.extra_array".into(), RawKv::ArrayU32(vec![0; 4096])));
        let extra_file = enlarged.write_temp("metadata-extra");
        let extra = CanonicalShardMetadata::from_path(&extra_file.0).unwrap();
        let low = base.estimate_range(1..2, 4096).unwrap();
        let high = extra.estimate_range(1..2, 4096).unwrap();
        assert!(
            high.peak_loading_budget_bytes - low.peak_loading_budget_bytes
                >= 4096 * std::mem::size_of::<gguf_file::Value>() as u64
        );
        assert!(
            extra
                .admit(1..2, low.peak_loading_budget_bytes, 4096)
                .is_err()
        );
    }

    #[test]
    fn first_shard_embedding_budget_dwarfs_interior_and_last_shard() {
        let fixture =
            HeaderFixture::llama(8, 512, 8, 2, 1_024, 128_000, true).write_temp("emb_vs_int");
        let meta =
            CanonicalShardMetadata::from_path(&fixture.0).expect("inspect llama-like header");
        let kv_budget = 64 * 1024;

        let first = meta.estimate_range(0..2, kv_budget).expect("first shard");
        let interior = meta
            .estimate_range(2..4, kv_budget)
            .expect("interior shard");
        let last = meta.estimate_range(6..8, kv_budget).expect("last shard");

        assert_eq!(
            first.first_embedding_bytes,
            128_000 * 512 * FIRST_EMBEDDING_BYTES_PER_ELEMENT
                + 128_000 * SCALE_BYTES_PER_ROW
                + WEIGHT_STRUCT_OVERHEAD_BYTES
        );
        assert_eq!(interior.first_embedding_bytes, 0);
        assert_eq!(interior.output_head_and_norm_bytes, 0);
        assert_eq!(
            last.output_head_and_norm_bytes,
            128_000 * 512 + 128_000 * SCALE_BYTES_PER_ROW + WEIGHT_STRUCT_OVERHEAD_BYTES + 512 * 8
        );
        assert!(first.resident_budget_bytes > last.resident_budget_bytes);
        assert!(last.resident_budget_bytes > interior.resident_budget_bytes);
        assert_eq!(first.max_loaded_tensor_elements, 128_000 * 512);
        assert_eq!(interior.max_loaded_tensor_elements, 1_024 * 512);
        assert!(first.peak_loading_budget_bytes > interior.peak_loading_budget_bytes);

        let cap = interior.peak_loading_budget_bytes;
        assert!(meta.admit(2..4, cap, kv_budget).is_ok());
        let Err(err) = meta.admit(0..2, cap, kv_budget) else {
            panic!("first shard must be rejected under interior-only RAM cap");
        };
        assert!(matches!(err, ShardAdmissionError::BudgetExceeded { .. }));
        assert!(err.to_string().contains("first-shard Q16+I8 embeddings"));
    }

    #[test]
    fn rejects_empty_reversed_out_of_bounds_full_and_over_half_ranges() {
        let f4 = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true).write_temp("range4");
        let m4 = CanonicalShardMetadata::from_path(&f4.0).expect("4-layer meta");
        let kv_budget = 4_096;
        for (s, e) in [
            (0, 0),
            (2, 2),
            (2, 1),
            (4, 0),
            (0, 5),
            (3, 5),
            (0, usize::MAX),
            (0, 4),
            (0, 3),
            (1, 4),
        ] {
            assert!(
                matches!(
                    m4.admit(s..e, u64::MAX, kv_budget),
                    Err(ShardAdmissionError::InvalidRange { .. })
                ),
                "range [{s}, {e}) must be rejected"
            );
        }

        let f1 = HeaderFixture::llama(1, 16, 4, 2, 28, 24, true).write_temp("range1");
        let m1 = CanonicalShardMetadata::from_path(&f1.0).expect("1-layer meta");
        assert!(matches!(
            m1.admit(0..1, u64::MAX, kv_budget),
            Err(ShardAdmissionError::InvalidRange { .. })
        ));

        let f5 = HeaderFixture::llama(5, 16, 4, 2, 28, 24, true).write_temp("range5");
        let m5 = CanonicalShardMetadata::from_path(&f5.0).expect("5-layer meta");
        assert!(m5.admit(0..3, u64::MAX, kv_budget).is_ok());
        assert!(m5.admit(2..5, u64::MAX, kv_budget).is_ok());
        assert!(matches!(
            m5.admit(0..4, u64::MAX, kv_budget),
            Err(ShardAdmissionError::InvalidRange { .. })
        ));
        assert!(matches!(
            m5.admit(0..5, u64::MAX, kv_budget),
            Err(ShardAdmissionError::InvalidRange { .. })
        ));
    }

    #[test]
    fn boundary_overflow_undersized_cap_reserved_kv_and_header_limit() {
        let fixture = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true).write_temp("bounds");
        let meta = CanonicalShardMetadata::from_path(&fixture.0).expect("inspect meta");
        let kv_budget = 4_096;
        let no_kv = meta.estimate_range(0..2, 0).expect("no-kv estimate");
        let with_kv = meta
            .estimate_range(0..2, kv_budget)
            .expect("with-kv estimate");
        assert_eq!(
            with_kv.peak_loading_budget_bytes,
            no_kv.peak_loading_budget_bytes + kv_budget
        );

        let Err(err) = meta.admit(0..2, with_kv.peak_loading_budget_bytes - 1, kv_budget) else {
            panic!("cap 1 byte below peak budget must be rejected");
        };
        assert!(matches!(
            err,
            ShardAdmissionError::BudgetExceeded {
                shortfall_bytes: 1,
                reserved_kv_cache_bytes: 4_096,
                ..
            }
        ));
        let msg = err.to_string();
        assert!(msg.contains("conservative working-set payload budget"));
        assert!(msg.contains("assign a smaller layer range or provision more RAM"));
        assert!(!msg.contains("exact RSS"));
        assert!(!msg.contains("hard OS limit"));

        let Err(kv_err) = meta.admit(0..2, no_kv.peak_loading_budget_bytes, 1) else {
            panic!("reserved KV byte must count toward working-set cap");
        };
        assert!(matches!(
            kv_err,
            ShardAdmissionError::BudgetExceeded {
                shortfall_bytes: 1,
                reserved_kv_cache_bytes: 1,
                ..
            }
        ));

        assert!(matches!(
            meta.admit(0..2, u64::MAX, u64::MAX),
            Err(ShardAdmissionError::ArithmeticOverflow { .. })
        ));

        let mut overflow_tensor = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true);
        overflow_tensor
            .tensors
            .push(("rope_freqs.weight".into(), vec![u64::MAX, 2], 0, 0));
        let f = overflow_tensor.write_temp("dim_overflow");
        assert!(matches!(
            CanonicalShardMetadata::from_path(&f.0),
            Err(ShardAdmissionError::ArithmeticOverflow { .. })
        ));

        let bytes = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true).to_bytes();
        assert!(matches!(
            read_bounded_header_with_limit(Cursor::new(bytes), 64),
            Err(ShardAdmissionError::HeaderTooLarge { limit_bytes: 64 })
        ));
        let truncated = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true).to_bytes();
        assert!(matches!(
            read_bounded_header_with_limit(Cursor::new(truncated[..64].to_vec()), 64),
            Err(ShardAdmissionError::Header(MetaError::Io(_)))
        ));
    }

    struct FailBeyondHeaderReader {
        data: Vec<u8>,
        pos: usize,
        header_len: usize,
    }

    impl Read for FailBeyondHeaderReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.header_len || buf.len() > self.header_len - self.pos {
                return Err(std::io::Error::other(
                    "read attempted beyond exact GGUF header boundary",
                ));
            }
            let end = self.pos + buf.len();
            buf.copy_from_slice(&self.data[self.pos..end]);
            self.pos = end;
            Ok(buf.len())
        }
    }

    struct MutateFileOnHeaderEndReader {
        file: std::fs::File,
        path: PathBuf,
        header_len: usize,
        read_bytes: usize,
        replacement: Vec<u8>,
        mutated: bool,
    }

    impl Read for MutateFileOnHeaderEndReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.read_bytes >= self.header_len {
                return Err(std::io::Error::other(
                    "read attempted past exact GGUF header boundary",
                ));
            }
            let allowed = (self.header_len - self.read_bytes).min(buf.len());
            let n = self.file.read(&mut buf[..allowed])?;
            self.read_bytes += n;
            if self.read_bytes == self.header_len && !self.mutated {
                use std::io::Write;
                let mut writer = std::fs::OpenOptions::new().write(true).open(&self.path)?;
                writer.write_all(&self.replacement)?;
                writer.flush()?;
                self.mutated = true;
            }
            Ok(n)
        }
    }

    #[test]
    fn content_parses_captured_header_only_never_reads_past_header_or_mutated_source() {
        let valid_header = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true).to_bytes();
        let header_len = valid_header.len();
        let mut with_trailing_weights = valid_header.clone();
        with_trailing_weights.extend_from_slice(&[0xFFu8; 256]);

        let strict_reader = FailBeyondHeaderReader {
            data: with_trailing_weights.clone(),
            pos: 0,
            header_len,
        };
        let meta =
            CanonicalShardMetadata::from_reader_with_limit(strict_reader, MAX_GGUF_HEADER_BYTES)
                .expect("single-pass captured header must not read past exact header");
        assert_eq!(meta.header_bytes(), header_len as u64);
        assert_eq!(meta.n_layers(), 4);
        assert_eq!(meta.d_model(), 16);
        assert_eq!(meta.vocab_size(), 24);

        let exact_limit_reader = FailBeyondHeaderReader {
            data: with_trailing_weights.clone(),
            pos: 0,
            header_len,
        };
        let exact_meta =
            CanonicalShardMetadata::from_reader_with_limit(exact_limit_reader, header_len as u64)
                .expect("exact header byte limit must succeed without probing weight bytes");
        assert_eq!(exact_meta, meta);

        assert!(matches!(
            CanonicalShardMetadata::from_reader_with_limit(
                Cursor::new(with_trailing_weights.clone()),
                (header_len - 1) as u64,
            ),
            Err(ShardAdmissionError::HeaderTooLarge { limit_bytes })
                if limit_bytes == (header_len - 1) as u64
        ));

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let file_guard = TempGguf(std::env::temp_dir().join(format!(
            "arc-shard-adm-{}-{id}-toctou.gguf",
            std::process::id()
        )));
        std::fs::write(&file_guard.0, &with_trailing_weights).expect("write initial GGUF");
        let commitment = ModelArtifactCommitment::from_path(&file_guard.0).expect("commit GGUF");

        let mut malicious = HeaderFixture::llama(2, 16, 4, 2, 28, 24, true);
        malicious.kvs[1].1 = RawKv::U32(4_096);
        malicious.kvs[2].1 = RawKv::U32(65_536);
        malicious.kvs[3].1 = RawKv::U32(1);
        malicious.kvs[4].1 = RawKv::U32(1);

        let mutating_reader = MutateFileOnHeaderEndReader {
            file: std::fs::File::open(&file_guard.0).expect("open GGUF"),
            path: file_guard.0.clone(),
            header_len,
            read_bytes: 0,
            replacement: malicious.to_bytes(),
            mutated: false,
        };
        let captured_meta =
            CanonicalShardMetadata::from_reader_with_limit(mutating_reader, MAX_GGUF_HEADER_BYTES)
                .expect(
                    "Content::read must parse immutable captured bytes, not re-read mutated file",
                );
        assert_eq!(captured_meta, meta);
        assert!(matches!(
            CanonicalShardMetadata::from_path(&file_guard.0),
            Err(ShardAdmissionError::DimensionCeilingExceeded { .. })
        ));
        assert!(commitment.verify_unchanged().is_err());
    }

    #[test]
    fn rejects_layer_ceiling_above_1024_and_accepts_1024_boundary_without_weights() {
        assert_eq!(MAX_N_LAYERS, 1_024);

        let mut over = HeaderFixture::llama(2, 16, 4, 2, 28, 24, true);
        over.kvs[1].1 = RawKv::U32(1_025);
        let over_bytes = over.to_bytes();
        let over_header_len = over_bytes.len();
        let mut over_with_payload = over_bytes;
        over_with_payload.extend_from_slice(&[0xEEu8; 64]);
        let over_reader = FailBeyondHeaderReader {
            data: over_with_payload,
            pos: 0,
            header_len: over_header_len,
        };
        assert!(matches!(
            CanonicalShardMetadata::from_reader_with_limit(over_reader, MAX_GGUF_HEADER_BYTES),
            Err(ShardAdmissionError::DimensionCeilingExceeded { .. })
        ));

        let at_cap = HeaderFixture::llama(1_024, 8, 2, 2, 8, 8, true).to_bytes();
        let at_cap_len = at_cap.len();
        let cap_reader = FailBeyondHeaderReader {
            data: at_cap,
            pos: 0,
            header_len: at_cap_len,
        };
        let meta =
            CanonicalShardMetadata::from_reader_with_limit(cap_reader, MAX_GGUF_HEADER_BYTES)
                .expect("1024-layer header-only GGUF at exact ceiling must pass");
        assert_eq!(meta.n_layers(), 1_024);
        assert_eq!(meta.max_shard_layers(), 512);
        assert!(meta.admit(0..512, u64::MAX, 4_096).is_ok());
        assert!(matches!(
            meta.admit(0..513, u64::MAX, 4_096),
            Err(ShardAdmissionError::InvalidRange { .. })
        ));
    }

    #[test]
    fn admit_rejects_zero_kv_cache_and_zero_working_set_budgets() {
        let fixture = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true).write_temp("zero_budget");
        let meta = CanonicalShardMetadata::from_path(&fixture.0).expect("inspect meta");
        let baseline = meta.estimate_range(0..2, 0).expect("baseline estimate");

        assert!(matches!(
            meta.admit(0..2, baseline.peak_loading_budget_bytes + 4_096, 0),
            Err(ShardAdmissionError::InvalidBudget {
                field: "max_kv_cache_bytes"
            })
        ));
        assert!(matches!(
            meta.admit(0..2, 0, 4_096),
            Err(ShardAdmissionError::InvalidBudget {
                field: "max_working_set_bytes"
            })
        ));
        assert!(matches!(
            meta.admit(0..2, 0, 0),
            Err(ShardAdmissionError::InvalidBudget { .. })
        ));
        let admitted = meta
            .admit(0..2, baseline.peak_loading_budget_bytes + 1, 1)
            .expect("positive kv and working-set budget admits");
        assert_eq!(admitted.reserved_kv_cache_bytes, 1);
    }

    struct CancelAfterFirstReadReader<'a> {
        inner: Cursor<Vec<u8>>,
        cancel: &'a AtomicBool,
        reads: usize,
    }

    impl Read for CancelAfterFirstReadReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            if n > 0 {
                self.reads += 1;
                if self.reads == 1 {
                    self.cancel.store(true, Ordering::Release);
                }
            }
            Ok(n)
        }
    }

    #[test]
    fn committed_artifact_cancellable_matches_noncancellable_and_aborts_mid_verification() {
        let mut bytes = HeaderFixture::llama(4, 16, 4, 2, 28, 24, true).to_bytes();
        let header_len = bytes.len();
        bytes.resize(header_len + 2 * 1024 * 1024 + 37, 0x5Au8);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let file = TempGguf(std::env::temp_dir().join(format!(
            "arc-shard-adm-{}-{id}-cancellable-commit.gguf",
            std::process::id()
        )));
        std::fs::write(&file.0, &bytes).expect("write fixture");

        let commitment = ModelArtifactCommitment::from_path(&file.0).expect("commit fixture");
        let uncancelled = AtomicBool::new(false);
        let baseline =
            CanonicalShardMetadata::from_committed_artifact(&commitment).expect("baseline commit");
        let cancellable =
            CanonicalShardMetadata::from_committed_artifact_cancellable(&commitment, &uncancelled)
                .expect("cancellable commit");
        assert_eq!(cancellable, baseline);
        assert_eq!(
            cancellable.trust_boundary(),
            ArtifactTrustBoundary::VerifiedBlake3Commitment {
                model_id: commitment.model_id(),
                size_bytes: commitment.size_bytes(),
            }
        );

        let mut tampered = bytes.clone();
        tampered[header_len + 1024 * 1024 + 13] ^= 0x01;
        std::fs::write(&file.0, &tampered).expect("write tampered fixture");
        let tamper_err =
            CanonicalShardMetadata::from_committed_artifact_cancellable(&commitment, &uncancelled)
                .expect_err("tampered payload must still fail verification");
        assert!(matches!(
            tamper_err,
            ShardAdmissionError::ArtifactVerification(_)
        ));
        assert!(!tamper_err.is_cancelled());
        std::fs::write(&file.0, &bytes).expect("restore fixture");

        let preset_cancel = AtomicBool::new(true);
        let preset_err = CanonicalShardMetadata::from_committed_artifact_cancellable(
            &commitment,
            &preset_cancel,
        )
        .expect_err("preset cancel must abort");
        assert!(preset_err.is_cancelled());
        assert!(matches!(preset_err, ShardAdmissionError::Cancelled));

        let header_cancel = AtomicBool::new(false);
        let mut header_reader = CancelAfterFirstReadReader {
            inner: Cursor::new(bytes),
            cancel: &header_cancel,
            reads: 0,
        };
        let header_err = CanonicalShardMetadata::from_reader_with_limit_cancellable(
            &mut header_reader,
            MAX_GGUF_HEADER_BYTES,
            Some(&header_cancel),
        )
        .expect_err("mid-header cancel must abort");
        assert!(header_err.is_cancelled());
        assert_eq!(header_reader.reads, 1);

        let verify_cancel = AtomicBool::new(false);
        let mut header_parsed = false;
        let verify_err = CanonicalShardMetadata::from_committed_artifact_cancellable_with_hook(
            &commitment,
            &verify_cancel,
            || {
                header_parsed = true;
                verify_cancel.store(true, Ordering::Release);
            },
        )
        .expect_err("cancel before second verification pass must abort");
        assert!(header_parsed);
        assert!(verify_err.is_cancelled());
        assert!(matches!(verify_err, ShardAdmissionError::Cancelled));
    }
}

#[cfg(all(test, not(feature = "candle")))]
mod non_candle_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NON_CANDLE_SEQ: AtomicU64 = AtomicU64::new(0);

    struct NonCandleTempFile(PathBuf);

    impl Drop for NonCandleTempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn non_candle_build_refuses_canonical_shard_metadata_inspection() {
        let Err(err) = CanonicalShardMetadata::from_path("x.gguf") else {
            panic!("non-candle build must refuse inspection");
        };
        assert!(matches!(err, ShardAdmissionError::CandleFeatureDisabled));
        assert!(err.to_string().contains("candle feature not enabled"));

        let seq = NON_CANDLE_SEQ.fetch_add(1, Ordering::Relaxed);
        let file = NonCandleTempFile(std::env::temp_dir().join(format!(
            "arc-shard-adm-non-candle-{}-{seq}.gguf",
            std::process::id()
        )));
        std::fs::write(&file.0, b"non-candle-stub").expect("write stub");
        let commitment = ModelArtifactCommitment::from_path(&file.0).expect("commit stub");

        let cancelled = AtomicBool::new(true);
        let cancel_err =
            CanonicalShardMetadata::from_committed_artifact_cancellable(&commitment, &cancelled)
                .expect_err("preset cancel must abort even without candle");
        assert!(cancel_err.is_cancelled());

        let uncancelled = AtomicBool::new(false);
        let disabled_err =
            CanonicalShardMetadata::from_committed_artifact_cancellable(&commitment, &uncancelled)
                .expect_err("non-candle build must refuse committed inspection");
        assert!(matches!(
            disabled_err,
            ShardAdmissionError::CandleFeatureDisabled
        ));
    }
}
