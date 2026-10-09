//! Explicit pending-slice -> YaRN package transaction. Legacy assembly stays closed.
use super::*;
use crate::modern::mla::yarn::{self, Preparation, Scope};
use sha2::{Digest, Sha256};

/// Validate independently pinned source/config and select a distinct preparation.
/// This phase checks metadata only; assembly below verifies the actual bytes.
pub fn prepare_yarn_manifest(
    input: &SliceManifest,
    config_bytes: &[u8],
    source_bytes: &[u8],
    scope: Scope,
    max_seq: usize,
) -> Result<(MlaConfig, Value), ModernError> {
    prepare_yarn_manifest_with_precision(
        input,
        config_bytes,
        source_bytes,
        scope,
        max_seq,
        Some(super::super::precision::Precision::all_int16()),
    )
}

/// Caller-owned policy must match the committed conversion policy.
pub fn prepare_yarn_manifest_with_precision(
    input: &SliceManifest,
    config_bytes: &[u8],
    source_bytes: &[u8],
    scope: Scope,
    max_seq: usize,
    precision: Option<super::super::precision::Precision>,
) -> Result<(MlaConfig, Value), ModernError> {
    if let Some(p) = &precision {
        p.validate()?;
    }
    if manifest_precision(&input.value)? != precision {
        return Err(invalid("slice precision differs from requested policy"));
    }
    // Reparse to reject mutations to the public parsed fields or value.
    let sealed = SliceManifest::parse(canonical(&input.value)?.as_bytes())?;
    if sealed.segments != input.segments || sealed.slices != input.slices {
        return Err(invalid(
            "slice manifest parsed fields differ from sealed input",
        ));
    }
    let source = SourceManifest::parse(source_bytes)?;
    let pin = source
        .files
        .iter()
        .find(|f| f.name == "config.json")
        .ok_or_else(|| invalid("source does not pin config.json"))?;
    if pin.bytes != config_bytes.len() as u64
        || pin.sha256 != hex_lower(&Sha256::digest(config_bytes))
        || source.header_json() != input.value["source"]
    {
        return Err(invalid("source/config substitution"));
    }
    let hf = parse_hf_weights_config(config_bytes, max_seq)?;
    let mut original = hf.config;
    original.expert_format = ExpertFormat::Int4G32;
    original.precision = precision.clone();
    if input.value["profile"] != original.profile()
        || input.value["pending"] != json!(["rope_scaling yarn"])
        || hf.pending != ["rope_scaling yarn"]
        || input.value["weights"] != json!({"prefix":hf.source.prefix,"packed_experts":true})
        || !hf.source.packed_experts
        || input.value["shape"] != shape_json(&original)
        || !["model", "tables", "model_root"]
            .iter()
            .all(|k| input.value.get(k).is_some_and(Value::is_null))
    {
        return Err(invalid("not the pinned pending YaRN slice layout"));
    }
    let mut c = match scope {
        Scope::SyntheticFixture => {
            let v: Value =
                serde_json::from_slice(config_bytes).map_err(|e| invalid(e.to_string()))?;
            let official: Value =
                serde_json::from_slice(yarn::CONFIG).map_err(|e| invalid(e.to_string()))?;
            if v["text_config"]["rope_scaling"] != official["text_config"]["rope_scaling"] {
                return Err(invalid("fixture must use the pinned YaRN equations"));
            }
            let mut c = original.clone();
            c.architecture = "arc-test/kimi-k26-yarn".into();
            c.attention_lambda = yarn::ATTENTION_LAMBDA;
            c.preparation = Some(Preparation { scope });
            c.validate()?;
            c
        }
        Scope::FullKimiK26 => yarn::official_config(config_bytes, max_seq, None)?,
        Scope::EarlyLayersWithHeadProbe { layers } => {
            yarn::official_config(config_bytes, max_seq, Some(layers))?
        }
    };
    c.precision = precision;
    c.validate()?;
    let allowed = package::segment_names(&original);
    let mut previous = 0;
    for s in &input.segments {
        let index = allowed
            .iter()
            .position(|n| n == &s.name)
            .ok_or_else(|| invalid("unknown source segment"))?;
        if index == 0 || index <= previous {
            return Err(invalid("source segments reordered or duplicated"));
        }
        previous = index;
    }
    let weights = package::segment_names(&c)
        .iter()
        .skip(1)
        .map(|name| {
            input
                .segments
                .iter()
                .find(|s| &s.name == name)
                .cloned()
                .ok_or_else(|| invalid(format!("missing selected segment {name}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let finalized = yarn::finalize_manifest(
        &c,
        &input.value["source"],
        &weights,
        &hf.eos,
        &json!({"status":"not_prepared","reason":"tokenizer provenance required before text serving"}),
    )?;
    Ok((c, finalized))
}

struct Tee<'a> {
    writer: &'a mut StageWriter,
    verifier: &'a mut SegmentSlicer,
}
impl TensorSink for Tee<'_> {
    fn begin(&mut self, name: &str) -> Result<(), ModernError> {
        self.writer.begin(name)?;
        self.verifier.begin(name)
    }
    fn chunk(&mut self, bytes: &[u8]) -> Result<(), ModernError> {
        self.writer.chunk(bytes)?;
        self.verifier.chunk(bytes)
    }
    fn end(&mut self) -> Result<(), ModernError> {
        self.writer.end()?;
        self.verifier.end()
    }
}

struct Staging(PathBuf, bool);
impl Drop for Staging {
    fn drop(&mut self) {
        if !self.1 {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Publish a new directory containing every stage, finalized manifest and report
/// only after byte verification. No caller-provided prepared model is trusted.
pub fn assemble_yarn_bundle(
    input: &SliceManifest,
    config_bytes: &[u8],
    source_bytes: &[u8],
    scope: Scope,
    dir: &Path,
    out: &Path,
    stage_count: usize,
) -> Result<Value, ModernError> {
    assemble_yarn_bundle_with_precision(
        input,
        config_bytes,
        source_bytes,
        scope,
        dir,
        out,
        stage_count,
        Some(super::super::precision::Precision::all_int16()),
    )
}

/// Atomic assembly with an explicitly expected conversion policy.
#[allow(clippy::too_many_arguments)]
pub fn assemble_yarn_bundle_with_precision(
    input: &SliceManifest,
    config_bytes: &[u8],
    source_bytes: &[u8],
    scope: Scope,
    dir: &Path,
    out: &Path,
    stage_count: usize,
    precision: Option<super::super::precision::Precision>,
) -> Result<Value, ModernError> {
    let source = SourceManifest::parse(source_bytes)?;
    let (c, finalized) = prepare_yarn_manifest_with_precision(
        input,
        config_bytes,
        source_bytes,
        scope,
        source.max_seq,
        precision,
    )?;
    if stage_count == 0 || stage_count > c.n_layers || out.exists() {
        return Err(invalid(
            "stage count invalid or output directory already exists",
        ));
    }
    let groups = input.value["expert_groups"]
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| invalid("expert groups missing"))?;
    if groups == 0 || groups > c.n_routed_experts || !c.n_routed_experts.is_multiple_of(groups) {
        return Err(invalid("invalid expert groups"));
    }
    let selected = package::segment_names(&c);
    let selected_slices: Vec<_> = input
        .slices
        .iter()
        .filter(|s| selected.contains(&s.segment))
        .collect();
    let mut expected_order = Vec::new();
    for name in selected.iter().skip(1) {
        let unit = Unit::parse(name)?;
        let slices = input.slices_of(name);
        let count = if unit.is_moe(&c) { groups + 1 } else { 1 };
        if slices.len() != count {
            return Err(invalid("selected slice coverage differs from layout"));
        }
        for s in slices {
            expected_order.push(s.name.clone());
            if s.tensors
                .iter()
                .any(|t| t.offset.checked_add(t.bytes).is_none_or(|n| n > s.bytes))
            {
                return Err(invalid("slice tensor range invalid"));
            }
            if std::fs::metadata(s.path(dir))
                .map_err(|e| io(&s.path(dir), e))?
                .len()
                != s.bytes
            {
                return Err(invalid("selected slice file length mismatch"));
            }
        }
    }
    if selected_slices
        .iter()
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
        != expected_order
    {
        return Err(invalid("selected slices reordered"));
    }
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staging_path = parent.join(format!(
        ".yarn-assembly-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&staging_path).map_err(|e| io(&staging_path, e))?;
    let mut staging = Staging(staging_path, false);
    let manifest_bytes = canonical(&finalized)?.into_bytes();
    let (cos, sin) = yarn::tables(&c)?;
    let mut packages = Vec::new();
    for i in 0..stage_count {
        let stage = StageSpec {
            first_layer: i * c.n_layers / stage_count,
            end_layer: (i + 1) * c.n_layers / stage_count,
        };
        let entries = package::layout(&c, stage);
        let path = staging.0.join(format!("stage-{i}.arcspkg"));
        let header = package::header_json(&c, &input.value["source"], stage, &entries);
        let mut writer = StageWriter::create(&path, &header, entries.clone())?;
        writer.write_tensor("rope.cos", &i32_bytes(&cos))?;
        writer.write_tensor("rope.sin", &i32_bytes(&sin))?;
        for name in selected.iter().skip(1) {
            let segment: Vec<_> = entries
                .iter()
                .filter(|e| &e.segment == name)
                .cloned()
                .collect();
            if segment.is_empty() {
                continue;
            }
            // Bound open descriptors/read buffers to one segment, even when a
            // stage contains many MoE layers.
            let mut reader = SliceReader {
                dir: dir.to_path_buf(),
                open: BTreeMap::new(),
            };
            let mut verifier =
                SegmentSlicer::create(&c, Unit::parse(name)?, groups, &staging.0, true)?;
            // Hash and reconstruct canonical slice records from exactly the bytes
            // delivered to the writer, preventing check-then-reopen substitution.
            for e in &segment {
                let total = input
                    .slices_of(name)
                    .iter()
                    .flat_map(|s| &s.tensors)
                    .filter(|t| t.name == e.name)
                    .try_fold(0u64, |a, t| a.checked_add(t.bytes));
                if total != Some(e.bytes) {
                    return Err(invalid("selected tensor coverage mismatch"));
                }
            }
            replay_segment(
                input,
                &mut reader,
                &segment,
                &mut Tee {
                    writer: &mut writer,
                    verifier: &mut verifier,
                },
            )?;
            let verified = verifier.finish()?;
            if input.segments.iter().find(|s| s.name == *name) != Some(&verified.segment)
                || input.slices_of(name) != verified.slices.iter().collect::<Vec<_>>()
            {
                return Err(invalid(format!(
                    "selected bytes/metadata differ from committed segment {name}"
                )));
            }
        }
        let (digest, segments) = writer.finish()?;
        if package::digest_file(&path)? != digest {
            return Err(invalid("assembled file digest mismatch"));
        }
        let verified = package::verify_against_manifest(
            &package::read_header_file(&path)?,
            &segments,
            &manifest_bytes,
        )?;
        packages.push(json!({"file":format!("stage-{i}.arcspkg"),"stage":stage.to_json(),"package":digest.to_json(),"verification":verified}));
    }
    let report = json!({"schema":"arc.yarn-slice-assembly.v1","profile":c.profile(),
        "slice_manifest_blake3":input.value["manifest_blake3"],"model_root":finalized["model_root"],
        "selected_bytes_verified":true,"packages_verified":true,"packages":packages});
    for (name, value) in [("manifest.json", &finalized), ("report.json", &report)] {
        let path = staging.0.join(name);
        std::fs::write(&path, canonical(value)?).map_err(|e| io(&path, e))?;
    }
    if out.exists() {
        return Err(invalid("output appeared during assembly"));
    }
    std::fs::rename(&staging.0, out).map_err(|e| io(out, e))?;
    staging.1 = true;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/protocol/reference/kimi-k26/source.json"
    ));

    fn pending_official() -> Value {
        let mut c = parse_hf_weights_config(yarn::CONFIG, 4096).unwrap().config;
        c.expert_format = ExpertFormat::Int4G32;
        let layout = package::layout(&c, StageSpec::full(&c));
        let segments: Vec<_> = package::segment_names(&c).iter().skip(1).map(|name| json!({
            "name":name, "bytes":layout.iter().filter(|e| &e.segment==name).map(|e|e.bytes).sum::<u64>(),
            "blake3":"a".repeat(64)
        })).collect();
        let mut v = json!({"schema":SLICE_MANIFEST_SCHEMA,"profile":super::super::super::PROFILE_I4G32,
            "source":SourceManifest::parse(SOURCE).unwrap().header_json(), "shape":shape_json(&c),
            "pending":["rope_scaling yarn"],"model":null,"tables":null,"model_root":null,
            "weights":{"prefix":"language_model.","packed_experts":true},"segments":segments,"slices":[]});
        seal(&mut v);
        v
    }
    fn seal(v: &mut Value) {
        v["manifest_blake3"] = crate::model_package::manifest_body_blake3(v)
            .unwrap()
            .into();
    }
    fn parsed(v: &Value) -> SliceManifest {
        SliceManifest::parse(canonical(v).unwrap().as_bytes()).unwrap()
    }

    #[test]
    fn precision_budgets_equal_actual_layout_and_reference_element_counts() {
        use crate::modern::mla::precision::{Bits, Precision};
        let data: Value = serde_json::from_str(include_str!(
            "../../../../../../docs/protocol/reference/kimi-k26/precision-budgets.json"
        ))
        .unwrap();
        for row in data["rows"].as_array().unwrap() {
            let depth = row["layers"].as_u64().unwrap() as usize;
            let mut c = parse_hf_weights_config(yarn::CONFIG, 4096).unwrap().config;
            c.expert_format = ExpertFormat::Int4G32;
            if row["policy"] != "legacy" {
                let mut p = Precision::all_int16();
                if row["policy"] == "mixed" {
                    p.embedding = Bits::Int8;
                    p.shared = Bits::Int8;
                }
                c.precision = Some(p);
            }
            let mut units = vec![Unit::Embed];
            units.extend((0..depth).map(Unit::Layer));
            units.push(Unit::Head);
            let entries: Vec<_> = units.iter().flat_map(|u| u.entries(&c)).collect();
            let bytes: u64 = entries.iter().map(|e| e.bytes).sum();
            assert_eq!(row["slice_payload_bytes"], bytes);
            let fp32: u64 = entries
                .iter()
                .map(|e| {
                    if e.name.ends_with(".q4") {
                        e.bytes * 8
                    } else if e.name.ends_with(".q")
                        || e.name.contains("norm")
                        || e.name.ends_with("router_bias")
                    {
                        e.shape.iter().map(|&n| n as u64).product::<u64>() * 4
                    } else {
                        0
                    }
                })
                .sum();
            assert_eq!(row["reference_fp32_parameters_bytes"], fp32);
            let expected = row["retained_source_bytes"].as_u64().unwrap()
                + 2 * bytes
                + row["canonical_table_bytes"].as_u64().unwrap()
                + row["disk_header_alignment_scratch_margin_bytes"]
                    .as_u64()
                    .unwrap()
                + row["disk_reserve_bytes"].as_u64().unwrap();
            assert_eq!(row["peak_disk_with_retained_sources_bytes"], expected);
        }
    }

    #[test]
    fn precision_official_full_probe_controls_bind_policy_and_layout() {
        use crate::modern::mla::precision::{Bits, Precision};
        for mixed in [false, true] {
            let mut precision = Precision::all_int16();
            if mixed {
                precision.embedding = Bits::Int8;
                precision.shared = Bits::Int8;
            }
            let mut original = parse_hf_weights_config(yarn::CONFIG, 4096).unwrap().config;
            original.expert_format = ExpertFormat::Int4G32;
            original.precision = Some(precision.clone());
            let layout = package::layout(&original, StageSpec::full(&original));
            let mut v = pending_official();
            v["precision"] = serde_json::to_value(&precision).unwrap();
            v["profile"] = original.profile().into();
            for seg in v["segments"].as_array_mut().unwrap() {
                let bytes: u64 = layout
                    .iter()
                    .filter(|e| e.segment == seg["name"].as_str().unwrap())
                    .map(|e| e.bytes)
                    .sum();
                seg["bytes"] = bytes.into();
            }
            seal(&mut v);
            for depth in [None, Some(1), Some(2), Some(3)] {
                let scope = depth.map_or(Scope::FullKimiK26, |layers| {
                    Scope::EarlyLayersWithHeadProbe { layers }
                });
                let (c, m) = prepare_yarn_manifest_with_precision(
                    &parsed(&v),
                    yarn::CONFIG,
                    SOURCE,
                    scope.clone(),
                    4096,
                    Some(precision.clone()),
                )
                .unwrap();
                assert_eq!(c.precision, Some(precision.clone()));
                assert_eq!(MlaConfig::from_json(&m["model"]).unwrap(), c);
                assert_eq!(m["profile"], c.profile());
                let default =
                    prepare_yarn_manifest(&parsed(&v), yarn::CONFIG, SOURCE, scope.clone(), 4096);
                if mixed {
                    assert!(default.is_err());
                } else {
                    assert_eq!(default.unwrap(), (c.clone(), m.clone()));
                }
                let meta = yarn::finalize_pending_slices_with_precision(
                    yarn::CONFIG,
                    &v,
                    4096,
                    depth,
                    Some(precision.clone()),
                )
                .unwrap();
                assert_eq!(meta["model_root"], m["model_root"]);
                if !mixed {
                    assert_eq!(
                        yarn::finalize_pending_slices(yarn::CONFIG, &v, 4096, depth).unwrap(),
                        meta
                    );
                }
                assert!(
                    prepare_yarn_manifest(
                        &parsed(&pending_official()),
                        yarn::CONFIG,
                        SOURCE,
                        scope.clone(),
                        4096
                    )
                    .is_err()
                );
                let mut relabeled = pending_official();
                relabeled["precision"] = v["precision"].clone();
                relabeled["profile"] = v["profile"].clone();
                seal(&mut relabeled);
                assert!(
                    prepare_yarn_manifest_with_precision(
                        &parsed(&relabeled),
                        yarn::CONFIG,
                        SOURCE,
                        scope,
                        4096,
                        Some(precision.clone())
                    )
                    .is_err()
                );
                if depth.is_some() {
                    println!("precision probe {mixed} {depth:?} root {}", m["model_root"]);
                }
            }
        }
    }

    #[test]
    fn official_full_and_probe_preparation_controls_keep_scope_and_source() {
        // Commitment metadata only: no large weight payload is allocated or claimed.
        let v = pending_official();
        for depth in [None, Some(1), Some(2), Some(3)] {
            let scope = depth.map_or(Scope::FullKimiK26, |layers| {
                Scope::EarlyLayersWithHeadProbe { layers }
            });
            let (c, m) = prepare_yarn_manifest_with_precision(
                &parsed(&v),
                yarn::CONFIG,
                SOURCE,
                scope.clone(),
                4096,
                None,
            )
            .unwrap();
            assert_eq!(c.preparation.as_ref().unwrap().scope, scope);
            assert_eq!(c.n_layers, depth.unwrap_or(61));
            assert_eq!(MlaConfig::from_json(&m["model"]).unwrap(), c);
            let old =
                yarn::finalize_pending_slices_with_precision(yarn::CONFIG, &v, 4096, depth, None)
                    .unwrap();
            assert_eq!(m["model_root"], old["model_root"]);
            assert_eq!(m["segments"], old["segments"]);
            assert_eq!(m["source"], old["source"]);
        }
        let mut c = yarn::CONFIG.to_vec();
        c.push(b' ');
        assert!(
            prepare_yarn_manifest_with_precision(
                &parsed(&v),
                &c,
                SOURCE,
                Scope::FullKimiK26,
                4096,
                None
            )
            .is_err()
        );
        for kind in ["source", "shape", "order", "missing", "scope"] {
            let mut bad = v.clone();
            match kind {
                "source" => bad["source"]["revision"] = "1".repeat(40).into(),
                "shape" => bad["shape"]["n_layers"] = 2.into(),
                "order" => bad["segments"].as_array_mut().unwrap().reverse(),
                "missing" => {
                    bad["segments"].as_array_mut().unwrap().remove(0);
                }
                "scope" => {
                    bad["model"] =
                        json!({"preparation":{"scope":{"kind":"full_kimi_k26","layers":2}}})
                }
                _ => unreachable!(),
            }
            seal(&mut bad);
            assert!(
                prepare_yarn_manifest_with_precision(
                    &parsed(&bad),
                    yarn::CONFIG,
                    SOURCE,
                    Scope::EarlyLayersWithHeadProbe { layers: 2 },
                    4096,
                    None
                )
                .is_err(),
                "{kind}"
            );
        }
    }
}
