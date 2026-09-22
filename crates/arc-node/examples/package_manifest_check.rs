//! Run the node's package-manifest check against a real artifact without a
//! qualification record (model package contract v1, section 7).
//!
//! Real execution needs a reference-qualification record, and none may be
//! written before its evidence exists (M4). This runs the same comparison the
//! node runs at startup - the same `package_facts` and
//! `verify_package_manifest` - on the loaded model, tokenizer and header, so
//! the Rust side's vocabulary digest, tensor inventory digest and canonical
//! JSON can be checked against the Python-derived manifest now.
//!
//! Heavy: loads the canonical model. Run in a model window:
//!   cargo run --release -p arc-node --example package_manifest_check -- \
//!     ~/.arc/models/standard.gguf docs/protocol/packages/llama-2-7b-q4km.manifest.json \
//!     fecaf64104b3be988ff5f3ddf3c38e6739787f9cd390b64835dc59aecb0309bf

use arc_crypto::Hash256;
use arc_node::native_inference::{package_facts, verify_package_manifest};
use std::io::Read;
use std::path::Path;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let [_, artifact, manifest, pinned] = args.as_slice() else {
        return Err(
            "usage: package_manifest_check GGUF MANIFEST_JSON PINNED_MANIFEST_BLAKE3".into(),
        );
    };
    let pinned = Hash256::from_hex(pinned.trim_start_matches("0x"))
        .map_err(|_| "the pinned manifest hash is not 32 bytes of hex".to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(artifact).map_err(|e| e.to_string())?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let artifact_hash = Hash256(*hasher.finalize().as_bytes());
    let model =
        arc_inference::cached_integer_model::load_cached_model_canonical_i8_interleaved_rope(
            artifact,
        )
        .map_err(|e| e.to_string())?;
    let tokenizer = arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer::from_gguf(artifact)
        .map_err(|e| e.to_string())?;
    let loaded = package_facts(&model, artifact_hash, Path::new(artifact), &tokenizer)
        .map_err(|e| e.to_string())?;
    println!("{loaded:#?}");
    verify_package_manifest(Path::new(manifest), pinned, &loaded).map_err(|e| e.to_string())?;
    println!("PASS: the pinned manifest describes exactly the loaded package");
    Ok(())
}
