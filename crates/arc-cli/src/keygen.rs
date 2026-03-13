//! Key generation, keyfile serialization and deserialization for ARC Chain.
//!
//! Keyfile format (JSON):
//! ```json
//! {
//!   "scheme": "ed25519",
//!   "secret_key": "<hex>",
//!   "public_key": "<hex>",
//!   "address": "<hex>"
//! }
//! ```

use anyhow::{Result, bail, Context};
use arc_crypto::signature::KeyPair;
use serde::{Deserialize, Serialize};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// JSON-serializable keyfile representation.
#[derive(Serialize, Deserialize)]
pub struct Keyfile {
    pub scheme: String,
    pub secret_key: String,
    pub public_key: String,
    pub address: String,
}

/// Generate a new keypair for the given signature scheme.
pub fn generate_keypair(scheme: &str) -> Result<KeyPair> {
    match scheme {
        "ed25519" => Ok(KeyPair::generate_ed25519()),
        "secp256k1" => Ok(KeyPair::generate_secp256k1()),
        "ml-dsa-65" | "ml_dsa_65" | "mldsa65" => Ok(KeyPair::generate_ml_dsa()),
        "falcon-512" | "falcon512" => Ok(KeyPair::generate_falcon512()),
        _ => bail!(
            "unknown signature scheme '{}'. Supported: ed25519, secp256k1, ml-dsa-65, falcon-512",
            scheme
        ),
    }
}

/// Save a keypair to a JSON keyfile with restricted permissions (0600).
pub fn save_keyfile(keypair: &KeyPair, path: &str) -> Result<()> {
    let keyfile = keypair_to_keyfile(keypair);
    let json = serde_json::to_string_pretty(&keyfile)
        .context("failed to serialize keyfile")?;

    fs::write(path, &json)
        .with_context(|| format!("failed to write keyfile to {}", path))?;

    // Set file permissions to owner-only read/write on Unix.
    #[cfg(unix)]
    {
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms)
            .with_context(|| format!("failed to set permissions on {}", path))?;
    }

    Ok(())
}

/// Load a keypair from a JSON keyfile.
pub fn load_keyfile(path: &str) -> Result<KeyPair> {
    let json = fs::read_to_string(path)
        .with_context(|| format!("failed to read keyfile from {}", path))?;
    let keyfile: Keyfile = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse keyfile {}", path))?;
    keyfile_to_keypair(&keyfile)
}

/// Convert a `KeyPair` to a serializable `Keyfile`.
fn keypair_to_keyfile(keypair: &KeyPair) -> Keyfile {
    let (scheme, secret_key_hex, public_key_hex) = match keypair {
        KeyPair::Ed25519(sk) => {
            let sk_bytes = sk.to_bytes();
            let pk_bytes = sk.verifying_key().as_bytes().to_vec();
            ("ed25519".to_string(), hex::encode(sk_bytes), hex::encode(pk_bytes))
        }
        KeyPair::Secp256k1(sk) => {
            let sk_bytes = sk.to_bytes();
            let vk = sk.verifying_key();
            let pk_bytes = vk.to_encoded_point(true).as_bytes().to_vec();
            ("secp256k1".to_string(), hex::encode(sk_bytes), hex::encode(pk_bytes))
        }
        KeyPair::MlDsa65 { sk_bytes, pk_bytes } => {
            ("ml-dsa-65".to_string(), hex::encode(sk_bytes), hex::encode(pk_bytes))
        }
        KeyPair::Falcon512 { sk_bytes, pk_bytes } => {
            ("falcon-512".to_string(), hex::encode(sk_bytes), hex::encode(pk_bytes))
        }
    };

    Keyfile {
        scheme,
        secret_key: secret_key_hex,
        public_key: public_key_hex,
        address: keypair.address().to_hex(),
    }
}

/// Reconstruct a `KeyPair` from a `Keyfile`.
fn keyfile_to_keypair(keyfile: &Keyfile) -> Result<KeyPair> {
    let sk_bytes = hex::decode(&keyfile.secret_key)
        .context("invalid hex in secret_key")?;

    match keyfile.scheme.as_str() {
        "ed25519" => {
            if sk_bytes.len() != 32 {
                bail!("ed25519 secret key must be 32 bytes, got {}", sk_bytes.len());
            }
            let sk_arr: [u8; 32] = sk_bytes.try_into().unwrap();
            let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_arr);
            Ok(KeyPair::Ed25519(signing_key))
        }
        "secp256k1" => {
            if sk_bytes.len() != 32 {
                bail!("secp256k1 secret key must be 32 bytes, got {}", sk_bytes.len());
            }
            let sk_arr: &[u8; 32] = sk_bytes.as_slice().try_into().unwrap();
            let signing_key = k256::ecdsa::SigningKey::from_bytes(sk_arr.into())
                .context("invalid secp256k1 secret key")?;
            Ok(KeyPair::Secp256k1(signing_key))
        }
        "ml-dsa-65" | "ml_dsa_65" | "mldsa65" => {
            let pk_bytes = hex::decode(&keyfile.public_key)
                .context("invalid hex in public_key")?;
            Ok(KeyPair::MlDsa65 { sk_bytes, pk_bytes })
        }
        "falcon-512" | "falcon512" => {
            let pk_bytes = hex::decode(&keyfile.public_key)
                .context("invalid hex in public_key")?;
            Ok(KeyPair::Falcon512 { sk_bytes, pk_bytes })
        }
        _ => bail!("unknown scheme '{}' in keyfile", keyfile.scheme),
    }
}

/// Run the keygen command: generate a keypair, save it, and print summary.
pub fn run(scheme: &str, output: &str) -> Result<()> {
    if Path::new(output).exists() {
        bail!(
            "keyfile '{}' already exists. Remove it first or choose a different path.",
            output
        );
    }

    let keypair = generate_keypair(scheme)?;
    let address = keypair.address();

    save_keyfile(&keypair, output)?;

    println!("Generated {} keypair", scheme);
    println!("  Address: {}", address.to_hex());
    println!("  Keyfile: {}", output);
    println!();
    println!("IMPORTANT: Keep your keyfile safe. Anyone with access can spend your funds.");

    Ok(())
}
