//! The stage model: a loaded stage package, its forward pass (spec §5.7),
//! generation (spec §7) and teacher-forced stage runs (spec §6.4).
//!
//! A stage holds layers `[a, b)` of the model; the whole model is the stage
//! `[0, L)`. INT8 weights stay in the package bytes (memory-mapped from disk,
//! so a 16 GB package does not need 16 GB of heap) and only scales, norms,
//! routers and tables are copied into memory. Every value the forward pass
//! produces is a pure function of the package bytes and the inputs.

use std::collections::BTreeMap;
use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use rayon::prelude::*;

use super::boundary::activation_hash;
use super::config::{ExpertFormat, MlaConfig};
use super::ops::{
    LatentCache, Q4_GROUP, Q4View, QView, combine, gated_ffn, gated_ffn_q4_rows, mla_attend,
    rope_interleaved, router_logits, routing_weights, select_experts, selection_keys,
};
use super::package::{self, StageHeader, StageSpec};
use super::precision::{I16Weights, Schedule, holds_int16_min};
use crate::modern::ModernError;
use crate::modern::arith::{self, ACTIVATION_LIMIT, Selection, add_residual, rms_norm};
use crate::modern::model::GenerationRequest;

/// The bytes of a stage package: memory-mapped from disk, or owned (tests).
pub enum PackageBytes {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl PackageBytes {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            PackageBytes::Mapped(map) => &map[..],
            PackageBytes::Owned(bytes) => bytes,
        }
    }
}

fn as_i8(bytes: &[u8]) -> &[i8] {
    // SAFETY: `i8` and `u8` have the same size and alignment and every bit
    // pattern is valid for both, so the slice is reinterpreted, not copied.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<i8>(), bytes.len()) }
}

fn invalid(what: impl Into<String>) -> ModernError {
    ModernError::Invalid(what.into())
}

/// One dyadic matrix, or a stack of `count` of them, whose INT8 weights stay
/// in the package bytes. INT16 matrices also stay mapped as little-endian bytes.
#[derive(Debug, Clone)]
struct MatRef {
    rows: usize,
    cols: usize,
    q: Range<usize>,
    wide: bool,
    mu: Vec<i32>,
    k: Vec<u8>,
}

impl MatRef {
    /// Stack element `index`, read from the package bytes the loader checked
    /// (every caller passes its own `StageModel`'s bytes).
    fn view<'a>(&'a self, bytes: &'a [u8], index: usize) -> QView<'a> {
        let size = self.rows * self.cols * if self.wide { 2 } else { 1 };
        let start = self.q.start + index * size;
        QView {
            rows: self.rows,
            cols: self.cols,
            q: if self.wide {
                &[]
            } else {
                as_i8(&bytes[start..start + size])
            },
            // `Loader::mat` refused any -32768 in the whole stack range `q`
            // before this `MatRef` existed; elements start at even offsets.
            q16: self
                .wide
                .then(|| I16Weights::admitted(&bytes[start..start + size])),
            mu: &self.mu[index * self.rows..(index + 1) * self.rows],
            k: &self.k[index * self.rows..(index + 1) * self.rows],
        }
    }
}

/// A stack of INT4 group-32 matrices whose packed values stay in the
/// package bytes (spec §13).
#[derive(Debug, Clone)]
struct Q4Ref {
    rows: usize,
    cols: usize,
    q4: Range<usize>,
    scales: Vec<u16>,
}

impl Q4Ref {
    fn view<'a>(&'a self, bytes: &'a [u8], index: usize) -> Q4View<'a> {
        let size = self.rows * self.cols / 2;
        let groups = self.rows * self.cols / Q4_GROUP;
        let start = self.q4.start + index * size;
        Q4View {
            rows: self.rows,
            cols: self.cols,
            q4: &bytes[start..start + size],
            scales: &self.scales[index * groups..(index + 1) * groups],
        }
    }
}

/// gate, up and down stacks of the routed experts.
enum ExpertStacks {
    Int8([MatRef; 3]),
    Int4([Q4Ref; 3]),
}

enum QueryProjection {
    Direct(MatRef),
    Lora {
        a: MatRef,
        norm: Vec<i64>,
        b: MatRef,
    },
}

struct MoeWeights {
    router_q: Vec<i16>,
    router_k: Vec<u8>,
    bias: Vec<i64>,
    /// gate, up, down of the shared experts.
    shared: [MatRef; 3],
    /// gate, up, down stacks of the routed experts.
    experts: ExpertStacks,
}

enum FfnWeights {
    Dense(Box<[MatRef; 3]>),
    Moe(Box<MoeWeights>),
}

struct LayerWeights {
    attn_norm: Vec<i64>,
    query: QueryProjection,
    wkv_a: MatRef,
    kv_a_norm: Vec<i64>,
    wk_b: MatRef,
    wv_b: MatRef,
    wo: MatRef,
    ffn_norm: Vec<i64>,
    ffn: FfnWeights,
}

struct HeadWeights {
    final_norm: Vec<i64>,
    lm_head: MatRef,
}

/// Reads typed tensors out of the package bytes and checks value ranges.
struct Loader<'a> {
    data: &'a [u8],
    header: &'a StageHeader,
}

impl Loader<'_> {
    fn bytes(&self, name: &str) -> Result<&[u8], ModernError> {
        let entry = self.header.entry(name)?;
        Ok(&self.data[self.header.range(entry)])
    }

    fn i64s(&self, name: &str) -> Result<Vec<i64>, ModernError> {
        let values: Vec<i64> = self
            .bytes(name)?
            .chunks_exact(8)
            .map(|c| i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
            .collect();
        if values
            .iter()
            .any(|v| u128::from(v.unsigned_abs()) > ACTIVATION_LIMIT)
        {
            return Err(invalid(format!("{name} holds a value beyond 2^62")));
        }
        Ok(values)
    }

    fn i32s(&self, name: &str) -> Result<Vec<i32>, ModernError> {
        Ok(self
            .bytes(name)?
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }

    fn u16s(&self, name: &str) -> Result<Vec<u16>, ModernError> {
        Ok(self
            .bytes(name)?
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect())
    }

    /// An INT4 group-32 stack; refuses negative, infinite or NaN scales
    /// (spec §13.1).
    fn q4(&self, name: &str, count: usize, rows: usize, cols: usize) -> Result<Q4Ref, ModernError> {
        let entry = self.header.entry(&format!("{name}.q4"))?;
        let q4 = self.header.range(entry);
        let scales = self.u16s(&format!("{name}.s"))?;
        if !cols.is_multiple_of(Q4_GROUP)
            || q4.len() != count * rows * cols / 2
            || scales.len() != count * rows * cols / Q4_GROUP
        {
            return Err(invalid(format!("{name}: inconsistent INT4 stack shape")));
        }
        if let Some(bad) = scales
            .iter()
            .find(|&&s| s >> 15 == 1 || (s >> 7) & 0xFF == 0xFF)
        {
            return Err(invalid(format!(
                "{name}: group scale {bad:#06x} is negative, infinite or NaN"
            )));
        }
        Ok(Q4Ref {
            rows,
            cols,
            q4,
            scales,
        })
    }

    fn i16s(&self, name: &str) -> Result<Vec<i16>, ModernError> {
        Ok(self
            .bytes(name)?
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect())
    }

    /// A dyadic matrix stack; refuses -128 weights and invalid scales
    /// (dyadic v1 §3).
    fn mat(
        &self,
        name: &str,
        count: usize,
        rows: usize,
        cols: usize,
    ) -> Result<MatRef, ModernError> {
        let entry = self.header.entry(&format!("{name}.q"))?;
        let q = self.header.range(entry);
        let mu = self.i32s(&format!("{name}.mu"))?;
        let k = self.bytes(&format!("{name}.k"))?.to_vec();
        let wide = entry.dtype == package::Dtype::I16;
        let width = if wide { 2 } else { 1 };
        if q.len() != count * rows * cols * width
            || mu.len() != count * rows
            || k.len() != count * rows
        {
            return Err(invalid(format!("{name}: inconsistent matrix shape")));
        }
        let weights = &self.data[q.clone()];
        // The one-time -32768 check of every INT16 matrix: projections rely on
        // it through `I16Weights` and never rescan.
        if if wide {
            holds_int16_min(weights)
        } else {
            weights.par_chunks(1 << 20).any(|c| c.contains(&0x80))
        } {
            return Err(invalid(format!(
                "{name}: weight minimum is not a profile value"
            )));
        }
        for (row, (&m, &s)) in mu.iter().zip(&k).enumerate() {
            let zero = m == 0 && s == 16;
            let normal = m >= 1 << 30 && (16..=62).contains(&s);
            if !(zero || normal) {
                return Err(invalid(format!(
                    "{name}: row {row} has an invalid dyadic scale ({m}, {s})"
                )));
            }
            if m == 0
                && weights[row * cols * width..(row + 1) * cols * width]
                    .iter()
                    .any(|&b| b != 0)
            {
                return Err(invalid(format!(
                    "{name}: row {row} has a zero scale but nonzero weights"
                )));
            }
        }
        Ok(MatRef {
            rows,
            cols,
            q,
            wide,
            mu,
            k,
        })
    }

    fn layer(&self, c: &MlaConfig, layer: usize) -> Result<LayerWeights, ModernError> {
        let p = format!("layers.{layer}");
        let d = c.d_model;
        let query = if c.q_lora_rank == 0 {
            QueryProjection::Direct(self.mat(&format!("{p}.wq"), 1, c.d_q(), d)?)
        } else {
            QueryProjection::Lora {
                a: self.mat(&format!("{p}.wq_a"), 1, c.q_lora_rank, d)?,
                norm: self.i64s(&format!("{p}.q_a_norm"))?,
                b: self.mat(&format!("{p}.wq_b"), 1, c.d_q(), c.q_lora_rank)?,
            }
        };
        let (h, rank) = (c.n_heads, c.kv_lora_rank);
        let ffn = if c.is_moe(layer) {
            let (e, fm, sf) = (c.n_routed_experts, c.moe_d_ff, c.shared_d_ff());
            let router_q = self.i16s(&format!("{p}.router.q"))?;
            let router_k = self.bytes(&format!("{p}.router.k"))?.to_vec();
            if router_q.contains(&i16::MIN) || router_k.iter().any(|&k| k > 62) {
                return Err(invalid(format!("{p}: router row outside the profile")));
            }
            FfnWeights::Moe(Box::new(MoeWeights {
                router_q,
                router_k,
                bias: self.i64s(&format!("{p}.router_bias"))?,
                shared: [
                    self.mat(&format!("{p}.shared.w_gate"), 1, sf, d)?,
                    self.mat(&format!("{p}.shared.w_up"), 1, sf, d)?,
                    self.mat(&format!("{p}.shared.w_down"), 1, d, sf)?,
                ],
                experts: match c.expert_format {
                    ExpertFormat::Int8Dyadic => ExpertStacks::Int8([
                        self.mat(&format!("{p}.experts.w_gate"), e, fm, d)?,
                        self.mat(&format!("{p}.experts.w_up"), e, fm, d)?,
                        self.mat(&format!("{p}.experts.w_down"), e, d, fm)?,
                    ]),
                    ExpertFormat::Int4G32 => ExpertStacks::Int4([
                        self.q4(&format!("{p}.experts.w_gate"), e, fm, d)?,
                        self.q4(&format!("{p}.experts.w_up"), e, fm, d)?,
                        self.q4(&format!("{p}.experts.w_down"), e, d, fm)?,
                    ]),
                },
            }))
        } else {
            FfnWeights::Dense(Box::new([
                self.mat(&format!("{p}.w_gate"), 1, c.d_ff, d)?,
                self.mat(&format!("{p}.w_up"), 1, c.d_ff, d)?,
                self.mat(&format!("{p}.w_down"), 1, d, c.d_ff)?,
            ]))
        };
        Ok(LayerWeights {
            attn_norm: self.i64s(&format!("{p}.attn_norm"))?,
            query,
            wkv_a: self.mat(&format!("{p}.wkv_a"), 1, c.d_kv_a(), d)?,
            kv_a_norm: self.i64s(&format!("{p}.kv_a_norm"))?,
            wk_b: self.mat(&format!("{p}.wk_b"), h, rank, c.qk_nope_dim)?,
            wv_b: self.mat(&format!("{p}.wv_b"), h, c.v_head_dim, rank)?,
            wo: self.mat(&format!("{p}.wo"), 1, d, c.d_attn_out())?,
            ffn_norm: self.i64s(&format!("{p}.ffn_norm"))?,
            ffn,
        })
    }
}

/// The per-layer MLA cache of a stage: latents and RoPE keys as i32.
#[derive(Debug, Clone)]
pub struct StageCache {
    latent: Vec<Vec<i32>>,
    rope_keys: Vec<Vec<i32>>,
    positions: usize,
}

impl StageCache {
    /// Number of cached positions.
    pub fn positions(&self) -> usize {
        self.positions
    }

    fn push(&mut self, layer: usize, latent: &[i64], key: &[i64]) -> Result<(), ModernError> {
        let narrow = |x: &i64| {
            i32::try_from(*x)
                .map_err(|_| ModernError::Domain("KV value outside i32 (|v| >= 2^31)".into()))
        };
        let latent = latent.iter().map(narrow).collect::<Result<Vec<i32>, _>>()?;
        let key = key.iter().map(narrow).collect::<Result<Vec<i32>, _>>()?;
        self.latent[layer].extend_from_slice(&latent);
        self.rope_keys[layer].extend_from_slice(&key);
        Ok(())
    }

    /// BLAKE3 over every cached latent then RoPE key, layer by layer (LE i32).
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for (latent, keys) in self.latent.iter().zip(&self.rope_keys) {
            for x in latent.iter().chain(keys) {
                hasher.update(&x.to_le_bytes());
            }
        }
        *hasher.finalize().as_bytes()
    }
}

/// The input of one stage at one position.
#[derive(Debug, Clone, Copy)]
pub enum StageInput<'a> {
    /// A token id (the first stage only).
    Token(u32),
    /// The boundary vector entering the stage (every later stage).
    Hidden(&'a [i64]),
}

/// A loaded stage package, executing the package's layer range or a
/// sub-range of it.
pub struct StageModel {
    pub header: StageHeader,
    /// The executed layer range (the package's own range unless narrowed).
    stage: StageSpec,
    bytes: PackageBytes,
    rope_cos: Vec<i32>,
    rope_sin: Vec<i32>,
    embed: Option<MatRef>,
    layers: Vec<LayerWeights>,
    head: Option<HeadWeights>,
    /// Tests force the head schedule of INT16 layers with this.
    #[cfg(test)]
    forced_head_schedule: Option<Schedule>,
}

/// Run `per_head` once per head on `schedule`: head `j` writes only the
/// `width`-value block `j` of `out` and reads shared, read-only inputs, so
/// the schedule cannot change a value. On failure the error of the lowest
/// failing head is returned, as the serial loop returns it.
pub(crate) fn for_each_head<F>(
    out: &mut [i64],
    width: usize,
    schedule: Schedule,
    per_head: F,
) -> Result<(), ModernError>
where
    F: Fn(usize, &mut [i64]) -> Result<(), ModernError> + Sync + Send,
{
    match schedule {
        Schedule::Serial => out
            .chunks_mut(width)
            .enumerate()
            .try_for_each(|(head, block)| per_head(head, block)),
        Schedule::Pool => out
            .par_chunks_mut(width)
            .enumerate()
            .map(|(head, block)| per_head(head, block))
            .collect::<Vec<_>>()
            .into_iter()
            .collect(),
    }
}

/// What [`StageModel::forward_rows`] computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowsOutput {
    /// Each row's output boundary vector, in row order.
    pub hidden: Vec<Vec<i64>>,
    /// Each row's logits, for the last stage.
    pub logits: Option<Vec<Vec<i64>>>,
    /// Per MoE layer of the stage, in layer order: how many distinct routed
    /// experts the pass's rows chose, each read once for all of them.
    pub expert_union: Vec<usize>,
}

/// One row's selected experts and their Q32 routing weights (spec §5.3–§5.5).
fn moe_route(
    c: &MlaConfig,
    m: &MoeWeights,
    x: &[i64],
) -> Result<(Vec<usize>, Vec<i64>), ModernError> {
    let mut logits = vec![0i64; c.n_routed_experts];
    router_logits(&m.router_q, &m.router_k, x, &mut logits)?;
    let (sigma, keys) = selection_keys(&logits, &m.bias)?;
    let chosen = select_experts(&keys, c.n_experts_per_tok, c.n_group, c.topk_group)?;
    let chosen_sigma: Vec<i64> = chosen.iter().map(|&e| sigma[e]).collect();
    let weights = routing_weights(&chosen_sigma, c.routed_scaling_q32, c.norm_topk_prob);
    Ok((chosen, weights))
}

/// The shared experts' output for one row (spec §5.6).
fn shared_ffn(data: &[u8], m: &MoeWeights, x: &[i64]) -> Result<Vec<i64>, ModernError> {
    let [gate, up, down] = &m.shared;
    gated_ffn(gate.view(data, 0), up.view(data, 0), down.view(data, 0), x)
}

/// The MoE layer for one row (spec §5.3–§5.6).
fn moe_forward(
    c: &MlaConfig,
    data: &[u8],
    m: &MoeWeights,
    x: &[i64],
) -> Result<Vec<i64>, ModernError> {
    let (chosen, weights) = moe_route(c, m, x)?;
    // Experts run one after another: each expert projection is already
    // parallel over its rows, and INT8 experts may use the opt-in limb
    // kernel, whose thread-local scratch a projection nested inside
    // another rayon task could re-enter through work stealing.
    let outputs = match &m.experts {
        ExpertStacks::Int8([gate, up, down]) => chosen
            .iter()
            .map(|&e| gated_ffn(gate.view(data, e), up.view(data, e), down.view(data, e), x))
            .collect::<Result<Vec<_>, _>>()?,
        ExpertStacks::Int4([gate, up, down]) => chosen
            .iter()
            .map(|&e| gated_ffn(gate.view(data, e), up.view(data, e), down.view(data, e), x))
            .collect::<Result<Vec<_>, _>>()?,
    };
    let shared = shared_ffn(data, m, x)?;
    let mut out = vec![0i64; c.d_model];
    combine(&weights, &outputs, &shared, &mut out)?;
    Ok(out)
}

/// [`moe_forward`] for every row of `xs`, with each routed expert's weights
/// read once for all the rows that chose it. Rows are routed exactly as
/// alone; the experts then run in ascending order, each over its rows
/// (INT4 stacks in one multi-vector projection per matrix, INT8 stacks row by
/// row), and each row's combine runs on its own experts in its own selection
/// order, so every row's bytes are those of [`moe_forward`]. Returns the rows'
/// outputs and how many distinct experts they chose.
fn moe_forward_rows(
    c: &MlaConfig,
    data: &[u8],
    m: &MoeWeights,
    xs: &[Vec<i64>],
) -> Result<(Vec<Vec<i64>>, usize), ModernError> {
    let routes = xs
        .iter()
        .map(|x| moe_route(c, m, x))
        .collect::<Result<Vec<_>, _>>()?;
    let mut rows_of: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (row, (chosen, _)) in routes.iter().enumerate() {
        for &e in chosen {
            rows_of.entry(e).or_default().push(row);
        }
    }
    let mut expert_outputs: BTreeMap<(usize, usize), Vec<i64>> = BTreeMap::new();
    for (&e, rows) in &rows_of {
        let inputs: Vec<&[i64]> = rows.iter().map(|&row| xs[row].as_slice()).collect();
        let ys = match &m.experts {
            ExpertStacks::Int8([gate, up, down]) => inputs
                .iter()
                .map(|x| gated_ffn(gate.view(data, e), up.view(data, e), down.view(data, e), x))
                .collect::<Result<Vec<_>, _>>()?,
            ExpertStacks::Int4([gate, up, down]) => gated_ffn_q4_rows(
                &gate.view(data, e),
                &up.view(data, e),
                &down.view(data, e),
                &inputs,
            )?,
        };
        for (&row, y) in rows.iter().zip(ys) {
            expert_outputs.insert((row, e), y);
        }
    }
    let mut out = Vec::with_capacity(xs.len());
    for (row, (x, (chosen, weights))) in xs.iter().zip(&routes).enumerate() {
        let outputs = chosen
            .iter()
            .map(|&e| {
                expert_outputs
                    .remove(&(row, e))
                    .ok_or_else(|| invalid("a selected expert produced no output"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let shared = shared_ffn(data, m, x)?;
        let mut y = vec![0i64; c.d_model];
        combine(weights, &outputs, &shared, &mut y)?;
        out.push(y);
    }
    Ok((out, rows_of.len()))
}

/// Tokens, digests and timings of one generation (spec §7).
#[derive(Debug, Clone)]
pub struct MlaGeneration {
    pub tokens: Vec<u32>,
    pub output_hash: [u8; 32],
    /// One hash per forward call, in order.
    pub logits_hashes: Vec<[u8; 32]>,
    pub logits_digest: [u8; 32],
    /// `boundary_digest` at boundaries `0 ..= L` over every forwarded position.
    pub boundary_digests: Vec<[u8; 32]>,
    pub prefill_seconds: f64,
    pub decode_seconds: f64,
    /// Forward calls made after the prompt.
    pub decode_forwards: usize,
}

/// One sequence run through a stage by teacher forcing (spec §6.4).
#[derive(Debug, Clone)]
pub struct SequenceRun {
    /// The stage's output boundary, `positions * d_model` values.
    pub hidden: Vec<i64>,
    /// Logits hashes per position (last stage only).
    pub logits_hashes: Vec<[u8; 32]>,
    /// Tokens re-derived from the logits by the sequence's selection rule,
    /// one per generated position (last stage only).
    pub derived: Vec<u32>,
    pub seconds: f64,
}

impl StageModel {
    /// Memory-map and validate a stage package.
    pub fn open(path: &Path) -> Result<Self, ModernError> {
        Self::open_range(path, None)
    }

    /// Memory-map a stage package and load only layers `range` (which must
    /// lie inside the package's range); `None` loads the package's range.
    /// Only the tensors of the executed range are read and validated, so a
    /// process holding a whole-model file can act as any single stage.
    pub fn open_range(path: &Path, range: Option<StageSpec>) -> Result<Self, ModernError> {
        let context = path.display().to_string();
        let file = File::open(path).map_err(|e| ModernError::io(&context, e))?;
        // SAFETY: the map is read-only. A package changed on disk while it is
        // mapped is outside the profile (nodes check its digests against the
        // pinned manifest); it can change values, never memory safety of the
        // integer code, which only reads bytes.
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| ModernError::io(&context, e))?;
        Self::from_bytes(PackageBytes::Mapped(map), range)
    }

    /// Validate a stage package held in memory.
    pub fn from_owned(bytes: Vec<u8>) -> Result<Self, ModernError> {
        Self::from_bytes(PackageBytes::Owned(bytes), None)
    }

    fn from_bytes(bytes: PackageBytes, range: Option<StageSpec>) -> Result<Self, ModernError> {
        let (header, stage, rope_cos, rope_sin, embed, layers, head) = {
            let data = bytes.as_slice();
            let header = package::parse_header(data, data.len() as u64)?;
            package::check_padding(data, &header)?;
            let stage = range.unwrap_or(header.stage);
            stage.validate(&header.config)?;
            if stage.first_layer < header.stage.first_layer
                || stage.end_layer > header.stage.end_layer
            {
                return Err(invalid(format!(
                    "layers [{}, {}) are not inside the package's [{}, {})",
                    stage.first_layer,
                    stage.end_layer,
                    header.stage.first_layer,
                    header.stage.end_layer
                )));
            }
            let loader = Loader {
                data,
                header: &header,
            };
            let c = &header.config;
            let rope_cos = loader.i32s("rope.cos")?;
            let rope_sin = loader.i32s("rope.sin")?;
            if c.preparation.is_some() {
                let (expected_cos, expected_sin) = super::yarn::tables(c)?;
                if rope_cos != expected_cos || rope_sin != expected_sin {
                    return Err(invalid(
                        "package tables differ from versioned YaRN preparation",
                    ));
                }
            }
            let embed = if stage.has_embed() {
                Some(loader.mat("embed", 1, c.vocab_size, c.d_model)?)
            } else {
                None
            };
            let layers = stage
                .layers()
                .map(|layer| loader.layer(c, layer))
                .collect::<Result<Vec<_>, _>>()?;
            let head = if stage.has_head(c) {
                Some(HeadWeights {
                    final_norm: loader.i64s("final_norm")?,
                    lm_head: loader.mat("lm_head", 1, c.vocab_size, c.d_model)?,
                })
            } else {
                None
            };
            (header, stage, rope_cos, rope_sin, embed, layers, head)
        };
        Ok(Self {
            header,
            stage,
            bytes,
            rope_cos,
            rope_sin,
            embed,
            layers,
            head,
            #[cfg(test)]
            forced_head_schedule: None,
        })
    }

    /// How the heads of layer `w` run. An INT16 layer runs its heads in
    /// parallel when their `wk_b` and `wv_b` projections hold at least 2^18
    /// weights together (K2.6: 64 heads x 2 x 65,536): each per-head slice is
    /// below the row-parallel threshold of `project_i16`, and INT16
    /// projections keep no thread-local state. INT8 heads stay on the calling
    /// thread: the opt-in limb kernel's thread-local scratch could be
    /// re-entered by a projection nested in another rayon task through work
    /// stealing.
    fn head_schedule(&self, w: &LayerWeights) -> Schedule {
        if !(w.wk_b.wide && w.wv_b.wide) {
            return Schedule::Serial;
        }
        #[cfg(test)]
        if let Some(forced) = self.forced_head_schedule {
            return forced;
        }
        let per_head = w.wk_b.rows * w.wk_b.cols + w.wv_b.rows * w.wv_b.cols;
        Schedule::for_work(self.config().n_heads.saturating_mul(per_head))
    }

    pub fn config(&self) -> &MlaConfig {
        &self.header.config
    }

    /// The executed layer range.
    pub fn stage(&self) -> StageSpec {
        self.stage
    }

    /// The package bytes (for segment digests).
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Whether a segment belongs to the executed range (spec §4.7).
    pub fn executes_segment(&self, segment: &str) -> bool {
        let c = self.config();
        match segment {
            "tables" => true,
            "embed" => self.stage.has_embed(),
            "head" => self.stage.has_head(c),
            other => other
                .strip_prefix("layer.")
                .and_then(|l| l.parse::<usize>().ok())
                .is_some_and(|l| self.stage.layers().contains(&l)),
        }
    }

    /// Digests of the segments this stage executes (spec §4.7).
    pub fn segments(&self) -> Vec<package::SegmentDigest> {
        package::segment_digests_where(self.bytes(), &self.header, |s| self.executes_segment(s))
    }

    /// INT8 weights this stage executes.
    pub fn weight_count(&self) -> usize {
        self.header
            .entries
            .iter()
            .filter(|e| e.dtype == package::Dtype::I8 && self.executes_segment(&e.segment))
            .map(|e| e.bytes as usize)
            .sum()
    }

    /// An empty cache for this stage's layers.
    pub fn new_cache(&self) -> StageCache {
        StageCache {
            latent: vec![Vec::new(); self.layers.len()],
            rope_keys: vec![Vec::new(); self.layers.len()],
            positions: 0,
        }
    }

    /// One position through the stage (spec §5.7). Returns the output
    /// boundary vector and, for the last stage, the logits. When `trace` is
    /// given it receives the activation hash at every boundary of the stage
    /// (`b - a + 1` hashes, input boundary first).
    ///
    /// On error the cache may hold a partial position and must be discarded.
    pub fn forward(
        &self,
        input: StageInput<'_>,
        cache: &mut StageCache,
        trace: Option<&mut Vec<[u8; 32]>>,
    ) -> Result<(Vec<i64>, Option<Vec<i64>>), ModernError> {
        let c = self.config();
        let data = self.bytes.as_slice();
        let position = cache.positions;
        if position >= c.max_seq {
            return Err(ModernError::Domain(format!(
                "position {position} is outside the {}-position context",
                c.max_seq
            )));
        }
        if cache.latent.len() != self.layers.len() {
            return Err(invalid("the cache belongs to another stage"));
        }
        let mut h = self.stage_input(input)?;
        let mut hashes = Vec::new();
        let tracing = trace.is_some();
        if tracing {
            hashes.push(activation_hash(&h));
        }
        let half = c.qk_rope_dim / 2;
        let cos = &self.rope_cos[position * half..(position + 1) * half];
        let sin = &self.rope_sin[position * half..(position + 1) * half];
        for (local, layer) in self.layers.iter().enumerate() {
            self.layer_forward(layer, local, &mut h, position, cos, sin, cache)?;
            if tracing {
                hashes.push(activation_hash(&h));
            }
        }
        cache.positions = position + 1;
        let logits = match &self.head {
            Some(head) => {
                let x = rms_norm(&h, &head.final_norm, c.rms_eps_q32)?;
                let mut logits = vec![0i64; c.vocab_size];
                head.lm_head.view(data, 0).project(&x, &mut logits)?;
                Some(logits)
            }
            None => None,
        };
        if let Some(trace) = trace {
            trace.extend(hashes);
        }
        Ok((h, logits))
    }

    /// The vector entering the stage at one position: a token's embedding
    /// row (the first stage) or the boundary values (every later stage).
    fn stage_input(&self, input: StageInput<'_>) -> Result<Vec<i64>, ModernError> {
        let c = self.config();
        let data = self.bytes.as_slice();
        match (input, &self.embed) {
            (StageInput::Token(token), Some(embed)) => {
                embed.view(data, 0).embed_row(token as usize)
            }
            (StageInput::Hidden(values), None) => {
                if values.len() != c.d_model {
                    return Err(invalid("boundary vector width"));
                }
                if values
                    .iter()
                    .any(|v| u128::from(v.unsigned_abs()) > ACTIVATION_LIMIT)
                {
                    return Err(ModernError::Domain("boundary value beyond 2^62".into()));
                }
                Ok(values.to_vec())
            }
            (StageInput::Token(_), None) => Err(invalid("only the first stage takes token ids")),
            (StageInput::Hidden(_), Some(_)) => Err(invalid("the first stage takes token ids")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn layer_forward(
        &self,
        w: &LayerWeights,
        local: usize,
        h: &mut [i64],
        position: usize,
        cos: &[i32],
        sin: &[i32],
        cache: &mut StageCache,
    ) -> Result<(), ModernError> {
        self.attention_block(w, local, h, position, cos, sin, cache)?;
        let x = rms_norm(h, &w.ffn_norm, self.config().rms_eps_q32)?;
        let out = match &w.ffn {
            FfnWeights::Dense(dense) => self.dense_ffn(dense, &x)?,
            FfnWeights::Moe(moe) => moe_forward(self.config(), self.bytes.as_slice(), moe, &x)?,
        };
        add_residual(h, &out)
    }

    /// The attention half of a layer at one position: appends the position's
    /// latent and RoPE key to the layer's cache and adds the attention output
    /// to `h` (spec §5.2).
    #[allow(clippy::too_many_arguments)]
    fn attention_block(
        &self,
        w: &LayerWeights,
        local: usize,
        h: &mut [i64],
        position: usize,
        cos: &[i32],
        sin: &[i32],
        cache: &mut StageCache,
    ) -> Result<(), ModernError> {
        let c = self.config();
        let data = self.bytes.as_slice();
        let eps = c.rms_eps_q32;
        // Multi-head latent attention, absorbed form (spec §5.2).
        let x = rms_norm(h, &w.attn_norm, eps)?;
        let mut q = vec![0i64; c.d_q()];
        match &w.query {
            QueryProjection::Direct(m) => m.view(data, 0).project(&x, &mut q)?,
            QueryProjection::Lora { a, norm, b } => {
                let mut qa = vec![0i64; c.q_lora_rank];
                a.view(data, 0).project(&x, &mut qa)?;
                let qa = rms_norm(&qa, norm, eps)?;
                b.view(data, 0).project(&qa, &mut q)?;
            }
        }
        let mut kv = vec![0i64; c.d_kv_a()];
        w.wkv_a.view(data, 0).project(&x, &mut kv)?;
        let rank = c.kv_lora_rank;
        let latent = rms_norm(&kv[..rank], &w.kv_a_norm, eps)?;
        let mut key = kv[rank..].to_vec();
        rope_interleaved(&mut key, cos, sin)?;
        cache.push(local, &latent, &key)?;
        let view = LatentCache {
            latent: &cache.latent[local],
            rope_keys: &cache.rope_keys[local],
            positions: position + 1,
            rank,
            rope_dim: c.qk_rope_dim,
        };
        let (nope, dqk, lambda) = (c.qk_nope_dim, c.d_qk(), c.attention_lambda);
        let mut heads = vec![0i64; c.d_attn_out()];
        // Heads are independent: head j reads q, the cache and its own weights
        // and writes only its output block. `head_schedule` says whether they
        // run in parallel (INT16 layers large enough) or one after another.
        for_each_head(&mut heads, c.v_head_dim, self.head_schedule(w), |j, out| {
            let base = j * dqk;
            let mut qp = q[base + nope..base + dqk].to_vec();
            rope_interleaved(&mut qp, cos, sin)?;
            let mut qa = vec![0i64; rank];
            w.wk_b
                .view(data, j)
                .project(&q[base..base + nope], &mut qa)?;
            let mut u = vec![0i64; rank];
            mla_attend(&qa, &qp, view, lambda, &mut u)?;
            w.wv_b.view(data, j).project(&u, out)
        })?;
        let mut y = vec![0i64; c.d_model];
        w.wo.view(data, 0).project(&heads, &mut y)?;
        add_residual(h, &y)
    }

    /// A dense FFN layer's output for one normed input (spec §5.6).
    fn dense_ffn(&self, dense: &[MatRef; 3], x: &[i64]) -> Result<Vec<i64>, ModernError> {
        let data = self.bytes.as_slice();
        let [gate, up, down] = dense;
        gated_ffn(gate.view(data, 0), up.view(data, 0), down.view(data, 0), x)
    }

    /// `inputs.len()` consecutive positions through the stage in one pass,
    /// the verification pass of speculative decoding. The result equals that
    /// many [`Self::forward`] calls in order, value for value: the same
    /// boundary vectors, logits and cache. The layers run in order with every
    /// row at each: attention row by row in position order (row r attends to
    /// the cache and rows 0 to r), then the FFN; in an MoE layer each routed
    /// expert's weights are read once for all the rows that chose it
    /// (`moe_forward_rows`), and the combine runs per row in its fixed order.
    ///
    /// On error the cache may hold a partial pass and must be discarded.
    pub fn forward_rows(
        &self,
        inputs: &[StageInput<'_>],
        cache: &mut StageCache,
    ) -> Result<RowsOutput, ModernError> {
        let c = self.config();
        let data = self.bytes.as_slice();
        let start = cache.positions;
        if inputs.is_empty() {
            return Err(invalid("a multi-row pass needs at least one row"));
        }
        if start + inputs.len() > c.max_seq {
            return Err(ModernError::Domain(format!(
                "position {} is outside the {}-position context",
                start.max(c.max_seq),
                c.max_seq
            )));
        }
        if cache.latent.len() != self.layers.len() {
            return Err(invalid("the cache belongs to another stage"));
        }
        let mut hs = inputs
            .iter()
            .map(|&input| self.stage_input(input))
            .collect::<Result<Vec<_>, _>>()?;
        let half = c.qk_rope_dim / 2;
        let mut expert_union = Vec::new();
        for (local, layer) in self.layers.iter().enumerate() {
            let mut xs = Vec::with_capacity(hs.len());
            for (row, h) in hs.iter_mut().enumerate() {
                let position = start + row;
                let cos = &self.rope_cos[position * half..(position + 1) * half];
                let sin = &self.rope_sin[position * half..(position + 1) * half];
                self.attention_block(layer, local, h, position, cos, sin, cache)?;
                xs.push(rms_norm(h, &layer.ffn_norm, c.rms_eps_q32)?);
            }
            let outs = match &layer.ffn {
                FfnWeights::Dense(dense) => xs
                    .iter()
                    .map(|x| self.dense_ffn(dense, x))
                    .collect::<Result<Vec<_>, _>>()?,
                FfnWeights::Moe(moe) => {
                    let (outs, experts) = moe_forward_rows(c, data, moe, &xs)?;
                    expert_union.push(experts);
                    outs
                }
            };
            for (h, out) in hs.iter_mut().zip(&outs) {
                add_residual(h, out)?;
            }
        }
        cache.positions = start + inputs.len();
        let logits = match &self.head {
            Some(head) => Some(
                hs.iter()
                    .map(|h| {
                        let x = rms_norm(h, &head.final_norm, c.rms_eps_q32)?;
                        let mut logits = vec![0i64; c.vocab_size];
                        head.lm_head.view(data, 0).project(&x, &mut logits)?;
                        Ok(logits)
                    })
                    .collect::<Result<Vec<_>, ModernError>>()?,
            ),
            None => None,
        };
        Ok(RowsOutput {
            hidden: hs,
            logits,
            expert_union,
        })
    }

    fn traced_step(
        &self,
        token: u32,
        cache: &mut StageCache,
        trace: &mut Vec<[u8; 32]>,
        boundaries: &mut [blake3::Hasher],
    ) -> Result<Vec<i64>, ModernError> {
        trace.clear();
        let (_, logits) = self.forward(StageInput::Token(token), cache, Some(trace))?;
        for (hasher, hash) in boundaries.iter_mut().zip(trace.iter()) {
            hasher.update(hash);
        }
        logits.ok_or_else(|| invalid("generation needs the last stage"))
    }

    /// Generation `arc.hf-chat.no-bos.*.le-u32.v1` (spec §7) with the whole
    /// model, recording `boundary_digest` at every boundary.
    pub fn generate(&self, request: &GenerationRequest<'_>) -> Result<MlaGeneration, ModernError> {
        let c = self.config();
        if self.embed.is_none() || self.head.is_none() {
            return Err(invalid(
                "generation needs the whole model (stage [0, n_layers))",
            ));
        }
        if request.prompt.is_empty() || request.max_tokens == 0 {
            return Err(invalid(
                "generation needs a non-empty prompt and max_tokens >= 1",
            ));
        }
        if request.prompt.len() + request.max_tokens > c.max_seq {
            return Err(ModernError::Domain(format!(
                "{} prompt tokens + {} generated tokens exceed the {}-position context",
                request.prompt.len(),
                request.max_tokens,
                c.max_seq
            )));
        }
        if let Some(&bad) = request.prompt.iter().find(|&&t| t as usize >= c.vocab_size) {
            return Err(ModernError::Domain(format!(
                "prompt token {bad} is outside the vocabulary"
            )));
        }
        let mut cache = self.new_cache();
        let mut boundaries = vec![blake3::Hasher::new(); self.layers.len() + 1];
        let mut trace = Vec::with_capacity(self.layers.len() + 1);
        let mut hashes = Vec::with_capacity(request.prompt.len() + request.max_tokens);
        let prefill_start = Instant::now();
        let mut logits = Vec::new();
        for &token in request.prompt {
            logits = self.traced_step(token, &mut cache, &mut trace, &mut boundaries)?;
            hashes.push(arith::logits_hash(&logits));
        }
        let prefill_seconds = prefill_start.elapsed().as_secs_f64();
        let decode_start = Instant::now();
        let mut tokens: Vec<u32> = Vec::new();
        loop {
            let next = arith::select(&logits, &tokens, request.selection)?;
            tokens.push(next);
            if request.eos.contains(&next) || tokens.len() == request.max_tokens {
                break;
            }
            logits = self.traced_step(next, &mut cache, &mut trace, &mut boundaries)?;
            hashes.push(arith::logits_hash(&logits));
        }
        let decode_seconds = decode_start.elapsed().as_secs_f64();
        Ok(MlaGeneration {
            output_hash: arith::tokens_hash(&tokens),
            logits_digest: arith::logits_digest(&hashes),
            boundary_digests: boundaries
                .into_iter()
                .map(|h| *h.finalize().as_bytes())
                .collect(),
            decode_forwards: tokens.len() - 1,
            tokens,
            logits_hashes: hashes,
            prefill_seconds,
            decode_seconds,
        })
    }

    /// Run one sequence through this stage alone (spec §6.4): `tokens` are the
    /// forwarded token ids; `inputs` the boundary-`a` values (`None` for the
    /// first stage). The last stage also re-derives the generated tokens.
    pub fn run_sequence(
        &self,
        tokens: &[u32],
        inputs: Option<&[i64]>,
        prompt_len: usize,
        selection: Selection,
    ) -> Result<SequenceRun, ModernError> {
        let c = self.config();
        let d = c.d_model;
        if tokens.is_empty() || prompt_len == 0 || prompt_len > tokens.len() {
            return Err(invalid(
                "a stage sequence needs 1 <= prompt_len <= positions",
            ));
        }
        if let Some(values) = inputs
            && values.len() != tokens.len() * d
        {
            return Err(invalid("boundary values do not match the positions"));
        }
        let start = Instant::now();
        let mut cache = self.new_cache();
        let mut hidden = Vec::with_capacity(tokens.len() * d);
        let mut logits_hashes = Vec::new();
        let mut derived = Vec::new();
        for (position, &token) in tokens.iter().enumerate() {
            let input = match inputs {
                Some(values) => StageInput::Hidden(&values[position * d..(position + 1) * d]),
                None => StageInput::Token(token),
            };
            let (h, logits) = self.forward(input, &mut cache, None)?;
            hidden.extend_from_slice(&h);
            if let Some(logits) = logits {
                logits_hashes.push(arith::logits_hash(&logits));
                if position + 1 >= prompt_len {
                    let history = &tokens[prompt_len..=position];
                    derived.push(arith::select(&logits, history, selection)?);
                }
            }
        }
        Ok(SequenceRun {
            hidden,
            logits_hashes,
            derived,
            seconds: start.elapsed().as_secs_f64(),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::boundary::{Boundary, BoundarySequence, boundary_digest};
    use super::super::package::{StageWriter, header_json, layout};
    use super::*;
    use crate::modern::arith::ONE;

    /// Deterministic generator for tiny packages.
    pub(crate) struct Lcg(u64);

    impl Lcg {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    /// A tiny configuration exercising query LoRA (when `lora`), group
    /// routing, a dense first layer and shared experts.
    pub(crate) fn tiny_config(lora: bool) -> MlaConfig {
        tiny_config_with(lora, ExpertFormat::Int8Dyadic)
    }

    /// The tiny configuration with the routed experts in `format`.
    pub(crate) fn tiny_config_with(lora: bool, format: ExpertFormat) -> MlaConfig {
        MlaConfig {
            architecture: "deepseek_v3".into(),
            n_layers: 4,
            d_model: 32,
            n_heads: 4,
            q_lora_rank: if lora { 24 } else { 0 },
            kv_lora_rank: 16,
            qk_nope_dim: 8,
            qk_rope_dim: 4,
            v_head_dim: 8,
            d_ff: 48,
            first_k_dense: 1,
            n_routed_experts: 8,
            n_experts_per_tok: 3,
            n_shared_experts: 2,
            moe_d_ff: 32,
            n_group: if lora { 4 } else { 1 },
            topk_group: if lora { 2 } else { 1 },
            norm_topk_prob: true,
            routed_scaling_q32: 10_505_490_006,
            vocab_size: 50,
            max_seq: 24,
            rms_eps_q32: 42_950,
            rope_theta: 50_000,
            attention_lambda: crate::modern::tables::attention_lambda(12),
            expert_format: format,
            preparation: None,
            precision: None,
        }
    }

    /// How the tiny package's routers are filled.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum TinyRouter {
        /// Random router rows and biases.
        Random,
        /// Expert `2j + 1` gets expert `2j`'s router row, shift and bias, so
        /// every token's keys tie in pairs and the top-k cut falls in a tie.
        Paired,
        /// Zero router rows and biases: every key ties for every token.
        Flat,
    }

    /// Write a random but valid package for `stage` of `c` into memory, with
    /// the same values whatever the stage (each tensor's content depends on
    /// its name only), so stages of different layouts are consistent.
    pub(crate) fn tiny_package(c: &MlaConfig, stage: StageSpec) -> Vec<u8> {
        tiny_package_routed(c, stage, TinyRouter::Random)
    }

    /// [`tiny_package`] with the routers filled as `router` says.
    pub(crate) fn tiny_package_routed(
        c: &MlaConfig,
        stage: StageSpec,
        router: TinyRouter,
    ) -> Vec<u8> {
        let entries = layout(c, stage);
        let header = header_json(
            c,
            &serde_json::json!({"repo": "arc-test/tiny-mla", "revision": "0", "files": []}),
            stage,
            &entries,
        );
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "arc-mla-tiny-{}-{unique}.arcspkg",
            std::process::id()
        ));
        let mut w = StageWriter::create(&path, &header, entries.clone()).unwrap();
        let (cos, sin) = super::super::yarn::tables(c).unwrap();
        for e in &entries {
            let seed = blake3::hash(e.name.as_bytes());
            let mut rng = Lcg(u64::from_le_bytes(seed.as_bytes()[..8].try_into().unwrap()));
            let count = e.shape.iter().product::<usize>();
            let bytes: Vec<u8> = if e.name == "rope.cos" {
                crate::modern::package::i32_bytes(&cos)
            } else if e.name == "rope.sin" {
                crate::modern::package::i32_bytes(&sin)
            } else if e.name.ends_with(".q4") {
                // Any nibble is a valid INT4 value.
                (0..count).map(|_| rng.next() as u8).collect()
            } else if e.name.ends_with(".s") {
                // Positive BF16 group scales near 2^-11.
                let values: Vec<u16> = (0..count)
                    .map(|_| (((114 + rng.next() % 3) << 7) | (rng.next() % 128)) as u16)
                    .collect();
                super::super::package::u16_bytes(&values)
            } else if e.name.ends_with(".q") && e.dtype == package::Dtype::I8 {
                (0..count)
                    .map(|_| ((rng.next() % 255) as i64 - 127) as i8 as u8)
                    .collect()
            } else if e.name.ends_with(".q")
                && e.dtype == package::Dtype::I16
                && !e.name.contains("router")
            {
                let values: Vec<i16> = (0..count)
                    .map(|_| ((rng.next() % 65535) as i64 - 32767) as i16)
                    .collect();
                super::super::package::i16_bytes(&values)
            } else if e.name.ends_with(".mu") {
                let values: Vec<i32> = (0..count)
                    .map(|_| ((1u64 << 30) + rng.next() % (1 << 30)) as i32)
                    .collect();
                crate::modern::package::i32_bytes(&values)
            } else if e.name.ends_with(".k") && !e.name.contains("router") {
                // Scales mu * 2^-k around 2^-8: projections of +-127 weights
                // keep activations of order one.
                (0..count)
                    .map(|_| {
                        38 + if c.precision.as_ref().is_some_and(|p| {
                            p.canonical(&e.name) == super::super::precision::Bits::Int16
                        }) {
                            8
                        } else {
                            0
                        } + (rng.next() % 2) as u8
                    })
                    .collect()
            } else if e.name.ends_with("router.q") {
                let values: Vec<i16> = (0..count)
                    .map(|_| ((rng.next() % 65_535) as i64 - 32_767) as i16)
                    .collect();
                super::super::package::i16_bytes(&values)
            } else if e.name.ends_with("router.k") {
                // Router weights q * 2^-k of order 0.1.
                (0..count).map(|_| 17 + (rng.next() % 3) as u8).collect()
            } else if e.name.ends_with("router_bias") {
                let values: Vec<i64> = (0..count)
                    .map(|_| (rng.next() % (1 << 30)) as i64 - (1 << 29))
                    .collect();
                crate::modern::package::i64_bytes(&values)
            } else {
                // Norm gains in [0.5, 1.5).
                let values: Vec<i64> = (0..count)
                    .map(|_| ONE / 2 + (rng.next() % ONE as u64) as i64)
                    .collect();
                crate::modern::package::i64_bytes(&values)
            };
            let routing = ["router.q", "router.k", "router_bias"]
                .iter()
                .any(|suffix| e.name.ends_with(suffix));
            let bytes = match router {
                TinyRouter::Paired if routing => {
                    let row = bytes.len() / c.n_routed_experts;
                    let mut paired = bytes;
                    for expert in (1..c.n_routed_experts).step_by(2) {
                        paired.copy_within((expert - 1) * row..expert * row, expert * row);
                    }
                    paired
                }
                TinyRouter::Flat if routing && !e.name.ends_with("router.k") => {
                    vec![0; bytes.len()]
                }
                _ => bytes,
            };
            w.write_tensor(&e.name, &bytes).unwrap();
        }
        w.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        bytes
    }

    /// Per-sequence boundary digests, logits hashes and re-derived tokens.
    type Reference = (Vec<[u8; 32]>, Vec<[u8; 32]>, Vec<u32>);

    /// Query LoRA off/on with INT8 and with INT4 group-32 experts.
    const FORMATS: [(bool, ExpertFormat); 4] = [
        (false, ExpertFormat::Int8Dyadic),
        (true, ExpertFormat::Int8Dyadic),
        (false, ExpertFormat::Int4G32),
        (true, ExpertFormat::Int4G32),
    ];

    fn sequences() -> Vec<(Vec<u32>, usize)> {
        vec![
            (vec![3, 17, 5, 49, 0, 22, 8], 3),
            (vec![41, 2], 1),
            (vec![7, 7, 7, 7, 7], 2),
        ]
    }

    #[test]
    fn forward_is_identical_across_thread_counts_and_kernels() {
        for (lora, format) in FORMATS {
            let c = tiny_config_with(lora, format);
            let model = StageModel::from_owned(tiny_package(&c, StageSpec::full(&c))).unwrap();
            let run = |threads: usize| {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap();
                pool.install(|| {
                    let mut cache = model.new_cache();
                    let mut hashes = Vec::new();
                    for token in [3u32, 17, 5, 49, 0] {
                        let (_, logits) = model
                            .forward(StageInput::Token(token), &mut cache, None)
                            .unwrap();
                        hashes.push(arith::logits_hash(&logits.unwrap()));
                    }
                    (hashes, cache.digest())
                })
            };
            let one = run(1);
            assert_eq!(one, run(3));
            let _guard = crate::canonical_simd::kernel_switch_guard();
            crate::canonical_simd::set_fast_canonical_kernel(true);
            let simd = run(2);
            crate::canonical_simd::set_fast_canonical_kernel(false);
            assert_eq!(one, simd, "lora = {lora}, {format:?}");
        }
    }

    /// The same sequences through 1, 2 and 4 stages, each stage loaded alone
    /// from its own package and fed only the serialised boundary file of the
    /// previous stage: identical boundaries, logits hashes and tokens.
    #[test]
    fn pipeline_layouts_are_byte_identical() {
        for (lora, format) in FORMATS {
            let c = tiny_config_with(lora, format);
            let full = StageModel::from_owned(tiny_package(&c, StageSpec::full(&c))).unwrap();
            // Reference: per-boundary digests from the single-stage generation path.
            let reference: Vec<Reference> = sequences()
                .into_iter()
                .map(|(tokens, prompt_len)| {
                    let mut cache = full.new_cache();
                    let mut trace = Vec::new();
                    let mut boundaries = vec![blake3::Hasher::new(); c.n_layers + 1];
                    let mut logits_hashes = Vec::new();
                    let mut derived = Vec::new();
                    for (p, &t) in tokens.iter().enumerate() {
                        let logits = full
                            .traced_step(t, &mut cache, &mut trace, &mut boundaries)
                            .unwrap();
                        logits_hashes.push(arith::logits_hash(&logits));
                        if p + 1 >= prompt_len {
                            derived.push(
                                arith::select(
                                    &logits,
                                    &tokens[prompt_len..=p],
                                    Selection::Rp64Argmax,
                                )
                                .unwrap(),
                            );
                        }
                    }
                    let digests = boundaries
                        .into_iter()
                        .map(|h| *h.finalize().as_bytes())
                        .collect();
                    (digests, logits_hashes, derived)
                })
                .collect();
            for cuts in [vec![0, 4], vec![0, 2, 4], vec![0, 1, 2, 3, 4]] {
                let mut boundary: Option<Boundary> = None;
                for pair in cuts.windows(2) {
                    let stage = StageSpec {
                        first_layer: pair[0],
                        end_layer: pair[1],
                    };
                    let model = StageModel::from_owned(tiny_package(&c, stage)).unwrap();
                    let mut next = Boundary {
                        profile: c.profile().into(),
                        layer: pair[1],
                        d_model: c.d_model,
                        model_root: "00".repeat(32),
                        sequences: Vec::new(),
                    };
                    for (s, (tokens, prompt_len)) in sequences().into_iter().enumerate() {
                        let inputs = boundary.as_ref().map(|b| {
                            assert_eq!(b.sequences[s].tokens, tokens);
                            b.sequences[s].values.clone()
                        });
                        let run = model
                            .run_sequence(
                                &tokens,
                                inputs.as_deref(),
                                prompt_len,
                                Selection::Rp64Argmax,
                            )
                            .unwrap();
                        // The stage's output boundary equals the single-stage
                        // run's boundary at the same layer.
                        assert_eq!(
                            boundary_digest(&run.hidden, c.d_model),
                            reference[s].0[pair[1]],
                            "cuts {cuts:?}, boundary {}",
                            pair[1]
                        );
                        if pair[1] == c.n_layers {
                            assert_eq!(run.logits_hashes, reference[s].1);
                            assert_eq!(run.derived, reference[s].2);
                        } else {
                            assert!(run.logits_hashes.is_empty());
                        }
                        next.sequences.push(BoundarySequence {
                            id: format!("s{s}"),
                            tokens,
                            prompt_len,
                            selection: Selection::Rp64Argmax,
                            eos: vec![],
                            max_tokens: 8,
                            values: run.hidden,
                        });
                    }
                    // Serialise and re-read, as a stage on another machine would.
                    let bytes = next.to_bytes().unwrap();
                    boundary = Some(Boundary::from_bytes(&bytes).unwrap());
                }
            }
        }
    }

    /// Tensor-parallel and expert-parallel splits inside an island: a row
    /// split, a K split with exact partial sums, and experts spread over two
    /// "devices" all give the bytes of the single-device computation.
    #[test]
    fn tensor_and_expert_splits_are_byte_identical() {
        let c = tiny_config(false);
        let model = StageModel::from_owned(tiny_package(&c, StageSpec::full(&c))).unwrap();
        let data = model.bytes();
        let layer = &model.layers[1];
        let x: Vec<i64> = (0..c.d_model as i64).map(|i| (i - 13) * 977).collect();
        // Row split (output features across devices).
        let wo = layer.wo.view(data, 0);
        let inputs: Vec<i64> = (0..wo.cols as i64).map(|i| (i * 31 - 7) * 123).collect();
        let mut whole = vec![0i64; wo.rows];
        wo.project(&inputs, &mut whole).unwrap();
        let half = wo.rows / 2;
        let top = QView {
            q16: None,
            rows: half,
            q: &wo.q[..half * wo.cols],
            mu: &wo.mu[..half],
            k: &wo.k[..half],
            ..wo
        };
        let bottom = QView {
            q16: None,
            rows: wo.rows - half,
            q: &wo.q[half * wo.cols..],
            mu: &wo.mu[half..],
            k: &wo.k[half..],
            ..wo
        };
        let mut split = vec![0i64; wo.rows];
        top.project(&inputs, &mut split[..half]).unwrap();
        bottom.project(&inputs, &mut split[half..]).unwrap();
        assert_eq!(whole, split);
        // K split (input features across devices): exact partial dot products
        // are summed before the shared epilogue.
        let cut = wo.cols / 3;
        let k_split: Vec<i64> = (0..wo.rows)
            .map(|r| {
                let row = wo.row(r);
                let a = arith::dot_i8_i64(&row[..cut], &inputs[..cut]);
                let b = arith::dot_i8_i64(&row[cut..], &inputs[cut..]);
                arith::dyadic_epilogue(a + b, wo.mu[r], wo.k[r]).unwrap()
            })
            .collect();
        assert_eq!(whole, k_split);
        // Expert parallelism: experts evaluated on two devices, partial sums
        // of w_e * y_e exchanged exactly, one floor shift at the end.
        let FfnWeights::Moe(moe) = &layer.ffn else {
            panic!("layer 1 is an MoE layer");
        };
        let single = moe_forward(model.config(), model.bytes(), moe, &x).unwrap();
        let mut logits = vec![0i64; c.n_routed_experts];
        router_logits(&moe.router_q, &moe.router_k, &x, &mut logits).unwrap();
        let (sigma, keys) = selection_keys(&logits, &moe.bias).unwrap();
        let chosen = select_experts(&keys, c.n_experts_per_tok, c.n_group, c.topk_group).unwrap();
        let weights = routing_weights(
            &chosen.iter().map(|&e| sigma[e]).collect::<Vec<_>>(),
            c.routed_scaling_q32,
            c.norm_topk_prob,
        );
        let ExpertStacks::Int8([gate, up, down]) = &moe.experts else {
            panic!("the INT8 tiny model has INT8 experts");
        };
        let device = |experts: &[(usize, i64)]| -> Vec<i128> {
            let mut partial = vec![0i128; c.d_model];
            for &(e, w) in experts {
                let y = gated_ffn(gate.view(data, e), up.view(data, e), down.view(data, e), &x)
                    .unwrap();
                for (p, &v) in partial.iter_mut().zip(&y) {
                    *p += i128::from(w) * i128::from(v);
                }
            }
            partial
        };
        let pairs: Vec<(usize, i64)> = chosen.iter().copied().zip(weights).collect();
        let device_a = device(&pairs[..1]);
        let device_b = device(&pairs[1..]);
        let [s_gate, s_up, s_down] = &moe.shared;
        let shared = gated_ffn(
            s_gate.view(data, 0),
            s_up.view(data, 0),
            s_down.view(data, 0),
            &x,
        )
        .unwrap();
        let expert_parallel: Vec<i64> = (0..c.d_model)
            .map(|j| (((device_a[j] + device_b[j]) >> 32) + i128::from(shared[j])) as i64)
            .collect();
        assert_eq!(single, expert_parallel);
    }

    #[test]
    fn a_sub_range_of_a_larger_package_is_the_same_stage() {
        let c = tiny_config(true);
        let path =
            std::env::temp_dir().join(format!("arc-mla-range-{}.arcspkg", std::process::id()));
        std::fs::write(&path, tiny_package(&c, StageSpec::full(&c))).unwrap();
        let narrowed = StageModel::open_range(
            &path,
            Some(StageSpec {
                first_layer: 1,
                end_layer: 3,
            }),
        )
        .unwrap();
        let separate = StageModel::from_owned(tiny_package(
            &c,
            StageSpec {
                first_layer: 1,
                end_layer: 3,
            },
        ))
        .unwrap();
        assert_eq!(narrowed.segments(), separate.segments());
        assert_eq!(narrowed.weight_count(), separate.weight_count());
        let inputs: Vec<i64> = (0..3 * c.d_model as i64).map(|i| (i - 40) * 1013).collect();
        let a = narrowed
            .run_sequence(&[4, 9, 2], Some(&inputs), 1, Selection::Argmax)
            .unwrap();
        let b = separate
            .run_sequence(&[4, 9, 2], Some(&inputs), 1, Selection::Argmax)
            .unwrap();
        assert_eq!(a.hidden, b.hidden);
        assert!(
            StageModel::open_range(
                &path,
                Some(StageSpec {
                    first_layer: 3,
                    end_layer: 5,
                }),
            )
            .is_err()
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn stages_refuse_the_wrong_kind_of_input() {
        let c = tiny_config(false);
        let first = StageModel::from_owned(tiny_package(
            &c,
            StageSpec {
                first_layer: 0,
                end_layer: 2,
            },
        ))
        .unwrap();
        let second = StageModel::from_owned(tiny_package(
            &c,
            StageSpec {
                first_layer: 2,
                end_layer: 4,
            },
        ))
        .unwrap();
        let zeros = [0i64; 32];
        assert_eq!(zeros.len(), c.d_model);
        let mut cache = first.new_cache();
        assert!(
            first
                .forward(StageInput::Hidden(&zeros), &mut cache, None)
                .is_err()
        );
        let mut cache = second.new_cache();
        assert!(
            second
                .forward(StageInput::Token(1), &mut cache, None)
                .is_err()
        );
        assert!(
            second
                .forward(StageInput::Hidden(&[1, 2]), &mut cache, None)
                .is_err()
        );
        // Generation needs the whole model.
        let request = GenerationRequest {
            prompt: &[1, 2],
            max_tokens: 2,
            eos: &[],
            selection: Selection::Argmax,
        };
        assert!(first.generate(&request).is_err());
        // A corrupted weight byte (-128) is refused at load.
        let mut bytes = tiny_package(&c, StageSpec::full(&c));
        let model = StageModel::from_owned(bytes.clone()).unwrap();
        let entry = model.header.entry("layers.0.wo.q").unwrap().clone();
        let at = model.header.range(&entry).start;
        bytes[at] = 0x80;
        assert!(StageModel::from_owned(bytes).is_err());
    }

    #[test]
    fn generation_matches_teacher_forcing_and_records_boundaries() {
        let c = tiny_config(true);
        let model = StageModel::from_owned(tiny_package(&c, StageSpec::full(&c))).unwrap();
        let prompt = [1u32, 2, 3];
        let request = GenerationRequest {
            prompt: &prompt,
            max_tokens: 6,
            eos: &[],
            selection: Selection::Rp64Argmax,
        };
        let out = model.generate(&request).unwrap();
        assert_eq!(out.tokens.len(), 6);
        assert_eq!(out.logits_hashes.len(), 3 + 6 - 1);
        assert_eq!(out.boundary_digests.len(), c.n_layers + 1);
        let forwarded: Vec<u32> = prompt.iter().chain(&out.tokens[..5]).copied().collect();
        let run = model
            .run_sequence(&forwarded, None, 3, Selection::Rp64Argmax)
            .unwrap();
        assert_eq!(run.logits_hashes, out.logits_hashes);
        assert_eq!(run.derived, out.tokens);
        assert_eq!(
            boundary_digest(&run.hidden, c.d_model),
            out.boundary_digests[c.n_layers]
        );
    }

    /// The routed output for experts `chosen`, computed by hand: the spec's
    /// weights from `sigma` and one exact combine with the shared expert.
    fn routed_by_hand(
        model: &StageModel,
        moe: &MoeWeights,
        x: &[i64],
        chosen: &[usize],
        sigma: &[i64],
    ) -> Vec<i64> {
        let c = model.config();
        let data = model.bytes();
        let chosen_sigma: Vec<i64> = chosen.iter().map(|&e| sigma[e]).collect();
        let weights = routing_weights(&chosen_sigma, c.routed_scaling_q32, c.norm_topk_prob);
        let outputs: Vec<Vec<i64>> = chosen
            .iter()
            .map(|&e| match &moe.experts {
                ExpertStacks::Int8([g, u, d]) => {
                    gated_ffn(g.view(data, e), u.view(data, e), d.view(data, e), x)
                }
                ExpertStacks::Int4([g, u, d]) => {
                    gated_ffn(g.view(data, e), u.view(data, e), d.view(data, e), x)
                }
            })
            .collect::<Result<_, _>>()
            .unwrap();
        let [s_gate, s_up, s_down] = &moe.shared;
        let shared = gated_ffn(
            s_gate.view(data, 0),
            s_up.view(data, 0),
            s_down.view(data, 0),
            x,
        )
        .unwrap();
        let mut out = vec![0i64; c.d_model];
        combine(&weights, &outputs, &shared, &mut out).unwrap();
        out
    }

    /// Routers whose keys tie for every token: with all keys equal the lowest
    /// experts (and groups) are taken; with keys tied in pairs the top-k cut
    /// falls inside a pair and keeps its lower expert. Every MoE layer of every
    /// tiny format, and the layer output is the hand-computed one.
    #[test]
    fn routing_ties_resolve_to_the_lower_index_end_to_end() {
        for (lora, format) in FORMATS {
            let c = tiny_config_with(lora, format);
            let k = c.n_experts_per_tok;
            assert_eq!(
                k, 3,
                "the pair argument below assumes three experts per token"
            );
            for router in [TinyRouter::Flat, TinyRouter::Paired] {
                let model =
                    StageModel::from_owned(tiny_package_routed(&c, StageSpec::full(&c), router))
                        .unwrap();
                let mut layers = 0;
                for (l, layer) in model.layers.iter().enumerate() {
                    let FfnWeights::Moe(moe) = &layer.ffn else {
                        continue;
                    };
                    layers += 1;
                    for t in 0..16i64 {
                        let x: Vec<i64> = (0..c.d_model as i64)
                            .map(|i| ((i * 37 + t * 101) % 61 - 30) * (1 << (8 + t % 5)))
                            .collect();
                        let mut logits = vec![0i64; c.n_routed_experts];
                        router_logits(&moe.router_q, &moe.router_k, &x, &mut logits).unwrap();
                        let (sigma, keys) = selection_keys(&logits, &moe.bias).unwrap();
                        let chosen = select_experts(&keys, k, c.n_group, c.topk_group).unwrap();
                        let at = format!("lora {lora}, {format:?}, {router:?}, layer {l}, x {t}");
                        if router == TinyRouter::Flat {
                            assert!(keys.iter().all(|&key| key == keys[0]), "{at}");
                            assert_eq!(chosen, vec![0, 1, 2], "{at}");
                        } else {
                            // [2a, 2a + 1, 2b]: one whole pair, then the lower
                            // expert of a pair whose upper expert ties with it.
                            assert!(
                                chosen[0].is_multiple_of(2) && chosen[1] == chosen[0] + 1,
                                "{at}"
                            );
                            assert!(chosen[2].is_multiple_of(2), "{at}: {chosen:?}");
                            assert_eq!(keys[chosen[2]], keys[chosen[2] + 1], "{at}");
                        }
                        assert_eq!(
                            moe_forward(model.config(), model.bytes(), moe, &x).unwrap(),
                            routed_by_hand(&model, moe, &x, &chosen, &sigma),
                            "{at}"
                        );
                    }
                }
                assert_eq!(layers, c.n_layers - c.first_k_dense);
            }
        }
    }

    /// Golden digests of the tiny MLA + MoE models: the package bytes, and one
    /// generation (tokens, every logits hash, every boundary digest). Scalar on
    /// one and three threads and the SIMD kernel must all give the pinned
    /// values, and CI checks the same constants on every OS and CPU.
    #[test]
    fn golden_digests_are_pinned_on_every_kernel() {
        use ExpertFormat::{Int4G32, Int8Dyadic};
        use TinyRouter::{Flat, Paired, Random};
        #[rustfmt::skip]
        const GOLDEN: [(&str, bool, ExpertFormat, TinyRouter, &str, &str); 7] = [
            ("int8", false, Int8Dyadic, Random,
             "1df20f1df05824b7d48eaa0b8c8808d8d2529779f724c1325b682c0e2a52c68d",
             "68b58e73eacc60547e9f977f60ce6c9c0b153f70bd023e0a2f7bcf3b440f0410"),
            ("int8 query-lora groups", true, Int8Dyadic, Random,
             "bf34916e74c2ff4d73877ad93df413e6172d9dc1efd4fc423f94ce13451e285b",
             "f9250a027dfe281902c02f369fb426866ecb136607eb0e8f620d3927c98fa8fb"),
            ("int4g32", false, Int4G32, Random,
             "4137e2ed9243e915f47875f722e0f5d2233a81d7211595abfb16ca0f20113548",
             "a233c3a605505e4ef2a11cba5c66d3c4a6d84ece4cc2c2af7cf0d5a5c3d5d264"),
            ("int4g32 query-lora groups", true, Int4G32, Random,
             "68a1bb8245acb16a55585bc652ca5dad0ba1f2391efc1bf1c363fa0d452002a7",
             "e3b5f2a88b3e3a6da299cb6817932d56fda80524703ebb2055ea2907eefe03f1"),
            ("int8 paired-router ties", false, Int8Dyadic, Paired,
             "0ddac66f936aa73e4456d8a63b8de577841696865e4547bf5e64e810a9723da8",
             "8237eda7f197cf92fbc5e741bd74be0887953ed2b4c5c15173fb25190d0e6f6f"),
            ("int4g32 query-lora groups paired-router ties", true, Int4G32, Paired,
             "bd71b84e22f6e3ad2e353d17d70c58a53bbba9db8f9dcf42249b79e72eb34bbd",
             "fa8a7a47a461c3edaa04e9fc6d494a0d9aa505aac23548f68fc50521dd50571e"),
            ("int8 query-lora groups flat-router ties", true, Int8Dyadic, Flat,
             "f267317159174653e87a2c8fb5a5ef31d0d6d377014ad34be82b25c0a1aea9b7",
             "54a6b84b4251c035b1eed1a90bf1ebcf39671149651aee46cf49a400e7625edc"),
        ];
        let mut mismatches = Vec::new();
        for (name, lora, format, router, package_golden, run_golden) in GOLDEN {
            let c = tiny_config_with(lora, format);
            let bytes = tiny_package_routed(&c, StageSpec::full(&c), router);
            let package = blake3::hash(&bytes).to_hex().to_string();
            let model = StageModel::from_owned(bytes).unwrap();
            let request = GenerationRequest {
                prompt: &[3, 17, 5, 49, 0],
                max_tokens: 8,
                eos: &[],
                selection: Selection::Rp64Argmax,
            };
            let run = |threads: usize| -> String {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap();
                let out = pool.install(|| model.generate(&request)).unwrap();
                assert_eq!(out.boundary_digests.len(), c.n_layers + 1);
                let mut h = blake3::Hasher::new();
                for t in &out.tokens {
                    h.update(&t.to_le_bytes());
                }
                for x in out.logits_hashes.iter().chain(&out.boundary_digests) {
                    h.update(x);
                }
                h.finalize().to_hex().to_string()
            };
            // Held throughout, so no other test flips the kernel mid-run.
            let _guard = crate::canonical_simd::kernel_switch_guard();
            crate::canonical_simd::set_fast_canonical_kernel(false);
            let scalar = run(1);
            assert_eq!(scalar, run(3), "{name}: thread count");
            crate::canonical_simd::set_fast_canonical_kernel(true);
            let simd_on = crate::canonical_simd::fast_canonical_kernel_enabled();
            let simd = run(2);
            crate::canonical_simd::set_fast_canonical_kernel(false);
            if cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
                assert!(
                    simd_on,
                    "{name}: the SIMD kernel is unavailable on this CPU"
                );
            }
            assert_eq!(scalar, simd, "{name}: SIMD kernel");
            let line = format!("golden {name}: package {package} run {scalar}");
            println!("{line}");
            if (package.as_str(), scalar.as_str()) != (package_golden, run_golden) {
                mismatches.push(line);
            }
        }
        assert!(
            mismatches.is_empty(),
            "golden digests differ: {mismatches:#?}"
        );
    }
    #[test]
    fn mixed_precision_goldens_and_split_stages() {
        use super::super::precision::{Bits, Precision};
        let _guard = crate::canonical_simd::kernel_switch_guard();
        for (name, dense_only, mixed, lora) in [
            ("int16-dense", true, false, false),
            ("int16-moe", false, false, true),
            ("mixed-moe", false, true, true),
            ("int16-yarn-moe", false, false, true),
        ] {
            let mut c = tiny_config_with(lora, ExpertFormat::Int4G32);
            if dense_only {
                c.n_layers = 1;
            }
            let mut precision = Precision::all_int16();
            if mixed {
                precision.head = Bits::Int8;
                precision.dense = Bits::Int8;
            }
            c.precision = Some(precision);
            if name == "int16-yarn-moe" {
                use super::super::yarn::{ATTENTION_LAMBDA, Preparation, Scope};
                c.architecture = "arc-test/kimi-k26-yarn".into();
                c.qk_nope_dim = 128;
                c.qk_rope_dim = 64;
                c.attention_lambda = ATTENTION_LAMBDA;
                c.preparation = Some(Preparation {
                    scope: Scope::SyntheticFixture,
                });
            }
            c.validate().unwrap();
            let bytes = tiny_package(&c, StageSpec::full(&c));
            let package_hash = blake3::hash(&bytes).to_hex().to_string();
            let model = StageModel::from_owned(bytes).unwrap();
            assert_eq!(model.config(), &c);
            let manifest = package::build_manifest(
                &c,
                &model.header.source,
                &model.segments(),
                None,
                &[],
                &serde_json::Value::Null,
            )
            .unwrap();
            let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
            package::verify_against_manifest(&model.header, &model.segments(), &manifest_bytes)
                .unwrap();
            let request = GenerationRequest {
                prompt: &[3, 17, 5, 49, 0],
                max_tokens: 8,
                eos: &[],
                selection: Selection::Rp64Argmax,
            };
            let mut golden = None;
            for (fast, threads) in [(false, 1), (false, 3), (true, 2)] {
                crate::canonical_simd::set_fast_canonical_kernel(fast);
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap();
                let run = pool.install(|| model.generate(&request)).unwrap();
                let mut h = blake3::Hasher::new();
                for t in &run.tokens {
                    h.update(&t.to_le_bytes());
                }
                for d in run.logits_hashes.iter().chain(&run.boundary_digests) {
                    h.update(d);
                }
                let digest = h.finalize().to_hex().to_string();
                if let Some(ref want) = golden {
                    assert_eq!(want, &digest);
                } else {
                    golden = Some(digest);
                }
                let tokens = [3, 17, 5, 49, 0];
                let reference = pool
                    .install(|| model.run_sequence(&tokens, None, 3, Selection::Rp64Argmax))
                    .unwrap();
                let mut inputs = None;
                for layer in 0..c.n_layers {
                    let stage = StageSpec {
                        first_layer: layer,
                        end_layer: layer + 1,
                    };
                    let part = StageModel::from_owned(tiny_package(&c, stage)).unwrap();
                    package::verify_against_manifest(
                        &part.header,
                        &part.segments(),
                        &manifest_bytes,
                    )
                    .unwrap();
                    let out = pool
                        .install(|| {
                            part.run_sequence(&tokens, inputs.as_deref(), 3, Selection::Rp64Argmax)
                        })
                        .unwrap();
                    if layer + 1 == c.n_layers {
                        assert_eq!(out.hidden, reference.hidden);
                        assert_eq!(out.logits_hashes, reference.logits_hashes);
                        assert_eq!(out.derived, reference.derived);
                    }
                    inputs = Some(out.hidden);
                }
            }
            let pinned: serde_json::Value = serde_json::from_str(include_str!(
                "../../../../../docs/protocol/reference/int16-fixture-goldens.json"
            ))
            .unwrap();
            assert_eq!(pinned[name][0], package_hash);
            assert_eq!(pinned[name][1], golden.as_ref().unwrap().as_str());
            assert_eq!(pinned[name][2], manifest["model_root"]);
            println!(
                "golden {name}: package {package_hash} run {} root {}",
                golden.unwrap(),
                manifest["model_root"]
            );
            // Published expert interpretation is unaffected by BF16 precision.
            let mut legacy = c.clone();
            legacy.precision = None;
            let old =
                StageModel::from_owned(tiny_package(&legacy, StageSpec::full(&legacy))).unwrap();
            let legacy_manifest = package::build_manifest(
                &legacy,
                &old.header.source,
                &old.segments(),
                None,
                &[],
                &serde_json::Value::Null,
            )
            .unwrap();
            assert!(
                package::verify_against_manifest(
                    &model.header,
                    &model.segments(),
                    &serde_json::to_vec(&legacy_manifest).unwrap()
                )
                .is_err()
            );
            for e in &model.header.entries {
                if e.name.contains(".experts.")
                    || e.name.contains("router")
                    || e.name.contains("norm")
                {
                    let old_e = old.header.entry(&e.name).unwrap();
                    assert_eq!(
                        &model.bytes()[model.header.range(e)],
                        &old.bytes()[old.header.range(old_e)]
                    );
                }
            }
        }
        crate::canonical_simd::set_fast_canonical_kernel(false);
    }

    #[test]
    fn mixed_precision_packages_reject_mislabeled_and_invalid_payloads() {
        use super::super::precision::Precision;
        let mut c = tiny_config_with(true, ExpertFormat::Int4G32);
        c.precision = Some(Precision::all_int16());
        let bytes = tiny_package(&c, StageSpec::full(&c));
        let model = StageModel::from_owned(bytes.clone()).unwrap();
        for tensor in [
            "embed.q",
            "layers.0.wq_a.q",
            "layers.0.w_gate.q",
            "layers.1.shared.w_down.q",
            "lm_head.q",
        ] {
            let range = model.header.range(model.header.entry(tensor).unwrap());
            let mut bad = bytes.clone();
            bad[range.start..range.start + 2].copy_from_slice(&i16::MIN.to_le_bytes());
            assert!(StageModel::from_owned(bad).is_err(), "{tensor}");
        }
        let entries = &model.header.entries;
        for kind in [
            "profile",
            "precision",
            "dtype",
            "unknown",
            "version",
            "zero-scale",
        ] {
            let mut h = model.header.value.clone();
            match kind {
                "profile" => h["profile"] = super::super::PROFILE_I4G32.into(),
                "precision" => h["model"]["precision"]["head"] = "int8".into(),
                "dtype" => h["tensors"][2]["dtype"] = "I8".into(),
                "unknown" => h["model"]["precision"]["experts"] = "int16".into(),
                "version" => h["model"]["precision"]["version"] = 2.into(),
                _ => {
                    let mut bad = bytes.clone();
                    let r = model.header.range(model.header.entry("embed.mu").unwrap());
                    bad[r.start..r.start + 4].fill(0);
                    let r = model.header.range(model.header.entry("embed.k").unwrap());
                    bad[r.start] = 16;
                    assert!(StageModel::from_owned(bad).is_err());
                    continue;
                }
            }
            let text = crate::model_package::canonical_json(&h).unwrap();
            let mut prefix = super::super::STAGE_MAGIC.to_vec();
            prefix.extend_from_slice(&(text.len() as u64).to_le_bytes());
            prefix.extend_from_slice(text.as_bytes());
            let last = entries.last().unwrap();
            let len = package::align_up(prefix.len() as u64)
                + package::align_up(last.offset + last.bytes);
            assert!(package::parse_header(&prefix, len).is_err(), "{kind}");
        }
        assert!(StageModel::from_owned(bytes[..bytes.len() - 1].to_vec()).is_err());
    }

    #[test]
    fn yarn_synthetic_package_engine_and_manifest_are_exact() {
        use super::super::yarn::{self, Preparation, Scope};
        let mut c = tiny_config_with(true, ExpertFormat::Int4G32);
        c.architecture = "arc-test/kimi-k26-yarn".into();
        c.qk_nope_dim = 128;
        c.qk_rope_dim = 64;
        c.attention_lambda = yarn::ATTENTION_LAMBDA;
        c.preparation = Some(Preparation {
            scope: Scope::SyntheticFixture,
        });
        c.validate().unwrap();
        let bytes = tiny_package(&c, StageSpec::full(&c));
        let digest = blake3::hash(&bytes).to_hex().to_string();
        assert_eq!(
            digest,
            "c482e37976e2e950e5dd853d0458a2184faaf65c5be83bf10b59d82476699810"
        );
        let model = StageModel::from_owned(bytes.clone()).unwrap();
        let segments = model.segments();
        let manifest = yarn::finalize_manifest(
            &c,
            &model.header.source,
            &segments[1..],
            &[],
            &serde_json::Value::Null,
        )
        .unwrap();
        package::verify_against_manifest(
            &model.header,
            &segments,
            serde_json::to_string(&manifest).unwrap().as_bytes(),
        )
        .unwrap();
        assert_eq!(
            manifest["segments"][0],
            yarn::tables_digest(&c).unwrap().to_json()
        );
        let request = GenerationRequest {
            prompt: &[3, 17, 5],
            max_tokens: 4,
            eos: &[],
            selection: Selection::Rp64Argmax,
        };
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let mut golden = None;
        for (fast, threads) in [(false, 1), (false, 3), (true, 2)] {
            crate::canonical_simd::set_fast_canonical_kernel(fast);
            if fast {
                assert!(crate::canonical_simd::fast_canonical_kernel_enabled());
            }
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let run = pool.install(|| model.generate(&request)).unwrap();
            let mut h = blake3::Hasher::new();
            for t in &run.tokens {
                h.update(&t.to_le_bytes());
            }
            for x in run.logits_hashes.iter().chain(&run.boundary_digests) {
                h.update(x);
            }
            let got = h.finalize().to_hex().to_string();
            if let Some(want) = &golden {
                assert_eq!(&got, want);
            } else {
                golden = Some(got);
            }
        }
        crate::canonical_simd::set_fast_canonical_kernel(false);
        let run_digest = golden.unwrap();
        assert_eq!(
            run_digest,
            "cb534beca94b40578c1daa226103e8cf0a62d58bdff2636620b5b9c49d9ac92c"
        );
        assert_eq!(
            manifest["model_root"],
            "585aa1317c6c5c56ef5ceb582324b994f8439bfb482ac8207decd658cc5a7c72"
        );
        println!(
            "yarn synthetic package {digest} run {run_digest} root {}",
            manifest["model_root"]
        );
        let mut wrong_tables = bytes.clone();
        let table = model.header.entry("rope.cos").unwrap();
        wrong_tables[model.header.range(table).start] ^= 1;
        assert!(StageModel::from_owned(wrong_tables).is_err());
        // Reject a profile downgrade without relying on the manifest verifier.
        let mut header = model.header.value.clone();
        header["profile"] = super::super::PROFILE_I4G32.into();
        let mut downgraded = bytes;
        let text = crate::model_package::canonical_json(&header).unwrap();
        let len = text.len() as u64;
        // Header length changes; construct just a header. Rejection must occur
        // for the profile/preparation mismatch before reading tensor payloads.
        downgraded.truncate(16);
        downgraded[8..16].copy_from_slice(&len.to_le_bytes());
        downgraded.extend_from_slice(text.as_bytes());
        assert!(StageModel::from_owned(downgraded).is_err());
    }

    /// The four INT16 fixtures of `int16-fixture-goldens.json`, configured as
    /// `mixed_precision_goldens_and_split_stages` configures them (the tests
    /// below check the package hashes against the pinned file).
    fn int16_fixtures() -> Vec<(&'static str, MlaConfig)> {
        use super::super::precision::{Bits, Precision};
        use super::super::yarn::{ATTENTION_LAMBDA, Preparation, Scope};
        [
            ("int16-dense", true, false, false),
            ("int16-moe", false, false, true),
            ("mixed-moe", false, true, true),
            ("int16-yarn-moe", false, false, true),
        ]
        .into_iter()
        .map(|(name, dense_only, mixed, lora)| {
            let mut c = tiny_config_with(lora, ExpertFormat::Int4G32);
            if dense_only {
                c.n_layers = 1;
            }
            let mut precision = Precision::all_int16();
            if mixed {
                precision.head = Bits::Int8;
                precision.dense = Bits::Int8;
            }
            c.precision = Some(precision);
            if name == "int16-yarn-moe" {
                c.architecture = "arc-test/kimi-k26-yarn".into();
                c.qk_nope_dim = 128;
                c.qk_rope_dim = 64;
                c.attention_lambda = ATTENTION_LAMBDA;
                c.preparation = Some(Preparation {
                    scope: Scope::SyntheticFixture,
                });
            }
            c.validate().unwrap();
            (name, c)
        })
        .collect()
    }

    /// Every dyadic matrix of a stage by package name: embedding, attention,
    /// dense or shared FFN, head. Routed experts are never INT16.
    fn dyadic_matrices(model: &StageModel) -> Vec<(String, &MatRef)> {
        let mut all = Vec::new();
        if let Some(embed) = &model.embed {
            all.push(("embed".to_string(), embed));
        }
        for (layer, w) in model.stage.layers().zip(&model.layers) {
            let p = format!("layers.{layer}");
            match &w.query {
                QueryProjection::Direct(m) => all.push((format!("{p}.wq"), m)),
                QueryProjection::Lora { a, b, .. } => {
                    all.push((format!("{p}.wq_a"), a));
                    all.push((format!("{p}.wq_b"), b));
                }
            }
            for (name, m) in [
                ("wkv_a", &w.wkv_a),
                ("wk_b", &w.wk_b),
                ("wv_b", &w.wv_b),
                ("wo", &w.wo),
            ] {
                all.push((format!("{p}.{name}"), m));
            }
            let (prefix, ffn) = match &w.ffn {
                FfnWeights::Dense(dense) => ("", &**dense),
                FfnWeights::Moe(moe) => ("shared.", &moe.shared),
            };
            for (name, m) in ["w_gate", "w_up", "w_down"].into_iter().zip(ffn) {
                all.push((format!("{p}.{prefix}{name}"), m));
            }
        }
        if let Some(head) = &model.head {
            all.push(("lm_head".to_string(), &head.lm_head));
        }
        all
    }

    /// Every INT16 view of the four pinned INT16 fixtures, every stack
    /// element: the bytes the loader admitted pass a fresh -32768 scan, and
    /// the projection gives the legacy kernel's bytes, serial and on 1, 2 and
    /// N threads, scalar and SIMD, for ordinary inputs, the SIMD digit-domain
    /// edges and an input just past them.
    #[test]
    fn int16_fixture_projections_match_the_legacy_kernel() {
        use super::super::precision::tests::{Case, thread_pools};
        use crate::canonical_simd::{LIMB_MAX, LIMB_MIN};
        let pinned: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../../docs/protocol/reference/int16-fixture-goldens.json"
        ))
        .unwrap();
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let pools = thread_pools();
        let mut rng = Lcg(156);
        let mut views = 0;
        for (name, c) in int16_fixtures() {
            let bytes = tiny_package(&c, StageSpec::full(&c));
            let package_hash = blake3::hash(&bytes).to_hex().to_string();
            assert_eq!(pinned[name][0], package_hash, "{name}");
            let model = StageModel::from_owned(bytes).unwrap();
            let data = model.bytes();
            for (label, m) in dyadic_matrices(&model) {
                if !m.wide {
                    continue;
                }
                for index in 0..m.mu.len() / m.rows {
                    let view = m.view(data, index);
                    let q = view.q16.expect("an INT16 view").as_bytes();
                    let what = format!("{name} {label}[{index}]");
                    assert!(I16Weights::new(q).is_ok(), "{what}");
                    let cols = view.cols;
                    let typical: Vec<i64> = (0..cols)
                        .map(|_| (rng.next() % (1 << 21)) as i64 - (1 << 20))
                        .collect();
                    let edges: Vec<i64> = (0..cols)
                        .map(|j| match j % 3 {
                            0 => LIMB_MAX,
                            1 => LIMB_MIN,
                            _ => 7,
                        })
                        .collect();
                    let mut outside = typical.clone();
                    outside[cols / 2] = LIMB_MIN - 1;
                    for x in [&typical, &edges, &outside] {
                        let case = Case {
                            q,
                            rows: view.rows,
                            cols,
                            mu: view.mu,
                            k: view.k,
                            x,
                        };
                        case.assert_matches_legacy(&pools, &what);
                        assert!(case.legacy().is_ok(), "{what}");
                    }
                    views += 1;
                }
            }
        }
        // 10 + 38 + 34 + 38 INT16 matrices; each per-head `wk_b`/`wv_b` stack
        // holds 4 views: 16 + 62 + 58 + 62.
        assert_eq!(views, 198);
    }

    /// The one-time -32768 check is complete: one -32768 at the first, a
    /// middle or the last weight of any INT16 matrix tensor of the four
    /// fixtures (every class and every stack element) refuses the package,
    /// whole or as any stage that executes the tensor. A stage that does not
    /// execute a tensor never reads it, so it can never view it either.
    #[test]
    fn int16_minimum_is_refused_at_load_in_every_matrix() {
        let minimum = i16::MIN.to_le_bytes();
        let mut tensors = 0;
        for (name, c) in int16_fixtures() {
            let bytes = tiny_package(&c, StageSpec::full(&c));
            let model = StageModel::from_owned(bytes.clone()).unwrap();
            for e in &model.header.entries {
                if e.dtype != package::Dtype::I16
                    || !e.name.ends_with(".q")
                    || e.name.contains("router")
                {
                    continue;
                }
                let matrix = e.name.strip_suffix(".q").unwrap();
                let range = model.header.range(e);
                let weights = range.len() / 2;
                for at in [0, weights / 2, weights - 1] {
                    let mut bad = bytes.clone();
                    let byte = range.start + 2 * at;
                    bad[byte..byte + 2].copy_from_slice(&minimum);
                    let Err(err) = StageModel::from_owned(bad) else {
                        panic!("{name}: -32768 at weight {at} of {} was admitted", e.name);
                    };
                    let err = err.to_string();
                    assert!(
                        err.contains(&format!("{matrix}: weight minimum is not a profile value")),
                        "{name} {} at {at}: {err}",
                        e.name
                    );
                }
                tensors += 1;
            }
        }
        // 10 + 38 + 34 + 38: every INT16 matrix tensor of the four fixtures.
        assert_eq!(tensors, 120);
        // A mapped package, opened whole or as stages (`open_range`).
        let (_, c) = int16_fixtures()
            .into_iter()
            .find(|(name, _)| *name == "int16-moe")
            .unwrap();
        let mut bytes = tiny_package(&c, StageSpec::full(&c));
        let model = StageModel::from_owned(bytes.clone()).unwrap();
        let range = model
            .header
            .range(model.header.entry("layers.2.wo.q").unwrap());
        bytes[range.end - 2..range.end].copy_from_slice(&minimum);
        let path = std::env::temp_dir().join(format!(
            "arc-mla-int16-minimum-{}.arcspkg",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        let open = |first_layer, end_layer| {
            StageModel::open_range(
                &path,
                Some(StageSpec {
                    first_layer,
                    end_layer,
                }),
            )
        };
        assert!(StageModel::open(&path).is_err());
        assert!(open(2, 3).is_err());
        assert!(open(1, 4).is_err());
        assert!(open(0, 2).is_ok());
        assert!(open(3, 4).is_ok());
        std::fs::remove_file(&path).unwrap();
    }

    /// The heads of INT16 layers run in parallel or one after another, on 1,
    /// 2 and N threads, scalar and SIMD: every generation equals the pinned
    /// fixture golden (tokens, every logits hash, every boundary digest).
    #[test]
    fn int16_heads_in_parallel_keep_the_pinned_goldens() {
        use super::super::precision::tests::thread_pools;
        // A K2.6 layer's heads (64 x (512 x 128 + 128 x 512) weights) are
        // parallel by size; a tiny fixture's are not, so both are forced here.
        assert_eq!(
            Schedule::for_work(64 * (512 * 128 + 128 * 512)),
            Schedule::Pool
        );
        let pinned: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../../docs/protocol/reference/int16-fixture-goldens.json"
        ))
        .unwrap();
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let pools = thread_pools();
        let request = GenerationRequest {
            prompt: &[3, 17, 5, 49, 0],
            max_tokens: 8,
            eos: &[],
            selection: Selection::Rp64Argmax,
        };
        for (name, c) in int16_fixtures() {
            let mut model = StageModel::from_owned(tiny_package(&c, StageSpec::full(&c))).unwrap();
            assert_eq!(model.head_schedule(&model.layers[0]), Schedule::Serial);
            for forced in [Schedule::Serial, Schedule::Pool] {
                model.forced_head_schedule = Some(forced);
                assert_eq!(model.head_schedule(&model.layers[0]), forced);
                for fast in [false, true] {
                    crate::canonical_simd::set_fast_canonical_kernel(fast);
                    for (threads, pool) in &pools {
                        let run = pool.install(|| model.generate(&request)).unwrap();
                        let mut h = blake3::Hasher::new();
                        for t in &run.tokens {
                            h.update(&t.to_le_bytes());
                        }
                        for d in run.logits_hashes.iter().chain(&run.boundary_digests) {
                            h.update(d);
                        }
                        assert_eq!(
                            pinned[name][1],
                            h.finalize().to_hex().to_string(),
                            "{name}: heads {forced:?}, fast {fast}, {threads} threads"
                        );
                    }
                }
            }
        }
        crate::canonical_simd::set_fast_canonical_kernel(false);
    }

    /// The head driver gives the same blocks on every schedule and thread
    /// count and, on failure, the lowest failing head's error, as the serial
    /// loop does.
    #[test]
    fn for_each_head_is_schedule_free_and_reports_the_lowest_failing_head() {
        use super::super::precision::tests::thread_pools;
        let fill = |head: usize, block: &mut [i64]| -> Result<(), ModernError> {
            for (i, v) in block.iter_mut().enumerate() {
                *v = (head * 1000 + i) as i64;
            }
            Ok(())
        };
        let fail = |head: usize, block: &mut [i64]| -> Result<(), ModernError> {
            block.fill(head as i64);
            match head {
                3 => Err(ModernError::Domain("head 3".into())),
                7 => Err(ModernError::Invalid("head 7".into())),
                _ => Ok(()),
            }
        };
        let mut want = vec![0i64; 40];
        for_each_head(&mut want, 4, Schedule::Serial, fill).unwrap();
        assert_eq!(want[37], 9001);
        for (threads, pool) in &thread_pools() {
            for schedule in [Schedule::Serial, Schedule::Pool] {
                let mut got = vec![0i64; 40];
                pool.install(|| for_each_head(&mut got, 4, schedule, fill))
                    .unwrap();
                assert_eq!(got, want, "{schedule:?}, {threads} threads");
                let mut out = vec![0i64; 40];
                let err = pool
                    .install(|| for_each_head(&mut out, 4, schedule, fail))
                    .unwrap_err();
                assert_eq!(
                    err.to_string(),
                    "out of the profile's domain: head 3",
                    "{schedule:?}, {threads} threads"
                );
            }
        }
    }

    /// The hash a generation's goldens pin: its tokens, every logits hash and
    /// every boundary digest.
    fn generation_hash(run: &MlaGeneration) -> String {
        let mut h = blake3::Hasher::new();
        for t in &run.tokens {
            h.update(&t.to_le_bytes());
        }
        for d in run.logits_hashes.iter().chain(&run.boundary_digests) {
            h.update(d);
        }
        h.finalize().to_hex().to_string()
    }

    /// The four pinned INT16 goldens hold with the scalar reference and with
    /// each INT4 kernel this CPU has pinned for the routed experts, and the
    /// pinned kernel is the one that ran in the MoE fixtures.
    #[test]
    fn int16_goldens_hold_with_each_q4_kernel_pinned() {
        use super::super::q4_kernels::q4_kernel_runs;
        use super::super::q4_kernels::tests::{KernelRestore, pin_label, pins, use_pin};
        let pinned: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../../docs/protocol/reference/int16-fixture-goldens.json"
        ))
        .unwrap();
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let _restore = KernelRestore;
        let request = GenerationRequest {
            prompt: &[3, 17, 5, 49, 0],
            max_tokens: 8,
            eos: &[],
            selection: Selection::Rp64Argmax,
        };
        for (name, c) in int16_fixtures() {
            let model = StageModel::from_owned(tiny_package(&c, StageSpec::full(&c))).unwrap();
            let moe = c.n_layers > c.first_k_dense;
            for pin in pins() {
                let counter = use_pin(pin);
                let before = q4_kernel_runs(counter);
                let hash = generation_hash(&model.generate(&request).unwrap());
                println!("golden {name} with {}: {hash}", pin_label(pin));
                assert_eq!(pinned[name][1], hash, "{name}: {}", pin_label(pin));
                if moe {
                    assert!(
                        q4_kernel_runs(counter) > before,
                        "{name}: {} did not run",
                        pin_label(pin)
                    );
                }
            }
        }
    }

    /// k-row passes equal single-row passes value for value: boundary
    /// vectors, logits, the cache and its positions, for k = 1, 2, 3, 4, 8 and
    /// 16, after a two-row prefix, on the tiny INT8 and INT4 models and the
    /// four INT16 fixtures, with the scalar reference and each INT4 kernel.
    #[test]
    fn forward_rows_equal_single_row_passes() {
        use super::super::q4_kernels::tests::{KernelRestore, pin_label, pins, use_pin};
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let _restore = KernelRestore;
        let tokens: [u32; 20] = [
            3, 17, 5, 49, 0, 22, 8, 41, 2, 7, 7, 30, 11, 45, 9, 1, 13, 29, 4, 6,
        ];
        let mut configs: Vec<(String, MlaConfig)> = FORMATS
            .iter()
            .map(|&(lora, format)| {
                (
                    format!("lora {lora}, {format:?}"),
                    tiny_config_with(lora, format),
                )
            })
            .collect();
        configs.extend(
            int16_fixtures()
                .into_iter()
                .map(|(name, c)| (name.to_string(), c)),
        );
        for (name, c) in configs {
            assert!(tokens.len() <= c.max_seq, "{name}");
            let model = StageModel::from_owned(tiny_package(&c, StageSpec::full(&c))).unwrap();
            let moe_layers = (0..c.n_layers).filter(|&l| c.is_moe(l)).count();
            crate::canonical_simd::set_fast_canonical_kernel(false);
            let mut cache = model.new_cache();
            let single: Vec<(Vec<i64>, Option<Vec<i64>>)> = tokens
                .iter()
                .map(|&t| {
                    model
                        .forward(StageInput::Token(t), &mut cache, None)
                        .unwrap()
                })
                .collect();
            let digest = cache.digest();
            for pin in pins() {
                use_pin(pin);
                for k in [1usize, 2, 3, 4, 8, 16] {
                    let at = format!("{name}, {}, k = {k}", pin_label(pin));
                    let mut cache = model.new_cache();
                    for &t in &tokens[..2] {
                        model
                            .forward(StageInput::Token(t), &mut cache, None)
                            .unwrap();
                    }
                    let mut start = 2;
                    while start < tokens.len() {
                        let end = (start + k).min(tokens.len());
                        let inputs: Vec<StageInput<'_>> = tokens[start..end]
                            .iter()
                            .map(|&t| StageInput::Token(t))
                            .collect();
                        let pass = model.forward_rows(&inputs, &mut cache).unwrap();
                        assert_eq!(pass.hidden.len(), end - start, "{at}");
                        assert_eq!(pass.expert_union.len(), moe_layers, "{at}");
                        let logits = pass.logits.as_ref().expect("the whole model has a head");
                        for (row, (h, l)) in pass.hidden.iter().zip(logits).enumerate() {
                            let (want_h, want_l) = &single[start + row];
                            assert_eq!(h, want_h, "{at}: row {}", start + row);
                            assert_eq!(Some(l), want_l.as_ref(), "{at}: row {}", start + row);
                        }
                        assert_eq!(cache.positions(), end, "{at}");
                        start = end;
                    }
                    assert_eq!(cache.digest(), digest, "{at}");
                }
            }
        }
    }

    /// A pass that does not fit the context is refused, and so is a pass
    /// with no rows; a pass fills the context exactly.
    #[test]
    fn forward_rows_refuses_what_single_rows_refuse() {
        let c = tiny_config_with(false, ExpertFormat::Int4G32);
        let model = StageModel::from_owned(tiny_package(&c, StageSpec::full(&c))).unwrap();
        let mut cache = model.new_cache();
        assert!(model.forward_rows(&[], &mut cache).is_err());
        let rows: Vec<StageInput<'_>> = (0..=c.max_seq)
            .map(|t| StageInput::Token(t as u32 % 50))
            .collect();
        let err = model.forward_rows(&rows, &mut cache).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "out of the profile's domain: position {} is outside the {}-position context",
                c.max_seq, c.max_seq
            )
        );
        assert_eq!(cache.positions(), 0);
        let pass = model.forward_rows(&rows[..c.max_seq], &mut cache).unwrap();
        assert_eq!(pass.hidden.len(), c.max_seq);
        assert!(model.forward_rows(&rows[..1], &mut cache).is_err());
    }

    /// Kimi K2.6's MoE layer shape (`docs/protocol/reference/kimi-k26/config.json`):
    /// hidden width 7,168, expert width 2,048, 384 routed experts with 8 per
    /// token, one shared expert, one routing group, routing scale 2.827,
    /// normalised weights.
    fn k26_moe_config() -> MlaConfig {
        let mut c = tiny_config_with(false, ExpertFormat::Int4G32);
        c.d_model = 7168;
        c.moe_d_ff = 2048;
        c.n_routed_experts = 384;
        c.n_experts_per_tok = 8;
        c.n_shared_experts = 1;
        c.n_group = 1;
        c.topk_group = 1;
        c.norm_topk_prob = true;
        c.routed_scaling_q32 = super::super::ops::f32_to_q32(2.827f32.to_bits()).unwrap();
        c
    }

    /// One K2.6-shaped MoE layer with random weights.
    struct K26Layer {
        c: MlaConfig,
        bytes: Vec<u8>,
        moe: MoeWeights,
    }

    const K26_SEED: u64 = 0x0026_C0DE_0000_0001;

    /// `count` random FFN inputs of a K2.6 MoE layer: RMS-normed scale, every
    /// value in [-2^17, 2^17].
    fn k26_inputs(count: usize) -> Vec<Vec<i64>> {
        use super::super::q4_kernels::tests::Rng;
        let mut rng = Rng(K26_SEED ^ 0x1111);
        (0..count)
            .map(|_| {
                (0..7168)
                    .map(|_| (rng.next() % (1 << 18)) as i64 - (1 << 17))
                    .collect()
            })
            .collect()
    }

    /// One K2.6-shaped MoE layer with random weights, whose routed experts
    /// get values only where `rows` route. The three expert stacks (9.5 GB)
    /// are allocated zeroed, and the pages of experts no row chooses are never
    /// written; the router, its bias and the shared expert are drawn, every
    /// row is routed, and only the chosen experts' values and scales are then
    /// drawn. Unchosen experts never contribute, so for these rows this is a
    /// complete random layer, at about 25 MB per chosen expert.
    fn k26_layer(rows: &[Vec<i64>]) -> K26Layer {
        use super::super::q4_kernels::tests::Rng;
        let c = k26_moe_config();
        let (d, f, experts) = (c.d_model, c.moe_d_ff, c.n_routed_experts);
        let mut rng = Rng(K26_SEED);
        let shared_bytes = 3 * f * d;
        let stack = experts * f * d / 2;
        let mut bytes = vec![0u8; shared_bytes + 3 * stack];
        for b in &mut bytes[..shared_bytes] {
            *b = ((rng.next() % 255) as i64 - 127) as i8 as u8;
        }
        let mut dyadic = |start: usize, rows: usize, cols: usize| MatRef {
            rows,
            cols,
            q: start..start + rows * cols,
            wide: false,
            mu: (0..rows)
                .map(|_| ((1u64 << 30) + rng.next() % (1 << 30)) as i32)
                .collect(),
            k: (0..rows).map(|_| 42 + (rng.next() % 2) as u8).collect(),
        };
        let shared = [
            dyadic(0, f, d),
            dyadic(f * d, f, d),
            dyadic(2 * f * d, d, f),
        ];
        let stacked = |which: usize, rows: usize, cols: usize| Q4Ref {
            rows,
            cols,
            q4: shared_bytes + which * stack..shared_bytes + (which + 1) * stack,
            scales: vec![0u16; experts * rows * cols / Q4_GROUP],
        };
        let mut moe = MoeWeights {
            router_q: (0..experts * d)
                .map(|_| ((rng.next() % 65_535) as i64 - 32_767) as i16)
                .collect(),
            router_k: (0..experts).map(|_| 21 + (rng.next() % 3) as u8).collect(),
            bias: (0..experts)
                .map(|_| (rng.next() % (1 << 30)) as i64 - (1 << 29))
                .collect(),
            shared,
            experts: ExpertStacks::Int4([stacked(0, f, d), stacked(1, f, d), stacked(2, d, f)]),
        };
        let mut chosen = std::collections::BTreeSet::new();
        for x in rows {
            chosen.extend(moe_route(&c, &moe, x).unwrap().0);
        }
        let ExpertStacks::Int4(stacks) = &mut moe.experts else {
            unreachable!("the layer has INT4 experts");
        };
        for (which, stack_ref) in stacks.iter_mut().enumerate() {
            let per = stack_ref.rows * stack_ref.cols / 2;
            let groups = stack_ref.rows * stack_ref.cols / Q4_GROUP;
            for &e in &chosen {
                let mut rng = Rng(K26_SEED ^ ((e * 3 + which) as u64).wrapping_mul(0x9E37_79B9));
                let start = stack_ref.q4.start + e * per;
                for word in bytes[start..start + per].chunks_exact_mut(8) {
                    word.copy_from_slice(&rng.next().to_le_bytes());
                }
                // Positive BF16 scales of 2^-10 to 2^-8: activations of order one.
                for scale in &mut stack_ref.scales[e * groups..(e + 1) * groups] {
                    *scale = (((117 + rng.next() % 3) << 7) | (rng.next() % 128)) as u16;
                }
            }
        }
        K26Layer { c, bytes, moe }
    }

    /// One K2.6-shaped MoE layer at real widths (384 experts, top-8): k-row
    /// passes, k = 1, 2, 4, 8 and 16, equal single-row passes byte for byte,
    /// with the scalar reference and with each INT4 kernel this CPU has; and
    /// every kernel's single rows equal the scalar reference's. Release
    /// mode, Linux (the zeroed 9.5 GB stack relies on lazily committed pages).
    #[test]
    #[ignore = "K2.6-width MoE layer: run in release with --ignored (Linux, about 3 GB)"]
    fn k26_moe_layer_rows_equal_single_rows() {
        use super::super::q4_kernels::tests::{KernelRestore, pin_label, pins, use_pin};
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let _restore = KernelRestore;
        let rows = k26_inputs(16);
        let layer = k26_layer(&rows);
        let (c, data, moe) = (&layer.c, layer.bytes.as_slice(), &layer.moe);
        use_pin(super::super::q4_kernels::Q4Pin::Scalar);
        let reference: Vec<Vec<i64>> = rows
            .iter()
            .map(|x| moe_forward(c, data, moe, x).unwrap())
            .collect();
        for pin in pins() {
            use_pin(pin);
            let label = pin_label(pin);
            for (row, x) in rows.iter().enumerate() {
                assert_eq!(
                    moe_forward(c, data, moe, x).unwrap(),
                    reference[row],
                    "{label}: row {row}"
                );
            }
            for k in [1usize, 2, 4, 8, 16] {
                let (outs, union) = moe_forward_rows(c, data, moe, &rows[..k]).unwrap();
                assert_eq!(outs, reference[..k], "{label}: k = {k}");
                println!(
                    "k26 {label}: k = {k}, expert union {union} of {} choices",
                    8 * k
                );
            }
        }
    }

    /// CI-runner benchmark of one K2.6-shaped MoE layer: the per-token time
    /// of single-row passes, and the per-row cost of k-row passes against k,
    /// for the scalar reference (opt-in off), and each INT4 kernel this CPU
    /// has (opt-in on, shared expert on the INT8 limb kernel). Every
    /// measured pass is also checked against the scalar bytes.
    #[test]
    #[ignore = "benchmark: run in release with --ignored --nocapture"]
    fn k26_moe_layer_benchmark() {
        use super::super::q4_kernels::Q4Pin;
        use super::super::q4_kernels::tests::{KernelRestore, pin_label, pins, use_pin};
        const REPEATS: usize = 3;
        let median = |mut samples: Vec<f64>| {
            samples.sort_by(f64::total_cmp);
            samples[samples.len() / 2]
        };
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let _restore = KernelRestore;
        let rows = k26_inputs(16);
        let build = Instant::now();
        let layer = k26_layer(&rows);
        let build_seconds = build.elapsed().as_secs_f64();
        let (c, data, moe) = (&layer.c, layer.bytes.as_slice(), &layer.moe);
        use_pin(Q4Pin::Scalar);
        crate::canonical_simd::set_fast_canonical_kernel(false);
        let reference: Vec<Vec<i64>> = rows
            .iter()
            .map(|x| moe_forward(c, data, moe, x).unwrap())
            .collect();
        let ks = [1usize, 2, 4, 8, 16];
        let mut table = Vec::new();
        let mut unions = Vec::new();
        for pin in pins() {
            let label = pin_label(pin);
            use_pin(pin);
            if pin == Q4Pin::Scalar {
                // The whole layer scalar: the shared expert too.
                crate::canonical_simd::set_fast_canonical_kernel(false);
            }
            let single = median(
                (0..REPEATS)
                    .map(|_| {
                        let start = Instant::now();
                        for (row, x) in rows.iter().enumerate() {
                            assert_eq!(moe_forward(c, data, moe, x).unwrap(), reference[row]);
                        }
                        start.elapsed().as_secs_f64() / rows.len() as f64
                    })
                    .collect(),
            );
            let mut per_row = Vec::new();
            unions.clear();
            for &k in &ks {
                let mut union = 0;
                let seconds = median(
                    (0..REPEATS)
                        .map(|_| {
                            let start = Instant::now();
                            let (outs, experts) =
                                moe_forward_rows(c, data, moe, &rows[..k]).unwrap();
                            let seconds = start.elapsed().as_secs_f64();
                            assert_eq!(outs, reference[..k], "{label}: k = {k}");
                            union = experts;
                            seconds
                        })
                        .collect(),
                );
                per_row.push(seconds / k as f64);
                unions.push(union);
            }
            table.push((label, single, per_row));
        }
        let ms = |s: f64| s * 1e3;
        let mut md = String::new();
        md.push_str("### One K2.6-shaped MoE layer on CPU: INT4 routed-expert kernels\n\n");
        md.push_str(
            "CI-runner measurement on a shared hosted runner, not product speed. One synthetic \
             MoE layer at Kimi K2.6 widths (hidden 7,168, expert width 2,048, 384 routed \
             experts, 8 per token, one shared expert), random weights, 16 random rows; times are \
             medians of 3 runs, and every run's bytes are checked against the scalar \
             reference.\n\n",
        );
        md.push_str(&format!(
            "Layer built in {build_seconds:.1} s (only routed experts that the rows choose get values).\n\n"
        ));
        md.push_str("| Kernel | Single-row pass, ms per token |");
        for k in ks {
            md.push_str(&format!(" k = {k}, ms per row |"));
        }
        md.push_str("\n|---|---|");
        md.push_str(&"---|".repeat(ks.len()));
        md.push('\n');
        for (label, single, per_row) in &table {
            md.push_str(&format!("| {label} | {:.1} |", ms(*single)));
            for &cost in per_row {
                md.push_str(&format!(
                    " {:.1} ({:.2}x) |",
                    ms(cost),
                    cost / single.max(f64::MIN_POSITIVE)
                ));
            }
            md.push('\n');
        }
        md.push_str("\nDistinct routed experts per pass (each read once for all its rows):");
        for (&k, &union) in ks.iter().zip(&unions) {
            md.push_str(&format!(" k = {k}: {union} of {} choices;", 8 * k));
        }
        md.push_str(
            "\n\nThe bracketed figure is the k-row cost per row as a fraction of the same \
             kernel's single-row pass.\n",
        );
        println!("{md}");
        let json = serde_json::json!({
            "label": "CI-runner measurement",
            "build_seconds": build_seconds,
            "ks": ks,
            "expert_union": unions,
            "kernels": table
                .iter()
                .map(|(label, single, per_row)| serde_json::json!({
                    "kernel": label,
                    "single_row_ms_per_token": ms(*single),
                    "multi_row_ms_per_row": per_row.iter().map(|&s| ms(s)).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        });
        println!("MLA_Q4_BENCH {json}");
        if let Ok(path) = std::env::var("ARC_MLA_Q4_BENCH_MD") {
            std::fs::write(path, md).unwrap();
        }
    }
}
