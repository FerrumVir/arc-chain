//! BF16 safetensors to the integer package (spec §4), on the device.
//!
//! Every step reads BF16 bit patterns as exact rationals and uses integer
//! arithmetic only, so identical source bytes give identical package bytes on
//! every OS and CPU. Rows are quantised in parallel; each row is independent,
//! so the thread count cannot change a byte.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::arith::DyadicMatrix;
use super::model::ModernConfig;
use super::package::{self, PackageDigest, PackageWriter};
use super::safetensors::SafetensorsFile;
use super::{ModernError, hex_lower, tables};

/// Source manifest schema (`docs/protocol/packages/*.source.json`).
pub const SOURCE_SCHEMA: &str = "arc.hf-source.v1";

/// One pinned source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

/// A pinned Hugging Face source: repository, revision and file digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceManifest {
    pub repo: String,
    pub revision: String,
    pub max_seq: usize,
    /// `config.json` and the safetensors shards, in conversion order.
    pub files: Vec<SourceFile>,
    pub tokenizer: Option<SourceFile>,
    pub chat_template: Option<SourceFile>,
}

fn source_file(value: &Value, what: &str) -> Result<SourceFile, ModernError> {
    let bad = || ModernError::Invalid(format!("source manifest {what} entry is malformed"));
    let sha256 = value
        .get("sha256")
        .and_then(Value::as_str)
        .ok_or_else(bad)?
        .to_string();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(bad)?
        .to_string();
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return Err(ModernError::Invalid(format!(
            "source file name {name:?} is not a plain file name"
        )));
    }
    Ok(SourceFile {
        name,
        bytes: value.get("bytes").and_then(Value::as_u64).ok_or_else(bad)?,
        sha256: sha256.to_ascii_lowercase(),
    })
}

impl SourceManifest {
    /// Parse a source manifest.
    pub fn parse(bytes: &[u8]) -> Result<Self, ModernError> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|e| ModernError::Invalid(format!("source manifest JSON: {e}")))?;
        if value.get("schema").and_then(Value::as_str) != Some(SOURCE_SCHEMA) {
            return Err(ModernError::Invalid(format!(
                "source manifest schema is not {SOURCE_SCHEMA}"
            )));
        }
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| ModernError::Invalid(format!("source manifest has no {key}")))
        };
        let files = value
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| ModernError::Invalid("source manifest has no files".into()))?
            .iter()
            .map(|f| source_file(f, "files"))
            .collect::<Result<Vec<_>, _>>()?;
        let optional = |key: &str| value.get(key).map(|v| source_file(v, key)).transpose();
        let max_seq = value
            .get("max_seq")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| ModernError::Invalid("source manifest has no max_seq".into()))?;
        Ok(Self {
            repo: text("repo")?,
            revision: text("revision")?,
            max_seq,
            files,
            tokenizer: optional("tokenizer")?,
            chat_template: optional("chat_template")?,
        })
    }

    /// Read and parse a source manifest file.
    pub fn read(path: &Path) -> Result<Self, ModernError> {
        let bytes =
            std::fs::read(path).map_err(|e| ModernError::io(&path.display().to_string(), e))?;
        Self::parse(&bytes)
    }

    /// The `source` object recorded in the package header (spec §4.9).
    pub fn header_json(&self) -> Value {
        let files: Vec<Value> = self
            .files
            .iter()
            .map(|f| json!({"name": f.name, "bytes": f.bytes, "sha256": f.sha256}))
            .collect();
        json!({"repo": self.repo, "revision": self.revision, "files": files})
    }
}

/// Stream `path` through SHA-256, returning `(bytes, hex digest)`.
pub fn sha256_file(path: &Path) -> Result<(u64, String), ModernError> {
    let context = path.display().to_string();
    let mut file = File::open(path).map_err(|e| ModernError::io(&context, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8 << 20];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| ModernError::io(&context, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        total += n as u64;
    }
    Ok((total, hex_lower(&hasher.finalize())))
}

/// Refuse unless `dir/name` has exactly the pinned length and SHA-256.
pub fn verify_source_file(dir: &Path, file: &SourceFile) -> Result<PathBuf, ModernError> {
    let path = dir.join(&file.name);
    let (bytes, sha256) = sha256_file(&path)?;
    if bytes != file.bytes || sha256 != file.sha256 {
        return Err(ModernError::Invalid(format!(
            "{} is {bytes} bytes with SHA-256 {sha256}; the pinned source is {} bytes with SHA-256 {}",
            path.display(),
            file.bytes,
            file.sha256
        )));
    }
    Ok(path)
}

/// Exact parts of a BF16 value: `(negative, M, e)` with value `(-1)^s * M * 2^e`.
pub fn bf16_parts(bits: u16) -> Result<(bool, u32, i32), ModernError> {
    let negative = bits >> 15 == 1;
    let exponent = (bits >> 7) & 0xFF;
    let mantissa = u32::from(bits & 0x7F);
    match exponent {
        0xFF => Err(ModernError::Invalid("BF16 infinity or NaN".into())),
        0 => Ok((negative, mantissa, -133)),
        e => Ok((negative, 128 + mantissa, i32::from(e) - 134)),
    }
}

/// Quantise one row to INT8 with a dyadic scale (spec §4.2).
pub fn quantize_row(bits: &[u16], q: &mut [i8]) -> Result<(i32, u8), ModernError> {
    if bits.len() != q.len() {
        return Err(ModernError::Invalid("quantize_row length mismatch".into()));
    }
    let mut max_magnitude = 0u16;
    for &b in bits {
        if (b >> 7) & 0xFF == 0xFF {
            return Err(ModernError::Invalid(
                "BF16 infinity or NaN in a weight row".into(),
            ));
        }
        max_magnitude = max_magnitude.max(b & 0x7FFF);
    }
    if max_magnitude == 0 {
        q.fill(0);
        return Ok((0, 16));
    }
    let (_, m_a, e_a) = bf16_parts(max_magnitude)?;
    let m_a = u64::from(m_a);
    for (slot, &b) in q.iter_mut().zip(bits) {
        let (negative, m_j, e_j) = bf16_parts(b)?;
        if m_j == 0 {
            *slot = 0;
            continue;
        }
        let d = e_a - e_j;
        if d < 0 {
            return Err(ModernError::Invalid(
                "BF16 row maximum is inconsistent".into(),
            ));
        }
        if d > 60 {
            *slot = 0;
            continue;
        }
        let denominator = u128::from(m_a) << d;
        let numerator = 2 * 127 * u128::from(m_j) + denominator;
        let magnitude = (numerator / (2 * denominator)) as i8;
        *slot = if negative { -magnitude } else { magnitude };
    }
    let target = 127u64 << 30;
    let mut t: i32 = 0;
    while (m_a << t) < target {
        t += 1;
    }
    let mut mu = (2 * (m_a << t) + 127) / 254;
    if mu == 1 << 31 {
        mu = 1 << 30;
        t -= 1;
    }
    let k = t - e_a;
    if !(16..=62).contains(&k) {
        return Err(ModernError::Invalid(format!(
            "row scale shift {k} is outside [16, 62] (row maximum too large or too small)"
        )));
    }
    Ok((mu as i32, k as u8))
}

/// Quantise a `[rows, cols]` BF16 matrix (spec §4.2).
pub fn quantize_matrix(
    bits: &[u16],
    rows: usize,
    cols: usize,
) -> Result<DyadicMatrix, ModernError> {
    if rows == 0 || cols == 0 || bits.len() != rows * cols {
        return Err(ModernError::Invalid("quantize_matrix shape".into()));
    }
    let mut q = vec![0i8; rows * cols];
    let mut mu = vec![0i32; rows];
    let mut k = vec![0u8; rows];
    q.par_chunks_mut(cols)
        .zip(bits.par_chunks(cols))
        .zip(mu.par_iter_mut().zip(k.par_iter_mut()))
        .try_for_each(|((q_row, bits_row), (mu_slot, k_slot))| {
            let (m, s) = quantize_row(bits_row, q_row)?;
            *mu_slot = m;
            *k_slot = s;
            Ok::<(), ModernError>(())
        })?;
    Ok(DyadicMatrix {
        rows,
        cols,
        q,
        mu,
        k,
    })
}

/// `round_half_away(v * 2^16)` of a BF16 value, exactly (spec §4.3).
pub fn bf16_to_q16(bits: u16) -> Result<i64, ModernError> {
    let (negative, m, e) = bf16_parts(bits)?;
    let m = i128::from(m);
    let shift = e + 16;
    let magnitude = if shift >= 0 {
        if shift > 54 {
            return Err(ModernError::Domain("norm gain beyond 2^62".into()));
        }
        m << shift
    } else {
        let c = -shift;
        (2 * m + (1i128 << c)) >> (c + 1)
    };
    Ok((if negative { -magnitude } else { magnitude }) as i64)
}

/// `round_half_away(eps * 2^32)` (spec §4.4).
pub fn eps_q32(eps: f64) -> Result<i64, ModernError> {
    let scaled = (eps * 4_294_967_296.0).round();
    if !(1.0..1.0e12).contains(&scaled) {
        return Err(ModernError::Invalid(format!(
            "rms_norm_eps {eps} is outside the supported range"
        )));
    }
    Ok(scaled as i64)
}

/// Parsed `config.json`: the model shape and the EOS ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfConfig {
    pub config: ModernConfig,
    pub eos: Vec<u32>,
}

fn config_flag_absent_or(value: &Value, key: &str, allowed: &Value) -> bool {
    match value.get(key) {
        None | Some(Value::Null) => true,
        Some(v) => v == allowed,
    }
}

/// Parse and check a Hugging Face `config.json` (spec §4.6).
pub fn parse_hf_config(bytes: &[u8], max_seq: usize) -> Result<HfConfig, ModernError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| ModernError::Invalid(format!("config.json: {e}")))?;
    let bad = |what: &str| ModernError::Invalid(format!("config.json: {what}"));
    let size = |key: &str| -> Result<usize, ModernError> {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| bad(&format!("{key} must be a positive integer")))
    };
    let architecture = value
        .get("model_type")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("model_type"))?
        .to_string();
    if !matches!(architecture.as_str(), "smollm3" | "llama") {
        return Err(bad(&format!("model_type {architecture} is not supported")));
    }
    if value.get("hidden_act").and_then(Value::as_str) != Some("silu") {
        return Err(bad("hidden_act must be silu"));
    }
    if value.get("tie_word_embeddings").and_then(Value::as_bool) != Some(true) {
        return Err(bad("only tied embeddings are supported"));
    }
    let checks = [
        ("rope_scaling", Value::Null),
        ("sliding_window", Value::Null),
        ("attention_bias", Value::Bool(false)),
        ("mlp_bias", Value::Bool(false)),
        ("use_sliding_window", Value::Bool(false)),
    ];
    for (key, allowed) in &checks {
        if !config_flag_absent_or(&value, key, allowed) {
            return Err(bad(&format!("{key} must be absent or {allowed}")));
        }
    }
    let d_model = size("hidden_size")?;
    let n_heads = size("num_attention_heads")?;
    let n_kv_heads = size("num_key_value_heads")?;
    let n_layers = size("num_hidden_layers")?;
    let d_head = match value.get("head_dim") {
        None | Some(Value::Null) => d_model / n_heads.max(1),
        Some(_) => size("head_dim")?,
    };
    if n_heads * d_head != d_model {
        return Err(bad("hidden_size must equal num_attention_heads * head_dim"));
    }
    let theta = value
        .get("rope_theta")
        .and_then(Value::as_f64)
        .ok_or_else(|| bad("rope_theta"))?;
    if !(2.0..=9_007_199_254_740_992.0).contains(&theta) || theta.fract().abs() > 0.0 {
        return Err(bad("rope_theta must be an integer in [2, 2^53]"));
    }
    let rope_layers = match value.get("no_rope_layers") {
        None | Some(Value::Null) => vec![true; n_layers],
        Some(list) => list
            .as_array()
            .ok_or_else(|| bad("no_rope_layers"))?
            .iter()
            .map(|v| match v.as_u64() {
                Some(1) => Ok(true),
                Some(0) => Ok(false),
                _ => Err(bad("no_rope_layers entries must be 0 or 1")),
            })
            .collect::<Result<Vec<bool>, _>>()?,
    };
    let eps = value
        .get("rms_norm_eps")
        .and_then(Value::as_f64)
        .ok_or_else(|| bad("rms_norm_eps"))?;
    let eos = match value.get("eos_token_id") {
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|v| v.as_u64().and_then(|id| u32::try_from(id).ok()))
            .collect::<Option<Vec<u32>>>()
            .ok_or_else(|| bad("eos_token_id"))?,
        Some(v) => vec![
            v.as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .ok_or_else(|| bad("eos_token_id"))?,
        ],
        None => return Err(bad("eos_token_id")),
    };
    let config = ModernConfig {
        architecture,
        n_layers,
        d_model,
        n_heads,
        n_kv_heads,
        d_head,
        d_ff: size("intermediate_size")?,
        vocab_size: size("vocab_size")?,
        max_seq,
        rms_eps_q32: eps_q32(eps)?,
        rope_theta: theta as u64,
        rope_layers,
    };
    config.validate()?;
    Ok(HfConfig { config, eos })
}

/// Hugging Face tensor name for each package matrix of layer `l`.
fn layer_sources(l: usize) -> [(&'static str, String); 7] {
    let p = format!("model.layers.{l}");
    [
        ("wq", format!("{p}.self_attn.q_proj.weight")),
        ("wk", format!("{p}.self_attn.k_proj.weight")),
        ("wv", format!("{p}.self_attn.v_proj.weight")),
        ("wo", format!("{p}.self_attn.o_proj.weight")),
        ("w_gate", format!("{p}.mlp.gate_proj.weight")),
        ("w_up", format!("{p}.mlp.up_proj.weight")),
        ("w_down", format!("{p}.mlp.down_proj.weight")),
    ]
}

/// Every source tensor with its required shape, in package order.
fn expected_tensors(c: &ModernConfig) -> Vec<(String, Vec<usize>)> {
    let mut out = vec![
        (
            "model.embed_tokens.weight".to_string(),
            vec![c.vocab_size, c.d_model],
        ),
        ("model.norm.weight".to_string(), vec![c.d_model]),
    ];
    for l in 0..c.n_layers {
        let p = format!("model.layers.{l}");
        out.push((format!("{p}.input_layernorm.weight"), vec![c.d_model]));
        out.push((
            format!("{p}.post_attention_layernorm.weight"),
            vec![c.d_model],
        ));
        for (name, source) in layer_sources(l) {
            let shape = match name {
                "wq" => vec![c.d_q(), c.d_model],
                "wk" | "wv" => vec![c.d_kv(), c.d_model],
                "wo" => vec![c.d_model, c.d_q()],
                "w_gate" | "w_up" => vec![c.d_ff, c.d_model],
                _ => vec![c.d_model, c.d_ff],
            };
            out.push((source, shape));
        }
    }
    out
}

/// All tensors of all shards, by name.
struct SourceTensors {
    shards: Vec<SafetensorsFile>,
    index: BTreeMap<String, usize>,
}

impl SourceTensors {
    fn open(paths: &[PathBuf], config: &ModernConfig) -> Result<Self, ModernError> {
        let mut shards = Vec::new();
        let mut index = BTreeMap::new();
        for path in paths {
            let shard = SafetensorsFile::open(path)?;
            for name in shard.tensors.keys() {
                if index.insert(name.clone(), shards.len()).is_some() {
                    return Err(ModernError::Invalid(format!(
                        "tensor {name} appears in two shards"
                    )));
                }
            }
            shards.push(shard);
        }
        let expected = expected_tensors(config);
        if index.len() != expected.len() {
            return Err(ModernError::Invalid(format!(
                "source has {} tensors; the profile needs exactly {}",
                index.len(),
                expected.len()
            )));
        }
        for (name, shape) in &expected {
            let shard = index
                .get(name)
                .ok_or_else(|| ModernError::Invalid(format!("source tensor {name} is missing")))?;
            let info = &shards[*shard].tensors[name];
            if info.dtype != "BF16" || info.shape != *shape {
                return Err(ModernError::Invalid(format!(
                    "source tensor {name} is {} {:?}; BF16 {shape:?} is required",
                    info.dtype, info.shape
                )));
            }
        }
        Ok(Self { shards, index })
    }

    fn read(&self, name: &str) -> Result<Vec<u16>, ModernError> {
        let shard = self
            .index
            .get(name)
            .ok_or_else(|| ModernError::Invalid(format!("source tensor {name} is missing")))?;
        self.shards[*shard].read_bf16(name)
    }

    fn norm(&self, name: &str) -> Result<Vec<i64>, ModernError> {
        self.read(name)?.into_iter().map(bf16_to_q16).collect()
    }
}

/// What a conversion produced.
#[derive(Debug, Clone)]
pub struct ConversionReport {
    pub digest: PackageDigest,
    pub header: Value,
    pub manifest: Value,
    pub config: ModernConfig,
    pub seconds: f64,
}

/// Convert the pinned BF16 source in `dir` to the package at `out`.
///
/// Every file named in the source manifest is checked for its exact length
/// and SHA-256 first; nothing is converted from an unverified byte.
pub fn convert(
    dir: &Path,
    source: &SourceManifest,
    out: &Path,
) -> Result<ConversionReport, ModernError> {
    let start = std::time::Instant::now();
    let mut verified = Vec::new();
    for file in &source.files {
        verified.push((file.name.clone(), verify_source_file(dir, file)?));
    }
    let config_path = verified
        .iter()
        .find(|(name, _)| name == "config.json")
        .map(|(_, path)| path.clone())
        .ok_or_else(|| ModernError::Invalid("source manifest does not pin config.json".into()))?;
    let config_bytes =
        std::fs::read(&config_path).map_err(|e| ModernError::io("config.json", e))?;
    let hf = parse_hf_config(&config_bytes, source.max_seq)?;
    let config = hf.config.clone();
    let shard_paths: Vec<PathBuf> = verified
        .iter()
        .filter(|(name, _)| name.ends_with(".safetensors"))
        .map(|(_, path)| path.clone())
        .collect();
    let tensors = SourceTensors::open(&shard_paths, &config)?;
    let (rope_cos, rope_sin) =
        tables::rope_tables(config.rope_theta, config.d_head, config.max_seq)?;
    let entries = package::layout(&config);
    let header = package::header_json(&config, &source.header_json(), &entries);
    let mut writer = PackageWriter::create(out, &header, entries)?;
    let matrix = |name: &str, rows: usize, cols: usize| -> Result<DyadicMatrix, ModernError> {
        quantize_matrix(&tensors.read(name)?, rows, cols)
    };
    let embed = matrix(
        "model.embed_tokens.weight",
        config.vocab_size,
        config.d_model,
    )?;
    package::write_matrix(&mut writer, "embed", &embed)?;
    drop(embed);
    writer.write_tensor(
        "final_norm",
        &package::i64_bytes(&tensors.norm("model.norm.weight")?),
    )?;
    writer.write_tensor("rope.cos", &package::i32_bytes(&rope_cos))?;
    writer.write_tensor("rope.sin", &package::i32_bytes(&rope_sin))?;
    for l in 0..config.n_layers {
        let p = format!("layers.{l}");
        let hf = format!("model.layers.{l}");
        writer.write_tensor(
            &format!("{p}.attn_norm"),
            &package::i64_bytes(&tensors.norm(&format!("{hf}.input_layernorm.weight"))?),
        )?;
        for (name, source_name) in layer_sources(l) {
            if name == "w_gate" {
                writer.write_tensor(
                    &format!("{p}.ffn_norm"),
                    &package::i64_bytes(
                        &tensors.norm(&format!("{hf}.post_attention_layernorm.weight"))?,
                    ),
                )?;
            }
            let (rows, cols) = match name {
                "wq" => (config.d_q(), config.d_model),
                "wk" | "wv" => (config.d_kv(), config.d_model),
                "wo" => (config.d_model, config.d_q()),
                "w_gate" | "w_up" => (config.d_ff, config.d_model),
                _ => (config.d_model, config.d_ff),
            };
            let m = matrix(&source_name, rows, cols)?;
            package::write_matrix(&mut writer, &format!("{p}.{name}"), &m)?;
        }
    }
    let digest = writer.finish()?;
    let extras = json!({
        "tokenizer": source.tokenizer.as_ref().map(|t| json!({
            "name": t.name,
            "bytes": t.bytes,
            "sha256": t.sha256,
            "identity": super::TOKENIZER_IDENTITY,
        })),
        "chat_template": source.chat_template.as_ref().map(|t| json!({
            "name": t.name,
            "bytes": t.bytes,
            "sha256": t.sha256,
            "rendering": "single user turn, no tools, add_generation_prompt, explicit today date, default /no_think",
        })),
    });
    let manifest = package::build_manifest(&header, &digest, &hf.eos, &extras)?;
    Ok(ConversionReport {
        digest,
        header,
        manifest,
        config,
        seconds: start.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bf16(value: f32) -> u16 {
        (value.to_bits() >> 16) as u16
    }

    #[test]
    fn bf16_parts_decode_normals_subnormals_and_refuse_nan() {
        assert_eq!(bf16_parts(0x3F80).unwrap(), (false, 128, -7)); // 1.0 = 128 * 2^-7
        assert_eq!(bf16_parts(0xBF80).unwrap(), (true, 128, -7));
        assert_eq!(bf16_parts(0x0001).unwrap(), (false, 1, -133));
        assert_eq!(bf16_parts(0x8000).unwrap(), (true, 0, -133));
        assert!(bf16_parts(0x7F80).is_err());
        assert!(bf16_parts(0xFFC1).is_err());
    }

    #[test]
    fn rows_quantise_exactly_with_dyadic_scales() {
        // Row [1.0, -0.5, 0.25, 0]: A = 1, q = rha(127 v).
        let bits = [bf16(1.0), bf16(-0.5), bf16(0.25), 0];
        let mut q = [0i8; 4];
        let (mu, k) = quantize_row(&bits, &mut q).unwrap();
        // 127 * 0.5 = 63.5 -> 64 (half away), 127 * 0.25 = 31.75 -> 32.
        assert_eq!(q, [127, -64, 32, 0]);
        // 1/127 = mu * 2^-k with mu in [2^30, 2^31).
        assert!((1 << 30..1i64 << 31).contains(&i64::from(mu)));
        let exact = 1.0 / 127.0;
        let got = f64::from(mu) / 2f64.powi(i32::from(k));
        assert!(((got - exact) / exact).abs() < 1e-9);
        assert_eq!((mu, k), (1_082_196_484, 37));
        // All-zero rows have the zero scale.
        let mut q = [5i8; 3];
        assert_eq!(quantize_row(&[0, 0x8000, 0], &mut q).unwrap(), (0, 16));
        assert_eq!(q, [0, 0, 0]);
        // NaN anywhere in a row is refused.
        assert!(quantize_row(&[bf16(1.0), 0x7FC0], &mut [0i8; 2]).is_err());
    }

    #[test]
    fn norm_gains_round_half_away_from_zero() {
        assert_eq!(bf16_to_q16(bf16(1.0)).unwrap(), 65_536);
        assert_eq!(bf16_to_q16(bf16(-2.5)).unwrap(), -163_840);
        // 2^-17 = 0.5 Q16 units -> 1 (away from zero); -2^-17 -> -1.
        assert_eq!(bf16_to_q16(bf16(2f32.powi(-17))).unwrap(), 1);
        assert_eq!(bf16_to_q16(bf16(-(2f32.powi(-17)))).unwrap(), -1);
        assert_eq!(bf16_to_q16(bf16(2f32.powi(-18))).unwrap(), 0);
        assert_eq!(eps_q32(1e-6).unwrap(), 4295);
    }

    #[test]
    fn smollm3_config_is_parsed_and_unsupported_ones_refused() {
        let config = br#"{"architectures":["SmolLM3ForCausalLM"],"attention_bias":false,
            "bos_token_id":128000,"eos_token_id":128012,"hidden_act":"silu","hidden_size":2048,
            "intermediate_size":11008,"max_position_embeddings":65536,"mlp_bias":false,
            "model_type":"smollm3","no_rope_layers":[1,1,1,0],"num_attention_heads":16,
            "num_hidden_layers":4,"num_key_value_heads":4,"rms_norm_eps":1e-06,
            "rope_scaling":null,"rope_theta":5000000.0,"sliding_window":null,
            "tie_word_embeddings":true,"use_sliding_window":false,"vocab_size":128256}"#;
        let hf = parse_hf_config(config, 4096).unwrap();
        assert_eq!(hf.eos, vec![128_012]);
        let c = hf.config;
        assert_eq!(
            (c.d_head, c.d_kv(), c.rms_eps_q32, c.rope_theta),
            (128, 512, 4295, 5_000_000)
        );
        assert_eq!(c.rope_layers, vec![true, true, true, false]);
        let text = std::str::from_utf8(config).unwrap();
        for (from, to) in [
            (
                "\"rope_scaling\":null",
                "\"rope_scaling\":{\"type\":\"yarn\"}",
            ),
            ("\"hidden_act\":\"silu\"", "\"hidden_act\":\"gelu\""),
            (
                "\"tie_word_embeddings\":true",
                "\"tie_word_embeddings\":false",
            ),
            ("\"rope_theta\":5000000.0", "\"rope_theta\":10000.5"),
            ("\"no_rope_layers\":[1,1,1,0]", "\"no_rope_layers\":[1,1,0]"),
        ] {
            let changed = text.replace(from, to);
            assert_ne!(changed, text);
            assert!(parse_hf_config(changed.as_bytes(), 4096).is_err(), "{to}");
        }
    }

    #[test]
    fn source_manifests_pin_plain_file_names() {
        let ok = br#"{"schema":"arc.hf-source.v1","repo":"a/b","revision":"r","max_seq":64,
            "files":[{"name":"config.json","bytes":3,"sha256":"aa00000000000000000000000000000000000000000000000000000000000000"}]}"#;
        let m = SourceManifest::parse(ok).unwrap();
        assert_eq!(m.files.len(), 1);
        assert!(m.tokenizer.is_none());
        let bad = std::str::from_utf8(ok)
            .unwrap()
            .replace("config.json", "../config.json");
        assert!(SourceManifest::parse(bad.as_bytes()).is_err());
    }
}
