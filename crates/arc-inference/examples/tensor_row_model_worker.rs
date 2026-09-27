//! Private-cohort row worker that holds the whole canonical artifact and
//! serves any row assignment of it: an operator's own machine under option
//! (a) of docs/design/assignment-node-integration.md, so the coordinator can
//! place any rows on it. Build it on that machine:
//! `cargo build -p arc-inference --example tensor_row_model_worker --features candle --release`.
//!
//!     tensor_row_model_worker --model <gguf> --artifact <blake3 hex>
//!
//! It refuses to start unless the file's BLAKE3 equals `--artifact`, the
//! pinned artifact hash of the chain's activation. Like
//! `tensor_row_stdio_worker` it has no network listener. Its only transport
//! is length-prefixed frames over stdin/stdout, reached through a persistent
//! SSH session with StrictHostKeyChecking=yes and a pinned known_hosts file.
//! Nothing else may be written to stdout: it is the protocol channel.

use arc_crypto::Hash256;
use arc_inference::cached_integer_model::load_cached_model_canonical_i8_interleaved_rope;
use arc_inference::tensor_parallel::serve_row_frames;
use std::io::Read;

const USAGE: &str = "usage: tensor_row_model_worker --model <gguf> --artifact <blake3 hex>";

fn file_blake3(path: &str) -> std::io::Result<Hash256> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Hash256(*hasher.finalize().as_bytes()))
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .cloned()
    };
    let model_path = value("--model").ok_or(USAGE)?;
    let artifact_hex = value("--artifact").ok_or(USAGE)?;
    let artifact = Hash256::from_hex(artifact_hex.trim_start_matches("0x"))
        .map_err(|error| format!("--artifact: {error}"))?;
    let actual = file_blake3(&model_path).map_err(|error| format!("hash {model_path}: {error}"))?;
    if actual != artifact {
        return Err(format!(
            "{model_path} is not the pinned artifact (BLAKE3 {}, expected {})",
            actual.to_hex(),
            artifact.to_hex()
        ));
    }
    let model = load_cached_model_canonical_i8_interleaved_rope(&model_path)
        .map_err(|error| error.to_string())?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_row_frames(&model, artifact, &mut stdin.lock(), &mut stdout.lock())
        .map_err(|error| error.to_string())
}
