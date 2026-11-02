//! The integer package file (`arc.integer-package.v1`) and its manifest.
//!
//! A package holds every integer the forward pass reads: INT8 weights with
//! dyadic row scales, Q16 norm gains and the RoPE tables (spec §4.8, §4.9).
//! Its layout is fully determined by the model configuration, so the header,
//! the offsets and every padding byte are identical on every platform. The
//! package's identity is the SHA-256 (and BLAKE3) of the whole file; a node
//! refuses a package whose digest differs from the pinned manifest (§4.10).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::arith::DyadicMatrix;
use super::model::{ModernConfig, ModernLayer, ModernModel};
use super::{ModernError, PROFILE, hex_lower, identity_blake3};

/// File magic.
pub const MAGIC: &[u8; 8] = b"ARCIPKG1";
/// Package schema identity.
pub const PACKAGE_SCHEMA: &str = "arc.integer-package.v1";
/// Package manifest schema identity.
pub const MANIFEST_SCHEMA: &str = "arc.integer-package-manifest.v1";
/// Alignment of the data region and of every tensor.
pub const ALIGN: u64 = 64;
/// Refuse package headers larger than this.
const MAX_HEADER_BYTES: u64 = 16 << 20;

/// Element type of a package tensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dtype {
    I8,
    U8,
    I32,
    I64,
}

impl Dtype {
    pub fn name(self) -> &'static str {
        match self {
            Dtype::I8 => "i8",
            Dtype::U8 => "u8",
            Dtype::I32 => "i32",
            Dtype::I64 => "i64",
        }
    }

    pub fn width(self) -> u64 {
        match self {
            Dtype::I8 | Dtype::U8 => 1,
            Dtype::I32 => 4,
            Dtype::I64 => 8,
        }
    }
}

/// One tensor of the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorEntry {
    pub name: String,
    pub dtype: Dtype,
    pub shape: Vec<usize>,
    /// Offset from the start of the data region.
    pub offset: u64,
    pub bytes: u64,
}

fn align_up(value: u64) -> u64 {
    value.div_ceil(ALIGN) * ALIGN
}

fn push_matrix(specs: &mut Vec<(String, Dtype, Vec<usize>)>, name: &str, rows: usize, cols: usize) {
    specs.push((format!("{name}.q"), Dtype::I8, vec![rows, cols]));
    specs.push((format!("{name}.mu"), Dtype::I32, vec![rows]));
    specs.push((format!("{name}.k"), Dtype::U8, vec![rows]));
}

/// The canonical tensor layout for `config` (spec §4.8).
pub fn layout(config: &ModernConfig) -> Vec<TensorEntry> {
    let (d, f, v) = (config.d_model, config.d_ff, config.vocab_size);
    let (dq, dkv) = (config.d_q(), config.d_kv());
    let mut specs: Vec<(String, Dtype, Vec<usize>)> = Vec::new();
    push_matrix(&mut specs, "embed", v, d);
    specs.push(("final_norm".into(), Dtype::I64, vec![d]));
    specs.push((
        "rope.cos".into(),
        Dtype::I32,
        vec![config.max_seq, config.d_head / 2],
    ));
    specs.push((
        "rope.sin".into(),
        Dtype::I32,
        vec![config.max_seq, config.d_head / 2],
    ));
    for l in 0..config.n_layers {
        let p = format!("layers.{l}");
        specs.push((format!("{p}.attn_norm"), Dtype::I64, vec![d]));
        push_matrix(&mut specs, &format!("{p}.wq"), dq, d);
        push_matrix(&mut specs, &format!("{p}.wk"), dkv, d);
        push_matrix(&mut specs, &format!("{p}.wv"), dkv, d);
        push_matrix(&mut specs, &format!("{p}.wo"), d, dq);
        specs.push((format!("{p}.ffn_norm"), Dtype::I64, vec![d]));
        push_matrix(&mut specs, &format!("{p}.w_gate"), f, d);
        push_matrix(&mut specs, &format!("{p}.w_up"), f, d);
        push_matrix(&mut specs, &format!("{p}.w_down"), d, f);
    }
    let mut offset = 0u64;
    specs
        .into_iter()
        .map(|(name, dtype, shape)| {
            let elements: u64 = shape.iter().map(|&s| s as u64).product();
            let bytes = elements * dtype.width();
            let entry = TensorEntry {
                name,
                dtype,
                shape,
                offset,
                bytes,
            };
            offset = align_up(offset + bytes);
            entry
        })
        .collect()
}

/// The `model` object of the header and manifest (spec §4.9).
pub fn model_json(config: &ModernConfig) -> Value {
    json!({
        "architecture": config.architecture,
        "n_layers": config.n_layers,
        "d_model": config.d_model,
        "n_heads": config.n_heads,
        "n_kv_heads": config.n_kv_heads,
        "d_head": config.d_head,
        "d_ff": config.d_ff,
        "vocab_size": config.vocab_size,
        "max_seq": config.max_seq,
        "rms_eps_q32": config.rms_eps_q32,
        "rope_theta": config.rope_theta,
        "rope_layers": config.rope_layers.iter().map(|&r| u8::from(r)).collect::<Vec<u8>>(),
        "tied_embeddings": true,
    })
}

/// Parse the header's `model` object back into a configuration.
pub fn config_from_json(model: &Value) -> Result<ModernConfig, ModernError> {
    let bad = |what: &str| ModernError::Invalid(format!("package model.{what}"));
    let size = |key: &str| -> Result<usize, ModernError> {
        model
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| bad(key))
    };
    let rope_layers = model
        .get("rope_layers")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("rope_layers"))?
        .iter()
        .map(|v| match v.as_u64() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(bad("rope_layers")),
        })
        .collect::<Result<Vec<bool>, ModernError>>()?;
    if model.get("tied_embeddings").and_then(Value::as_bool) != Some(true) {
        return Err(bad("tied_embeddings"));
    }
    let config = ModernConfig {
        architecture: model
            .get("architecture")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("architecture"))?
            .to_string(),
        n_layers: size("n_layers")?,
        d_model: size("d_model")?,
        n_heads: size("n_heads")?,
        n_kv_heads: size("n_kv_heads")?,
        d_head: size("d_head")?,
        d_ff: size("d_ff")?,
        vocab_size: size("vocab_size")?,
        max_seq: size("max_seq")?,
        rms_eps_q32: model
            .get("rms_eps_q32")
            .and_then(Value::as_i64)
            .ok_or_else(|| bad("rms_eps_q32"))?,
        rope_theta: model
            .get("rope_theta")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("rope_theta"))?,
        rope_layers,
    };
    config.validate()?;
    Ok(config)
}

/// The header object (spec §4.9).
pub fn header_json(config: &ModernConfig, source: &Value, entries: &[TensorEntry]) -> Value {
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
        "schema": PACKAGE_SCHEMA,
        "profile": PROFILE,
        "model": model_json(config),
        "source": source,
        "tensors": tensors,
    })
}

fn canonical(value: &Value) -> Result<String, ModernError> {
    crate::model_package::canonical_json(value)
        .map_err(|e| ModernError::Invalid(format!("canonical JSON: {e}")))
}

/// Size and digests of a package file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDigest {
    pub bytes: u64,
    pub sha256: String,
    pub blake3: String,
}

impl PackageDigest {
    pub fn to_json(&self) -> Value {
        json!({"bytes": self.bytes, "sha256": self.sha256, "blake3": self.blake3})
    }
}

/// Streams a package to disk in layout order, hashing every byte written.
pub struct PackageWriter {
    out: BufWriter<File>,
    sha256: Sha256,
    blake3: blake3::Hasher,
    written: u64,
    data_start: u64,
    entries: Vec<TensorEntry>,
    next: usize,
}

impl PackageWriter {
    /// Create `path` and write the magic, the header and its padding.
    pub fn create(
        path: &Path,
        header: &Value,
        entries: Vec<TensorEntry>,
    ) -> Result<Self, ModernError> {
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
        };
        let header_text = canonical(header)?;
        writer.emit(MAGIC)?;
        writer.emit(&(header_text.len() as u64).to_le_bytes())?;
        writer.emit(header_text.as_bytes())?;
        writer.data_start = align_up(16 + header_text.len() as u64);
        writer.pad_to(writer.data_start)?;
        Ok(writer)
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<(), ModernError> {
        self.out
            .write_all(bytes)
            .map_err(|e| ModernError::io("package write", e))?;
        self.sha256.update(bytes);
        self.blake3.update(bytes);
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn pad_to(&mut self, position: u64) -> Result<(), ModernError> {
        if position < self.written || position - self.written >= ALIGN {
            return Err(ModernError::Invalid("package padding out of order".into()));
        }
        let zeros = [0u8; ALIGN as usize];
        let count = (position - self.written) as usize;
        self.emit(&zeros[..count])
    }

    /// Write the next tensor of the layout; its name and size must match.
    pub fn write_tensor(&mut self, name: &str, bytes: &[u8]) -> Result<(), ModernError> {
        let entry = self
            .entries
            .get(self.next)
            .cloned()
            .ok_or_else(|| ModernError::Invalid(format!("unexpected extra tensor {name}")))?;
        if entry.name != name || entry.bytes != bytes.len() as u64 {
            return Err(ModernError::Invalid(format!(
                "tensor {name} ({} bytes) written where {} ({} bytes) belongs",
                bytes.len(),
                entry.name,
                entry.bytes
            )));
        }
        self.pad_to(self.data_start + entry.offset)?;
        self.emit(bytes)?;
        self.next += 1;
        Ok(())
    }

    /// Write the final padding, flush, and return the file's digests.
    pub fn finish(mut self) -> Result<PackageDigest, ModernError> {
        if self.next != self.entries.len() {
            return Err(ModernError::Invalid(format!(
                "package finished after {} of {} tensors",
                self.next,
                self.entries.len()
            )));
        }
        let end = align_up(self.written);
        self.pad_to(end)?;
        self.out
            .flush()
            .map_err(|e| ModernError::io("package flush", e))?;
        Ok(PackageDigest {
            bytes: self.written,
            sha256: hex_lower(&self.sha256.finalize()),
            blake3: hex_lower(self.blake3.finalize().as_bytes()),
        })
    }
}

/// Little-endian bytes of an i8 slice.
pub fn i8_bytes(values: &[i8]) -> Vec<u8> {
    values.iter().map(|&v| v as u8).collect()
}

/// Little-endian bytes of an i32 slice.
pub fn i32_bytes(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Little-endian bytes of an i64 slice.
pub fn i64_bytes(values: &[i64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Write the three tensors of one dyadic matrix.
pub fn write_matrix(
    writer: &mut PackageWriter,
    name: &str,
    matrix: &DyadicMatrix,
) -> Result<(), ModernError> {
    writer.write_tensor(&format!("{name}.q"), &i8_bytes(&matrix.q))?;
    writer.write_tensor(&format!("{name}.mu"), &i32_bytes(&matrix.mu))?;
    writer.write_tensor(&format!("{name}.k"), &matrix.k)
}

/// The parsed header of a package file.
#[derive(Debug, Clone)]
pub struct PackageHeader {
    pub value: Value,
    pub config: ModernConfig,
    pub entries: Vec<TensorEntry>,
    /// Bytes before the header padding: magic, length and header text.
    pub header_end: u64,
    pub data_start: u64,
}

fn read_header(reader: &mut impl Read, file_len: u64) -> Result<PackageHeader, ModernError> {
    let mut magic = [0u8; 8];
    reader
        .read_exact(&mut magic)
        .map_err(|e| ModernError::io("package magic", e))?;
    if magic != *MAGIC {
        return Err(ModernError::Invalid("not an ARC integer package".into()));
    }
    let mut len_bytes = [0u8; 8];
    reader
        .read_exact(&mut len_bytes)
        .map_err(|e| ModernError::io("package header length", e))?;
    let header_len = u64::from_le_bytes(len_bytes);
    if header_len > MAX_HEADER_BYTES || 16 + header_len > file_len {
        return Err(ModernError::Invalid(
            "package header length is invalid".into(),
        ));
    }
    let mut text = vec![0u8; header_len as usize];
    reader
        .read_exact(&mut text)
        .map_err(|e| ModernError::io("package header", e))?;
    let value: Value = serde_json::from_slice(&text)
        .map_err(|e| ModernError::Invalid(format!("package header JSON: {e}")))?;
    // The header must be the canonical encoding of itself.
    if canonical(&value)?.as_bytes() != text.as_slice() {
        return Err(ModernError::Invalid(
            "package header is not canonical JSON".into(),
        ));
    }
    if value.get("schema").and_then(Value::as_str) != Some(PACKAGE_SCHEMA) {
        return Err(ModernError::Invalid(
            "package schema is not arc.integer-package.v1".into(),
        ));
    }
    if value.get("profile").and_then(Value::as_str) != Some(PROFILE) {
        return Err(ModernError::Invalid(format!(
            "package profile is not {PROFILE}"
        )));
    }
    let model = value
        .get("model")
        .ok_or_else(|| ModernError::Invalid("package header has no model".into()))?;
    let config = config_from_json(model)?;
    let entries = layout(&config);
    // The header's tensor table must be exactly the canonical layout.
    let expected = header_json(
        &config,
        value.get("source").unwrap_or(&Value::Null),
        &entries,
    );
    if expected.get("tensors") != value.get("tensors") {
        return Err(ModernError::Invalid(
            "package tensor table differs from the canonical layout".into(),
        ));
    }
    let data_start = align_up(16 + header_len);
    let data_end = entries
        .last()
        .map(|e| align_up(e.offset + e.bytes))
        .unwrap_or(0);
    if data_start + data_end != file_len {
        return Err(ModernError::Invalid(format!(
            "package is {file_len} bytes; its layout needs {}",
            data_start + data_end
        )));
    }
    Ok(PackageHeader {
        value,
        config,
        entries,
        header_end: 16 + header_len,
        data_start,
    })
}

/// Read only the header of a package file.
pub fn read_package_header(path: &Path) -> Result<PackageHeader, ModernError> {
    let context = path.display().to_string();
    let file = File::open(path).map_err(|e| ModernError::io(&context, e))?;
    let len = file
        .metadata()
        .map_err(|e| ModernError::io(&context, e))?
        .len();
    read_header(&mut BufReader::new(file), len)
}

struct TensorReader<R: Read> {
    reader: R,
    position: u64,
    data_start: u64,
    entries: Vec<TensorEntry>,
    next: usize,
}

impl<R: Read> TensorReader<R> {
    fn read(&mut self, name: &str) -> Result<Vec<u8>, ModernError> {
        let entry = self
            .entries
            .get(self.next)
            .cloned()
            .ok_or_else(|| ModernError::Invalid(format!("package has no tensor {name}")))?;
        if entry.name != name {
            return Err(ModernError::Invalid(format!(
                "package tensor {} found where {name} was expected",
                entry.name
            )));
        }
        let start = self.data_start + entry.offset;
        if start < self.position || start - self.position >= ALIGN {
            return Err(ModernError::Invalid(format!(
                "tensor {name} offset is invalid"
            )));
        }
        let mut padding = vec![0u8; (start - self.position) as usize];
        self.reader
            .read_exact(&mut padding)
            .map_err(|e| ModernError::io("package padding", e))?;
        if padding.iter().any(|&b| b != 0) {
            return Err(ModernError::Invalid(format!(
                "nonzero padding before {name}"
            )));
        }
        let mut bytes = vec![0u8; entry.bytes as usize];
        self.reader
            .read_exact(&mut bytes)
            .map_err(|e| ModernError::io(&format!("package tensor {name}"), e))?;
        self.position = start + entry.bytes;
        self.next += 1;
        Ok(bytes)
    }

    fn i64s(&mut self, name: &str) -> Result<Vec<i64>, ModernError> {
        Ok(self
            .read(name)?
            .chunks_exact(8)
            .map(|c| i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
            .collect())
    }

    fn i32s(&mut self, name: &str) -> Result<Vec<i32>, ModernError> {
        Ok(self
            .read(name)?
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }

    fn matrix(
        &mut self,
        name: &str,
        rows: usize,
        cols: usize,
    ) -> Result<DyadicMatrix, ModernError> {
        let q: Vec<i8> = self
            .read(&format!("{name}.q"))?
            .into_iter()
            .map(|b| b as i8)
            .collect();
        let mu = self.i32s(&format!("{name}.mu"))?;
        let k = self.read(&format!("{name}.k"))?;
        let matrix = DyadicMatrix {
            rows,
            cols,
            q,
            mu,
            k,
        };
        matrix.validate(name)?;
        Ok(matrix)
    }
}

/// Load a package into memory, validating every value range.
pub fn load_package(path: &Path) -> Result<ModernModel, ModernError> {
    let context = path.display().to_string();
    let file = File::open(path).map_err(|e| ModernError::io(&context, e))?;
    let len = file
        .metadata()
        .map_err(|e| ModernError::io(&context, e))?
        .len();
    let mut reader = BufReader::with_capacity(8 << 20, file);
    let header = read_header(&mut reader, len)?;
    let config = header.config.clone();
    let mut tensors = TensorReader {
        reader,
        position: header.header_end,
        data_start: header.data_start,
        entries: header.entries,
        next: 0,
    };
    let (d, f, v) = (config.d_model, config.d_ff, config.vocab_size);
    let (dq, dkv) = (config.d_q(), config.d_kv());
    let embed = tensors.matrix("embed", v, d)?;
    let final_norm = tensors.i64s("final_norm")?;
    let rope_cos = tensors.i32s("rope.cos")?;
    let rope_sin = tensors.i32s("rope.sin")?;
    let mut layers = Vec::with_capacity(config.n_layers);
    for l in 0..config.n_layers {
        let p = format!("layers.{l}");
        let attn_norm = tensors.i64s(&format!("{p}.attn_norm"))?;
        let wq = tensors.matrix(&format!("{p}.wq"), dq, d)?;
        let wk = tensors.matrix(&format!("{p}.wk"), dkv, d)?;
        let wv = tensors.matrix(&format!("{p}.wv"), dkv, d)?;
        let wo = tensors.matrix(&format!("{p}.wo"), d, dq)?;
        let ffn_norm = tensors.i64s(&format!("{p}.ffn_norm"))?;
        let w_gate = tensors.matrix(&format!("{p}.w_gate"), f, d)?;
        let w_up = tensors.matrix(&format!("{p}.w_up"), f, d)?;
        let w_down = tensors.matrix(&format!("{p}.w_down"), d, f)?;
        layers.push(ModernLayer {
            attn_norm,
            wq,
            wk,
            wv,
            wo,
            ffn_norm,
            w_gate,
            w_up,
            w_down,
        });
    }
    let mut trailing = Vec::new();
    tensors
        .reader
        .read_to_end(&mut trailing)
        .map_err(|e| ModernError::io("package tail", e))?;
    if trailing.len() >= ALIGN as usize || trailing.iter().any(|&b| b != 0) {
        return Err(ModernError::Invalid(
            "package has unexpected trailing bytes".into(),
        ));
    }
    let model = ModernModel {
        config,
        embed,
        final_norm,
        rope_cos,
        rope_sin,
        layers,
    };
    model.validate()?;
    Ok(model)
}

/// Stream a file through SHA-256 and BLAKE3.
pub fn digest_file(path: &Path) -> Result<PackageDigest, ModernError> {
    let context = path.display().to_string();
    let mut file = File::open(path).map_err(|e| ModernError::io(&context, e))?;
    let mut sha = Sha256::new();
    let mut blake = blake3::Hasher::new();
    let mut buffer = vec![0u8; 8 << 20];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| ModernError::io(&context, e))?;
        if n == 0 {
            break;
        }
        sha.update(&buffer[..n]);
        blake.update(&buffer[..n]);
        total += n as u64;
    }
    Ok(PackageDigest {
        bytes: total,
        sha256: hex_lower(&sha.finalize()),
        blake3: hex_lower(blake.finalize().as_bytes()),
    })
}

/// Build the package manifest (spec §4.10) for a finished package.
pub fn build_manifest(
    header: &Value,
    digest: &PackageDigest,
    eos: &[u32],
    extras: &Value,
) -> Result<Value, ModernError> {
    let generation = json!({
        "semantics": super::GENERATION_RP64,
        "semantics_blake3": identity_blake3(super::GENERATION_RP64),
        "diagnostic_semantics": super::GENERATION_ARGMAX,
        "eos": eos,
        "bos_forwarded": false,
        "max_seq": header.pointer("/model/max_seq").cloned().unwrap_or(Value::Null),
    });
    let mut manifest = json!({
        "schema": MANIFEST_SCHEMA,
        "package": digest.to_json(),
        "profile": PROFILE,
        "profile_blake3": identity_blake3(PROFILE),
        "contract": "docs/protocol/integer-profile-hf-llama-dyadic-v1.md",
        "model": header.get("model").cloned().unwrap_or(Value::Null),
        "source": header.get("source").cloned().unwrap_or(Value::Null),
        "generation": generation,
        "tokenizer": extras.get("tokenizer").cloned().unwrap_or(Value::Null),
        "chat_template": extras.get("chat_template").cloned().unwrap_or(Value::Null),
    });
    let hash = crate::model_package::manifest_body_blake3(&manifest)
        .map_err(|e| ModernError::Invalid(format!("manifest hash: {e}")))?;
    manifest["manifest_blake3"] = Value::from(hash);
    Ok(manifest)
}

/// Canonical text of a manifest (what is written to disk).
pub fn manifest_text(manifest: &Value) -> Result<String, ModernError> {
    canonical(manifest)
}

/// Check a package file against a pinned manifest: the manifest must hash to
/// its own `manifest_blake3`, and the package's length, SHA-256, BLAKE3,
/// profile, model and source must be exactly what the manifest records.
pub fn verify_package(path: &Path, manifest_bytes: &[u8]) -> Result<PackageDigest, ModernError> {
    let manifest: Value = serde_json::from_slice(manifest_bytes)
        .map_err(|e| ModernError::Invalid(format!("manifest JSON: {e}")))?;
    if manifest.get("schema").and_then(Value::as_str) != Some(MANIFEST_SCHEMA) {
        return Err(ModernError::Invalid("manifest schema mismatch".into()));
    }
    let recorded = manifest
        .get("manifest_blake3")
        .and_then(Value::as_str)
        .ok_or_else(|| ModernError::Invalid("manifest has no manifest_blake3".into()))?;
    let recomputed = crate::model_package::manifest_body_blake3(&manifest)
        .map_err(|e| ModernError::Invalid(format!("manifest hash: {e}")))?;
    if recomputed != recorded {
        return Err(ModernError::Invalid(format!(
            "manifest_blake3 is {recorded}, but the manifest hashes to {recomputed}"
        )));
    }
    let header = read_package_header(path)?;
    for key in ["model", "source"] {
        if header.value.get(key) != manifest.get(key) {
            return Err(ModernError::Invalid(format!(
                "package header {key} differs from the manifest"
            )));
        }
    }
    if manifest.get("profile").and_then(Value::as_str) != Some(PROFILE) {
        return Err(ModernError::Invalid("manifest profile mismatch".into()));
    }
    let digest = digest_file(path)?;
    let pinned = manifest
        .get("package")
        .ok_or_else(|| ModernError::Invalid("manifest has no package".into()))?;
    if pinned.get("bytes").and_then(Value::as_u64) != Some(digest.bytes)
        || pinned.get("sha256").and_then(Value::as_str) != Some(digest.sha256.as_str())
        || pinned.get("blake3").and_then(Value::as_str) != Some(digest.blake3.as_str())
    {
        return Err(ModernError::Invalid(format!(
            "package digest {} ({} bytes) differs from the manifest's {}",
            digest.sha256, digest.bytes, pinned
        )));
    }
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_config() -> ModernConfig {
        ModernConfig {
            architecture: "smollm3".into(),
            n_layers: 2,
            d_model: 8,
            n_heads: 2,
            n_kv_heads: 1,
            d_head: 4,
            d_ff: 12,
            vocab_size: 5,
            max_seq: 3,
            rms_eps_q32: 4295,
            rope_theta: 10_000,
            rope_layers: vec![true, false],
        }
    }

    #[test]
    fn layout_is_aligned_ordered_and_complete() {
        let config = tiny_config();
        let entries = layout(&config);
        assert_eq!(entries.len(), 3 + 3 + 2 * (2 + 7 * 3));
        assert_eq!(entries[0].name, "embed.q");
        assert_eq!(entries[0].bytes, 40);
        assert_eq!(entries[1].offset, 64);
        assert!(entries.iter().all(|e| e.offset.is_multiple_of(ALIGN)));
        assert!(
            entries
                .windows(2)
                .all(|w| w[1].offset >= w[0].offset + w[0].bytes)
        );
        assert_eq!(entries.last().unwrap().name, "layers.1.w_down.k");
    }

    #[test]
    fn the_pinned_smollm3_manifest_is_self_consistent() {
        // Produced byte-identically by `arc-modern convert` on ubuntu x86-64,
        // windows x86-64, macOS arm64 and macOS x86-64 CI runners.
        let text =
            include_str!("../../../../docs/protocol/packages/smollm3-3b.integer-package.json");
        let manifest: Value = serde_json::from_str(text).unwrap();
        assert_eq!(manifest["schema"], MANIFEST_SCHEMA);
        assert_eq!(manifest["profile"], PROFILE);
        assert_eq!(manifest["profile_blake3"], identity_blake3(PROFILE));
        let recorded = manifest["manifest_blake3"].as_str().unwrap();
        assert_eq!(
            crate::model_package::manifest_body_blake3(&manifest).unwrap(),
            recorded
        );
        assert_eq!(
            recorded,
            "af388d01c3578c5f97238fd74aaa3d0d8194d29fc6fbd99f3aae226064659fa2"
        );
        assert_eq!(
            manifest["package"]["sha256"],
            "19c67496ee23fe5da0e12eb1f22cb17f6386c560071587b5b8dfa68731c0aa91"
        );
        assert_eq!(manifest["package"]["bytes"], 3_084_214_016u64);
        // The source it was converted from is exactly the pinned source.
        let source = super::super::convert::SourceManifest::parse(include_bytes!(
            "../../../../docs/protocol/packages/smollm3-3b.source.json"
        ))
        .unwrap();
        assert_eq!(manifest["source"], source.header_json());
        // The shape the converter derived: GQA 16:4, NoPE on every 4th layer.
        let config = config_from_json(&manifest["model"]).unwrap();
        assert_eq!(
            (
                config.n_layers,
                config.d_model,
                config.n_heads,
                config.n_kv_heads
            ),
            (36, 2048, 16, 4)
        );
        assert_eq!(
            (config.d_ff, config.vocab_size, config.max_seq),
            (11_008, 128_256, 4096)
        );
        assert_eq!((config.rms_eps_q32, config.rope_theta), (4295, 5_000_000));
        for (layer, &rope) in config.rope_layers.iter().enumerate() {
            assert_eq!(rope, !(layer + 1).is_multiple_of(4), "layer {layer}");
        }
        assert_eq!(manifest["generation"]["eos"], json!([128_012]));
        assert_eq!(
            manifest["generation"]["bos_forwarded"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn a_header_round_trips_through_the_canonical_encoding() {
        let config = tiny_config();
        let entries = layout(&config);
        let source = json!({"repo": "x/y", "revision": "r", "files": []});
        let header = header_json(&config, &source, &entries);
        let text = canonical(&header).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(config_from_json(&parsed["model"]).unwrap(), config);
        assert!(!text.contains(' '));
        assert!(text.starts_with("{\"model\":"));
    }
}
