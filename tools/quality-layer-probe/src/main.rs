//! Diagnostic export only: call the pinned engine's unchanged public forward API.
use arc_inference::modern::convert::SourceManifest;
use arc_inference::modern::mla::{
    model::{StageInput, StageModel},
    package,
    yarn::Scope,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{error::Error, fs, path::Path};

const ENGINE: &str = "b0673684a8c16f83b318fe1a33ab6b33f3dc79df";
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
    if c.n_layers != 1
        || model.header.stage.first_layer != 0
        || model.header.stage.end_layer != 1
        || !matches!(
            prep.scope,
            Scope::SyntheticFixture | Scope::EarlyLayersWithHeadProbe { layers: 1 }
        )
    {
        return Err("diagnostic supports explicit one-layer fixture/probe only".into());
    }
    if request["engine_sha"] != ENGINE
        || request["model_root"] != m["model_root"]
        || request["source_manifest_sha256"] != hash(&source)
        || request["package_sha256"] != hash(model.bytes())
        || request["graph"]["executed_layers"] != json!([0])
        || request["graph"]["head"] != "original"
        || request["graph"]["embedding"] != "original"
        || request["scope"] != c.to_json()["preparation"]["scope"]
        || request["mask"] != "causal_no_padding"
        || request["shape"] != json!({"hidden_size": c.d_model, "vocab_size": c.vocab_size})
    {
        return Err("request model/input/scope provenance mismatch".into());
    }
    let tokens = request["token_ids"].as_array().ok_or("token_ids missing")?;
    let positions = request["positions"].as_array().ok_or("positions missing")?;
    if tokens.is_empty() || tokens.len() != positions.len() || tokens.len() > c.max_seq {
        return Err("invalid sequence length".into());
    }
    let mut cache = model.new_cache();
    let mut boundary = Vec::new();
    let mut logits = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if positions[i] != json!(i) {
            return Err("positions must start at zero and be contiguous".into());
        }
        let token = u32::try_from(t.as_u64().ok_or("invalid token")?)?;
        let (h, l) = model.forward(StageInput::Token(token), &mut cache, None)?;
        boundary.push(h);
        logits.push(l.ok_or("missing original head")?);
    }
    let out = json!({"schema":"arc.layer-probe.raw.v1","engine":"arc-integer","alignment":request,
        "provenance":{"engine_sha":ENGINE,"request_sha256":hash(&request_bytes),"package_sha256":hash(model.bytes()),"source_manifest_sha256":hash(&source)},
        "numeric":{"dtype":"int64","fraction_bits":16,"unit_scale":1.0/65536.0},
        "tensors":{"layer.0.output":boundary,"logits":logits}});
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
