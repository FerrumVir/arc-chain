//! Weight slices (spec §14): a checkpoint's converted weights as
//! content-addressed files, written one source shard at a time.
//!
//! A **unit** is one segment of the model other than `tables` (spec §4.7):
//! `embed`, `layer.l` or `head`. Each unit is converted with the same code as a
//! stage package ([`super::convert`]), through a [`SegmentSlicer`] instead of a
//! [`super::package::StageWriter`], so the bytes are the package's bytes. The
//! slicer writes them to one file per slice:
//!
//! * `embed`, `head` and a dense layer: one slice holding the whole segment;
//! * an MoE layer `l`: `layer.l.core` (everything but the routed experts) and
//!   `layer.l.experts.g` for `g < G`, the routed experts
//!   `[g·E/G, (g+1)·E/G)` of every expert tensor.
//!
//! A slice file holds its tensors' bytes back to back, without padding, and is
//! named by its BLAKE3. The segment digest of the unit is computed in the same
//! pass, in layout order, so the slice manifest also carries exactly the
//! digests a stage manifest pins.
//!
//! [`plan`] orders the units so that the source shards can be downloaded,
//! converted and deleted one at a time; [`build_manifest`] collects the unit
//! records into the slice manifest; [`verify_slices`] re-hashes slice files
//! against it; [`assemble_stage`] rebuilds a stage package from slices.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};

use super::config::{ExpertFormat, HfWeightsConfig, MlaConfig, parse_hf_weights_config};
use super::convert::{
    SourceKind, SourceTensors, convert_embed, convert_head, convert_layer, layer_source_tensors,
    stage_source_tensors_in,
};
use super::package::{
    self, CONTRACT, Dtype, Entry, SegmentDigest, StageSpec, StageWriter, TensorSink,
};
use crate::modern::convert::{SourceFile, SourceManifest, verify_source_file};
use crate::modern::package::i32_bytes;
use crate::modern::tables::rope_tables;
use crate::modern::{ModernError, hex_lower, identity_blake3};

/// Slice manifest schema.
mod prepared;
pub use prepared::{
    assemble_yarn_bundle, assemble_yarn_bundle_with_precision, prepare_yarn_manifest,
    prepare_yarn_manifest_with_precision,
};

pub const SLICE_MANIFEST_SCHEMA: &str = "arc.integer-slice-manifest.v1";
/// Unit record schema (one file per converted unit, under `units/`).
pub const SLICE_UNIT_SCHEMA: &str = "arc.integer-slice-unit.v1";
/// Conversion plan schema.
pub const SLICE_PLAN_SCHEMA: &str = "arc.integer-slice-plan.v1";
/// File name suffix of a slice: `<blake3 hex>.slice`.
pub const SLICE_SUFFIX: &str = ".slice";
/// Buffer per open slice file; an MoE layer keeps `1 + G` files open.
const SLICE_BUFFER: usize = 1 << 20;

fn invalid(message: impl Into<String>) -> ModernError {
    ModernError::Invalid(message.into())
}

fn canonical(value: &Value) -> Result<String, ModernError> {
    crate::model_package::canonical_json(value).map_err(|e| invalid(format!("canonical JSON: {e}")))
}

fn io(path: &Path, error: std::io::Error) -> ModernError {
    ModernError::io(&path.display().to_string(), error)
}

/// One segment of the model other than `tables` (spec §4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Unit {
    Embed,
    Layer(usize),
    Head,
}

impl Unit {
    /// The segment name: `embed`, `layer.l` or `head`.
    pub fn name(self) -> String {
        match self {
            Unit::Embed => "embed".into(),
            Unit::Layer(l) => package::layer_segment(l),
            Unit::Head => "head".into(),
        }
    }

    pub fn parse(text: &str) -> Result<Self, ModernError> {
        match text {
            "embed" => Ok(Unit::Embed),
            "head" => Ok(Unit::Head),
            _ => text
                .strip_prefix("layer.")
                .and_then(|l| l.parse().ok())
                .map(Unit::Layer)
                .ok_or_else(|| invalid(format!("{text:?} is not a unit (embed, layer.N, head)"))),
        }
    }

    /// A one-layer stage whose layout contains this segment.
    fn stage(self, c: &MlaConfig) -> StageSpec {
        let l = match self {
            Unit::Embed => 0,
            Unit::Layer(l) => l,
            Unit::Head => c.n_layers - 1,
        };
        StageSpec {
            first_layer: l,
            end_layer: l + 1,
        }
    }

    /// The segment's layout entries (spec §4.6), offsets as in that stage.
    pub fn entries(self, c: &MlaConfig) -> Vec<Entry> {
        let name = self.name();
        package::layout(c, self.stage(c))
            .into_iter()
            .filter(|e| e.segment == name)
            .collect()
    }

    /// Whether this unit's routed experts are split into expert groups.
    fn is_moe(self, c: &MlaConfig) -> bool {
        matches!(self, Unit::Layer(l) if c.is_moe(l))
    }

    /// The source tensors the unit reads (spec §4.1, §14.1).
    pub fn source_tensors(
        self,
        c: &MlaConfig,
        hf: &HfWeightsConfig,
    ) -> BTreeMap<String, (Vec<usize>, SourceKind)> {
        match self {
            Unit::Layer(l) => {
                let mut out = BTreeMap::new();
                layer_source_tensors(c, &hf.source, l, &mut out);
                out
            }
            Unit::Embed => {
                let stage = self.stage(c);
                let mut all = stage_source_tensors_in(c, &hf.source, stage);
                let keep = format!("{}model.embed_tokens.weight", hf.source.prefix);
                all.retain(|name, _| *name == keep);
                all
            }
            Unit::Head => {
                let stage = self.stage(c);
                let mut all = stage_source_tensors_in(c, &hf.source, stage);
                let p = &hf.source.prefix;
                let keep = [
                    format!("{p}model.norm.weight"),
                    format!("{p}lm_head.weight"),
                ];
                all.retain(|name, _| keep.contains(name));
                all
            }
        }
    }
}

/// Every unit of the model, in canonical segment order.
pub fn all_units(c: &MlaConfig) -> Vec<Unit> {
    let mut units = vec![Unit::Embed];
    units.extend((0..c.n_layers).map(Unit::Layer));
    units.push(Unit::Head);
    units
}

/// The units a selection names: layers `[a, b)` plus `embed` and `head` when
/// asked; every unit when nothing is selected.
pub fn select_units(
    c: &MlaConfig,
    layers: Option<StageSpec>,
    embed: bool,
    head: bool,
) -> Result<Vec<Unit>, ModernError> {
    if layers.is_none() && !embed && !head {
        return Ok(all_units(c));
    }
    let mut units = Vec::new();
    if embed {
        units.push(Unit::Embed);
    }
    if let Some(stage) = layers {
        stage.validate(c)?;
        units.extend(stage.layers().map(Unit::Layer));
    }
    if head {
        units.push(Unit::Head);
    }
    Ok(units)
}

/// A pinned source, its `config.json` read for its weights, and the routed
/// expert format the slices use.
pub struct SliceSource {
    pub manifest: SourceManifest,
    /// The raw source manifest (for the optional `index` entry).
    pub raw: Value,
    pub hf: HfWeightsConfig,
    pub config: MlaConfig,
    pub expert_groups: usize,
}

impl SliceSource {
    /// Verify `config.json` in `dir` against the source manifest and read it.
    /// `experts` defaults to i4g32 for pre-quantised experts and i8 otherwise.
    pub fn open(
        dir: &Path,
        manifest_path: &Path,
        experts: Option<ExpertFormat>,
        expert_groups: usize,
    ) -> Result<Self, ModernError> {
        let bytes = std::fs::read(manifest_path).map_err(|e| io(manifest_path, e))?;
        let manifest = SourceManifest::parse(&bytes)?;
        let raw: Value = serde_json::from_slice(&bytes)
            .map_err(|e| invalid(format!("source manifest JSON: {e}")))?;
        let entry = manifest
            .files
            .iter()
            .find(|f| f.name == "config.json")
            .ok_or_else(|| invalid("source manifest does not pin config.json"))?;
        let path = verify_source_file(dir, entry)?;
        let config_bytes = std::fs::read(&path).map_err(|e| io(&path, e))?;
        let hf = parse_hf_weights_config(&config_bytes, manifest.max_seq)?;
        let mut config = hf.config.clone();
        config.expert_format = experts.unwrap_or(if hf.source.packed_experts {
            ExpertFormat::Int4G32
        } else {
            ExpertFormat::Int8Dyadic
        });
        if hf.source.packed_experts && config.expert_format != ExpertFormat::Int4G32 {
            return Err(invalid(
                "pre-quantised INT4 experts are stored as i4g32, never requantised",
            ));
        }
        config.validate()?;
        if expert_groups == 0 || !config.n_routed_experts.is_multiple_of(expert_groups) {
            return Err(invalid(format!(
                "{expert_groups} expert groups do not divide {} routed experts",
                config.n_routed_experts
            )));
        }
        Ok(Self {
            manifest,
            raw,
            hf,
            config,
            expert_groups,
        })
    }

    /// Select a complete policy before planning, conversion or record hashing.
    pub fn with_precision(
        mut self,
        precision: Option<super::precision::Precision>,
    ) -> Result<Self, ModernError> {
        self.config.precision = precision;
        self.config.validate()?;
        Ok(self)
    }

    /// The pinned `model.safetensors.index.json`, if the manifest has one.
    pub fn index_entry(&self) -> Result<Option<SourceFile>, ModernError> {
        let Some(value) = self.raw.get("index") else {
            return Ok(None);
        };
        let bad = || invalid("source manifest index entry is malformed");
        let name = value.get("name").and_then(Value::as_str).ok_or_else(bad)?;
        if name.contains('/') || name.contains('\\') || name.starts_with('.') {
            return Err(bad());
        }
        Ok(Some(SourceFile {
            name: name.to_string(),
            bytes: value.get("bytes").and_then(Value::as_u64).ok_or_else(bad)?,
            sha256: value
                .get("sha256")
                .and_then(Value::as_str)
                .ok_or_else(bad)?
                .to_ascii_lowercase(),
        }))
    }

    /// The verified index's `weight_map` (tensor name to shard name).
    pub fn weight_map(&self, dir: &Path) -> Result<Option<BTreeMap<String, String>>, ModernError> {
        let Some(entry) = self.index_entry()? else {
            return Ok(None);
        };
        let path = verify_source_file(dir, &entry)?;
        let value: Value = serde_json::from_slice(&std::fs::read(&path).map_err(|e| io(&path, e))?)
            .map_err(|e| invalid(format!("{}: {e}", entry.name)))?;
        let map = value
            .get("weight_map")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid(format!("{} has no weight_map", entry.name)))?;
        map.iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_string()))
                    .ok_or_else(|| invalid(format!("{}: {k} has no shard", entry.name)))
            })
            .collect::<Result<_, _>>()
            .map(Some)
    }

    /// What every unit record and the manifest are bound to: the profile,
    /// the source and the expert grouping.
    fn context(&self) -> Value {
        let mut value = json!({
            "profile": self.config.profile(),
            "source_blake3": blake3::hash(canonical(&self.manifest.header_json()).unwrap_or_default().as_bytes()).to_hex().to_string(),
            "expert_groups": self.expert_groups,
        });
        if let Some(p) = &self.config.precision {
            value["precision"] = serde_json::to_value(p).unwrap();
        }
        value
    }

    /// The safetensors shards of the manifest that hold `names`; every shard
    /// present in `dir` when the manifest pins no index.
    fn shards_for(
        &self,
        map: Option<&BTreeMap<String, String>>,
        names: impl Iterator<Item = String>,
        dir: &Path,
    ) -> Result<Vec<String>, ModernError> {
        let order: Vec<&str> = self
            .manifest
            .files
            .iter()
            .filter(|f| f.name.ends_with(".safetensors"))
            .map(|f| f.name.as_str())
            .collect();
        let wanted: BTreeSet<String> = match map {
            Some(map) => names
                .map(|n| {
                    map.get(&n)
                        .cloned()
                        .ok_or_else(|| invalid(format!("source tensor {n} is not in the index")))
                })
                .collect::<Result<_, _>>()?,
            None => order
                .iter()
                .filter(|n| dir.join(n).exists())
                .map(|n| n.to_string())
                .collect(),
        };
        if let Some(stray) = wanted.iter().find(|n| !order.contains(&n.as_str())) {
            return Err(invalid(format!(
                "the index names {stray}, which the source manifest does not pin"
            )));
        }
        Ok(order
            .into_iter()
            .filter(|n| wanted.contains(*n))
            .map(str::to_string)
            .collect())
    }
}

/// The order in which to convert `units` so that each source shard is
/// downloaded once and deleted as soon as no later step needs it.
pub fn plan(src: &SliceSource, dir: &Path, units: &[Unit]) -> Result<Value, ModernError> {
    let map = src.weight_map(dir)?;
    let sizes: BTreeMap<&str, u64> = src
        .manifest
        .files
        .iter()
        .map(|f| (f.name.as_str(), f.bytes))
        .collect();
    let position: BTreeMap<&str, usize> = src
        .manifest
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    // Units with the same shard set form one step; steps run in the order of
    // their first shard, then of their first unit.
    let mut steps: Vec<(Vec<String>, Vec<Unit>)> = Vec::new();
    for &unit in units {
        let names = unit.source_tensors(&src.config, &src.hf).into_keys();
        let shards = src.shards_for(map.as_ref(), names, dir)?;
        match steps.iter_mut().find(|(s, _)| *s == shards) {
            Some((_, list)) => list.push(unit),
            None => steps.push((shards, vec![unit])),
        }
    }
    steps
        .sort_by_key(|(shards, list)| (shards.iter().map(|s| position[s.as_str()]).min(), list[0]));
    let mut last_use: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, (shards, _)) in steps.iter().enumerate() {
        for s in shards {
            last_use.insert(s.as_str(), i);
        }
    }
    let mut held: BTreeSet<&str> = BTreeSet::new();
    let mut peak = 0u64;
    let mut out = Vec::new();
    for (i, (shards, list)) in steps.iter().enumerate() {
        held.extend(shards.iter().map(String::as_str));
        peak = peak.max(held.iter().map(|s| sizes[s]).sum());
        let release: Vec<&str> = held.iter().copied().filter(|s| last_use[s] == i).collect();
        for s in &release {
            held.remove(s);
        }
        out.push(json!({
            "units": list.iter().map(|u| u.name()).collect::<Vec<_>>(),
            "shards": shards,
            "release": release,
            "source_bytes": shards.iter().map(|s| sizes[s.as_str()]).sum::<u64>(),
        }));
    }
    let mut value = json!({
        "schema": SLICE_PLAN_SCHEMA,
        "profile": src.config.profile(),
        "expert_groups": src.expert_groups,
        "indexed": map.is_some(),
        "steps": out,
        "peak_source_bytes": peak,
    });
    if let Some(p) = &src.config.precision {
        value["precision"] = serde_json::to_value(p).unwrap();
    }
    Ok(value)
}

/// One tensor's place in a slice file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceTensor {
    pub name: String,
    pub dtype: Dtype,
    pub shape: Vec<usize>,
    pub offset: u64,
    pub bytes: u64,
}

impl SliceTensor {
    fn to_json(&self) -> Value {
        json!({"name": self.name, "dtype": self.dtype.name(), "shape": self.shape,
               "offset": self.offset, "bytes": self.bytes})
    }

    fn from_json(value: &Value) -> Result<Self, ModernError> {
        let bad = || invalid("slice tensor entry is malformed");
        let dtype = match value.get("dtype").and_then(Value::as_str).ok_or_else(bad)? {
            "i8" => Dtype::I8,
            "u8" => Dtype::U8,
            "i16" => Dtype::I16,
            "u16" => Dtype::U16,
            "i32" => Dtype::I32,
            "i64" => Dtype::I64,
            _ => return Err(bad()),
        };
        Ok(Self {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(bad)?
                .into(),
            dtype,
            shape: value
                .get("shape")
                .and_then(Value::as_array)
                .ok_or_else(bad)?
                .iter()
                .map(|v| v.as_u64().map(|v| v as usize))
                .collect::<Option<_>>()
                .ok_or_else(bad)?,
            offset: value
                .get("offset")
                .and_then(Value::as_u64)
                .ok_or_else(bad)?,
            bytes: value.get("bytes").and_then(Value::as_u64).ok_or_else(bad)?,
        })
    }
}

/// One slice: its name, content address, size, expert range and tensors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceRecord {
    pub name: String,
    pub segment: String,
    pub blake3: String,
    pub bytes: u64,
    /// `[first, end)` of the routed experts it holds, for expert slices.
    pub experts: Option<(usize, usize)>,
    pub tensors: Vec<SliceTensor>,
}

impl SliceRecord {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "segment": self.segment,
            "blake3": self.blake3,
            "bytes": self.bytes,
            "experts": self.experts.map(|(a, b)| json!([a, b])).unwrap_or(Value::Null),
            "tensors": self.tensors.iter().map(SliceTensor::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(value: &Value) -> Result<Self, ModernError> {
        let bad = || invalid("slice entry is malformed");
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(bad)
        };
        let experts = match value.get("experts") {
            None | Some(Value::Null) => None,
            Some(Value::Array(pair)) if pair.len() == 2 => Some((
                pair[0].as_u64().ok_or_else(bad)? as usize,
                pair[1].as_u64().ok_or_else(bad)? as usize,
            )),
            _ => return Err(bad()),
        };
        let blake3 = text("blake3")?;
        if blake3.len() != 64 || !blake3.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        Ok(Self {
            name: text("name")?,
            segment: text("segment")?,
            blake3,
            bytes: value.get("bytes").and_then(Value::as_u64).ok_or_else(bad)?,
            experts,
            tensors: value
                .get("tensors")
                .and_then(Value::as_array)
                .ok_or_else(bad)?
                .iter()
                .map(SliceTensor::from_json)
                .collect::<Result<_, _>>()?,
        })
    }

    /// The slice file under `dir`.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}{SLICE_SUFFIX}", self.blake3))
    }
}

/// A converted unit: its segment digest and its slices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRecord {
    pub unit: Unit,
    pub segment: SegmentDigest,
    pub slices: Vec<SliceRecord>,
}

struct SliceOut {
    record: SliceRecord,
    tmp: PathBuf,
    /// `None` when the slices are only hashed ([`SegmentSlicer::create`]).
    file: Option<BufWriter<File>>,
    hasher: blake3::Hasher,
}

impl SliceOut {
    fn write(&mut self, bytes: &[u8]) -> Result<(), ModernError> {
        if let Some(file) = self.file.as_mut() {
            file.write_all(bytes).map_err(|e| io(&self.tmp, e))?;
        }
        self.hasher.update(bytes);
        self.record.bytes += bytes.len() as u64;
        Ok(())
    }
}

/// Receives one unit's tensors in layout order (as a stage package would)
/// and writes them to the unit's slice files, hashing every slice and the
/// segment.
pub struct SegmentSlicer {
    unit: Unit,
    dir: PathBuf,
    entries: Vec<Entry>,
    /// Whether each entry is a routed-expert stack split across the groups.
    split: Vec<bool>,
    next: usize,
    open: Option<(usize, u64)>,
    segment: blake3::Hasher,
    segment_bytes: u64,
    /// `outs[0]` is the whole segment or the MoE core; `outs[1 + g]` is
    /// expert group `g`.
    outs: Vec<SliceOut>,
}

impl SegmentSlicer {
    /// Start writing `unit`'s slices into `dir` (temporary names until
    /// [`SegmentSlicer::finish`]); with `discard`, only hash them.
    pub fn create(
        c: &MlaConfig,
        unit: Unit,
        groups: usize,
        dir: &Path,
        discard: bool,
    ) -> Result<Self, ModernError> {
        let entries = unit.entries(c);
        let name = unit.name();
        let expert_prefix = match unit {
            Unit::Layer(l) => format!("layers.{l}.experts."),
            _ => String::new(),
        };
        let moe = unit.is_moe(c);
        let split: Vec<bool> = entries
            .iter()
            .map(|e| moe && e.name.starts_with(&expert_prefix))
            .collect();
        let e = c.n_routed_experts;
        let mut specs = vec![(
            if moe {
                format!("{name}.core")
            } else {
                name.clone()
            },
            None,
        )];
        if moe {
            for g in 0..groups {
                specs.push((
                    format!("{name}.experts.{g}"),
                    Some((g * e / groups, (g + 1) * e / groups)),
                ));
            }
        }
        let mut outs = Vec::new();
        for (slice_name, experts) in specs {
            let tmp = dir.join(format!(".{slice_name}.{}.partial", std::process::id()));
            let file = if discard {
                None
            } else {
                let file = File::create(&tmp).map_err(|e| io(&tmp, e))?;
                Some(BufWriter::with_capacity(SLICE_BUFFER, file))
            };
            outs.push(SliceOut {
                record: SliceRecord {
                    name: slice_name,
                    segment: name.clone(),
                    blake3: String::new(),
                    bytes: 0,
                    experts,
                    tensors: Vec::new(),
                },
                tmp,
                file,
                hasher: blake3::Hasher::new(),
            });
        }
        Ok(Self {
            unit,
            dir: dir.to_path_buf(),
            entries,
            split,
            next: 0,
            open: None,
            segment: blake3::Hasher::new(),
            segment_bytes: 0,
            outs,
        })
    }

    fn groups(&self) -> usize {
        self.outs.len() - 1
    }

    /// Rename every slice to its content address and return the record.
    pub fn finish(mut self) -> Result<UnitRecord, ModernError> {
        if self.next != self.entries.len() || self.open.is_some() {
            return Err(invalid(format!(
                "{}: finished after {} of {} tensors",
                self.unit.name(),
                self.next,
                self.entries.len()
            )));
        }
        let mut slices = Vec::new();
        for mut out in self.outs.drain(..) {
            out.record.blake3 = hex_lower(out.hasher.finalize().as_bytes());
            let Some(mut file) = out.file.take() else {
                slices.push(out.record);
                continue;
            };
            file.flush().map_err(|e| io(&out.tmp, e))?;
            drop(file);
            let path = out.record.path(&self.dir);
            if path.exists()
                && std::fs::metadata(&path).map(|m| m.len()).ok() == Some(out.record.bytes)
            {
                // The same content is already stored under its address.
                std::fs::remove_file(&out.tmp).map_err(|e| io(&out.tmp, e))?;
            } else {
                std::fs::rename(&out.tmp, &path).map_err(|e| io(&path, e))?;
            }
            slices.push(out.record);
        }
        Ok(UnitRecord {
            unit: self.unit,
            segment: SegmentDigest {
                name: self.unit.name(),
                bytes: self.segment_bytes,
                blake3: hex_lower(self.segment.finalize().as_bytes()),
            },
            slices,
        })
    }
}

impl TensorSink for SegmentSlicer {
    fn begin(&mut self, name: &str) -> Result<(), ModernError> {
        if self.open.is_some() {
            return Err(invalid(format!(
                "tensor {name} started before the previous one ended"
            )));
        }
        let index = self.next;
        let entry = self
            .entries
            .get(index)
            .cloned()
            .ok_or_else(|| invalid(format!("unexpected extra tensor {name}")))?;
        if entry.name != name {
            return Err(invalid(format!(
                "tensor {name} written where {} belongs",
                entry.name
            )));
        }
        if self.split[index] {
            let groups = self.groups();
            let mut shape = entry.shape.clone();
            shape[0] /= groups;
            for out in &mut self.outs[1..] {
                out.record.tensors.push(SliceTensor {
                    name: entry.name.clone(),
                    dtype: entry.dtype,
                    shape: shape.clone(),
                    offset: out.record.bytes,
                    bytes: entry.bytes / groups as u64,
                });
            }
        } else {
            let out = &mut self.outs[0];
            out.record.tensors.push(SliceTensor {
                name: entry.name.clone(),
                dtype: entry.dtype,
                shape: entry.shape.clone(),
                offset: out.record.bytes,
                bytes: entry.bytes,
            });
        }
        self.open = Some((index, 0));
        Ok(())
    }

    fn chunk(&mut self, bytes: &[u8]) -> Result<(), ModernError> {
        let (index, done) = self.open.ok_or_else(|| invalid("no tensor is open"))?;
        let limit = self.entries[index].bytes;
        if done + bytes.len() as u64 > limit {
            return Err(invalid(format!(
                "tensor {} would exceed its {limit} bytes",
                self.entries[index].name
            )));
        }
        self.segment.update(bytes);
        self.segment_bytes += bytes.len() as u64;
        if self.split[index] {
            let per_group = limit / self.groups() as u64;
            let mut position = done;
            let mut rest = bytes;
            while !rest.is_empty() {
                let group = (position / per_group) as usize;
                let room = (group as u64 + 1) * per_group - position;
                let take = rest.len().min(room as usize);
                self.outs[1 + group].write(&rest[..take])?;
                position += take as u64;
                rest = &rest[take..];
            }
        } else {
            self.outs[0].write(bytes)?;
        }
        self.open = Some((index, done + bytes.len() as u64));
        Ok(())
    }

    fn end(&mut self) -> Result<(), ModernError> {
        let (index, done) = self
            .open
            .take()
            .ok_or_else(|| invalid("no tensor is open"))?;
        if done != self.entries[index].bytes {
            return Err(invalid(format!(
                "tensor {} has {done} bytes; its layout needs {}",
                self.entries[index].name, self.entries[index].bytes
            )));
        }
        self.next += 1;
        Ok(())
    }
}

impl UnitRecord {
    pub fn to_json(&self, context: &Value) -> Value {
        json!({
            "schema": SLICE_UNIT_SCHEMA,
            "context": context,
            "unit": self.unit.name(),
            "segment": self.segment.to_json(),
            "slices": self.slices.iter().map(SliceRecord::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(value: &Value, context: &Value) -> Result<Self, ModernError> {
        if value.get("schema").and_then(Value::as_str) != Some(SLICE_UNIT_SCHEMA) {
            return Err(invalid(format!(
                "unit record schema is not {SLICE_UNIT_SCHEMA}"
            )));
        }
        if value.get("context") != Some(context) {
            return Err(invalid(format!(
                "unit record {} was made for {}, not {context}",
                value["unit"], value["context"]
            )));
        }
        Ok(Self {
            unit: Unit::parse(value.get("unit").and_then(Value::as_str).unwrap_or(""))?,
            segment: SegmentDigest::from_json(&value["segment"])?,
            slices: value
                .get("slices")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("unit record has no slices"))?
                .iter()
                .map(SliceRecord::from_json)
                .collect::<Result<_, _>>()?,
        })
    }
}

/// The directory of unit records under a slice directory.
pub fn units_dir(out: &Path) -> PathBuf {
    out.join("units")
}

/// What one `slice` run converted.
pub struct SliceReport {
    pub records: Vec<UnitRecord>,
    pub shards_read: Vec<String>,
    /// Seconds per unit, in conversion order.
    pub seconds: Vec<(String, f64)>,
}

/// Convert `units` from the shards in `dir` into slices under `out`, and
/// write one unit record per unit under `out/units/`. With `discard`, the
/// slices are hashed but not stored (to check a source against a manifest).
pub fn convert_units(
    src: &SliceSource,
    dir: &Path,
    units: &[Unit],
    out: &Path,
    discard: bool,
) -> Result<SliceReport, ModernError> {
    let c = &src.config;
    let map = src.weight_map(dir)?;
    let mut needed: BTreeSet<String> = BTreeSet::new();
    for unit in units {
        needed.extend(unit.source_tensors(c, &src.hf).into_keys());
    }
    let shard_names = src.shards_for(map.as_ref(), needed.iter().cloned(), dir)?;
    let mut paths = Vec::new();
    for name in &shard_names {
        let file = src
            .manifest
            .files
            .iter()
            .find(|f| f.name == *name)
            .ok_or_else(|| invalid(format!("{name} is not pinned")))?;
        paths.push(verify_source_file(dir, file)?);
    }
    let expected = stage_source_tensors_in(c, &src.hf.source, StageSpec::full(c));
    let mut tensors = SourceTensors::open(&paths, &expected, &src.hf.source)?;
    tensors.precision = c.precision.clone();
    tensors.require(needed.iter())?;
    std::fs::create_dir_all(units_dir(out)).map_err(|e| io(out, e))?;
    let context = src.context();
    let mut records = Vec::new();
    let mut seconds = Vec::new();
    for &unit in units {
        let start = Instant::now();
        let mut w = SegmentSlicer::create(c, unit, src.expert_groups, out, discard)?;
        match unit {
            Unit::Embed => convert_embed(&mut w, &tensors, c)?,
            Unit::Layer(l) => convert_layer(&mut w, &tensors, c, l)?,
            Unit::Head => convert_head(&mut w, &tensors, c)?,
        }
        let record = w.finish()?;
        let path = units_dir(out).join(format!("{}.json", unit.name()));
        let text = canonical(&record.to_json(&context))?;
        let tmp = path.with_extension("json.partial");
        std::fs::write(&tmp, text + "\n").map_err(|e| io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io(&path, e))?;
        seconds.push((unit.name(), start.elapsed().as_secs_f64()));
        records.push(record);
    }
    Ok(SliceReport {
        records,
        shards_read: shard_names,
        seconds,
    })
}

/// Read every unit record under `out/units/` that belongs to `src`.
pub fn read_records(src: &SliceSource, out: &Path) -> Result<Vec<UnitRecord>, ModernError> {
    let context = src.context();
    let mut records = Vec::new();
    for unit in all_units(&src.config) {
        let path = units_dir(out).join(format!("{}.json", unit.name()));
        if !path.exists() {
            continue;
        }
        let value: Value = serde_json::from_slice(&std::fs::read(&path).map_err(|e| io(&path, e))?)
            .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
        let record = UnitRecord::from_json(&value, &context)?;
        if record.unit != unit {
            return Err(invalid(format!(
                "{} records {}",
                path.display(),
                record.unit.name()
            )));
        }
        records.push(record);
    }
    Ok(records)
}

/// The layout fields of the `model` object: everything that fixes the bytes
/// of the weight segments (spec §14.3).
fn shape_json(c: &MlaConfig) -> Value {
    let model = c.to_json();
    let keys = [
        "architecture",
        "n_layers",
        "d_model",
        "n_heads",
        "q_lora_rank",
        "kv_lora_rank",
        "qk_nope_dim",
        "qk_rope_dim",
        "v_head_dim",
        "d_ff",
        "first_k_dense",
        "n_routed_experts",
        "n_shared_experts",
        "moe_d_ff",
        "vocab_size",
    ];
    Value::Object(
        keys.iter()
            .map(|k| (k.to_string(), model[*k].clone()))
            .collect(),
    )
}

/// The `tables` segment digest (spec §4.5, §4.7).
fn tables_digest(c: &MlaConfig) -> Result<SegmentDigest, ModernError> {
    let (cos, sin) = rope_tables(c.rope_theta, c.qk_rope_dim, c.max_seq)?;
    let mut hasher = blake3::Hasher::new();
    let (cos, sin) = (i32_bytes(&cos), i32_bytes(&sin));
    hasher.update(&cos);
    hasher.update(&sin);
    Ok(SegmentDigest {
        name: "tables".into(),
        bytes: (cos.len() + sin.len()) as u64,
        blake3: hex_lower(hasher.finalize().as_bytes()),
    })
}

/// The slice manifest (spec §14.3) of the records `records`.
pub fn build_manifest(src: &SliceSource, records: &[UnitRecord]) -> Result<Value, ModernError> {
    let c = &src.config;
    let source = src.manifest.header_json();
    let complete = records.len() == all_units(c).len();
    let pending = &src.hf.pending;
    let prepared = pending.is_empty();
    let tables = if prepared {
        Some(tables_digest(c)?)
    } else {
        None
    };
    let model_root = match (&tables, complete) {
        (Some(tables), true) => {
            let mut segments = vec![tables.clone()];
            segments.extend(records.iter().map(|r| r.segment.clone()));
            Value::from(package::model_root(c, &source, &segments)?)
        }
        _ => Value::Null,
    };
    let mut manifest = json!({
        "schema": SLICE_MANIFEST_SCHEMA,
        "profile": c.profile(),
        "profile_blake3": identity_blake3(c.profile()),
        "contract": CONTRACT,
        "source": source,
        "weights": {
            "prefix": src.hf.source.prefix,
            "packed_experts": src.hf.source.packed_experts,
        },
        "shape": shape_json(c),
        "model": if prepared { c.to_json() } else { Value::Null },
        "pending": pending,
        "expert_groups": src.expert_groups,
        "complete": complete,
        "tables": tables.as_ref().map(SegmentDigest::to_json).unwrap_or(Value::Null),
        "segments": records.iter().map(|r| r.segment.to_json()).collect::<Vec<_>>(),
        "slices": records.iter().flat_map(|r| r.slices.iter().map(SliceRecord::to_json)).collect::<Vec<_>>(),
        "model_root": model_root,
    });
    if let Some(p) = &c.precision {
        manifest["precision"] = serde_json::to_value(p).unwrap();
    }
    let hash = crate::model_package::manifest_body_blake3(&manifest)
        .map_err(|e| invalid(format!("manifest hash: {e}")))?;
    manifest["manifest_blake3"] = Value::from(hash);
    Ok(manifest)
}

/// Strict optional policy: absence is the legacy identity; null is invalid.
pub fn manifest_precision(
    value: &Value,
) -> Result<Option<super::precision::Precision>, ModernError> {
    value
        .get("precision")
        .map(super::precision::Precision::from_json)
        .transpose()
}

/// A parsed slice manifest whose `manifest_blake3` has been checked.
pub struct SliceManifest {
    pub value: Value,
    pub segments: Vec<SegmentDigest>,
    pub slices: Vec<SliceRecord>,
}

impl SliceManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, ModernError> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|e| invalid(format!("slice manifest JSON: {e}")))?;
        if value.get("schema").and_then(Value::as_str) != Some(SLICE_MANIFEST_SCHEMA) {
            return Err(invalid(format!(
                "slice manifest schema is not {SLICE_MANIFEST_SCHEMA}"
            )));
        }
        manifest_precision(&value)?;
        let recorded = value
            .get("manifest_blake3")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("slice manifest has no manifest_blake3"))?;
        let recomputed = crate::model_package::manifest_body_blake3(&value)
            .map_err(|e| invalid(format!("manifest hash: {e}")))?;
        if recomputed != recorded {
            return Err(invalid(format!(
                "manifest_blake3 is {recorded}, but the manifest hashes to {recomputed}"
            )));
        }
        let list = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_array)
                .ok_or_else(|| invalid(format!("slice manifest has no {key}")))
        };
        let segments = list("segments")?
            .iter()
            .map(SegmentDigest::from_json)
            .collect::<Result<_, _>>()?;
        let slices = list("slices")?
            .iter()
            .map(SliceRecord::from_json)
            .collect::<Result<_, _>>()?;
        Ok(Self {
            value,
            segments,
            slices,
        })
    }

    pub fn read(path: &Path) -> Result<Self, ModernError> {
        Self::parse(&std::fs::read(path).map_err(|e| io(path, e))?)
    }

    /// The `model` object, when the profile defines the model's preparation.
    pub fn config(&self) -> Result<MlaConfig, ModernError> {
        let model = self
            .value
            .get("model")
            .filter(|m| !m.is_null())
            .ok_or_else(|| {
                invalid(format!(
                    "the slice manifest has no model object (pending: {})",
                    self.value["pending"]
                ))
            })?;
        let mut c = MlaConfig::from_json(model)?;
        c.expert_format = self
            .value
            .get("profile")
            .and_then(Value::as_str)
            .and_then(ExpertFormat::from_profile)
            .ok_or_else(|| invalid("slice manifest profile is not an MLA + MoE profile"))?;
        c.validate()?;
        if self.value["profile"] != c.profile() || c.precision != manifest_precision(&self.value)? {
            return Err(invalid("slice model/profile/precision mismatch"));
        }
        Ok(c)
    }

    /// The slices of segment `name`, in manifest order.
    fn slices_of(&self, name: &str) -> Vec<&SliceRecord> {
        self.slices.iter().filter(|s| s.segment == name).collect()
    }
}

/// Stream a slice file through BLAKE3 and check its length and address.
fn check_slice_file(dir: &Path, slice: &SliceRecord) -> Result<(), ModernError> {
    let path = slice.path(dir);
    let mut file = File::open(&path).map_err(|e| io(&path, e))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 8 << 20];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buffer).map_err(|e| io(&path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        total += n as u64;
    }
    let digest = hex_lower(hasher.finalize().as_bytes());
    if total != slice.bytes || digest != slice.blake3 {
        return Err(invalid(format!(
            "slice {} is {total} bytes with BLAKE3 {digest}; the manifest pins {} bytes with BLAKE3 {}",
            slice.name, slice.bytes, slice.blake3
        )));
    }
    Ok(())
}

/// Copies byte ranges of slice files into a [`TensorSink`] in layout order.
struct SliceReader {
    dir: PathBuf,
    open: BTreeMap<String, BufReader<File>>,
}

impl SliceReader {
    fn copy(
        &mut self,
        slice: &SliceRecord,
        tensor: &SliceTensor,
        sink: &mut impl FnMut(&[u8]) -> Result<(), ModernError>,
    ) -> Result<(), ModernError> {
        let path = slice.path(&self.dir);
        if !self.open.contains_key(&slice.blake3) {
            let file = File::open(&path).map_err(|e| io(&path, e))?;
            self.open.insert(
                slice.blake3.clone(),
                BufReader::with_capacity(8 << 20, file),
            );
        }
        let reader = self.open.get_mut(&slice.blake3).expect("inserted above");
        reader
            .seek(SeekFrom::Start(tensor.offset))
            .map_err(|e| io(&path, e))?;
        let mut left = tensor.bytes;
        let mut buffer = vec![0u8; (8 << 20).min(left.max(1) as usize)];
        while left > 0 {
            let take = buffer.len().min(left as usize);
            reader
                .read_exact(&mut buffer[..take])
                .map_err(|e| io(&path, e))?;
            sink(&buffer[..take])?;
            left -= take as u64;
        }
        Ok(())
    }
}

/// Replay segment `name` from its slices into `sink` in layout order: each
/// tensor from the whole/core slice, or for an expert stack the expert
/// groups' parts in order. `entries` is the segment's layout.
fn replay_segment<S: TensorSink>(
    manifest: &SliceManifest,
    reader: &mut SliceReader,
    entries: &[Entry],
    sink: &mut S,
) -> Result<(), ModernError> {
    let Some(first) = entries.first() else {
        return Ok(());
    };
    let slices = manifest.slices_of(&first.segment);
    if slices.is_empty() {
        return Err(invalid(format!("no slices of segment {}", first.segment)));
    }
    for entry in entries {
        let holders: Vec<(&SliceRecord, &SliceTensor)> = slices
            .iter()
            .filter_map(|s| {
                s.tensors
                    .iter()
                    .find(|t| t.name == entry.name)
                    .map(|t| (*s, t))
            })
            .collect();
        let total: u64 = holders.iter().map(|(_, t)| t.bytes).sum();
        if holders.is_empty() || total != entry.bytes {
            return Err(invalid(format!(
                "slices of {} hold {total} bytes of {}; the layout needs {}",
                first.segment, entry.name, entry.bytes
            )));
        }
        sink.begin(&entry.name)?;
        for (slice, tensor) in holders {
            reader.copy(slice, tensor, &mut |bytes| sink.chunk(bytes))?;
        }
        sink.end()?;
    }
    Ok(())
}

/// Hashes a replayed segment.
struct SegmentHasher {
    hasher: blake3::Hasher,
    bytes: u64,
}

impl TensorSink for SegmentHasher {
    fn begin(&mut self, _: &str) -> Result<(), ModernError> {
        Ok(())
    }
    fn chunk(&mut self, bytes: &[u8]) -> Result<(), ModernError> {
        self.hasher.update(bytes);
        self.bytes += bytes.len() as u64;
        Ok(())
    }
    fn end(&mut self) -> Result<(), ModernError> {
        Ok(())
    }
}

/// Check slice files under `dir` against a slice manifest: the files of the
/// slices whose name starts with one of `only` (every slice when empty), and,
/// with `segments`, every segment whose slices are all present re-hashed in
/// layout order against the manifest's segment digest.
pub fn verify_slices(
    manifest: &SliceManifest,
    dir: &Path,
    only: &[String],
    segments: bool,
) -> Result<Value, ModernError> {
    let selected: Vec<&SliceRecord> = manifest
        .slices
        .iter()
        .filter(|s| {
            only.is_empty()
                || only
                    .iter()
                    .any(|p| s.name == *p || s.name.starts_with(&format!("{p}.")))
        })
        .collect();
    if selected.is_empty() {
        return Err(invalid("no slice of the manifest matches the selection"));
    }
    let mut checked = Vec::new();
    for slice in &selected {
        check_slice_file(dir, slice)?;
        checked.push(Value::from(slice.name.clone()));
    }
    let mut segments_checked = Vec::new();
    if segments {
        let shape = &manifest.value["shape"];
        let c = config_for_layout(shape, &manifest.value)?;
        for segment in &manifest.segments {
            let slices = manifest.slices_of(&segment.name);
            if slices.iter().any(|s| !s.path(dir).exists()) {
                continue;
            }
            let unit = Unit::parse(&segment.name)?;
            let entries = unit.entries(&c);
            let mut reader = SliceReader {
                dir: dir.to_path_buf(),
                open: BTreeMap::new(),
            };
            let mut hasher = SegmentHasher {
                hasher: blake3::Hasher::new(),
                bytes: 0,
            };
            replay_segment(manifest, &mut reader, &entries, &mut hasher)?;
            let digest = hex_lower(hasher.hasher.finalize().as_bytes());
            if digest != segment.blake3 || hasher.bytes != segment.bytes {
                return Err(invalid(format!(
                    "segment {} replays to {} bytes with BLAKE3 {digest}; the manifest pins {} bytes with BLAKE3 {}",
                    segment.name, hasher.bytes, segment.bytes, segment.blake3
                )));
            }
            segments_checked.push(Value::from(segment.name.clone()));
        }
    }
    Ok(json!({
        "verified": true,
        "manifest_blake3": manifest.value["manifest_blake3"],
        "slices_checked": checked,
        "segments_checked": segments_checked,
    }))
}

/// A configuration with the manifest's layout fields, for replaying weight
/// segments (the preparation fields do not change their layout).
fn config_for_layout(shape: &Value, manifest: &Value) -> Result<MlaConfig, ModernError> {
    let field = |k: &str| {
        shape
            .get(k)
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .ok_or_else(|| invalid(format!("slice manifest shape.{k}")))
    };
    let n_routed_experts = field("n_routed_experts")?;
    Ok(MlaConfig {
        architecture: shape
            .get("architecture")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        n_layers: field("n_layers")?,
        d_model: field("d_model")?,
        n_heads: field("n_heads")?,
        q_lora_rank: field("q_lora_rank")?,
        kv_lora_rank: field("kv_lora_rank")?,
        qk_nope_dim: field("qk_nope_dim")?,
        qk_rope_dim: field("qk_rope_dim")?,
        v_head_dim: field("v_head_dim")?,
        d_ff: field("d_ff")?,
        first_k_dense: field("first_k_dense")?,
        n_routed_experts,
        n_experts_per_tok: 1,
        n_shared_experts: field("n_shared_experts")?,
        moe_d_ff: field("moe_d_ff")?,
        n_group: 1,
        topk_group: 1,
        norm_topk_prob: true,
        routed_scaling_q32: 1 << 32,
        vocab_size: field("vocab_size")?,
        max_seq: 1,
        rms_eps_q32: 1,
        rope_theta: 2,
        attention_lambda: 0,
        preparation: None,
        precision: manifest_precision(manifest)?,
        expert_format: manifest
            .get("profile")
            .and_then(Value::as_str)
            .and_then(ExpertFormat::from_profile)
            .ok_or_else(|| invalid("slice manifest profile is not an MLA + MoE profile"))?,
    })
}

/// Rebuild the stage package `[a, b)` from slices (spec §14.4): the RoPE
/// tables are computed, every other tensor is copied from the slices. Needs a
/// manifest with a `model` object. Every slice used is verified first.
pub fn assemble_stage(
    manifest: &SliceManifest,
    dir: &Path,
    stage: StageSpec,
    out: &Path,
) -> Result<Value, ModernError> {
    let c = manifest.config()?;
    stage.validate(&c)?;
    let entries = package::layout(&c, stage);
    let mut names: Vec<String> = Vec::new();
    for e in &entries {
        if e.segment != "tables" && !names.contains(&e.segment) {
            names.push(e.segment.clone());
        }
    }
    for name in &names {
        let slices = manifest.slices_of(name);
        if slices.is_empty() {
            return Err(invalid(format!("the slice manifest has no segment {name}")));
        }
        for slice in slices {
            check_slice_file(dir, slice)?;
        }
    }
    let source = manifest.value["source"].clone();
    let header = package::header_json(&c, &source, stage, &entries);
    let mut w = StageWriter::create(out, &header, entries.clone())?;
    let (cos, sin) = rope_tables(c.rope_theta, c.qk_rope_dim, c.max_seq)?;
    TensorSink::write_tensor(&mut w, "rope.cos", &i32_bytes(&cos))?;
    TensorSink::write_tensor(&mut w, "rope.sin", &i32_bytes(&sin))?;
    let mut reader = SliceReader {
        dir: dir.to_path_buf(),
        open: BTreeMap::new(),
    };
    for name in &names {
        let segment: Vec<Entry> = entries
            .iter()
            .filter(|e| e.segment == *name)
            .cloned()
            .collect();
        replay_segment(manifest, &mut reader, &segment, &mut w)?;
    }
    let (digest, segments) = w.finish()?;
    for s in segments.iter().filter(|s| s.name != "tables") {
        let pinned = manifest.segments.iter().find(|p| p.name == s.name);
        if pinned != Some(s) {
            return Err(invalid(format!(
                "assembled segment {} ({} bytes, BLAKE3 {}) differs from the manifest",
                s.name, s.bytes, s.blake3
            )));
        }
    }
    Ok(json!({
        "package": digest.to_json(),
        "stage": stage.to_json(),
        "segments": segments.iter().map(SegmentDigest::to_json).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::super::config::{ExpertFormat, MlaConfig};
    use super::super::convert::{convert_stage, repack_int4_words};
    use super::super::model::tests::{Lcg, tiny_config_with};
    use super::super::ops::{Q4_GROUP, pack_q4, quantize_q4_group};
    use super::*;
    use sha2::{Digest, Sha256};

    /// How the generated checkpoint stores its weights.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Storage {
        /// Plain BF16 safetensors, `model.` names.
        Bf16,
        /// As Kimi-K2.6: a multimodal wrapper, `language_model.` names, a
        /// vision tower, and the routed experts as compressed-tensors INT4.
        Packed,
    }

    struct Tensor {
        name: String,
        dtype: &'static str,
        shape: Vec<usize>,
        bytes: Vec<u8>,
    }

    fn bf16_tensor(name: String, shape: &[usize], values: &[u16]) -> Tensor {
        Tensor {
            name,
            dtype: "BF16",
            shape: shape.to_vec(),
            bytes: package::u16_bytes(values),
        }
    }

    fn weights(rng: &mut Lcg, count: usize) -> Vec<u16> {
        (0..count)
            .map(|_| {
                let r = rng.next();
                ((((r >> 31) & 1) << 15) | ((120 + (r >> 8) % 5) << 7) | (r & 0x7F)) as u16
            })
            .collect()
    }

    fn gains(rng: &mut Lcg, count: usize) -> Vec<u16> {
        (0..count)
            .map(|_| (((126 + rng.next() % 2) << 7) | (rng.next() % 128)) as u16)
            .collect()
    }

    /// compressed-tensors `pack_to_int32` (4 bits, packed along columns):
    /// value `j` of a row at bits `4(j mod 8)` of word `j / 8`, stored `v + 8`.
    fn ct_pack(values: &[i8], cols: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(values.len() / 2);
        for row in values.chunks(cols) {
            for word in row.chunks(8) {
                let w = word.iter().enumerate().fold(0u32, |acc, (i, &v)| {
                    acc | (((v as i32 + 8) as u32) << (4 * i))
                });
                out.extend_from_slice(&(w as i32).to_le_bytes());
            }
        }
        out
    }

    /// Spec 13.3 values and scales of one BF16 matrix.
    fn quantize(bits: &[u16], cols: usize) -> (Vec<i8>, Vec<u16>) {
        let mut q = vec![0i8; bits.len()];
        let mut scales = Vec::new();
        for (group, out) in bits.chunks(Q4_GROUP).zip(q.chunks_mut(Q4_GROUP)) {
            scales.push(quantize_q4_group(group, out).unwrap());
        }
        assert!(cols.is_multiple_of(Q4_GROUP));
        (q, scales)
    }

    fn layer_tensors(c: &MlaConfig, rng: &mut Lcg, layer: usize, storage: Storage) -> Vec<Tensor> {
        let p = format!("model.layers.{layer}");
        let d = c.d_model;
        let mut out = vec![
            bf16_tensor(format!("{p}.input_layernorm.weight"), &[d], &gains(rng, d)),
            bf16_tensor(
                format!("{p}.post_attention_layernorm.weight"),
                &[d],
                &gains(rng, d),
            ),
        ];
        let mut matrix = |name: String, rows: usize, cols: usize| {
            bf16_tensor(name, &[rows, cols], &weights(rng, rows * cols))
        };
        let r = c.q_lora_rank;
        out.push(matrix(format!("{p}.self_attn.q_a_proj.weight"), r, d));
        out.push(matrix(format!("{p}.self_attn.q_b_proj.weight"), c.d_q(), r));
        out.push(matrix(
            format!("{p}.self_attn.kv_a_proj_with_mqa.weight"),
            c.d_kv_a(),
            d,
        ));
        out.push(matrix(
            format!("{p}.self_attn.kv_b_proj.weight"),
            c.n_heads * (c.qk_nope_dim + c.v_head_dim),
            c.kv_lora_rank,
        ));
        out.push(matrix(
            format!("{p}.self_attn.o_proj.weight"),
            d,
            c.d_attn_out(),
        ));
        out.push(bf16_tensor(
            format!("{p}.self_attn.q_a_layernorm.weight"),
            &[r],
            &gains(rng, r),
        ));
        out.push(bf16_tensor(
            format!("{p}.self_attn.kv_a_layernorm.weight"),
            &[c.kv_lora_rank],
            &gains(rng, c.kv_lora_rank),
        ));
        if !c.is_moe(layer) {
            for (name, rows, cols) in [
                ("gate_proj", c.d_ff, d),
                ("up_proj", c.d_ff, d),
                ("down_proj", d, c.d_ff),
            ] {
                out.push(bf16_tensor(
                    format!("{p}.mlp.{name}.weight"),
                    &[rows, cols],
                    &weights(rng, rows * cols),
                ));
            }
            return out;
        }
        let (e, fm, sf) = (c.n_routed_experts, c.moe_d_ff, c.shared_d_ff());
        out.push(bf16_tensor(
            format!("{p}.mlp.gate.weight"),
            &[e, d],
            &weights(rng, e * d),
        ));
        let bias: Vec<u8> = (0..e)
            .flat_map(|_| {
                let r = rng.next() as u32;
                ((r & 0x8000_0000) | ((118 + r % 7) << 23) | ((r >> 9) & 0x7F_FFFF)).to_le_bytes()
            })
            .collect();
        out.push(Tensor {
            name: format!("{p}.mlp.gate.e_score_correction_bias"),
            dtype: "F32",
            shape: vec![e],
            bytes: bias,
        });
        for (name, rows, cols) in [
            ("gate_proj", sf, d),
            ("up_proj", sf, d),
            ("down_proj", d, sf),
        ] {
            out.push(bf16_tensor(
                format!("{p}.mlp.shared_experts.{name}.weight"),
                &[rows, cols],
                &weights(rng, rows * cols),
            ));
        }
        for expert in 0..e {
            for (name, rows, cols) in [
                ("gate_proj", fm, d),
                ("up_proj", fm, d),
                ("down_proj", d, fm),
            ] {
                let base = format!("{p}.mlp.experts.{expert}.{name}");
                let bits = weights(rng, rows * cols);
                if storage == Storage::Bf16 {
                    out.push(bf16_tensor(format!("{base}.weight"), &[rows, cols], &bits));
                    continue;
                }
                let (q, scales) = quantize(&bits, cols);
                out.push(Tensor {
                    name: format!("{base}.weight_packed"),
                    dtype: "I32",
                    shape: vec![rows, cols / 8],
                    bytes: ct_pack(&q, cols),
                });
                out.push(bf16_tensor(
                    format!("{base}.weight_scale"),
                    &[rows, cols / 32],
                    &scales,
                ));
                out.push(Tensor {
                    name: format!("{base}.weight_shape"),
                    dtype: "I32",
                    shape: vec![2],
                    bytes: [rows as i32, cols as i32]
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect(),
                });
            }
        }
        out
    }

    fn safetensors(tensors: &[Tensor]) -> Vec<u8> {
        let mut header = serde_json::Map::new();
        let mut offset = 0usize;
        for t in tensors {
            header.insert(
                t.name.clone(),
                json!({"dtype": t.dtype, "shape": t.shape, "data_offsets": [offset, offset + t.bytes.len()]}),
            );
            offset += t.bytes.len();
        }
        let text = serde_json::to_string(&Value::Object(header)).unwrap();
        let mut out = (text.len() as u64).to_le_bytes().to_vec();
        out.extend_from_slice(text.as_bytes());
        for t in tensors {
            out.extend_from_slice(&t.bytes);
        }
        out
    }

    fn hf_config(c: &MlaConfig, storage: Storage, yarn: bool) -> Value {
        let mut text = json!({
            "model_type": "kimi_k2", "hidden_act": "silu", "scoring_func": "sigmoid",
            "topk_method": "noaux_tc", "attention_bias": false, "moe_layer_freq": 1,
            "num_nextn_predict_layers": 0, "tie_word_embeddings": false,
            "num_hidden_layers": c.n_layers, "hidden_size": c.d_model,
            "num_attention_heads": c.n_heads, "num_key_value_heads": c.n_heads,
            "q_lora_rank": c.q_lora_rank, "kv_lora_rank": c.kv_lora_rank,
            "qk_nope_head_dim": c.qk_nope_dim, "qk_rope_head_dim": c.qk_rope_dim,
            "v_head_dim": c.v_head_dim, "intermediate_size": c.d_ff,
            "first_k_dense_replace": c.first_k_dense, "n_routed_experts": c.n_routed_experts,
            "num_experts_per_tok": c.n_experts_per_tok, "n_shared_experts": c.n_shared_experts,
            "moe_intermediate_size": c.moe_d_ff, "n_group": c.n_group, "topk_group": c.topk_group,
            "norm_topk_prob": true, "routed_scaling_factor": 2.446, "vocab_size": c.vocab_size,
            "max_position_embeddings": 256, "rms_norm_eps": 1e-5, "rope_theta": 50000.0,
            "eos_token_id": 7,
        });
        if yarn {
            text["rope_scaling"] = json!({"type": "yarn", "factor": 64.0});
        }
        if storage == Storage::Bf16 {
            return text;
        }
        text["quantization_config"] = json!({
            "quant_method": "compressed-tensors", "format": "pack-quantized", "kv_cache_scheme": null,
            "config_groups": {"group_0": {"input_activations": null, "output_activations": null,
                "targets": ["Linear"], "weights": {"num_bits": 4, "type": "int", "symmetric": true,
                "strategy": "group", "group_size": 32, "dynamic": false, "actorder": null}}},
        });
        json!({"model_type": "kimi_k25", "text_config": text})
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex_lower(&Sha256::digest(bytes))
    }

    /// Write a checkpoint of `c` (one shard per layer, then embed and head,
    /// then a vision shard when packed) with an index and a source manifest;
    /// the BF16 values are the same in both storages.
    fn write_checkpoint(c: &MlaConfig, storage: Storage, yarn: bool, dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let prefix = if storage == Storage::Packed {
            "language_model."
        } else {
            ""
        };
        let mut rng = Lcg(0x534C_4943_4553);
        let mut shards: Vec<Vec<Tensor>> = (0..c.n_layers)
            .map(|l| layer_tensors(c, &mut rng, l, storage))
            .collect();
        let (v, d) = (c.vocab_size, c.d_model);
        shards.push(vec![
            bf16_tensor(
                "model.embed_tokens.weight".into(),
                &[v, d],
                &weights(&mut rng, v * d),
            ),
            bf16_tensor("model.norm.weight".into(), &[d], &gains(&mut rng, d)),
            bf16_tensor("lm_head.weight".into(), &[v, d], &weights(&mut rng, v * d)),
        ]);
        for shard in &mut shards {
            for t in shard.iter_mut() {
                t.name = format!("{prefix}{}", t.name);
            }
        }
        if storage == Storage::Packed {
            shards.push(vec![bf16_tensor(
                "vision_tower.patch_embed.proj.weight".into(),
                &[4, 8],
                &weights(&mut rng, 32),
            )]);
        }
        let n = shards.len();
        let mut files = vec![(
            "config.json".to_string(),
            serde_json::to_vec(&hf_config(c, storage, yarn)).unwrap(),
        )];
        let mut map = serde_json::Map::new();
        for (i, shard) in shards.iter().enumerate() {
            let name = format!("model-{:05}-of-{n:05}.safetensors", i + 1);
            for t in shard {
                map.insert(t.name.clone(), Value::from(name.clone()));
            }
            files.push((name, safetensors(shard)));
        }
        let index = serde_json::to_vec(&json!({"weight_map": map})).unwrap();
        std::fs::write(dir.join("model.safetensors.index.json"), &index).unwrap();
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let entry = |name: &str, bytes: &[u8]| json!({"name": name, "bytes": bytes.len(), "sha256": sha256_hex(bytes)});
        let manifest = json!({
            "schema": crate::modern::convert::SOURCE_SCHEMA,
            "repo": "arc-test/slices",
            "revision": "0",
            "max_seq": c.max_seq,
            "files": files.iter().map(|(n, b)| entry(n, b)).collect::<Vec<_>>(),
            "index": entry("model.safetensors.index.json", &index),
        });
        let path = dir.join("source.json");
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("arc-mla-slices-{}-{tag}-{n}", std::process::id()))
    }

    fn tiny() -> MlaConfig {
        tiny_config_with(true, ExpertFormat::Int4G32)
    }

    /// Slice every unit of the checkpoint in `dir` (one call per unit, as the
    /// streaming driver does) and build the manifest.
    fn slice_all(dir: &Path, manifest: &Path, groups: usize, out: &Path) -> Value {
        let src = SliceSource::open(dir, manifest, None, groups).unwrap();
        for unit in all_units(&src.config) {
            convert_units(&src, dir, &[unit], out, false).unwrap();
        }
        build_manifest(&src, &read_records(&src, out).unwrap()).unwrap()
    }

    fn segments_of(value: &Value) -> Vec<Value> {
        value["segments"].as_array().unwrap().clone()
    }

    #[test]
    fn repacking_follows_the_compressed_tensors_bit_order() {
        // Every value, including -8, in every nibble position of a word.
        let values: Vec<i8> = (0..64).map(|j| ((j * 7 + 3) % 16) as i8 - 8).collect();
        let mut bytes = ct_pack(&values, 64);
        repack_int4_words(&mut bytes);
        assert_eq!(bytes, pack_q4(&values));
        assert!(values.contains(&-8) && values.contains(&7));
    }

    #[test]
    fn packed_checkpoint_slices_equal_the_bf16_twin_and_rebuild_its_package() {
        let c = tiny();
        let (bf16_dir, packed_dir) = (temp_dir("bf16"), temp_dir("packed"));
        let bf16_manifest = write_checkpoint(&c, Storage::Bf16, false, &bf16_dir);
        let packed_manifest = write_checkpoint(&c, Storage::Packed, false, &packed_dir);
        // The BF16 twin quantised by spec 13.3 gives the same segments as the
        // packed checkpoint repacked.
        let twin = convert_stage(
            &bf16_dir,
            &SourceManifest::read(&bf16_manifest).unwrap(),
            None,
            ExpertFormat::Int4G32,
            &bf16_dir.join("twin.arcspkg"),
        )
        .unwrap();
        let direct = convert_stage(
            &packed_dir,
            &SourceManifest::read(&packed_manifest).unwrap(),
            None,
            ExpertFormat::Int4G32,
            &packed_dir.join("direct.arcspkg"),
        )
        .unwrap();
        let weight_segments = |segments: &[SegmentDigest]| -> Vec<SegmentDigest> {
            segments
                .iter()
                .filter(|s| s.name != "tables")
                .cloned()
                .collect()
        };
        assert_eq!(
            weight_segments(&twin.segments),
            weight_segments(&direct.segments)
        );
        let out = packed_dir.join("slices");
        let manifest = slice_all(&packed_dir, &packed_manifest, 4, &out);
        assert_eq!(manifest["complete"], true);
        assert_eq!(manifest["pending"], json!([]));
        assert_eq!(
            manifest["weights"],
            json!({"prefix": "language_model.", "packed_experts": true})
        );
        let pinned: Vec<Value> = weight_segments(&direct.segments)
            .iter()
            .map(SegmentDigest::to_json)
            .collect();
        assert_eq!(segments_of(&manifest), pinned);
        assert_eq!(
            manifest["model_root"],
            direct.manifest.as_ref().unwrap()["model_root"]
        );
        // A stage package assembled from the slices is the converter's package.
        let text = crate::modern::package::manifest_text(&manifest).unwrap();
        let parsed = SliceManifest::parse(text.as_bytes()).unwrap();
        let report = assemble_stage(
            &parsed,
            &out,
            StageSpec::full(&c),
            &packed_dir.join("assembled.arcspkg"),
        )
        .unwrap();
        assert_eq!(report["package"], direct.digest.to_json());
        let stage = StageSpec {
            first_layer: 1,
            end_layer: 3,
        };
        let part = assemble_stage(&parsed, &out, stage, &packed_dir.join("part.arcspkg")).unwrap();
        let converted = convert_stage(
            &packed_dir,
            &SourceManifest::read(&packed_manifest).unwrap(),
            Some(stage),
            ExpertFormat::Int4G32,
            &packed_dir.join("part-direct.arcspkg"),
        )
        .unwrap();
        assert_eq!(part["package"], converted.digest.to_json());
        let verified = verify_slices(&parsed, &out, &[], true).unwrap();
        assert_eq!(
            verified["segments_checked"].as_array().unwrap().len(),
            c.n_layers + 2
        );
    }

    /// The slice manifests of the generated checkpoints, pinned: any change to
    /// the conversion, the slicing or the manifest shows up on every runner.
    #[test]
    fn slice_manifests_are_pinned() {
        let c = tiny();
        let mut got = Vec::new();
        for (storage, groups, yarn) in [
            (Storage::Packed, 2, false),
            (Storage::Packed, 4, true),
            (Storage::Bf16, 1, false),
        ] {
            let dir = temp_dir("pinned");
            let manifest = write_checkpoint(&c, storage, yarn, &dir);
            let m = slice_all(&dir, &manifest, groups, &dir.join("slices"));
            got.push(m["manifest_blake3"].as_str().unwrap().to_string());
        }
        assert_eq!(
            got,
            [
                "1b904eecc67e10237241aa2cd0b2651e6589207455af416139286af8ddd19051",
                "7ab9582ffc26adf1cc6686efe321c0d60e9d0aa70e9e03f7734f436de8419cb4",
                "95f5cba84460044788a61e2e8b1dd17766eab155a01e6163dbff08f6aa91ebfb",
            ]
        );
    }

    #[test]
    fn expert_grouping_changes_slices_but_not_segments() {
        let c = tiny();
        let dir = temp_dir("groups");
        let manifest = write_checkpoint(&c, Storage::Packed, false, &dir);
        let mut seen = Vec::new();
        for groups in [1, 2, 4, 8] {
            let m = slice_all(&dir, &manifest, groups, &dir.join(format!("g{groups}")));
            let slices = m["slices"].as_array().unwrap();
            // embed, head, the dense layer, and 1 + G per MoE layer.
            assert_eq!(slices.len(), 3 + (c.n_layers - 1) * (1 + groups));
            let group1 = slices.iter().find(|s| s["name"] == "layer.1.experts.1");
            assert_eq!(group1.is_some(), groups > 1);
            if groups == 4 {
                assert_eq!(group1.unwrap()["experts"], json!([2, 4]));
                let q4 = &group1.unwrap()["tensors"][0];
                assert_eq!(q4["name"], "layers.1.experts.w_gate.q4");
                assert_eq!(q4["shape"], json!([2, 32, 16]));
            }
            seen.push(segments_of(&m));
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]));
        assert!(SliceSource::open(&dir, &manifest, None, 3).is_err());
    }

    #[test]
    fn slices_do_not_depend_on_the_thread_count() {
        let c = tiny();
        let dir = temp_dir("threads");
        let manifest = write_checkpoint(&c, Storage::Bf16, false, &dir);
        let mut digests = Vec::new();
        for threads in [1, 3] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let out = dir.join(format!("t{threads}"));
            let m = pool.install(|| slice_all(&dir, &manifest, 2, &out));
            digests.push(m["manifest_blake3"].clone());
        }
        assert_eq!(digests[0], digests[1]);
    }

    #[test]
    fn yarn_checkpoints_slice_but_have_no_model_object() {
        let c = tiny();
        let dir = temp_dir("yarn");
        let manifest = write_checkpoint(&c, Storage::Packed, true, &dir);
        let out = dir.join("slices");
        let m = slice_all(&dir, &manifest, 2, &out);
        assert_eq!(m["pending"], json!(["rope_scaling yarn"]));
        assert!(m["model"].is_null() && m["model_root"].is_null() && m["tables"].is_null());
        assert_eq!(m["shape"]["n_routed_experts"], 8);
        let parsed = SliceManifest::parse(
            crate::modern::package::manifest_text(&m)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        // The segment digests still check from the slices alone.
        let verified = verify_slices(&parsed, &out, &[], true).unwrap();
        assert_eq!(
            verified["segments_checked"].as_array().unwrap().len(),
            c.n_layers + 2
        );
        assert!(assemble_stage(&parsed, &out, StageSpec::full(&c), &dir.join("x")).is_err());
        let err = convert_stage(
            &dir,
            &SourceManifest::read(&manifest).unwrap(),
            None,
            ExpertFormat::Int4G32,
            &dir.join("y"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("rope_scaling yarn"), "{err}");
    }

    #[test]
    fn changed_slices_and_manifests_are_detected() {
        let c = tiny();
        let dir = temp_dir("tamper");
        let manifest = write_checkpoint(&c, Storage::Packed, false, &dir);
        let out = dir.join("slices");
        let m = slice_all(&dir, &manifest, 2, &out);
        let parsed = SliceManifest::parse(
            crate::modern::package::manifest_text(&m)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let only = vec!["layer.2.experts".to_string()];
        let ok = verify_slices(&parsed, &out, &only, false).unwrap();
        assert_eq!(
            ok["slices_checked"],
            json!(["layer.2.experts.0", "layer.2.experts.1"])
        );
        let target = parsed
            .slices
            .iter()
            .find(|s| s.name == "layer.2.experts.1")
            .unwrap();
        let path = target.path(&out);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[5] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        assert!(verify_slices(&parsed, &out, &only, false).is_err());
        assert!(verify_slices(&parsed, &out, &["layer.1".to_string()], false).is_ok());
        let mut forged = m.clone();
        forged["slices"][0]["bytes"] = json!(1);
        let text = crate::modern::package::manifest_text(&forged).unwrap();
        assert!(SliceManifest::parse(text.as_bytes()).is_err());
        let _ = c;
    }

    #[test]
    fn bad_packed_sources_are_refused() {
        let c = tiny();
        let dir = temp_dir("bad");
        let manifest = write_checkpoint(&c, Storage::Packed, false, &dir);
        let src = SliceSource::open(&dir, &manifest, None, 2).unwrap();
        assert_eq!(src.config.expert_format, ExpertFormat::Int4G32);
        assert!(SliceSource::open(&dir, &manifest, Some(ExpertFormat::Int8Dyadic), 2).is_err());
        // A negative group scale in layer 1's shard (the manifest is re-pinned
        // so only the value check can refuse it).
        let shard = dir.join(format!("model-00002-of-{:05}.safetensors", c.n_layers + 2));
        let file = crate::modern::safetensors::SafetensorsFile::open(&shard).unwrap();
        let info =
            &file.tensors["language_model.model.layers.1.mlp.experts.3.up_proj.weight_scale"];
        let mut bytes = std::fs::read(&shard).unwrap();
        bytes[info.begin as usize + 1] |= 0x80;
        std::fs::write(&shard, &bytes).unwrap();
        let mut pinned: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        for f in pinned["files"].as_array_mut().unwrap() {
            if f["name"] == shard.file_name().unwrap().to_str().unwrap() {
                f["sha256"] = Value::from(sha256_hex(&bytes));
            }
        }
        std::fs::write(&manifest, serde_json::to_vec(&pinned).unwrap()).unwrap();
        let src = SliceSource::open(&dir, &manifest, None, 2).unwrap();
        let err = convert_units(&src, &dir, &[Unit::Layer(1)], &dir.join("out"), false)
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("negative, infinite or NaN"),
            "{err}"
        );
        // An unpinned shard is refused before any conversion.
        bytes[info.begin as usize + 1] ^= 0x80;
        std::fs::write(&shard, &bytes).unwrap();
        assert!(convert_units(&src, &dir, &[Unit::Layer(1)], &dir.join("out"), false).is_err());
    }

    #[test]
    fn the_plan_holds_one_shard_at_a_time() {
        let c = tiny();
        let dir = temp_dir("plan");
        let manifest = write_checkpoint(&c, Storage::Packed, false, &dir);
        let src = SliceSource::open(&dir, &manifest, None, 2).unwrap();
        let plan = plan(&src, &dir, &all_units(&c)).unwrap();
        let steps = plan["steps"].as_array().unwrap();
        assert_eq!(steps.len(), c.n_layers + 1);
        assert_eq!(steps[0]["units"], json!(["layer.0"]));
        assert_eq!(steps[c.n_layers]["units"], json!(["embed", "head"]));
        let largest = steps
            .iter()
            .map(|s| s["source_bytes"].as_u64().unwrap())
            .max()
            .unwrap();
        assert_eq!(plan["peak_source_bytes"].as_u64().unwrap(), largest);
        for step in steps {
            assert_eq!(step["shards"], step["release"]);
        }
        let some = plan_units(&src, &dir, &[Unit::Layer(2), Unit::Layer(3)]);
        assert_eq!(some.len(), 2);
    }

    fn plan_units(src: &SliceSource, dir: &Path, units: &[Unit]) -> Vec<Value> {
        plan(src, dir, units).unwrap()["steps"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[test]
    fn kimi_k26_slice_sizes() {
        // Kimi-K2.6's text model with the section 13 experts: per MoE layer a
        // core of attention, norms, router and shared expert, and 384 experts
        // of 3 x 7168 x 2048 INT4 values with BF16 group-32 scales.
        let c = MlaConfig {
            architecture: "kimi_k2".into(),
            n_layers: 61,
            d_model: 7168,
            n_heads: 64,
            q_lora_rank: 1536,
            kv_lora_rank: 512,
            qk_nope_dim: 128,
            qk_rope_dim: 64,
            v_head_dim: 128,
            d_ff: 18432,
            first_k_dense: 1,
            n_routed_experts: 384,
            n_experts_per_tok: 8,
            n_shared_experts: 1,
            moe_d_ff: 2048,
            n_group: 1,
            topk_group: 1,
            norm_topk_prob: true,
            routed_scaling_q32: 12_141_872_546,
            vocab_size: 163_840,
            max_seq: 4096,
            rms_eps_q32: 42_950,
            rope_theta: 50_000,
            attention_lambda: crate::modern::tables::attention_lambda(192),
            expert_format: ExpertFormat::Int4G32,
            preparation: None,
            precision: None,
        };
        c.validate().unwrap();
        let bytes = |unit: Unit, experts: bool| -> u64 {
            unit.entries(&c)
                .iter()
                .filter(|e| e.name.contains(".experts.") == experts)
                .map(|e| e.bytes)
                .sum()
        };
        let per_expert = 3 * 7168 * 2048 / 2 + 3 * 7168 * 2048 / 32 * 2;
        assert_eq!(per_expert, 24_772_608);
        assert_eq!(bytes(Unit::Layer(1), true), 384 * per_expert);
        assert_eq!(bytes(Unit::Layer(1), false), 151_170_752);
        assert_eq!(bytes(Unit::Layer(0), false), 498_147_648);
        assert_eq!(bytes(Unit::Embed, false), 1_175_224_320);
        assert_eq!(bytes(Unit::Head, false), 1_175_281_664);
        let total: u64 = all_units(&c)
            .into_iter()
            .map(|u| bytes(u, false) + bytes(u, true))
            .sum();
        assert_eq!(total, 582_679_787_072);
    }
}
