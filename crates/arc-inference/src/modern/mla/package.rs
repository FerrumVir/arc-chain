//! Stage packages (spec §4.6–§4.8): the tensor layout of any contiguous layer
//! range, the file container, segment digests, the layout-independent model
//! root and the stage manifest.
//!
//! The layout is a pure function of the model shape and the stage, so every
//! offset, padding byte and header byte is identical on every platform. A
//! verifier holding layers `[a, b)` checks its file segment by segment against
//! the pinned stage manifest; it never needs the rest of the model.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::ops::Range;
use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::config::MlaConfig;
use super::{PROFILE, STAGE_MAGIC, STAGE_MANIFEST_SCHEMA, STAGE_PACKAGE_SCHEMA};
pub use crate::modern::package::PackageDigest;
use crate::modern::{GENERATION_ARGMAX, GENERATION_RP64, ModernError, hex_lower, identity_blake3};

/// Alignment of the data region and of every tensor.
pub const ALIGN: u64 = 64;
/// Refuse package headers larger than this.
const MAX_HEADER_BYTES: u64 = 64 << 20;
/// Where the profile is specified (recorded in manifests).
pub const CONTRACT: &str = "docs/protocol/integer-profile-mla-moe-dyadic-v1.md";

/// Element type of a package tensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dtype {
    I8,
    U8,
    I16,
    I32,
    I64,
}

impl Dtype {
    pub fn name(self) -> &'static str {
        match self {
            Dtype::I8 => "i8",
            Dtype::U8 => "u8",
            Dtype::I16 => "i16",
            Dtype::I32 => "i32",
            Dtype::I64 => "i64",
        }
    }

    pub fn width(self) -> u64 {
        match self {
            Dtype::I8 | Dtype::U8 => 1,
            Dtype::I16 => 2,
            Dtype::I32 => 4,
            Dtype::I64 => 8,
        }
    }
}

/// One tensor of a stage layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dtype: Dtype,
    pub shape: Vec<usize>,
    /// Offset from the start of the data region.
    pub offset: u64,
    pub bytes: u64,
    /// Segment the tensor belongs to (spec §4.7).
    pub segment: String,
}

/// Round up to the alignment.
pub fn align_up(value: u64) -> u64 {
    value.div_ceil(ALIGN) * ALIGN
}

/// A contiguous layer range `[first_layer, end_layer)` (spec §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageSpec {
    pub first_layer: usize,
    pub end_layer: usize,
}

impl StageSpec {
    /// The whole model as one stage.
    pub fn full(config: &MlaConfig) -> Self {
        Self {
            first_layer: 0,
            end_layer: config.n_layers,
        }
    }

    /// `0 <= first < end <= n_layers`.
    pub fn validate(&self, config: &MlaConfig) -> Result<(), ModernError> {
        if self.first_layer >= self.end_layer || self.end_layer > config.n_layers {
            return Err(ModernError::Invalid(format!(
                "stage [{}, {}) is not a layer range of a {}-layer model",
                self.first_layer, self.end_layer, config.n_layers
            )));
        }
        Ok(())
    }

    /// The first stage embeds tokens.
    pub fn has_embed(&self) -> bool {
        self.first_layer == 0
    }

    /// The last stage produces logits.
    pub fn has_head(&self, config: &MlaConfig) -> bool {
        self.end_layer == config.n_layers
    }

    pub fn layers(&self) -> Range<usize> {
        self.first_layer..self.end_layer
    }

    pub fn to_json(&self) -> Value {
        json!({"first_layer": self.first_layer, "end_layer": self.end_layer})
    }

    pub fn from_json(value: &Value) -> Result<Self, ModernError> {
        let bad = || ModernError::Invalid("package stage object is malformed".into());
        let object = value.as_object().ok_or_else(bad)?;
        if object.len() != 2 {
            return Err(bad());
        }
        let field = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(bad)
        };
        Ok(Self {
            first_layer: field("first_layer")?,
            end_layer: field("end_layer")?,
        })
    }

    /// Parse `a:b`.
    pub fn parse(text: &str) -> Result<Self, ModernError> {
        let bad = || ModernError::Invalid(format!("stage {text:?} is not of the form a:b"));
        let (a, b) = text.split_once(':').ok_or_else(bad)?;
        Ok(Self {
            first_layer: a.trim().parse().map_err(|_| bad())?,
            end_layer: b.trim().parse().map_err(|_| bad())?,
        })
    }
}

struct Spec {
    name: String,
    dtype: Dtype,
    shape: Vec<usize>,
    segment: String,
}

fn vector(specs: &mut Vec<Spec>, segment: &str, name: &str, dtype: Dtype, shape: &[usize]) {
    specs.push(Spec {
        name: name.to_string(),
        dtype,
        shape: shape.to_vec(),
        segment: segment.to_string(),
    });
}

/// The three tensors of a dyadic matrix (`[rows, cols]` or `[n, rows, cols]`).
fn dyadic(specs: &mut Vec<Spec>, segment: &str, name: &str, dims: &[usize]) {
    let rows = &dims[..dims.len() - 1];
    vector(specs, segment, &format!("{name}.q"), Dtype::I8, dims);
    vector(specs, segment, &format!("{name}.mu"), Dtype::I32, rows);
    vector(specs, segment, &format!("{name}.k"), Dtype::U8, rows);
}

/// Segment name of layer `l`.
pub fn layer_segment(layer: usize) -> String {
    format!("layer.{layer}")
}

fn layer_specs(c: &MlaConfig, layer: usize, specs: &mut Vec<Spec>) {
    let segment = layer_segment(layer);
    let s = segment.as_str();
    let p = format!("layers.{layer}");
    let (d, h) = (c.d_model, c.n_heads);
    let (rank, nope, vh) = (c.kv_lora_rank, c.qk_nope_dim, c.v_head_dim);
    vector(specs, s, &format!("{p}.attn_norm"), Dtype::I64, &[d]);
    if c.q_lora_rank == 0 {
        dyadic(specs, s, &format!("{p}.wq"), &[c.d_q(), d]);
    } else {
        dyadic(specs, s, &format!("{p}.wq_a"), &[c.q_lora_rank, d]);
        vector(
            specs,
            s,
            &format!("{p}.q_a_norm"),
            Dtype::I64,
            &[c.q_lora_rank],
        );
        dyadic(specs, s, &format!("{p}.wq_b"), &[c.d_q(), c.q_lora_rank]);
    }
    dyadic(specs, s, &format!("{p}.wkv_a"), &[c.d_kv_a(), d]);
    vector(specs, s, &format!("{p}.kv_a_norm"), Dtype::I64, &[rank]);
    dyadic(specs, s, &format!("{p}.wk_b"), &[h, rank, nope]);
    dyadic(specs, s, &format!("{p}.wv_b"), &[h, vh, rank]);
    dyadic(specs, s, &format!("{p}.wo"), &[d, c.d_attn_out()]);
    vector(specs, s, &format!("{p}.ffn_norm"), Dtype::I64, &[d]);
    if c.is_moe(layer) {
        let (e, fm, sf) = (c.n_routed_experts, c.moe_d_ff, c.shared_d_ff());
        vector(specs, s, &format!("{p}.router.q"), Dtype::I16, &[e, d]);
        vector(specs, s, &format!("{p}.router.k"), Dtype::U8, &[e]);
        vector(specs, s, &format!("{p}.router_bias"), Dtype::I64, &[e]);
        dyadic(specs, s, &format!("{p}.shared.w_gate"), &[sf, d]);
        dyadic(specs, s, &format!("{p}.shared.w_up"), &[sf, d]);
        dyadic(specs, s, &format!("{p}.shared.w_down"), &[d, sf]);
        dyadic(specs, s, &format!("{p}.experts.w_gate"), &[e, fm, d]);
        dyadic(specs, s, &format!("{p}.experts.w_up"), &[e, fm, d]);
        dyadic(specs, s, &format!("{p}.experts.w_down"), &[e, d, fm]);
    } else {
        dyadic(specs, s, &format!("{p}.w_gate"), &[c.d_ff, d]);
        dyadic(specs, s, &format!("{p}.w_up"), &[c.d_ff, d]);
        dyadic(specs, s, &format!("{p}.w_down"), &[d, c.d_ff]);
    }
}

/// The canonical tensor layout of a stage (spec §4.6).
pub fn layout(c: &MlaConfig, stage: StageSpec) -> Vec<Entry> {
    let mut specs = Vec::new();
    let half = c.qk_rope_dim / 2;
    vector(
        &mut specs,
        "tables",
        "rope.cos",
        Dtype::I32,
        &[c.max_seq, half],
    );
    vector(
        &mut specs,
        "tables",
        "rope.sin",
        Dtype::I32,
        &[c.max_seq, half],
    );
    if stage.has_embed() {
        dyadic(&mut specs, "embed", "embed", &[c.vocab_size, c.d_model]);
    }
    for layer in stage.layers() {
        layer_specs(c, layer, &mut specs);
    }
    if stage.has_head(c) {
        vector(&mut specs, "head", "final_norm", Dtype::I64, &[c.d_model]);
        dyadic(&mut specs, "head", "lm_head", &[c.vocab_size, c.d_model]);
    }
    let mut offset = 0u64;
    specs
        .into_iter()
        .map(|spec| {
            let elements: u64 = spec.shape.iter().map(|&s| s as u64).product();
            let bytes = elements * spec.dtype.width();
            let entry = Entry {
                name: spec.name,
                dtype: spec.dtype,
                shape: spec.shape,
                offset,
                bytes,
                segment: spec.segment,
            };
            offset = align_up(offset + bytes);
            entry
        })
        .collect()
}

/// Every segment of the whole model, in canonical order (spec §4.7).
pub fn segment_names(c: &MlaConfig) -> Vec<String> {
    let mut names = vec!["tables".to_string(), "embed".to_string()];
    names.extend((0..c.n_layers).map(layer_segment));
    names.push("head".to_string());
    names
}

/// The package header (spec §4.6).
pub fn header_json(c: &MlaConfig, source: &Value, stage: StageSpec, entries: &[Entry]) -> Value {
    let tensors: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "name": e.name,
                "dtype": e.dtype.name(),
                "shape": e.shape,
                "offset": e.offset,
                "bytes": e.bytes,
            })
        })
        .collect();
    json!({
        "schema": STAGE_PACKAGE_SCHEMA,
        "profile": PROFILE,
        "model": c.to_json(),
        "source": source,
        "stage": stage.to_json(),
        "tensors": tensors,
    })
}

fn canonical(value: &Value) -> Result<String, ModernError> {
    crate::model_package::canonical_json(value)
        .map_err(|e| ModernError::Invalid(format!("canonical JSON: {e}")))
}

/// BLAKE3 digest of one segment's tensor bytes (spec §4.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentDigest {
    pub name: String,
    pub bytes: u64,
    pub blake3: String,
}

impl SegmentDigest {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "bytes": self.bytes, "blake3": self.blake3})
    }

    pub fn from_json(value: &Value) -> Result<Self, ModernError> {
        let bad = || ModernError::Invalid("manifest segment entry is malformed".into());
        Ok(Self {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(bad)?
                .to_string(),
            bytes: value.get("bytes").and_then(Value::as_u64).ok_or_else(bad)?,
            blake3: value
                .get("blake3")
                .and_then(Value::as_str)
                .ok_or_else(bad)?
                .to_string(),
        })
    }
}

/// Streams a stage package to disk in layout order, hashing the file
/// (SHA-256, BLAKE3) and every segment (BLAKE3 of its tensor bytes).
pub struct StageWriter {
    out: BufWriter<File>,
    sha256: Sha256,
    blake3: blake3::Hasher,
    written: u64,
    data_start: u64,
    entries: Vec<Entry>,
    next: usize,
    open: Option<(usize, u64)>,
    segment: Option<(String, blake3::Hasher, u64)>,
    segments: Vec<SegmentDigest>,
}

impl StageWriter {
    /// Create `path` and write the magic, the header and its padding.
    pub fn create(path: &Path, header: &Value, entries: Vec<Entry>) -> Result<Self, ModernError> {
        let context = path.display().to_string();
        let file = File::create(path).map_err(|e| ModernError::io(&context, e))?;
        let mut writer = Self {
            out: BufWriter::with_capacity(8 << 20, file),
            sha256: Sha256::new(),
            blake3: blake3::Hasher::new(),
            written: 0,
            data_start: 0,
            entries,
            next: 0,
            open: None,
            segment: None,
            segments: Vec::new(),
        };
        let text = canonical(header)?;
        writer.emit(STAGE_MAGIC)?;
        writer.emit(&(text.len() as u64).to_le_bytes())?;
        writer.emit(text.as_bytes())?;
        writer.data_start = align_up(16 + text.len() as u64);
        writer.pad_to(writer.data_start)?;
        Ok(writer)
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<(), ModernError> {
        self.out
            .write_all(bytes)
            .map_err(|e| ModernError::io("stage package write", e))?;
        self.sha256.update(bytes);
        self.blake3.update(bytes);
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn pad_to(&mut self, position: u64) -> Result<(), ModernError> {
        if position < self.written || position - self.written >= ALIGN {
            return Err(ModernError::Invalid(
                "stage package padding out of order".into(),
            ));
        }
        let zeros = [0u8; ALIGN as usize];
        let count = (position - self.written) as usize;
        self.emit(&zeros[..count])
    }

    fn close_segment(&mut self) {
        if let Some((name, hasher, bytes)) = self.segment.take() {
            self.segments.push(SegmentDigest {
                name,
                bytes,
                blake3: hex_lower(hasher.finalize().as_bytes()),
            });
        }
    }

    /// Start the next tensor of the layout; its name must match.
    pub fn begin(&mut self, name: &str) -> Result<(), ModernError> {
        if self.open.is_some() {
            return Err(ModernError::Invalid(format!(
                "tensor {name} started before the previous one ended"
            )));
        }
        let entry = self
            .entries
            .get(self.next)
            .cloned()
            .ok_or_else(|| ModernError::Invalid(format!("unexpected extra tensor {name}")))?;
        if entry.name != name {
            return Err(ModernError::Invalid(format!(
                "tensor {name} written where {} belongs",
                entry.name
            )));
        }
        if self.segment.as_ref().map(|s| s.0.as_str()) != Some(entry.segment.as_str()) {
            self.close_segment();
            self.segment = Some((entry.segment.clone(), blake3::Hasher::new(), 0));
        }
        self.pad_to(self.data_start + entry.offset)?;
        self.open = Some((self.next, 0));
        Ok(())
    }

    /// Append bytes to the open tensor.
    pub fn chunk(&mut self, bytes: &[u8]) -> Result<(), ModernError> {
        let (index, done) = self
            .open
            .ok_or_else(|| ModernError::Invalid("no tensor is open".into()))?;
        let limit = self.entries[index].bytes;
        if done + bytes.len() as u64 > limit {
            return Err(ModernError::Invalid(format!(
                "tensor {} would exceed its {limit} bytes",
                self.entries[index].name
            )));
        }
        self.emit(bytes)?;
        if let Some((_, hasher, count)) = self.segment.as_mut() {
            hasher.update(bytes);
            *count += bytes.len() as u64;
        }
        self.open = Some((index, done + bytes.len() as u64));
        Ok(())
    }

    /// Close the open tensor; it must have exactly its layout size.
    pub fn end(&mut self) -> Result<(), ModernError> {
        let (index, done) = self
            .open
            .take()
            .ok_or_else(|| ModernError::Invalid("no tensor is open".into()))?;
        if done != self.entries[index].bytes {
            return Err(ModernError::Invalid(format!(
                "tensor {} has {done} bytes; its layout needs {}",
                self.entries[index].name, self.entries[index].bytes
            )));
        }
        self.next += 1;
        Ok(())
    }

    /// Write a whole tensor.
    pub fn write_tensor(&mut self, name: &str, bytes: &[u8]) -> Result<(), ModernError> {
        self.begin(name)?;
        self.chunk(bytes)?;
        self.end()
    }

    /// Final padding, flush, and the file and segment digests.
    pub fn finish(mut self) -> Result<(PackageDigest, Vec<SegmentDigest>), ModernError> {
        if self.next != self.entries.len() || self.open.is_some() {
            return Err(ModernError::Invalid(format!(
                "stage package finished after {} of {} tensors",
                self.next,
                self.entries.len()
            )));
        }
        self.close_segment();
        let end = align_up(self.written);
        self.pad_to(end)?;
        self.out
            .flush()
            .map_err(|e| ModernError::io("stage package flush", e))?;
        let digest = PackageDigest {
            bytes: self.written,
            sha256: hex_lower(&self.sha256.finalize()),
            blake3: hex_lower(self.blake3.finalize().as_bytes()),
        };
        Ok((digest, self.segments))
    }
}

/// Little-endian bytes of an i16 slice.
pub fn i16_bytes(values: &[i16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// The parsed header of a stage package.
#[derive(Debug, Clone)]
pub struct StageHeader {
    pub value: Value,
    pub config: MlaConfig,
    pub stage: StageSpec,
    pub source: Value,
    pub entries: Vec<Entry>,
    /// Bytes before the header padding: magic, length and header text.
    pub header_end: u64,
    pub data_start: u64,
    pub file_len: u64,
}

impl StageHeader {
    /// The layout entry named `name`.
    pub fn entry(&self, name: &str) -> Result<&Entry, ModernError> {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| ModernError::Invalid(format!("stage package has no tensor {name}")))
    }

    /// Absolute byte range of a tensor in the file.
    pub fn range(&self, entry: &Entry) -> Range<usize> {
        let start = (self.data_start + entry.offset) as usize;
        start..start + entry.bytes as usize
    }
}

/// Parse and validate a stage header from the file's first bytes
/// (`prefix` holds at least the magic, length and header text).
pub fn parse_header(prefix: &[u8], file_len: u64) -> Result<StageHeader, ModernError> {
    if prefix.len() < 16 || &prefix[..8] != STAGE_MAGIC {
        return Err(ModernError::Invalid("not an ARC stage package".into()));
    }
    let mut len_bytes = [0u8; 8];
    len_bytes.copy_from_slice(&prefix[8..16]);
    let header_len = u64::from_le_bytes(len_bytes);
    if header_len > MAX_HEADER_BYTES || 16 + header_len > file_len {
        return Err(ModernError::Invalid(
            "stage package header length is invalid".into(),
        ));
    }
    let end = 16 + header_len as usize;
    if prefix.len() < end {
        return Err(ModernError::Invalid(
            "stage package header is truncated".into(),
        ));
    }
    let text = &prefix[16..end];
    let value: Value = serde_json::from_slice(text)
        .map_err(|e| ModernError::Invalid(format!("stage package header JSON: {e}")))?;
    if canonical(&value)?.as_bytes() != text {
        return Err(ModernError::Invalid(
            "stage package header is not canonical JSON".into(),
        ));
    }
    let object = value
        .as_object()
        .ok_or_else(|| ModernError::Invalid("stage package header is not an object".into()))?;
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if keys != ["model", "profile", "schema", "source", "stage", "tensors"] {
        return Err(ModernError::Invalid(format!(
            "stage package header fields {keys:?}"
        )));
    }
    if value.get("schema").and_then(Value::as_str) != Some(STAGE_PACKAGE_SCHEMA) {
        return Err(ModernError::Invalid(format!(
            "stage package schema is not {STAGE_PACKAGE_SCHEMA}"
        )));
    }
    if value.get("profile").and_then(Value::as_str) != Some(PROFILE) {
        return Err(ModernError::Invalid(format!(
            "stage package profile is not {PROFILE}"
        )));
    }
    let config = MlaConfig::from_json(&value["model"])?;
    let stage = StageSpec::from_json(&value["stage"])?;
    stage.validate(&config)?;
    let source = value["source"].clone();
    let entries = layout(&config, stage);
    let expected = header_json(&config, &source, stage, &entries);
    if expected.get("tensors") != value.get("tensors") {
        return Err(ModernError::Invalid(
            "stage package tensor table differs from the canonical layout".into(),
        ));
    }
    let data_start = align_up(16 + header_len);
    let data_end = entries
        .last()
        .map(|e| align_up(e.offset + e.bytes))
        .unwrap_or(0);
    if data_start + data_end != file_len {
        return Err(ModernError::Invalid(format!(
            "stage package is {file_len} bytes; its layout needs {}",
            data_start + data_end
        )));
    }
    Ok(StageHeader {
        value,
        config,
        stage,
        source,
        entries,
        header_end: 16 + header_len,
        data_start,
        file_len,
    })
}

/// Read only the header of a stage package file.
pub fn read_header_file(path: &Path) -> Result<StageHeader, ModernError> {
    let context = path.display().to_string();
    let file = File::open(path).map_err(|e| ModernError::io(&context, e))?;
    let file_len = file
        .metadata()
        .map_err(|e| ModernError::io(&context, e))?
        .len();
    let mut reader = BufReader::new(file);
    let mut head = [0u8; 16];
    reader
        .read_exact(&mut head)
        .map_err(|e| ModernError::io(&context, e))?;
    let header_len = u64::from_le_bytes([
        head[8], head[9], head[10], head[11], head[12], head[13], head[14], head[15],
    ]);
    if &head[..8] != STAGE_MAGIC || header_len > MAX_HEADER_BYTES || 16 + header_len > file_len {
        return Err(ModernError::Invalid(format!(
            "{context} is not an ARC stage package"
        )));
    }
    let mut prefix = head.to_vec();
    prefix.resize(16 + header_len as usize, 0);
    reader
        .read_exact(&mut prefix[16..])
        .map_err(|e| ModernError::io(&context, e))?;
    parse_header(&prefix, file_len)
}

/// Every padding byte of the file must be zero.
pub fn check_padding(bytes: &[u8], header: &StageHeader) -> Result<(), ModernError> {
    let mut regions = vec![(header.header_end, header.data_start)];
    for e in &header.entries {
        let end = header.data_start + e.offset + e.bytes;
        regions.push((end, header.data_start + align_up(e.offset + e.bytes)));
    }
    for (start, end) in regions {
        if bytes[start as usize..end as usize].iter().any(|&b| b != 0) {
            return Err(ModernError::Invalid(format!(
                "non-zero padding at byte {start}"
            )));
        }
    }
    Ok(())
}

/// Segment digests of a stage package's bytes (spec §4.7).
pub fn segment_digests(bytes: &[u8], header: &StageHeader) -> Vec<SegmentDigest> {
    segment_digests_where(bytes, header, |_| true)
}

/// Digests of the segments for which `keep` holds, in layout order.
pub fn segment_digests_where(
    bytes: &[u8],
    header: &StageHeader,
    keep: impl Fn(&str) -> bool,
) -> Vec<SegmentDigest> {
    let mut out: Vec<SegmentDigest> = Vec::new();
    let mut current: Option<(String, blake3::Hasher, u64)> = None;
    for e in header.entries.iter().filter(|e| keep(&e.segment)) {
        if current.as_ref().map(|c| c.0.as_str()) != Some(e.segment.as_str()) {
            if let Some((name, hasher, count)) = current.take() {
                out.push(SegmentDigest {
                    name,
                    bytes: count,
                    blake3: hex_lower(hasher.finalize().as_bytes()),
                });
            }
            current = Some((e.segment.clone(), blake3::Hasher::new(), 0));
        }
        if let Some((_, hasher, count)) = current.as_mut() {
            hasher.update_rayon(&bytes[header.range(e)]);
            *count += e.bytes;
        }
    }
    if let Some((name, hasher, count)) = current {
        out.push(SegmentDigest {
            name,
            bytes: count,
            blake3: hex_lower(hasher.finalize().as_bytes()),
        });
    }
    out
}

/// The layout-independent model root (spec §4.7).
pub fn model_root(
    c: &MlaConfig,
    source: &Value,
    segments: &[SegmentDigest],
) -> Result<String, ModernError> {
    let names: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
    let expected = segment_names(c);
    if names != expected.iter().map(String::as_str).collect::<Vec<_>>() {
        return Err(ModernError::Invalid(
            "the model root needs every segment of the model, in canonical order".into(),
        ));
    }
    let list: Vec<Value> = segments.iter().map(|s| json!([s.name, s.blake3])).collect();
    let body = json!({
        "model": c.to_json(),
        "profile": PROFILE,
        "segments": list,
        "source": source,
    });
    Ok(blake3::hash(canonical(&body)?.as_bytes())
        .to_hex()
        .to_string())
}

/// Build the stage manifest (spec §4.7) from a full-model conversion.
pub fn build_manifest(
    c: &MlaConfig,
    source: &Value,
    segments: &[SegmentDigest],
    full_package: Option<&PackageDigest>,
    eos: &[u32],
    tokenizer: &Value,
) -> Result<Value, ModernError> {
    let root = model_root(c, source, segments)?;
    let segment_list: Vec<Value> = segments.iter().map(SegmentDigest::to_json).collect();
    let mut manifest = json!({
        "schema": STAGE_MANIFEST_SCHEMA,
        "profile": PROFILE,
        "profile_blake3": identity_blake3(PROFILE),
        "contract": CONTRACT,
        "model": c.to_json(),
        "source": source,
        "segments": segment_list,
        "model_root": root,
        "full_package": full_package.map(PackageDigest::to_json).unwrap_or(Value::Null),
        "generation": {
            "semantics": GENERATION_RP64,
            "semantics_blake3": identity_blake3(GENERATION_RP64),
            "diagnostic_semantics": GENERATION_ARGMAX,
            "eos": eos,
            "bos_forwarded": false,
            "max_seq": c.max_seq,
        },
        "tokenizer": tokenizer,
    });
    let hash = crate::model_package::manifest_body_blake3(&manifest)
        .map_err(|e| ModernError::Invalid(format!("manifest hash: {e}")))?;
    manifest["manifest_blake3"] = Value::from(hash);
    Ok(manifest)
}

/// Check a stage package (header plus its segment digests) against a pinned
/// stage manifest; returns a report of what was checked.
pub fn verify_against_manifest(
    header: &StageHeader,
    segments: &[SegmentDigest],
    manifest_bytes: &[u8],
) -> Result<Value, ModernError> {
    let manifest: Value = serde_json::from_slice(manifest_bytes)
        .map_err(|e| ModernError::Invalid(format!("stage manifest JSON: {e}")))?;
    if manifest.get("schema").and_then(Value::as_str) != Some(STAGE_MANIFEST_SCHEMA)
        || manifest.get("profile").and_then(Value::as_str) != Some(PROFILE)
    {
        return Err(ModernError::Invalid(
            "stage manifest schema or profile mismatch".into(),
        ));
    }
    let recorded = manifest
        .get("manifest_blake3")
        .and_then(Value::as_str)
        .ok_or_else(|| ModernError::Invalid("stage manifest has no manifest_blake3".into()))?;
    let recomputed = crate::model_package::manifest_body_blake3(&manifest)
        .map_err(|e| ModernError::Invalid(format!("manifest hash: {e}")))?;
    if recomputed != recorded {
        return Err(ModernError::Invalid(format!(
            "manifest_blake3 is {recorded}, but the manifest hashes to {recomputed}"
        )));
    }
    if manifest.get("model") != header.value.get("model")
        || manifest.get("source") != Some(&header.source)
    {
        return Err(ModernError::Invalid(
            "stage package model or source differs from the manifest".into(),
        ));
    }
    let pinned: Vec<SegmentDigest> = manifest
        .get("segments")
        .and_then(Value::as_array)
        .ok_or_else(|| ModernError::Invalid("stage manifest has no segments".into()))?
        .iter()
        .map(SegmentDigest::from_json)
        .collect::<Result<_, _>>()?;
    let root = model_root(&header.config, &header.source, &pinned)?;
    if manifest.get("model_root").and_then(Value::as_str) != Some(root.as_str()) {
        return Err(ModernError::Invalid(
            "stage manifest model_root does not match its segments".into(),
        ));
    }
    let mut checked = Vec::new();
    for segment in segments {
        let want = pinned
            .iter()
            .find(|p| p.name == segment.name)
            .ok_or_else(|| {
                ModernError::Invalid(format!("segment {} is not pinned", segment.name))
            })?;
        if want != segment {
            return Err(ModernError::Invalid(format!(
                "segment {} is {} bytes with BLAKE3 {}; the manifest pins {} bytes with BLAKE3 {}",
                segment.name, segment.bytes, segment.blake3, want.bytes, want.blake3
            )));
        }
        checked.push(Value::from(segment.name.clone()));
    }
    Ok(json!({
        "verified": true,
        "stage": header.stage.to_json(),
        "segments_checked": checked,
        "model_root": root,
    }))
}

/// Stream a file through SHA-256 and BLAKE3.
pub fn digest_file(path: &Path) -> Result<PackageDigest, ModernError> {
    crate::modern::package::digest_file(path)
}

#[cfg(test)]
mod tests {
    use super::super::config::tests::MOONLIGHT_CONFIG;
    use super::super::config::{MlaConfig, parse_hf_config};
    use super::*;

    fn moonlight() -> MlaConfig {
        parse_hf_config(MOONLIGHT_CONFIG.as_bytes(), 4096)
            .unwrap()
            .config
    }

    #[test]
    fn stage_layouts_partition_the_full_layout() {
        let c = moonlight();
        let full = layout(&c, StageSpec::full(&c));
        assert_eq!(full[0].name, "rope.cos");
        assert_eq!(full[2].name, "embed.q");
        assert_eq!(full.last().unwrap().name, "lm_head.k");
        assert!(full.iter().all(|e| e.offset.is_multiple_of(ALIGN)));
        // Dense layer 0 and MoE layer 1.
        assert!(full.iter().any(|e| e.name == "layers.0.w_gate.q"));
        assert!(!full.iter().any(|e| e.name == "layers.0.router.q"));
        let gate = full
            .iter()
            .find(|e| e.name == "layers.1.experts.w_gate.q")
            .unwrap();
        assert_eq!(gate.shape, vec![64, 1408, 2048]);
        let wk_b = full.iter().find(|e| e.name == "layers.1.wk_b.q").unwrap();
        assert_eq!(wk_b.shape, vec![16, 512, 128]);
        let wk_b_mu = full.iter().find(|e| e.name == "layers.1.wk_b.mu").unwrap();
        assert_eq!(wk_b_mu.shape, vec![16, 512]);
        // INT8 weights: every BF16 parameter of the published checkpoint
        // (15,960,111,936) except routers (16-bit), norms, biases and the
        // ignored rotary buffers.
        let int8: u64 = full
            .iter()
            .filter(|e| e.dtype == Dtype::I8)
            .map(|e| e.bytes)
            .sum();
        assert_eq!(int8, 15_956_574_208);
        // Every tensor of the full layout appears in exactly one stage of a
        // four-way split, with the same shape and segment.
        let cuts = [0, 7, 14, 21, 27];
        let mut names = Vec::new();
        for pair in cuts.windows(2) {
            let stage = StageSpec {
                first_layer: pair[0],
                end_layer: pair[1],
            };
            stage.validate(&c).unwrap();
            let entries = layout(&c, stage);
            assert_eq!(entries[0].name, "rope.cos");
            assert_eq!(entries[2].name == "embed.q", pair[0] == 0);
            assert_eq!(entries.last().unwrap().name == "lm_head.k", pair[1] == 27);
            names.extend(
                entries
                    .into_iter()
                    .filter(|e| e.segment != "tables")
                    .map(|e| (e.name, e.shape, e.segment)),
            );
        }
        let full_names: Vec<_> = full
            .into_iter()
            .filter(|e| e.segment != "tables")
            .map(|e| (e.name, e.shape, e.segment))
            .collect();
        assert_eq!(names, full_names);
        assert!(StageSpec::parse("3:3").unwrap().validate(&c).is_err());
        assert!(StageSpec::parse("0:28").unwrap().validate(&c).is_err());
        assert_eq!(
            StageSpec::parse("25:27").unwrap(),
            StageSpec {
                first_layer: 25,
                end_layer: 27
            }
        );
    }

    #[test]
    fn query_lora_layers_have_the_two_projections_and_a_norm() {
        let lora = MOONLIGHT_CONFIG.replace("\"q_lora_rank\": null", "\"q_lora_rank\": 1536");
        let c = parse_hf_config(lora.as_bytes(), 4096).unwrap().config;
        let entries = layout(
            &c,
            StageSpec {
                first_layer: 3,
                end_layer: 4,
            },
        );
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"layers.3.wq_a.q"));
        assert!(names.contains(&"layers.3.q_a_norm"));
        assert!(names.contains(&"layers.3.wq_b.q"));
        assert!(!names.contains(&"layers.3.wq.q"));
        assert_eq!(segment_names(&c).len(), 27 + 3);
    }
}
