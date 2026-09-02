use base64::{engine::general_purpose::STANDARD, Engine as _};
use minisign_verify::{PublicKey, Signature};
use std::{
    env,
    error::Error,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

fn decode_utf8(label: &str, encoded: &str) -> Result<String, Box<dyn Error>> {
    let decoded = STANDARD
        .decode(encoded.trim())
        .map_err(|error| format!("invalid base64 {label}: {error}"))?;
    String::from_utf8(decoded).map_err(|error| format!("invalid UTF-8 {label}: {error}").into())
}

fn verify(
    public_key_base64: &str,
    payload_path: &Path,
    signature_path: &Path,
) -> Result<(), Box<dyn Error>> {
    // Tauri stores both the complete minisign public-key document in its JSON
    // config and the complete minisign signature document in `.sig` as outer
    // base64 strings. Decode those exactly as tauri-plugin-updater does.
    let public_key_document = decode_utf8("Tauri updater public key", public_key_base64)?;
    let signature_base64 = fs::read_to_string(signature_path)?;
    let signature_document = decode_utf8("Tauri updater signature", &signature_base64)?;

    let public_key = PublicKey::decode(&public_key_document)?;
    let signature = Signature::decode(&signature_document)?;
    let mut verifier = public_key.verify_stream(&signature)?;
    let mut payload = File::open(payload_path)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = payload.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        verifier.update(&buffer[..read]);
    }
    verifier.finalize()?;
    Ok(())
}

fn required_arg(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<std::ffi::OsString, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("missing required argument: {name}").into())
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let public_key_base64 = required_arg(&mut args, "PUBLIC_KEY_BASE64")?
        .into_string()
        .map_err(|_| "PUBLIC_KEY_BASE64 must be valid UTF-8")?;
    let payload_path = PathBuf::from(required_arg(&mut args, "PAYLOAD")?);
    let signature_path = PathBuf::from(required_arg(&mut args, "SIGNATURE")?);
    if args.next().is_some() {
        return Err("unexpected extra argument".into());
    }

    verify(&public_key_base64, &payload_path, &signature_path)?;
    println!("verified updater signature: {}", payload_path.display());
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("tauri updater signature verification failed: {error}");
        std::process::exit(1);
    }
}
