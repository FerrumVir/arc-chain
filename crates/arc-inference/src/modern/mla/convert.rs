//! BF16 safetensors to stage packages (spec §4), integer-only, on the device.
//!
//! The converter verifies `config.json` and every safetensors shard present in
//! the source directory against the pinned source manifest (length and
//! SHA-256), then converts one layer range. A verifier that holds only a
//! slice of the model downloads only the shards that slice needs: shards that
//! are absent are not read, and any tensor the stage needs from them is
//! reported missing. The package header still names the whole pinned source,
//! so every stage of every layout descends from the same identity.
//!
//! A checkpoint stored as Kimi-K2.6 stores its weights (a multimodal wrapper,
//! `language_model.` names, routed experts pre-quantised as compressed-tensors
//! INT4) is read too (spec §14.1-§14.2); its experts are repacked into the §13
//! layout, never requantised. The layer, embedding and head conversions write
//! through [`TensorSink`], so [`super::slices`] produces the same bytes.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use serde_json::{Value, json};

use super::config::{ExpertFormat, MlaConfig, WeightsSource, parse_hf_weights_config};
use super::ops::{
    Q4_GROUP, bf16_to_q32, f32_to_q32, pack_q4, quantize_q4_group, quantize_router_row,
};
use super::package::{
    self, PackageDigest, SegmentDigest, StageSpec, StageWriter, TensorSink, i16_bytes, u16_bytes,
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
    /// INT4 values packed eight to a little-endian int32 word, offset by 8
    /// (compressed-tensors `weight_packed`, spec §14.2).
    Packed,
    /// The `[rows, cols]` of a packed matrix (`weight_shape`, I32).
    PackedShape,
}

impl SourceKind {
    fn accepts(self, dtype: &str) -> bool {
        match self {
            SourceKind::Bf16 => dtype == "BF16",
            SourceKind::Bias => dtype == "BF16" || dtype == "F32",
            SourceKind::Packed | SourceKind::PackedShape => dtype == "I32",
        }
    }
}

fn add(
    out: &mut BTreeMap<String, (Vec<usize>, SourceKind)>,
    name: String,
    shape: &[usize],
    kind: SourceKind,
) {
    out.insert(name, (shape.to_vec(), kind));
}

/// The source tensors of one routed expert projection `[rows, cols]` named
/// `base` (`….experts.e.gate_proj`): BF16, or pre-quantised INT4 (spec §14.2).
fn add_expert(
    out: &mut BTreeMap<String, (Vec<usize>, SourceKind)>,
    base: &str,
    rows: usize,
    cols: usize,
    packed: bool,
) {
    if packed {
        add(
            out,
            format!("{base}.weight_packed"),
            &[rows, cols / 8],
            SourceKind::Packed,
        );
        add(
            out,
            format!("{base}.weight_scale"),
            &[rows, cols / Q4_GROUP],
            SourceKind::Bf16,
        );
        add(
            out,
            format!("{base}.weight_shape"),
            &[2],
            SourceKind::PackedShape,
        );
    } else {
        add(
            out,
            format!("{base}.weight"),
            &[rows, cols],
            SourceKind::Bf16,
        );
    }
}

pub(crate) fn layer_source_tensors(
    c: &MlaConfig,
    src: &WeightsSource,
    layer: usize,
    out: &mut BTreeMap<String, (Vec<usize>, SourceKind)>,
) {
    use SourceKind::{Bf16, Bias};
    let p = format!("{}model.layers.{layer}", src.prefix);
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
            add_expert(out, &format!("{q}.gate_proj"), fm, d, src.packed_experts);
            add_expert(out, &format!("{q}.up_proj"), fm, d, src.packed_experts);
            add_expert(out, &format!("{q}.down_proj"), d, fm, src.packed_experts);
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
    stage_source_tensors_in(c, &WeightsSource::default(), stage)
}

/// [`stage_source_tensors`] for a checkpoint stored as `src` describes
/// (spec §14.1).
pub fn stage_source_tensors_in(
    c: &MlaConfig,
    src: &WeightsSource,
    stage: StageSpec,
) -> BTreeMap<String, (Vec<usize>, SourceKind)> {
    let mut out = BTreeMap::new();
    let p = &src.prefix;
    if stage.has_embed() {
        add(
            &mut out,
            format!("{p}model.embed_tokens.weight"),
            &[c.vocab_size, c.d_model],
            SourceKind::Bf16,
        );
    }
    for layer in stage.layers() {
        layer_source_tensors(c, src, layer, &mut out);
    }
    if stage.has_head(c) {
        add(
            &mut out,
            format!("{p}model.norm.weight"),
            &[c.d_model],
            SourceKind::Bf16,
        );
        add(
            &mut out,
            format!("{p}lm_head.weight"),
            &[c.vocab_size, c.d_model],
            SourceKind::Bf16,
        );
    }
    out
}

/// Tensors the source carries that the profile ignores: rotary buffers
/// (spec §4.1) and, in a multimodal checkpoint, the vision tower and its
/// projector (spec §14.1).
fn ignored(name: &str, src: &WeightsSource) -> bool {
    let rest = name.strip_prefix(src.prefix.as_str()).unwrap_or(name);
    (rest.starts_with("model.layers.") && rest.ends_with(".self_attn.rotary_emb.inv_freq"))
        || (!src.prefix.is_empty()
            && (name.starts_with("vision_tower.") || name.starts_with("mm_projector.")))
}

/// The tensors of the shards present on disk, by name.
pub(crate) struct SourceTensors {
    pub(crate) precision: Option<Precision>,
    shards: Vec<SafetensorsFile>,
    index: BTreeMap<String, usize>,
    dtypes: BTreeMap<String, String>,
    src: WeightsSource,
}

impl SourceTensors {
    pub(crate) fn open(
        paths: &[PathBuf],
        expected: &BTreeMap<String, (Vec<usize>, SourceKind)>,
        src: &WeightsSource,
    ) -> Result<Self, ModernError> {
        let mut shards = Vec::new();
        let mut index = BTreeMap::new();
        let mut dtypes = BTreeMap::new();
        for path in paths {
            let shard = SafetensorsFile::open(path)?;
            for (name, info) in &shard.tensors {
                if ignored(name, src) {
                    continue;
                }
                let (shape, kind) = expected.get(name).ok_or_else(|| {
                    ModernError::Invalid(format!("unexpected source tensor {name}"))
                })?;
                if !kind.accepts(&info.dtype) || info.shape != *shape {
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
            src: src.clone(),
        })
    }

    /// The source name of a language-model tensor (`model.…`, `lm_head.…`).
    fn hf(&self, name: &str) -> String {
        format!("{}{name}", self.src.prefix)
    }

    pub(crate) fn require<'a>(
        &self,
        names: impl Iterator<Item = &'a String>,
    ) -> Result<(), ModernError> {
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
            self.precision.as_ref().map_or(Bits::Int8, |p| {
                p.source(name.strip_prefix(&self.src.prefix).unwrap_or(name))
            }),
        )
        .map_err(|e| ModernError::Invalid(format!("source tensor {name}: {e}")))
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

fn write_dyadic<W: TensorSink>(w: &mut W, name: &str, m: &Matrix) -> Result<(), ModernError> {
    w.write_tensor(&format!("{name}.q"), &m.q)?;
    w.write_tensor(&format!("{name}.mu"), &i32_bytes(&m.mu))?;
    w.write_tensor(&format!("{name}.k"), &m.k)
}

/// A stack of `count` matrices read one at a time and written as one
/// `[count, rows, cols]` dyadic tensor (spec §4.1).
fn write_stack<W: TensorSink>(
    w: &mut W,
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
fn write_stack_q4<W: TensorSink>(
    w: &mut W,
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

/// A stack of `count` pre-quantised INT4 matrices (`base.weight_packed`,
/// `.weight_scale`, `.weight_shape`) repacked losslessly into the §13 layout
/// (spec §14.2): the values and scales are the checkpoint's own.
fn write_stack_q4_packed<W: TensorSink>(
    w: &mut W,
    t: &SourceTensors,
    name: &str,
    bases: &[String],
    rows: usize,
    cols: usize,
) -> Result<(), ModernError> {
    if !cols.is_multiple_of(Q4_GROUP) {
        return Err(ModernError::Invalid(format!(
            "{name}: {cols} inputs are not a multiple of the INT4 group"
        )));
    }
    w.begin(&format!("{name}.q4"))?;
    for base in bases {
        let shape = t.raw(&format!("{base}.weight_shape"))?;
        let shape: Vec<i64> = shape
            .chunks_exact(4)
            .map(|c| i64::from(i32::from_le_bytes([c[0], c[1], c[2], c[3]])))
            .collect();
        if shape != [rows as i64, cols as i64] {
            return Err(ModernError::Invalid(format!(
                "{base}.weight_shape is {shape:?}; [{rows}, {cols}] is required"
            )));
        }
        let mut bytes = t.raw(&format!("{base}.weight_packed"))?;
        repack_int4_words(&mut bytes);
        w.chunk(&bytes)?;
    }
    w.end()?;
    w.begin(&format!("{name}.s"))?;
    for base in bases {
        let scales = t.bf16(&format!("{base}.weight_scale"))?;
        check_q4_scales(&scales, base)?;
        w.chunk(&u16_bytes(&scales))?;
    }
    w.end()
}

/// compressed-tensors packs value `j` of a row into bits `4(j mod 8)` of
/// little-endian int32 word `j / 8`, as the nibble `v + 8`. In the word's
/// bytes that is value `j` in byte `j / 2`, low nibble for even `j`: the
/// §13.1 order. The §13.1 nibble is `v` in two's complement, `(v + 8) XOR 8`,
/// so every byte is XORed with 0x88 (spec §14.2).
pub fn repack_int4_words(bytes: &mut [u8]) {
    for b in bytes {
        *b ^= 0x88;
    }
}

/// A §13.1 group scale: a BF16 with the sign bit clear, finite.
pub fn check_q4_scales(scales: &[u16], what: &str) -> Result<(), ModernError> {
    match scales
        .iter()
        .find(|&&s| s >> 15 != 0 || (s >> 7) & 0xFF == 0xFF)
    {
        Some(bad) => Err(ModernError::Invalid(format!(
            "{what}: group scale 0x{bad:04x} is negative, infinite or NaN"
        ))),
        None => Ok(()),
    }
}

/// Convert one layer's source tensors into its layout tensors (spec §4.1).
pub(crate) fn convert_layer<W: TensorSink>(
    w: &mut W,
    t: &SourceTensors,
    c: &MlaConfig,
    layer: usize,
) -> Result<(), ModernError> {
    let hf = t.hf(&format!("model.layers.{layer}"));
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
        )
        .map_err(|e| {
            ModernError::Invalid(format!(
                "{hf}.self_attn.kv_b_proj.weight: transposed key rows [head,rank,nope]: {e}"
            ))
        })?,
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
        )
        .map_err(|e| {
            ModernError::Invalid(format!(
                "{hf}.self_attn.kv_b_proj.weight: value rows [head,value,rank]: {e}"
            ))
        })?,
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
            let bases: Vec<String> = (0..e)
                .map(|expert| format!("{hf}.mlp.experts.{expert}.{hf_name}"))
                .collect();
            let tensor = format!("{p}.experts.{name}");
            if t.src.packed_experts {
                if c.expert_format != ExpertFormat::Int4G32 {
                    return Err(ModernError::Invalid(
                        "pre-quantised INT4 experts are stored as i4g32, never requantised".into(),
                    ));
                }
                write_stack_q4_packed(w, t, &tensor, &bases, rows, cols)?;
                continue;
            }
            let sources: Vec<String> = bases.iter().map(|b| format!("{b}.weight")).collect();
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

/// The `embed` segment (spec §4.1).
pub(crate) fn convert_embed<W: TensorSink>(
    w: &mut W,
    t: &SourceTensors,
    c: &MlaConfig,
) -> Result<(), ModernError> {
    let m = t.matrix(&t.hf("model.embed_tokens.weight"), c.vocab_size, c.d_model)?;
    write_dyadic(w, "embed", &m)
}

/// The `head` segment (spec §4.1).
pub(crate) fn convert_head<W: TensorSink>(
    w: &mut W,
    t: &SourceTensors,
    c: &MlaConfig,
) -> Result<(), ModernError> {
    w.write_tensor(
        "final_norm",
        &i64_bytes(&t.norm(&t.hf("model.norm.weight"))?),
    )?;
    let m = t.matrix(&t.hf("lm_head.weight"), c.vocab_size, c.d_model)?;
    write_dyadic(w, "lm_head", &m)
}

/// The pinned safetensors shards present in `dir`, each verified (length and
/// SHA-256), in source-manifest order; returns their paths and names.
pub(crate) fn present_shards(
    dir: &Path,
    source: &SourceManifest,
) -> Result<(Vec<PathBuf>, Vec<String>), ModernError> {
    let mut paths = Vec::new();
    let mut names = Vec::new();
    for file in source
        .files
        .iter()
        .filter(|f| f.name.ends_with(".safetensors"))
    {
        if dir.join(&file.name).exists() {
            paths.push(verify_source_file(dir, file)?);
            names.push(file.name.clone());
        }
    }
    Ok((paths, names))
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
    let hf = parse_hf_weights_config(&config_bytes, source.max_seq)?;
    if !hf.pending.is_empty() {
        return Err(ModernError::Invalid(format!(
            "config.json: not supported in a stage package: {} (arc-mla slice converts the weights)",
            hf.pending.join("; ")
        )));
    }
    let mut c = hf.config.clone();
    // Refuse before creating a package, even for a dense-only stage. The
    // source's packed-expert contract must not silently become an i8 model.
    if hf.source.packed_experts && experts != ExpertFormat::Int4G32 {
        return Err(ModernError::Invalid(
            "pre-quantised INT4 experts require --experts i4g32, never requantised".into(),
        ));
    }
    c.expert_format = experts;
    c.precision = precision;
    c.validate()?;
    let full = StageSpec::full(&c);
    let stage = stage.unwrap_or(full);
    stage.validate(&c)?;
    let (paths, shards_read) = present_shards(dir, source)?;
    let expected = stage_source_tensors_in(&c, &hf.source, full);
    let mut tensors = SourceTensors::open(&paths, &expected, &hf.source)?;
    tensors.precision = c.precision.clone();
    tensors.require(stage_source_tensors_in(&c, &hf.source, stage).keys())?;
    let (cos, sin) = rope_tables(c.rope_theta, c.qk_rope_dim, c.max_seq)?;
    let source_json = source.header_json();
    let entries = package::layout(&c, stage);
    let header = package::header_json(&c, &source_json, stage, &entries);
    let mut w = StageWriter::create(out, &header, entries)?;
    w.write_tensor("rope.cos", &i32_bytes(&cos))?;
    w.write_tensor("rope.sin", &i32_bytes(&sin))?;
    if stage.has_embed() {
        convert_embed(&mut w, &tensors, &c)?;
    }
    for layer in stage.layers() {
        convert_layer(&mut w, &tensors, &c, layer)?;
    }
    if stage.has_head(&c) {
        convert_head(&mut w, &tensors, &c)?;
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
