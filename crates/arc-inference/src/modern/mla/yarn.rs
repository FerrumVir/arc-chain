//! Pinned K2.6 YaRN preparation and manifest finalization. No weight fetching.
//! Original v1 profiles and their refusal paths remain unchanged.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::config::{ExpertFormat, MlaConfig};
use super::package::{self, SegmentDigest, StageSpec};
pub use super::yarn_constants::ATTENTION_LAMBDA;
use crate::modern::{ModernError, hex_lower};

pub const CONTRACT: &str = "docs/protocol/kimi-k26-yarn-v1.md";
pub const PROFILE: &str = "arc.kimi-k26.mla-moe.i4g32.yarn.q16.v1";
pub const PROBE_PROFILE: &str = "arc.experimental.kimi-k26.early-layers-head.i4g32.yarn.q16.v1";
pub const FIXTURE_PROFILE: &str = "arc.synthetic.kimi-k26-yarn.i4g32.q16.v1";
pub const CONFIG_SHA256: &str = "85825ca6e18cbe539eb83ee09eedfb3f4222265929f06e9f535a6d9364f55899";
pub const CONFIG: &[u8] =
    include_bytes!("../../../../../docs/protocol/reference/kimi-k26/config.json");
const SOURCE: &[u8] = include_bytes!("../../../../../docs/protocol/reference/kimi-k26/source.json");
const SCHEME: &str = "arc.kimi-k26.yarn-q62-preparation.v1";

fn invalid(s: impl Into<String>) -> ModernError {
    ModernError::Invalid(s.into())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope {
    FullKimiK26,
    /// Original layers [0, layers), followed directly by the original head.
    /// This deliberately is NOT complete K2.6 generation.
    EarlyLayersWithHeadProbe {
        layers: usize,
    },
    /// Tiny random weights; preparation constants alone match the official config.
    SyntheticFixture,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preparation {
    pub scope: Scope,
}

pub fn is_profile(p: &str) -> bool {
    [PROFILE, PROBE_PROFILE, FIXTURE_PROFILE].contains(&p)
}

impl Preparation {
    pub fn profile(&self) -> &'static str {
        match self.scope {
            Scope::FullKimiK26 => PROFILE,
            Scope::EarlyLayersWithHeadProbe { .. } => PROBE_PROFILE,
            Scope::SyntheticFixture => FIXTURE_PROFILE,
        }
    }
    pub fn to_json(&self) -> Value {
        json!({"scheme": SCHEME, "config_sha256": CONFIG_SHA256, "scope": self.scope})
    }
    pub fn from_json(v: &Value) -> Result<Self, ModernError> {
        if v.as_object().map(|o| o.len()) != Some(3)
            || v["scheme"] != SCHEME
            || v["config_sha256"] != CONFIG_SHA256
        {
            return Err(invalid("unsupported or unpinned YaRN preparation"));
        }
        let scope: Scope = serde_json::from_value(v["scope"].clone())
            .map_err(|e| invalid(format!("YaRN scope: {e}")))?;
        // Serde's internally tagged unit variants can discard unknown fields
        // despite deny_unknown_fields. Identity-bearing metadata must survive
        // parsing exactly, for unit scopes as well as the layer probe.
        if json!(scope) != v["scope"] {
            return Err(invalid("YaRN scope fields differ from the canonical scope"));
        }
        Ok(Self { scope })
    }
    pub fn validate(&self, c: &MlaConfig) -> Result<(), ModernError> {
        if c.expert_format != ExpertFormat::Int4G32
            || c.rope_theta != 50_000
            || c.qk_rope_dim != 64
            || c.qk_nope_dim != 128
            || !(1..=262_144).contains(&c.max_seq)
            || c.attention_lambda != ATTENTION_LAMBDA
        {
            return Err(invalid("model differs from versioned K2.6 YaRN constants"));
        }
        match self.scope {
            Scope::SyntheticFixture => {
                if c.architecture != "arc-test/kimi-k26-yarn" {
                    return Err(invalid(
                        "synthetic YaRN scope requires synthetic architecture",
                    ));
                }
            }
            _ => {
                let mut expected = official_base(c.max_seq)?;
                if let Scope::EarlyLayersWithHeadProbe { layers } = self.scope {
                    if !(1..=3).contains(&layers) {
                        return Err(invalid("probe needs 1..=3 early layers"));
                    }
                    expected.n_layers = layers;
                }
                expected.expert_format = ExpertFormat::Int4G32;
                expected.attention_lambda = ATTENTION_LAMBDA;
                expected.preparation = Some(self.clone());
                if &expected != c {
                    return Err(invalid(
                        "YaRN model shape differs from pinned official scope",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn official_base(max_seq: usize) -> Result<MlaConfig, ModernError> {
    let v: Value = serde_json::from_slice(CONFIG).map_err(|e| invalid(e.to_string()))?;
    // New, explicitly versioned parser entry. The legacy parser still refuses
    // this file. No source fields or pending conditions are deleted or hidden.
    Ok(super::config::parse_text_config(&v["text_config"], max_seq)?.config)
}

pub fn official_config(
    bytes: &[u8],
    max_seq: usize,
    probe_layers: Option<usize>,
) -> Result<MlaConfig, ModernError> {
    if hex_lower(&Sha256::digest(bytes)) != CONFIG_SHA256 {
        return Err(invalid(
            "K2.6 config SHA-256 does not match the pinned official revision",
        ));
    }
    let mut c = official_base(max_seq)?;
    let scope = if let Some(layers) = probe_layers {
        if !(1..=3).contains(&layers) {
            return Err(invalid("probe needs 1..=3 early layers"));
        }
        c.n_layers = layers;
        Scope::EarlyLayersWithHeadProbe { layers }
    } else {
        Scope::FullKimiK26
    };
    c.expert_format = ExpertFormat::Int4G32;
    c.attention_lambda = ATTENTION_LAMBDA;
    c.preparation = Some(Preparation { scope });
    c.validate()?;
    Ok(c)
}

pub fn tables(c: &MlaConfig) -> Result<(Vec<i32>, Vec<i32>), ModernError> {
    c.validate()?;
    if c.preparation.is_some() {
        crate::modern::tables::rope_tables_frequencies(
            &super::yarn_constants::FREQUENCIES,
            c.max_seq,
        )
    } else {
        crate::modern::tables::rope_tables(c.rope_theta, c.qk_rope_dim, c.max_seq)
    }
}

pub fn tables_digest(c: &MlaConfig) -> Result<SegmentDigest, ModernError> {
    let (cos, sin) = tables(c)?;
    let mut hasher = blake3::Hasher::new();
    for v in cos.iter().chain(&sin) {
        hasher.update(&v.to_le_bytes());
    }
    Ok(SegmentDigest {
        name: "tables".into(),
        bytes: ((cos.len() + sin.len()) * 4) as u64,
        blake3: hex_lower(hasher.finalize().as_bytes()),
    })
}

/// Commit a complete set of weight segments under the new preparation.
/// This checks commitment metadata, not unseen weight bytes. Stage/package
/// verification must still rehash bytes before execution.
pub fn finalize_manifest(
    c: &MlaConfig,
    source: &Value,
    weights: &[SegmentDigest],
    eos: &[u32],
    tokenizer: &Value,
) -> Result<Value, ModernError> {
    c.validate()?;
    let prep = c
        .preparation
        .as_ref()
        .ok_or_else(|| invalid("finalization requires explicit YaRN preparation"))?;
    if prep.scope != Scope::SyntheticFixture {
        let official = crate::modern::convert::SourceManifest::parse(SOURCE)?.header_json();
        if source != &official {
            return Err(invalid("weight source differs from pinned K2.6 source"));
        }
    } else if !source["repo"]
        .as_str()
        .is_some_and(|s| s.starts_with("arc-test/"))
    {
        return Err(invalid("fixture finalization requires an arc-test source"));
    }
    let layout = package::layout(c, StageSpec::full(c));
    let names = package::segment_names(c);
    if weights.len() + 1 != names.len() {
        return Err(invalid("finalization needs every selected weight segment"));
    }
    for (s, name) in weights.iter().zip(names.iter().skip(1)) {
        let bytes: u64 = layout
            .iter()
            .filter(|e| &e.segment == name)
            .map(|e| e.bytes)
            .sum();
        if &s.name != name
            || s.bytes != bytes
            || s.blake3.len() != 64
            || !s
                .blake3
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid(format!("noncanonical weight segment {}", s.name)));
        }
    }
    let mut segments = vec![tables_digest(c)?];
    segments.extend_from_slice(weights);
    package::build_manifest(c, source, &segments, None, eos, tokenizer)
}

/// Finalize a legacy *pending* ARC-72 manifest without mutating it. Produces
/// a stage manifest with an explicit new profile; the old slice consumer must
/// not silently consume it. Full scope needs all 61 layers; probe scope keeps
/// only embed, early layers, and head, with a distinct model-root identity.
pub fn finalize_pending_slices(
    config: &[u8],
    input: &Value,
    max_seq: usize,
    probe_layers: Option<usize>,
) -> Result<Value, ModernError> {
    let hash =
        crate::model_package::manifest_body_blake3(input).map_err(|e| invalid(e.to_string()))?;
    if input["manifest_blake3"] != hash
        || input["schema"] != "arc.integer-slice-manifest.v1"
        || input["profile"] != super::PROFILE_I4G32
        || input["pending"] != json!(["rope_scaling yarn"])
        || !["model", "tables", "model_root"]
            .iter()
            .all(|k| input.get(k).is_some_and(Value::is_null))
        || input["weights"] != json!({"prefix":"language_model.","packed_experts":true})
    {
        return Err(invalid("not a valid pending K2.6 weight manifest"));
    }
    let c = official_config(config, max_seq, probe_layers)?;
    let original = official_config(config, max_seq, None)?;
    let all: Vec<SegmentDigest> = input["segments"]
        .as_array()
        .ok_or_else(|| invalid("no segments"))?
        .iter()
        .map(SegmentDigest::from_json)
        .collect::<Result<_, _>>()?;
    let layout = package::layout(&original, StageSpec::full(&original));
    let mut seen = std::collections::BTreeSet::new();
    for s in &all {
        let size: u64 = layout
            .iter()
            .filter(|e| e.segment == s.name && e.segment != "tables")
            .map(|e| e.bytes)
            .sum();
        if size == 0
            || size != s.bytes
            || !seen.insert(&s.name)
            || s.blake3.len() != 64
            || !s
                .blake3
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("invalid/duplicate source weight segment"));
        }
    }
    let mut selected = Vec::new();
    for name in package::segment_names(&c).iter().skip(1) {
        selected.push(
            all.iter()
                .find(|s| &s.name == name)
                .ok_or_else(|| invalid(format!("missing selected segment {name}")))?
                .clone(),
        );
    }
    finalize_manifest(
        &c,
        &input["source"],
        &selected,
        &[163586],
        &json!({
            "status":"not_prepared", "reason":"tokenizer provenance must be supplied before text-serving certification"
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const VECTORS: &str =
        include_str!("../../../../../docs/protocol/reference/kimi-k26/yarn-vectors.json");

    #[test]
    fn official_yarn_vectors_and_full_context_are_pinned() {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(v["correction_range"], json!([8, 20]));
        assert_eq!(v["attention_lambda_q30"], ATTENTION_LAMBDA);
        for (got, expected) in super::super::yarn_constants::FREQUENCIES
            .iter()
            .zip(v["frequencies_q62"].as_array().unwrap())
        {
            assert_eq!(*got, i128::from(expected.as_i64().unwrap()));
        }
        let c = official_config(CONFIG, 262_144, None).unwrap();
        let (cos, sin) = tables(&c).unwrap();
        for sample in v["samples"].as_array().unwrap() {
            let ix = sample["position"].as_u64().unwrap() as usize * 32
                + sample["frequency"].as_u64().unwrap() as usize;
            assert_eq!(
                i64::from(cos[ix]),
                sample["cos_q16"].as_i64().unwrap(),
                "{sample}"
            );
            assert_eq!(
                i64::from(sin[ix]),
                sample["sin_q16"].as_i64().unwrap(),
                "{sample}"
            );
            assert!(
                (f64::from(cos[ix]) / 65536. - sample["reference_cos_f64"].as_f64().unwrap()).abs()
                    <= 0.5001 / 65536.
            );
            assert!(
                (f64::from(sin[ix]) / 65536. - sample["reference_sin_f64"].as_f64().unwrap()).abs()
                    <= 0.5001 / 65536.
            );
        }
        let c = official_config(CONFIG, 4096, None).unwrap();
        let digest = tables_digest(&c).unwrap().blake3;
        assert_eq!(
            digest,
            "0085d663f9d7284af6105325f57a9584c498c2e673f1df81b970e40f29c28e70"
        );
        println!("yarn tables 4096: {digest}");
        assert!(super::super::config::parse_hf_config(CONFIG, 4096).is_err());
        assert!(official_config(CONFIG, 0, None).is_err());
        assert!(official_config(CONFIG, 262_145, None).is_err());
        let mut altered = CONFIG.to_vec();
        altered.push(b' ');
        assert!(official_config(&altered, 4096, None).is_err());
    }

    // Real package-header parser with canonical JSON, the complete tensor table
    // and its correct declared file length. No weight payload is allocated.
    fn scope_header(c: &MlaConfig, model: Value) -> Result<package::StageHeader, ModernError> {
        let stage = StageSpec::full(c);
        let entries = package::layout(c, stage);
        let mut header = package::header_json(c, &json!({}), stage, &entries);
        header["model"] = model;
        let text = crate::model_package::canonical_json(&header).unwrap();
        let mut prefix = super::super::STAGE_MAGIC.to_vec();
        prefix.extend_from_slice(&(text.len() as u64).to_le_bytes());
        prefix.extend_from_slice(text.as_bytes());
        let last = entries.last().unwrap();
        let file_len =
            package::align_up(prefix.len() as u64) + package::align_up(last.offset + last.bytes);
        package::parse_header(&prefix, file_len)
    }

    fn scope_controls() -> Vec<MlaConfig> {
        let full = official_config(CONFIG, 4096, None).unwrap();
        let mut fixture = full.clone();
        fixture.architecture = "arc-test/kimi-k26-yarn".into();
        fixture.preparation = Some(Preparation {
            scope: Scope::SyntheticFixture,
        });
        let mut controls = vec![full, fixture];
        controls.extend((1..=3).map(|n| official_config(CONFIG, 4096, Some(n)).unwrap()));
        controls
    }

    #[test]
    fn scope_identity_round_trips_through_model_and_package_header() {
        for c in scope_controls() {
            let model = c.to_json();
            let parsed = MlaConfig::from_json(&model).unwrap();
            assert_eq!(parsed, c);
            assert_eq!(parsed.to_json(), model);
            let header = scope_header(&c, model.clone()).unwrap();
            assert_eq!(header.config, c);
            assert_eq!(header.value["model"], header.config.to_json());
            assert_eq!(header.value["profile"], c.profile());
        }
    }

    #[test]
    fn unit_scope_extra_fields_reject_in_model_and_package_header() {
        // Includes the review's full + layers:2 and synthetic + unexpected
        // reproductions, as well as null, matching-depth and nested extras.
        for c in scope_controls().into_iter().take(2) {
            for (key, value) in [
                ("layers", json!(2)),
                ("layers", json!(61)),
                ("layers", Value::Null),
                ("unexpected", json!("ignored")),
                ("probe", json!({"layers": 2})),
            ] {
                let mut model = c.to_json();
                model["preparation"]["scope"][key] = value;
                let direct = MlaConfig::from_json(&model);
                let header = scope_header(&c, model.clone());
                assert!(
                    direct.is_err(),
                    "model accepted {}",
                    model["preparation"]["scope"]
                );
                assert!(
                    header.is_err(),
                    "header accepted {}",
                    model["preparation"]["scope"]
                );
                assert!(direct.unwrap_err().to_string().contains("YaRN scope"));
                assert!(header.unwrap_err().to_string().contains("YaRN scope"));
            }
        }
    }

    #[test]
    fn probe_scope_remains_strict_in_model_and_package_header() {
        let c = official_config(CONFIG, 4096, Some(2)).unwrap();
        for scope in [
            json!({"kind":"early_layers_with_head_probe"}),
            json!({"kind":"early_layers_with_head_probe", "layers":0}),
            json!({"kind":"early_layers_with_head_probe", "layers":4}),
            json!({"kind":"early_layers_with_head_probe", "layers":1}),
            json!({"kind":"early_layers_with_head_probe", "layers":-1}),
            json!({"kind":"early_layers_with_head_probe", "layers":"2"}),
            json!({"kind":"early_layers_with_head_probe", "layers":null}),
            json!({"kind":"early_layers_with_head_probe", "layers":2, "unexpected":true}),
            json!({"kind":"other", "layers":2}),
        ] {
            let mut model = c.to_json();
            model["preparation"]["scope"] = scope;
            assert!(MlaConfig::from_json(&model).is_err(), "model: {model}");
            assert!(scope_header(&c, model.clone()).is_err(), "header: {model}");
        }
    }

    fn pending() -> Value {
        let c = official_config(CONFIG, 4096, None).unwrap();
        let layout = package::layout(&c, StageSpec::full(&c));
        let segments: Vec<Value>=package::segment_names(&c).iter().skip(1).map(|name|json!({
            "name":name,"bytes":layout.iter().filter(|e| &e.segment==name).map(|e|e.bytes).sum::<u64>(),
            "blake3":"a".repeat(64)
        })).collect();
        // Synthetic commitment METADATA only: never claims to verify weights.
        let mut v = json!({"schema":"arc.integer-slice-manifest.v1", "profile":super::super::PROFILE_I4G32,
            "pending":["rope_scaling yarn"], "model":null,"tables":null,"model_root":null,
            "weights":{"prefix":"language_model.","packed_experts":true},
            "source":crate::modern::convert::SourceManifest::parse(SOURCE).unwrap().header_json(),
            "segments":segments});
        seal(&mut v);
        v
    }
    fn seal(v: &mut Value) {
        v["manifest_blake3"] = crate::model_package::manifest_body_blake3(v)
            .unwrap()
            .into();
    }

    #[test]
    fn full_and_probe_finalization_have_distinct_explicit_identities() {
        let input = pending();
        let saved = input.clone();
        let full = finalize_pending_slices(CONFIG, &input, 4096, None).unwrap();
        for layers in 1..=3 {
            let probe = finalize_pending_slices(CONFIG, &input, 4096, Some(layers)).unwrap();
            assert_eq!(probe["profile"], PROBE_PROFILE);
            assert_eq!(probe["model"]["n_layers"], layers);
            assert_eq!(
                probe["model"]["preparation"]["scope"],
                json!({"kind":"early_layers_with_head_probe","layers":layers})
            );
            assert_ne!(probe["model_root"], full["model_root"]);
            assert_eq!(probe["segments"].as_array().unwrap().len(), layers + 3);
            println!("yarn probe {layers} metadata root: {}", probe["model_root"]);
        }
        assert_eq!(input, saved, "must not clear the caller's pending fields");
        assert_eq!(full["profile"], PROFILE);
        assert!(finalize_pending_slices(CONFIG, &input, 4096, Some(0)).is_err());
        assert!(finalize_pending_slices(CONFIG, &input, 4096, Some(4)).is_err());
    }

    #[test]
    fn finalization_rejects_missing_forged_duplicate_or_unpinned_inputs() {
        for kind in [
            "missing",
            "length",
            "duplicate",
            "source",
            "pending",
            "digest",
            "model",
            "scheme",
        ] {
            let mut v = pending();
            match kind {
                "missing" => {
                    v["segments"].as_array_mut().unwrap().remove(0);
                }
                "length" => v["segments"][0]["bytes"] = 0.into(),
                "duplicate" => {
                    let s = v["segments"][0].clone();
                    v["segments"].as_array_mut().unwrap().push(s);
                }
                "source" => v["source"]["revision"] = "forged".into(),
                "pending" => v["pending"] = json!([]),
                "digest" => v["segments"][0]["blake3"] = "invalid".into(),
                "model" => v["model"] = json!({}),
                "scheme" => v["profile"] = super::super::PROFILE.into(),
                _ => unreachable!(),
            }
            seal(&mut v);
            assert!(
                finalize_pending_slices(CONFIG, &v, 4096, Some(2)).is_err(),
                "{kind}"
            );
        }
        let mut v = pending();
        v["manifest_blake3"] = "0".repeat(64).into();
        assert!(finalize_pending_slices(CONFIG, &v, 4096, None).is_err());
        let mut c = official_config(CONFIG, 4096, Some(2)).unwrap();
        c.n_layers = 61;
        assert!(c.validate().is_err());
        let mut c = official_config(CONFIG, 4096, None).unwrap();
        c.attention_lambda = crate::modern::tables::attention_lambda(192);
        assert!(c.validate().is_err());
    }
}
