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

use anyhow::{Context, Result, bail, ensure};
use arc_crypto::signature::KeyPair;
use arc_crypto::{Hash256, hash_bytes};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use zeroize::{Zeroize, Zeroizing};

const MAX_KEYFILE_BYTES: u64 = 1024 * 1024;
const KEYFILE_VALIDATION_CHALLENGE: &[u8] = b"ARC-keyfile-public-secret-consistency-v1";

/// JSON-serializable keyfile representation.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keyfile {
    pub scheme: String,
    pub secret_key: String,
    pub public_key: String,
    pub address: String,
}

impl Drop for Keyfile {
    fn drop(&mut self) {
        self.secret_key.zeroize();
    }
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
    let json =
        Zeroizing::new(serde_json::to_vec_pretty(&keyfile).context("failed to serialize keyfile")?);
    let path = Path::new(path);

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path).with_context(|| {
        format!(
            "failed to create new keyfile {} (the target must not already exist or be a symlink)",
            path.display()
        )
    })?;

    // `mode(0600)` guarantees the inode is never born group/world-accessible.
    // fchmod through the already-open descriptor also defeats an unusually
    // restrictive/modified umask without introducing a path-following race.
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .context("failed to enforce mode 0600 on new keyfile")?;

    file.write_all(&json)
        .context("failed to write complete keyfile")?;
    file.write_all(b"\n").context("failed to finish keyfile")?;
    file.sync_all().context("failed to fsync keyfile")?;
    sync_parent_directory(path)?;
    Ok(())
}

/// Load a keypair from a JSON keyfile.
pub fn load_keyfile(path: &str) -> Result<KeyPair> {
    let path = Path::new(path);
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path).with_context(|| {
        format!(
            "failed to open keyfile {} without following symlinks",
            path.display()
        )
    })?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat keyfile {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "keyfile {} is not a regular file",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_KEYFILE_BYTES,
        "keyfile {} exceeds {} bytes",
        path.display(),
        MAX_KEYFILE_BYTES
    );
    #[cfg(unix)]
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "keyfile {} has group/world permissions {:03o}; require 0600 or stricter",
        path.display(),
        metadata.permissions().mode() & 0o777
    );

    let mut json = Zeroizing::new(Vec::with_capacity(metadata.len() as usize + 1));
    file.take(MAX_KEYFILE_BYTES + 1)
        .read_to_end(&mut json)
        .with_context(|| format!("failed to read keyfile {}", path.display()))?;
    ensure!(
        json.len() as u64 <= MAX_KEYFILE_BYTES,
        "keyfile {} exceeds {} bytes",
        path.display(),
        MAX_KEYFILE_BYTES
    );
    let keyfile: Keyfile = serde_json::from_slice(&json)
        .with_context(|| format!("failed to parse keyfile {}", path.display()))?;
    keyfile_to_keypair(&keyfile)
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        File::open(parent)
            .with_context(|| format!("failed to open parent directory {}", parent.display()))?
            .sync_all()
            .with_context(|| format!("failed to fsync parent directory {}", parent.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Convert a `KeyPair` to a serializable `Keyfile`.
fn keypair_to_keyfile(keypair: &KeyPair) -> Keyfile {
    let (scheme, secret_key_bytes, public_key_bytes) = match keypair {
        KeyPair::Ed25519(sk) => (
            "ed25519".to_string(),
            sk.to_bytes().to_vec(),
            sk.verifying_key().as_bytes().to_vec(),
        ),
        KeyPair::Secp256k1(sk) => {
            let vk = sk.verifying_key();
            (
                "secp256k1".to_string(),
                sk.to_bytes().to_vec(),
                vk.to_encoded_point(true).as_bytes().to_vec(),
            )
        }
        KeyPair::MlDsa65 { sk_bytes, pk_bytes } => {
            ("ml-dsa-65".to_string(), sk_bytes.clone(), pk_bytes.clone())
        }
        KeyPair::Falcon512 { sk_bytes, pk_bytes } => {
            ("falcon-512".to_string(), sk_bytes.clone(), pk_bytes.clone())
        }
    };
    let secret_key_bytes = Zeroizing::new(secret_key_bytes);

    Keyfile {
        scheme,
        secret_key: hex::encode(&*secret_key_bytes),
        public_key: hex::encode(public_key_bytes),
        address: keypair.address().to_hex(),
    }
}

/// Reconstruct a `KeyPair` from a `Keyfile`.
fn keyfile_to_keypair(keyfile: &Keyfile) -> Result<KeyPair> {
    let sk_bytes =
        Zeroizing::new(hex::decode(&keyfile.secret_key).context("invalid hex in secret_key")?);
    let public_key = hex::decode(&keyfile.public_key).context("invalid hex in public_key")?;

    let keypair = match keyfile.scheme.as_str() {
        "ed25519" => {
            if sk_bytes.len() != 32 {
                bail!(
                    "ed25519 secret key must be 32 bytes, got {}",
                    sk_bytes.len()
                );
            }
            let mut sk_arr = Zeroizing::new([0u8; 32]);
            sk_arr.copy_from_slice(&sk_bytes);
            KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&sk_arr))
        }
        "secp256k1" => {
            if sk_bytes.len() != 32 {
                bail!(
                    "secp256k1 secret key must be 32 bytes, got {}",
                    sk_bytes.len()
                );
            }
            let sk_arr: &[u8; 32] = sk_bytes.as_slice().try_into().expect("length checked");
            let signing_key = k256::ecdsa::SigningKey::from_bytes(sk_arr.into())
                .context("invalid secp256k1 secret key")?;
            KeyPair::Secp256k1(signing_key)
        }
        "ml-dsa-65" | "ml_dsa_65" | "mldsa65" => KeyPair::MlDsa65 {
            sk_bytes: sk_bytes.to_vec(),
            pk_bytes: public_key.clone(),
        },
        "falcon-512" | "falcon512" => KeyPair::Falcon512 {
            sk_bytes: sk_bytes.to_vec(),
            pk_bytes: public_key.clone(),
        },
        _ => bail!("unknown scheme '{}' in keyfile", keyfile.scheme),
    };

    ensure!(
        keypair.public_key_bytes() == public_key,
        "keyfile public_key does not match secret_key"
    );
    let claimed_address = Hash256::from_hex(&keyfile.address)
        .map_err(|_| anyhow::anyhow!("address must be exactly 32 bytes of hexadecimal"))?;
    let derived_address = keypair.address();
    ensure!(
        claimed_address == derived_address,
        "keyfile address does not match its public key"
    );
    let challenge = hash_bytes(KEYFILE_VALIDATION_CHALLENGE);
    let signature = keypair
        .sign(&challenge)
        .context("secret_key cannot produce a validation signature")?;
    signature
        .verify(&challenge, &derived_address)
        .context("keyfile public_key does not correspond to secret_key")?;
    Ok(keypair)
}

/// Run the keygen command: generate a keypair, save it, and print summary.
pub fn run(scheme: &str, output: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "arc-cli-keygen-{name}-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_test_keyfile(path: &Path, value: &serde_json::Value) {
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn every_supported_scheme_round_trips_with_consistent_identity() {
        let dir = TestDir::new("round-trip");
        for scheme in ["ed25519", "secp256k1", "ml-dsa-65", "falcon-512"] {
            let path = dir.join(&format!("{scheme}.json"));
            let generated = generate_keypair(scheme).unwrap();
            save_keyfile(&generated, path.to_str().unwrap()).unwrap();
            let loaded = load_keyfile(path.to_str().unwrap()).unwrap();
            assert_eq!(loaded.address(), generated.address());
            assert_eq!(loaded.public_key_bytes(), generated.public_key_bytes());
        }
    }

    #[test]
    fn create_new_never_overwrites_an_existing_keyfile() {
        let dir = TestDir::new("existing");
        let path = dir.join("validator.json");
        save_keyfile(&KeyPair::generate_ed25519(), path.to_str().unwrap()).unwrap();
        let original = std::fs::read(&path).unwrap();

        let error = save_keyfile(&KeyPair::generate_ed25519(), path.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("failed to create new keyfile"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn keyfile_is_born_0600_and_loader_rejects_permissive_mode() {
        let dir = TestDir::new("permissions");
        let path = dir.join("validator.json");
        save_keyfile(&KeyPair::generate_ed25519(), path.to_str().unwrap()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        load_keyfile(path.to_str().unwrap()).unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = load_keyfile(path.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("group/world permissions"));
    }

    #[cfg(unix)]
    #[test]
    fn save_and_load_refuse_symlinks_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::new("symlink");
        let victim = dir.join("victim");
        std::fs::write(&victim, b"must-not-change").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("validator.json");
        symlink(&victim, &link).unwrap();

        assert!(save_keyfile(&KeyPair::generate_ed25519(), link.to_str().unwrap()).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"must-not-change");
        assert!(load_keyfile(link.to_str().unwrap()).is_err());
    }

    #[test]
    fn malformed_and_mismatched_keyfiles_fail_closed() {
        let dir = TestDir::new("tamper");
        let path = dir.join("validator.json");
        let original_key = KeyPair::generate_ed25519();
        save_keyfile(&original_key, path.to_str().unwrap()).unwrap();
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();

        let mut malformed_secret = original.clone();
        malformed_secret["secret_key"] = serde_json::json!("not-hex");
        write_test_keyfile(&path, &malformed_secret);
        assert!(load_keyfile(path.to_str().unwrap()).is_err());

        let mut wrong_public = original.clone();
        wrong_public["public_key"] =
            serde_json::json!(hex::encode(KeyPair::generate_ed25519().public_key_bytes()));
        write_test_keyfile(&path, &wrong_public);
        let error = load_keyfile(path.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("public_key does not match"));

        let mut wrong_address = original.clone();
        wrong_address["address"] = serde_json::json!("00".repeat(32));
        write_test_keyfile(&path, &wrong_address);
        let error = load_keyfile(path.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("address does not match"));

        let mut unknown_field = original;
        unknown_field["unexpected"] = serde_json::json!(true);
        write_test_keyfile(&path, &unknown_field);
        assert!(load_keyfile(path.to_str().unwrap()).is_err());

        std::fs::write(&path, b"{not-json").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_keyfile(path.to_str().unwrap()).is_err());
    }

    #[test]
    fn post_quantum_public_key_cannot_be_swapped_with_matching_address() {
        for (secret_owner, public_owner) in [
            (KeyPair::generate_ml_dsa(), KeyPair::generate_ml_dsa()),
            (KeyPair::generate_falcon512(), KeyPair::generate_falcon512()),
        ] {
            let mut keyfile = keypair_to_keyfile(&secret_owner);
            keyfile.public_key = hex::encode(public_owner.public_key_bytes());
            keyfile.address = public_owner.address().to_hex();
            let error = keyfile_to_keypair(&keyfile).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("public_key does not correspond to secret_key")
            );
        }
    }
}
