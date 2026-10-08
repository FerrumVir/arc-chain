//! BF16 safetensors to stage packages (spec §4), integer-only, on the device.
//!
//! The converter verifies `config.json` and every safetensors shard present in
//! the source directory against the pinned source manifest (length and
//! SHA-256), then converts one layer range. A verifier that holds only a
//! slice of the model downloads only the shards that slice needs: shards that
//! are absent are not read, and any tensor the stage needs from them is
//! reported missing. The package header still names the whole pinned source,
//! so every stage of every layout descends from the same identity.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use serde_json::{Value, json};

use super::config::{ExpertFormat, MlaConfig, parse_hf_config};
use super::ops::{
    Q4_GROUP, bf16_to_q32, f32_to_q32, pack_q4, quantize_q4_group, quantize_router_row,
};
use super::package::{
    self, PackageDigest, SegmentDigest, StageSpec, StageWriter, i16_bytes, u16_bytes,
};
use super::precision::{Bits, Matrix, Precision, quantize_matrix};
use crate::modern::convert::{SourceManifest, bf16_to_q16, verify_source_file};
use crate::modern::package::{i32_bytes, i64_bytes};
use crate::modern::safetensors::SafetensorsFile;
use crate::modern::tables::rope_tables;
use crate::modern::tiktoken::TOKENIZER_IDENTITY;
use crate::modern::{ModernError, hex_lower};

/// How a source tensor is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// BF16 only.
    Bf16,
    /// The routing correction bias: BF16 or F32 (spec §4.4).
    Bias,
}

fn add(
    out: &mut BTreeMap<String, (Vec<usize>, SourceKind)>,
    name: String,
    shape: &[usize],
    kind: SourceKind,
) {
    out.insert(name, (shape.to_vec(), kind));
}

fn layer_source_tensors(
    c: &MlaConfig,
    layer: usize,
    out: &mut BTreeMap<String, (Vec<usize>, SourceKind)>,
) {
    use SourceKind::{Bf16, Bias};
    let p = format!("model.layers.{layer}");
    let d = c.d_model;
    add(out, format!("{p}.input_layernorm.weight"), &[d], Bf16);
    add(
        out,
        format!("{p}.post_attention_layernorm.weight"),
        &[d],
        Bf16,
    );
    if c.q_lora_rank == 0 {
        add(
            out,
            format!("{p}.self_attn.q_proj.weight"),
            &[c.d_q(), d],
            Bf16,
        );
    } else {
        let r = c.q_lora_rank;
        add(out, format!("{p}.self_attn.q_a_proj.weight"), &[r, d], Bf16);
        add(
            out,
            format!("{p}.self_attn.q_a_layernorm.weight"),
            &[r],
            Bf16,
        );
        add(
            out,
            format!("{p}.self_attn.q_b_proj.weight"),
            &[c.d_q(), r],
            Bf16,
        );
    }
    add(
        out,
        format!("{p}.self_attn.kv_a_proj_with_mqa.weight"),
        &[c.d_kv_a(), d],
        Bf16,
    );
    add(
        out,
        format!("{p}.self_attn.kv_a_layernorm.weight"),
        &[c.kv_lora_rank],
        Bf16,
    );
    add(
        out,
        format!("{p}.self_attn.kv_b_proj.weight"),
        &[c.n_heads * (c.qk_nope_dim + c.v_head_dim), c.kv_lora_rank],
        Bf16,
    );
    add(
        out,
        format!("{p}.self_attn.o_proj.weight"),
        &[d, c.d_attn_out()],
        Bf16,
    );
    if c.is_moe(layer) {
        let (e, fm, sf) = (c.n_routed_experts, c.moe_d_ff, c.shared_d_ff());
        add(out, format!("{p}.mlp.gate.weight"), &[e, d], Bf16);
        add(
            out,
            format!("{p}.mlp.gate.e_score_correction_bias"),
            &[e],
            Bias,
        );
        add(
            out,
            format!("{p}.mlp.shared_experts.gate_proj.weight"),
            &[sf, d],
            Bf16,
        );
        add(
            out,
            format!("{p}.mlp.shared_experts.up_proj.weight"),
            &[sf, d],
            Bf16,
        );
        add(
            out,
            format!("{p}.mlp.shared_experts.down_proj.weight"),
            &[d, sf],
            Bf16,
        );
        for expert in 0..e {
            let q = format!("{p}.mlp.experts.{expert}");
            add(out, format!("{q}.gate_proj.weight"), &[fm, d], Bf16);
            add(out, format!("{q}.up_proj.weight"), &[fm, d], Bf16);
            add(out, format!("{q}.down_proj.weight"), &[d, fm], Bf16);
        }
    } else {
        add(out, format!("{p}.mlp.gate_proj.weight"), &[c.d_ff, d], Bf16);
        add(out, format!("{p}.mlp.up_proj.weight"), &[c.d_ff, d], Bf16);
        add(out, format!("{p}.mlp.down_proj.weight"), &[d, c.d_ff], Bf16);
    }
}

/// The source tensors a stage reads (spec §4.1), with their shapes.
pub fn stage_source_tensors(
    c: &MlaConfig,
    stage: StageSpec,
) -> BTreeMap<String, (Vec<usize>, SourceKind)> {
    let mut out = BTreeMap::new();
    if stage.has_embed() {
        add(
            &mut out,
            "model.embed_tokens.weight".into(),
            &[c.vocab_size, c.d_model],
            SourceKind::Bf16,
        );
    }
    for layer in stage.layers() {
        layer_source_tensors(c, layer, &mut out);
    }
    if stage.has_head(c) {
        add(
            &mut out,
            "model.norm.weight".into(),
            &[c.d_model],
            SourceKind::Bf16,
        );
        add(
            &mut out,
            "lm_head.weight".into(),
            &[c.vocab_size, c.d_model],
            SourceKind::Bf16,
        );
    }
    out
}

/// Buffers the source carries that the profile ignores (spec §4.1).
fn ignored(name: &str) -> bool {
    name.starts_with("model.layers.") && name.ends_with(".self_attn.rotary_emb.inv_freq")
}

/// The tensors of the shards present on disk, by name.
struct SourceTensors {
    precision: Option<Precision>,
    shards: Vec<SafetensorsFile>,
    index: BTreeMap<String, usize>,
    dtypes: BTreeMap<String, String>,
}

impl SourceTensors {
    fn open(
        paths: &[PathBuf],
        expected: &BTreeMap<String, (Vec<usize>, SourceKind)>,
    ) -> Result<Self, ModernError> {
        let mut shards = Vec::new();
        let mut index = BTreeMap::new();
        let mut dtypes = BTreeMap::new();
        for path in paths {
            let shard = SafetensorsFile::open(path)?;
            for (name, info) in &shard.tensors {
                if ignored(name) {
                    continue;
                }
                let (shape, kind) = expected.get(name).ok_or_else(|| {
                    ModernError::Invalid(format!("unexpected source tensor {name}"))
                })?;
                let dtype_ok = match kind {
                    SourceKind::Bf16 => info.dtype == "BF16",
                    SourceKind::Bias => info.dtype == "BF16" || info.dtype == "F32",
                };
                if !dtype_ok || info.shape != *shape {
                    return Err(ModernError::Invalid(format!(
                        "source tensor {name} is {} {:?}; {:?} {shape:?} is required",
                        info.dtype, info.shape, kind
                    )));
                }
                if index.insert(name.clone(), shards.len()).is_some() {
                    return Err(ModernError::Invalid(format!(
                        "tensor {name} appears in two shards"
                    )));
                }
                dtypes.insert(name.clone(), info.dtype.clone());
            }
            shards.push(shard);
        }
        Ok(Self {
            precision: None,
            shards,
            index,
            dtypes,
        })
    }

    fn require<'a>(&self, names: impl Iterator<Item = &'a String>) -> Result<(), ModernError> {
        for name in names {
            if !self.index.contains_key(name) {
                return Err(ModernError::Invalid(format!(
                    "source tensor {name} is not in any shard present in the source directory"
                )));
            }
        }
        Ok(())
    }

    fn shard(&self, name: &str) -> Result<&SafetensorsFile, ModernError> {
        let i = self
            .index
            .get(name)
            .ok_or_else(|| ModernError::Invalid(format!("source tensor {name} is missing")))?;
        Ok(&self.shards[*i])
    }

    fn bf16(&self, name: &str) -> Result<Vec<u16>, ModernError> {
        self.shard(name)?.read_bf16(name)
    }

    fn raw(&self, name: &str) -> Result<Vec<u8>, ModernError> {
        let shard = self.shard(name)?;
        let info = &shard.tensors[name];
        let context = shard.path.display().to_string();
        let mut file = File::open(&shard.path).map_err(|e| ModernError::io(&context, e))?;
        file.seek(SeekFrom::Start(info.begin))
            .map_err(|e| ModernError::io(&context, e))?;
        let mut bytes = vec![0u8; (info.end - info.begin) as usize];
        file.read_exact(&mut bytes)
            .map_err(|e| ModernError::io(&context, e))?;
        Ok(bytes)
    }

    fn matrix(&self, name: &str, rows: usize, cols: usize) -> Result<Matrix, ModernError> {
        quantize_matrix(
            &self.bf16(name)?,
            rows,
            cols,
            self.precision
                .as_ref()
                .map_or(Bits::Int8, |p| p.source(name)),
        )
    }

    fn norm(&self, name: &str) -> Result<Vec<i64>, ModernError> {
        self.bf16(name)?.into_iter().map(bf16_to_q16).collect()
    }

    fn bias(&self, name: &str) -> Result<Vec<i64>, ModernError> {
        let bytes = self.raw(name)?;
        match self.dtypes.get(name).map(String::as_str) {
            Some("F32") => bytes
                .chunks_exact(4)
                .map(|c| f32_to_q32(u32::from_le_bytes([c[0], c[1], c[2], c[3]])))
                .collect(),
            _ => bytes
                .chunks_exact(2)
                .map(|c| bf16_to_q32(u16::from_le_bytes([c[0], c[1]])))
                .collect(),
        }
    }
}

fn write_dyadic(w: &mut StageWriter, name: &str, m: &Matrix) -> Result<(), ModernError> {
    w.write_tensor(&format!("{name}.q"), &m.q)?;
    w.write_tensor(&format!("{name}.mu"), &i32_bytes(&m.mu))?;
    w.write_tensor(&format!("{name}.k"), &m.k)
}

/// A stack of `count` matrices read one at a time and written as one
/// `[count, rows, cols]` dyadic tensor (spec §4.1).
fn write_stack(
    w: &mut StageWriter,
    t: &SourceTensors,
    name: &str,
    sources: &[String],
    rows: usize,
    cols: usize,
) -> Result<(), ModernError> {
    w.begin(&format!("{name}.q"))?;
    let mut mu = Vec::with_capacity(sources.len() * rows);
    let mut k = Vec::with_capacity(sources.len() * rows);
    for source in sources {
        let m = t.matrix(source, rows, cols)?;
        w.chunk(&m.q)?;
        mu.extend_from_slice(&m.mu);
        k.extend_from_slice(&m.k);
    }
    w.end()?;
    w.write_tensor(&format!("{name}.mu"), &i32_bytes(&mu))?;
    w.write_tensor(&format!("{name}.k"), &k)
}

/// A stack of `count` matrices quantised to INT4 with BF16 group-32 scales
/// (spec §13.3) and written as one `[count, rows, cols]` INT4 stack.
fn write_stack_q4(
    w: &mut StageWriter,
    t: &SourceTensors,
    name: &str,
    sources: &[String],
    rows: usize,
    cols: usize,
) -> Result<(), ModernError> {
    if !cols.is_multiple_of(Q4_GROUP) {
        return Err(ModernError::Invalid(format!(
            "{name}: {cols} inputs are not a multiple of the INT4 group"
        )));
    }
    w.begin(&format!("{name}.q4"))?;
    let mut scales: Vec<u16> = Vec::with_capacity(sources.len() * rows * cols / Q4_GROUP);
    for source in sources {
        let bits = t.bf16(source)?;
        if bits.len() != rows * cols {
            return Err(ModernError::Invalid(format!("{source}: shape")));
        }
        let quantised = bits
            .par_chunks(cols)
            .map(|row| -> Result<(Vec<u16>, Vec<u8>), ModernError> {
                let mut q = vec![0i8; cols];
                let mut row_scales = Vec::with_capacity(cols / Q4_GROUP);
                for (group, out) in row.chunks_exact(Q4_GROUP).zip(q.chunks_exact_mut(Q4_GROUP)) {
                    row_scales.push(quantize_q4_group(group, out)?);
                }
                Ok((row_scales, pack_q4(&q)))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut packed = Vec::with_capacity(rows * cols / 2);
        for (row_scales, row_packed) in quantised {
            scales.extend_from_slice(&row_scales);
            packed.extend_from_slice(&row_packed);
        }
        w.chunk(&packed)?;
    }
    w.end()?;
    w.write_tensor(&format!("{name}.s"), &u16_bytes(&scales))
}

fn convert_layer(
    w: &mut StageWriter,
    t: &SourceTensors,
    c: &MlaConfig,
    layer: usize,
) -> Result<(), ModernError> {
    let hf = format!("model.layers.{layer}");
    let p = format!("layers.{layer}");
    let d = c.d_model;
    w.write_tensor(
        &format!("{p}.attn_norm"),
        &i64_bytes(&t.norm(&format!("{hf}.input_layernorm.weight"))?),
    )?;
    if c.q_lora_rank == 0 {
        let m = t.matrix(&format!("{hf}.self_attn.q_proj.weight"), c.d_q(), d)?;
        write_dyadic(w, &format!("{p}.wq"), &m)?;
    } else {
        let r = c.q_lora_rank;
        let m = t.matrix(&format!("{hf}.self_attn.q_a_proj.weight"), r, d)?;
        write_dyadic(w, &format!("{p}.wq_a"), &m)?;
        w.write_tensor(
            &format!("{p}.q_a_norm"),
            &i64_bytes(&t.norm(&format!("{hf}.self_attn.q_a_layernorm.weight"))?),
        )?;
        let m = t.matrix(&format!("{hf}.self_attn.q_b_proj.weight"), c.d_q(), r)?;
        write_dyadic(w, &format!("{p}.wq_b"), &m)?;
    }
    let m = t.matrix(
        &format!("{hf}.self_attn.kv_a_proj_with_mqa.weight"),
        c.d_kv_a(),
        d,
    )?;
    write_dyadic(w, &format!("{p}.wkv_a"), &m)?;
    w.write_tensor(
        &format!("{p}.kv_a_norm"),
        &i64_bytes(&t.norm(&format!("{hf}.self_attn.kv_a_layernorm.weight"))?),
    )?;
    // Split kv_b_proj [H(N+Vh), C] into the absorbed key matrices (the
    // transpose of each head's key block) and the value matrices (spec §4.1).
    let (h, nope, vh, rank) = (c.n_heads, c.qk_nope_dim, c.v_head_dim, c.kv_lora_rank);
    let per_head = nope + vh;
    let b = t.bf16(&format!("{hf}.self_attn.kv_b_proj.weight"))?;
    let mut key_bits = vec![0u16; h * rank * nope];
    for j in 0..h {
        for r in 0..rank {
            let row = &mut key_bits[(j * rank + r) * nope..(j * rank + r + 1) * nope];
            for (t_index, slot) in row.iter_mut().enumerate() {
                *slot = b[(j * per_head + t_index) * rank + r];
            }
        }
    }
    write_dyadic(
        w,
        &format!("{p}.wk_b"),
        &quantize_matrix(
            &key_bits,
            h * rank,
            nope,
            c.precision.as_ref().map_or(Bits::Int8, |p| p.attention),
        )?,
    )?;
    drop(key_bits);
    let mut value_bits = vec![0u16; h * vh * rank];
    for j in 0..h {
        for t_index in 0..vh {
            let src = (j * per_head + nope + t_index) * rank;
            let dst = (j * vh + t_index) * rank;
            value_bits[dst..dst + rank].copy_from_slice(&b[src..src + rank]);
        }
    }
    write_dyadic(
        w,
        &format!("{p}.wv_b"),
        &quantize_matrix(
            &value_bits,
            h * vh,
            rank,
            c.precision.as_ref().map_or(Bits::Int8, |p| p.attention),
        )?,
    )?;
    drop(value_bits);
    drop(b);
    let m = t.matrix(&format!("{hf}.self_attn.o_proj.weight"), d, c.d_attn_out())?;
    write_dyadic(w, &format!("{p}.wo"), &m)?;
    w.write_tensor(
        &format!("{p}.ffn_norm"),
        &i64_bytes(&t.norm(&format!("{hf}.post_attention_layernorm.weight"))?),
    )?;
    if c.is_moe(layer) {
        let e = c.n_routed_experts;
        let gate = t.bf16(&format!("{hf}.mlp.gate.weight"))?;
        let mut router_q = vec![0i16; e * d];
        let mut router_k = vec![0u8; e];
        for (expert, k) in router_k.iter_mut().enumerate() {
            *k = quantize_router_row(
                &gate[expert * d..(expert + 1) * d],
                &mut router_q[expert * d..(expert + 1) * d],
            )?;
        }
        w.write_tensor(&format!("{p}.router.q"), &i16_bytes(&router_q))?;
        w.write_tensor(&format!("{p}.router.k"), &router_k)?;
        w.write_tensor(
            &format!("{p}.router_bias"),
            &i64_bytes(&t.bias(&format!("{hf}.mlp.gate.e_score_correction_bias"))?),
        )?;
        let sf = c.shared_d_ff();
        let s = format!("{hf}.mlp.shared_experts");
        write_dyadic(
            w,
            &format!("{p}.shared.w_gate"),
            &t.matrix(&format!("{s}.gate_proj.weight"), sf, d)?,
        )?;
        write_dyadic(
            w,
            &format!("{p}.shared.w_up"),
            &t.matrix(&format!("{s}.up_proj.weight"), sf, d)?,
        )?;
        write_dyadic(
            w,
            &format!("{p}.shared.w_down"),
            &t.matrix(&format!("{s}.down_proj.weight"), d, sf)?,
        )?;
        let fm = c.moe_d_ff;
        for (name, hf_name, rows, cols) in [
            ("w_gate", "gate_proj", fm, d),
            ("w_up", "up_proj", fm, d),
            ("w_down", "down_proj", d, fm),
        ] {
            let sources: Vec<String> = (0..e)
                .map(|expert| format!("{hf}.mlp.experts.{expert}.{hf_name}.weight"))
                .collect();
            let tensor = format!("{p}.experts.{name}");
            match c.expert_format {
                ExpertFormat::Int8Dyadic => write_stack(w, t, &tensor, &sources, rows, cols)?,
                ExpertFormat::Int4G32 => write_stack_q4(w, t, &tensor, &sources, rows, cols)?,
            }
        }
    } else {
        let f = c.d_ff;
        for (name, hf_name, rows, cols) in [
            ("w_gate", "gate_proj", f, d),
            ("w_up", "up_proj", f, d),
            ("w_down", "down_proj", d, f),
        ] {
            let m = t.matrix(&format!("{hf}.mlp.{hf_name}.weight"), rows, cols)?;
            write_dyadic(w, &format!("{p}.{name}"), &m)?;
        }
    }
    Ok(())
}

/// The tokenizer files a source manifest pins, as recorded in manifests.
pub fn tokenizer_json(source: &SourceManifest) -> Value {
    let file = |f: &crate::modern::convert::SourceFile| json!({"name": f.name, "bytes": f.bytes, "sha256": f.sha256});
    match &source.tokenizer {
        None => Value::Null,
        Some(model) => json!({
            "identity": TOKENIZER_IDENTITY,
            "model": file(model),
            "config": source.chat_template.as_ref().map(file),
        }),
    }
}

/// What a conversion produced.
#[derive(Debug, Clone)]
pub struct ConversionReport {
    pub digest: PackageDigest,
    pub segments: Vec<SegmentDigest>,
    pub header: Value,
    pub config: MlaConfig,
    pub stage: StageSpec,
    pub eos: Vec<u32>,
    /// The stage manifest, for a whole-model conversion only.
    pub manifest: Option<Value>,
    /// Safetensors shards that were present, verified and opened.
    pub shards_read: Vec<String>,
    pub seconds: f64,
}

impl ConversionReport {
    pub fn to_json(&self) -> Value {
        let segments: Vec<Value> = self.segments.iter().map(SegmentDigest::to_json).collect();
        json!({
            "package": self.digest.to_json(),
            "profile": self.config.profile(),
            "stage": self.stage.to_json(),
            "segments": segments,
            "model_root": self.manifest.as_ref().and_then(|m| m.get("model_root").cloned()),
            "manifest_blake3": self.manifest.as_ref().and_then(|m| m.get("manifest_blake3").cloned()),
            "shards_read": self.shards_read,
            "seconds": self.seconds,
        })
    }
}

/// Convert layer range `stage` (the whole model when `None`) of the pinned
/// BF16 source in `dir` to the stage package `out`, with the routed experts
/// in `experts` format.
pub fn convert_stage(
    dir: &Path,
    source: &SourceManifest,
    stage: Option<StageSpec>,
    experts: ExpertFormat,
    out: &Path,
) -> Result<ConversionReport, ModernError> {
    convert_stage_with_precision(dir, source, stage, experts, None, out)
}

/// Convert with explicit class precision, binding the choice to the model identity.
pub fn convert_stage_with_precision(
    dir: &Path,
    source: &SourceManifest,
    stage: Option<StageSpec>,
    experts: ExpertFormat,
    precision: Option<Precision>,
    out: &Path,
) -> Result<ConversionReport, ModernError> {
    let start = Instant::now();
    let config_entry = source
        .files
        .iter()
        .find(|f| f.name == "config.json")
        .ok_or_else(|| ModernError::Invalid("source manifest does not pin config.json".into()))?;
    let config_path = verify_source_file(dir, config_entry)?;
    let config_bytes =
        std::fs::read(&config_path).map_err(|e| ModernError::io("config.json", e))?;
    let hf = parse_hf_config(&config_bytes, source.max_seq)?;
    let mut c = hf.config.clone();
    c.expert_format = experts;
    c.precision = precision;
    c.validate()?;
    let full = StageSpec::full(&c);
    let stage = stage.unwrap_or(full);
    stage.validate(&c)?;
    let mut paths = Vec::new();
    let mut shards_read = Vec::new();
    for file in source
        .files
        .iter()
        .filter(|f| f.name.ends_with(".safetensors"))
    {
        if dir.join(&file.name).exists() {
            paths.push(verify_source_file(dir, file)?);
            shards_read.push(file.name.clone());
        }
    }
    let expected = stage_source_tensors(&c, full);
    let mut tensors = SourceTensors::open(&paths, &expected)?;
    tensors.precision = c.precision.clone();
    tensors.require(stage_source_tensors(&c, stage).keys())?;
    let (cos, sin) = rope_tables(c.rope_theta, c.qk_rope_dim, c.max_seq)?;
    let source_json = source.header_json();
    let entries = package::layout(&c, stage);
    let header = package::header_json(&c, &source_json, stage, &entries);
    let mut w = StageWriter::create(out, &header, entries)?;
    w.write_tensor("rope.cos", &i32_bytes(&cos))?;
    w.write_tensor("rope.sin", &i32_bytes(&sin))?;
    if stage.has_embed() {
        let m = tensors.matrix("model.embed_tokens.weight", c.vocab_size, c.d_model)?;
        write_dyadic(&mut w, "embed", &m)?;
    }
    for layer in stage.layers() {
        convert_layer(&mut w, &tensors, &c, layer)?;
    }
    if stage.has_head(&c) {
        w.write_tensor(
            "final_norm",
            &i64_bytes(&tensors.norm("model.norm.weight")?),
        )?;
        let m = tensors.matrix("lm_head.weight", c.vocab_size, c.d_model)?;
        write_dyadic(&mut w, "lm_head", &m)?;
    }
    let (digest, segments) = w.finish()?;
    let manifest = if stage == full {
        Some(package::build_manifest(
            &c,
            &source_json,
            &segments,
            Some(&digest),
            &hf.eos,
            &tokenizer_json(source),
        )?)
    } else {
        None
    };
    Ok(ConversionReport {
        digest,
        segments,
        header,
        config: c,
        stage,
        eos: hf.eos,
        manifest,
        shards_read,
        seconds: start.elapsed().as_secs_f64(),
    })
}

/// BLAKE3 (hex) of a byte string, for reports.
pub fn blake3_hex(bytes: &[u8]) -> String {
    hex_lower(blake3::hash(bytes).as_bytes())
}
