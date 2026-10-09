//! Diagnostic export only: call the pinned engine's unchanged public forward API.
use arc_inference::modern::convert::SourceManifest;
use arc_inference::modern::mla::{
    model::{StageInput, StageModel},
    package::{self, StageSpec},
    yarn::Scope,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{error::Error, fs, path::Path};

#[allow(dead_code)]
mod observed {
    include!(concat!(env!("OUT_DIR"), "/observed_model.rs"));
}

const ENGINE: &str = "8bd1e6a1696304517a261a06aee43c142b44f128";
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 5 {
        return Err("usage: arc-quality-layer-probe PACKAGE MANIFEST SOURCE REQUEST OUTPUT".into());
    }
    let model = StageModel::open(Path::new(&args[0]))?;
    let manifest = fs::read(&args[1])?;
    package::verify_against_manifest(&model.header, &model.segments(), &manifest)?;
    let m: Value = serde_json::from_slice(&manifest)?;
    let source = fs::read(&args[2])?;
    if SourceManifest::parse(&source)?.header_json() != model.header.source {
        return Err("source mismatch".into());
    }
    let request_bytes = fs::read(&args[3])?;
    let request: Value = serde_json::from_slice(&request_bytes)?;
    let c = model.config();
    let prep = c.preparation.as_ref().ok_or("missing preparation")?;
    if !(1..=3).contains(&c.n_layers)
        || model.header.stage.first_layer != 0
        || model.header.stage.end_layer != c.n_layers
        || !(matches!(prep.scope, Scope::SyntheticFixture)
            || (c.n_layers == 1
                && matches!(prep.scope, Scope::EarlyLayersWithHeadProbe { layers: 1 })))
    {
        return Err("diagnostic supports 1-3 layer fixtures or a one-layer real probe".into());
    }
    if request.get("precision").is_none()
        || request["precision"] != c.to_json()["precision"]
        || request["engine_sha"] != ENGINE
        || request["model_root"] != m["model_root"]
        || request["source_manifest_sha256"] != hash(&source)
        || request["package_sha256"] != hash(model.bytes())
        || request["graph"]["executed_layers"] != json!((0..c.n_layers).collect::<Vec<_>>())
        || request["graph"]["head"] != "original"
        || request["graph"]["embedding"] != "original"
        || request["scope"] != c.to_json()["preparation"]["scope"]
        || request["mask"] != "causal_no_padding"
        || request["shape"] != json!({"hidden_size": c.d_model, "vocab_size": c.vocab_size})
    {
        return Err("request model/input/scope provenance mismatch".into());
    }
    if c.n_layers > 1
        && request["moe"]
            != json!({"n_routed_experts":c.n_routed_experts,"num_experts_per_tok":c.n_experts_per_tok,"n_shared_experts":c.n_shared_experts,"n_group":c.n_group,"topk_group":c.topk_group})
    {
        return Err("MoE configuration mismatch".into());
    }
    let tokens = request["token_ids"].as_array().ok_or("token_ids missing")?;
    let positions = request["positions"].as_array().ok_or("positions missing")?;
    if tokens.is_empty() || tokens.len() != positions.len() || tokens.len() > c.max_seq {
        return Err("invalid sequence length".into());
    }
    arc_inference::canonical_simd::set_fast_canonical_kernel(false);
    let mut cache = model.new_cache();
    let mut simd_cache = model.new_cache();
    let stages = (0..c.n_layers)
        .map(|i| {
            StageModel::open_range(
                Path::new(&args[0]),
                Some(StageSpec {
                    first_layer: i,
                    end_layer: i + 1,
                }),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let observed = (0..c.n_layers)
        .map(|i| {
            observed::StageModel::open_range(
                Path::new(&args[0]),
                Some(StageSpec {
                    first_layer: i,
                    end_layer: i + 1,
                }),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut caches = stages.iter().map(StageModel::new_cache).collect::<Vec<_>>();
    let mut observed_caches = observed
        .iter()
        .map(observed::StageModel::new_cache)
        .collect::<Vec<_>>();
    let mut boundaries = vec![Vec::new(); c.n_layers];
    let mut routing = serde_json::Map::new();
    for layer in c.first_k_dense..c.n_layers {
        routing.insert(format!("layer.{layer}"), json!([]));
    }
    let mut logits = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if positions[i] != json!(i) {
            return Err("positions must start at zero and be contiguous".into());
        }
        let token = u32::try_from(t.as_u64().ok_or("invalid token")?)?;
        let mut trace = Vec::new();
        let (whole, whole_logits) =
            model.forward(StageInput::Token(token), &mut cache, Some(&mut trace))?;
        arc_inference::canonical_simd::set_fast_canonical_kernel(true);
        if !arc_inference::canonical_simd::fast_canonical_kernel_enabled() {
            return Err("SIMD required for this diagnostic control".into());
        }
        let mut simd_trace = Vec::new();
        let (simd_hidden, simd_logits) = model.forward(
            StageInput::Token(token),
            &mut simd_cache,
            Some(&mut simd_trace),
        )?;
        arc_inference::canonical_simd::set_fast_canonical_kernel(false);
        if simd_hidden != whole || simd_logits != whole_logits || simd_trace != trace {
            return Err("scalar/SIMD mismatch".into());
        }
        let mut hidden = Vec::new();
        for layer in 0..c.n_layers {
            let input = if layer == 0 {
                StageInput::Token(token)
            } else {
                StageInput::Hidden(&hidden)
            };
            let observer_input = if layer == 0 {
                observed::StageInput::Token(token)
            } else {
                observed::StageInput::Hidden(&hidden)
            };
            let (h, l) = stages[layer].forward(input, &mut caches[layer], None)?;
            let (oh, ol) =
                observed[layer].forward(observer_input, &mut observed_caches[layer], None)?;
            if h != oh
                || l != ol
                || arc_inference::modern::mla::boundary::activation_hash(&h) != trace[layer + 1]
            {
                return Err("observer/split stage changed pinned engine output".into());
            }
            let routes = observed::take_routes();
            if layer >= c.first_k_dense {
                if routes.len() != 1 {
                    return Err("missing routing observation".into());
                }
                let mut row = routes[0].clone();
                row["position"] = json!(i);
                routing[&format!("layer.{layer}")]
                    .as_array_mut()
                    .unwrap()
                    .push(row);
            } else if !routes.is_empty() {
                return Err("unexpected dense routing".into());
            }
            boundaries[layer].push(h.clone());
            hidden = h;
            if layer + 1 == c.n_layers && (hidden != whole || l != whole_logits) {
                return Err("split/full mismatch".into());
            }
        }
        logits.push(whole_logits.ok_or("missing original head")?);
    }
    let mut tensors = serde_json::Map::new();
    for (layer, values) in boundaries.into_iter().enumerate() {
        tensors.insert(format!("layer.{layer}.output"), json!(values));
    }
    tensors.insert("logits".into(), json!(logits));
    let tensor_hashes = tensors
        .iter()
        .map(|(k, v)| Ok((k.clone(), json!(hash(&serde_json::to_vec(v)?)))))
        .collect::<Result<serde_json::Map<_, _>, serde_json::Error>>()?;
    let out = json!({"schema":"arc.layer-probe.raw.v2","engine":"arc-integer","alignment":request,
        "provenance":{"engine_sha":ENGINE,"request_sha256":hash(&request_bytes),"package_sha256":hash(model.bytes()),"source_manifest_sha256":hash(&source)},
        "numeric":{"dtype":"int64","fraction_bits":16,"unit_scale":1.0/65536.0},
        "tensors":tensors,"tensor_sha256":tensor_hashes,"layer_order":(0..c.n_layers).collect::<Vec<_>>(),"routing":routing,"routing_numeric":{"weights_fraction_bits":32,"activations_fraction_bits":16},"observer":{"base_model_sha256":"15f2baef5e3db2a54ecba6831ee25d02570c84b3ce6026ba87cd3ca012af29eb","verified_against_unmodified_engine":true,"scalar_simd_full_split_equal":true}});
    // Validation completes before creating the output; never replace a record.
    let out_path = Path::new(&args[4]);
    let bytes = serde_json::to_vec_pretty(&out)?;
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out_path)?;
    f.write_all(&bytes)?;
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("layer diagnostic: {e}");
        std::process::exit(1);
    }
}
