// ARC identity = BIP-39 phrase → ed25519 keypair → BLAKE3(pk) address.
//
// This MUST match ARC's historical recovery-phrase derivation, otherwise the
// phrase shown to a user in this app will not restore the same wallet/node
// address when materialized as a persistent Ed25519 keyfile.
//
// Chain derivation (crates/arc-node/src/main.rs:310–311,
// crates/arc-crypto/src/signature.rs:404,521–522):
//
//     seed_bytes = blake3::derive_key(DOMAIN_TAG, seed_str.as_bytes())
//     signing_key = ed25519_dalek::SigningKey::from_bytes(&seed_bytes)
//     public_key  = signing_key.verifying_key().as_bytes()
//     address     = blake3::hash(public_key).as_bytes()  // 32 bytes
//
// The DOMAIN_TAG below matches the chain's constant verbatim. Do not change.

use crate::types::Identity;
use bip39::{Language, Mnemonic};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, Zeroizing};

const DOMAIN_TAG: &str = "ARC-chain-validator-keypair-v1";
const MAX_VALIDATOR_KEYFILE_BYTES: u64 = 16 * 1024;
const VALIDATOR_KEYFILE_DIR: &str = "identity";
const VALIDATOR_KEYFILE_NAME: &str = "validator-key.json";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ValidatorKeyfile {
    scheme: String,
    secret_key: String,
    public_key: String,
    address: String,
}

impl Drop for ValidatorKeyfile {
    fn drop(&mut self) {
        self.secret_key.zeroize();
    }
}

/// Generate a fresh 12-word BIP-39 mnemonic and derive the ARC identity.
pub fn generate() -> Identity {
    let mut entropy = [0u8; 16]; // 128 bits → 12 words
    OsRng.fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy_in(Language::English, &entropy)
        .expect("valid entropy always yields a mnemonic");
    derive(&mnemonic.to_string()).expect("freshly-generated mnemonic derives successfully")
}

/// Derive an ARC identity from any seed string (typically a BIP-39 phrase).
/// The same input always produces the same (sk, pk, address) triple, on
/// every machine, forever.
pub fn derive(phrase: &str) -> Result<Identity, String> {
    // Validate BIP-39 format + checksum if it parses as one. Accept non-BIP-39
    // strings too for backward-compatible desktop identity recovery.
    let is_bip39 = Mnemonic::parse_in(Language::English, phrase.trim()).is_ok();
    let normalized = phrase.trim().to_string();

    let signing_key = signing_key_from_phrase(&normalized);
    let pk_bytes = signing_key.verifying_key().to_bytes();
    let address_bytes = blake3::hash(&pk_bytes);

    Ok(Identity {
        address: hex::encode(address_bytes.as_bytes()),
        public_key: format!("0x{}", hex::encode(pk_bytes)),
        seed_phrase: normalized,
        created_at: chrono::Utc::now().timestamp_millis(),
    })
    .inspect(|_| {
        // Tag logging only - identity still returned unchanged.
        if !is_bip39 {
            tracing::warn!(
                "imported identity is not a BIP-39 phrase - derivation still deterministic but no checksum protection"
            );
        }
    })
}

fn signing_key_from_phrase(phrase: &str) -> SigningKey {
    let mut seed_bytes = blake3::derive_key(DOMAIN_TAG, phrase.trim().as_bytes());
    let signing_key = SigningKey::from_bytes(&seed_bytes);
    seed_bytes.zeroize();
    signing_key
}

/// Materialize the desktop wallet's existing chain identity as the exact JSON
/// format consumed by `arc-node --validator-key-file`.
///
/// The phrase never enters argv, the environment, or logs. Creation is
/// private from the first open, durable, and no-replace; an existing file is
/// accepted only when its secret, public key, and address reproduce the
/// persisted wallet address exactly.
pub(crate) fn ensure_validator_keyfile(
    app_data_dir: &Path,
    phrase: &str,
    persisted_address: &str,
) -> Result<PathBuf, String> {
    if app_data_dir.as_os_str().is_empty() {
        return Err("application data directory is not initialized".to_string());
    }
    let identity_dir = app_data_dir.join(VALIDATOR_KEYFILE_DIR);
    ensure_identity_directory(&identity_dir)?;
    let target = identity_dir.join(VALIDATOR_KEYFILE_NAME);

    match fs::symlink_metadata(&target) {
        Ok(_) => {
            validate_validator_keyfile(&target, phrase, persisted_address)?;
            return Ok(target);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect {}: {error}", target.display())),
    }

    let signing_key = signing_key_from_phrase(phrase);
    let public_key = signing_key.verifying_key().to_bytes();
    let address = hex::encode(blake3::hash(&public_key).as_bytes());
    if address != persisted_address {
        return Err(format!(
            "refusing node keyfile creation because the persisted wallet address does not match the recovery phrase (expected {persisted_address}, derived {address})"
        ));
    }
    let mut secret = signing_key.to_bytes();
    let keyfile = ValidatorKeyfile {
        scheme: "ed25519".to_string(),
        secret_key: hex::encode(secret),
        public_key: hex::encode(public_key),
        address,
    };
    secret.zeroize();
    let encoded = Zeroizing::new(
        serde_json::to_vec_pretty(&keyfile)
            .map_err(|error| format!("serialize validator keyfile: {error}"))?,
    );
    let (sidecar, mut file) = create_keyfile_sidecar(&target)?;
    let mut pending = PendingKeyfileSidecar(Some(sidecar.clone()));
    file.write_all(&encoded)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|error| {
            format!(
                "persist private keyfile sidecar {}: {error}",
                sidecar.display()
            )
        })?;
    drop(file);

    match fs::hard_link(&sidecar, &target) {
        Ok(()) => {
            fs::remove_file(&sidecar).map_err(|error| {
                format!("remove keyfile sidecar {}: {error}", sidecar.display())
            })?;
            pending.0 = None;
            arc_crypto::secret_file::sync_parent_directory(&target)
                .map_err(|error| format!("sync identity directory: {error}"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another desktop instance won the no-replace publication race.
            // Never overwrite it; accept only the exact same identity.
            validate_validator_keyfile(&target, phrase, persisted_address)?;
            return Ok(target);
        }
        Err(error) => {
            return Err(format!(
                "publish {} without replacing an existing path: {error}",
                target.display()
            ));
        }
    }
    validate_validator_keyfile(&target, phrase, persisted_address)?;
    Ok(target)
}

struct PendingKeyfileSidecar(Option<PathBuf>);

impl Drop for PendingKeyfileSidecar {
    fn drop(&mut self) {
        if let Some(path) = self.0.as_ref() {
            let _ = fs::remove_file(path);
        }
    }
}

fn ensure_identity_directory(path: &Path) -> Result<(), String> {
    match arc_crypto::secret_file::validate_private_directory(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            arc_crypto::secret_file::create_new_private_directory(path).map_err(|error| {
                format!(
                    "create private identity directory {}: {error}",
                    path.display()
                )
            })
        }
        Err(error) => Err(format!(
            "identity directory failed owner/permission/reparse validation {}: {error}",
            path.display()
        )),
    }
}

fn create_keyfile_sidecar(target: &Path) -> Result<(PathBuf, fs::File), String> {
    let parent = target
        .parent()
        .ok_or_else(|| "validator keyfile has no parent directory".to_string())?;
    let name = target
        .file_name()
        .ok_or_else(|| "validator keyfile has no filename".to_string())?;
    for _ in 0..64 {
        let mut sidecar_name = OsString::from(".");
        sidecar_name.push(name);
        sidecar_name.push(format!(
            ".tmp-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let sidecar = parent.join(sidecar_name);
        match arc_crypto::secret_file::create_new_private(&sidecar) {
            Ok(file) => return Ok((sidecar, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "create private keyfile sidecar {}: {error}",
                    sidecar.display()
                ));
            }
        }
    }
    Err("could not allocate a unique validator keyfile sidecar".to_string())
}

fn validate_validator_keyfile(
    path: &Path,
    phrase: &str,
    persisted_address: &str,
) -> Result<(), String> {
    let file = arc_crypto::secret_file::open_private(path)
        .map_err(|error| format!("open private validator keyfile {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect validator keyfile {}: {error}", path.display()))?;
    if metadata.len() > MAX_VALIDATOR_KEYFILE_BYTES {
        return Err(format!(
            "validator keyfile {} exceeds the {}-byte limit",
            path.display(),
            MAX_VALIDATOR_KEYFILE_BYTES
        ));
    }
    let mut encoded = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.take(MAX_VALIDATOR_KEYFILE_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(|error| format!("read validator keyfile {}: {error}", path.display()))?;
    if encoded.len() as u64 > MAX_VALIDATOR_KEYFILE_BYTES {
        return Err("validator keyfile grew past its safety limit while reading".to_string());
    }
    let parsed: ValidatorKeyfile = serde_json::from_slice(&encoded)
        .map_err(|error| format!("parse validator keyfile {}: {error}", path.display()))?;
    if parsed.scheme != "ed25519" {
        return Err("desktop validator keyfile scheme must be ed25519".to_string());
    }
    let secret = Zeroizing::new(
        hex::decode(&parsed.secret_key)
            .map_err(|_| "desktop validator secret_key is not hexadecimal".to_string())?,
    );
    let mut secret_bytes: [u8; 32] = secret
        .as_slice()
        .try_into()
        .map_err(|_| "desktop validator secret_key must contain 32 bytes".to_string())?;
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    secret_bytes.zeroize();
    let expected_signing_key = signing_key_from_phrase(phrase);
    if signing_key.verifying_key().to_bytes() != expected_signing_key.verifying_key().to_bytes() {
        return Err(
            "existing desktop validator keyfile does not match the recovery phrase".to_string(),
        );
    }
    let public_key = signing_key.verifying_key().to_bytes();
    let address = hex::encode(blake3::hash(&public_key).as_bytes());
    if parsed.public_key != hex::encode(public_key)
        || parsed.address != address
        || parsed.address != persisted_address
    {
        return Err(
            "existing desktop validator keyfile does not preserve the persisted wallet public identity"
                .to_string(),
        );
    }
    Ok(())
}

/// Attempt to parse an input as a BIP-39 mnemonic, surfacing a clear error
/// to the user if checksum or word list validation fails.
pub fn validate_bip39(phrase: &str) -> Result<(), String> {
    Mnemonic::parse_in(Language::English, phrase.trim())
        .map(|_| ())
        .map_err(|e| format!("invalid seed phrase: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "arc-desktop-identity-{label}-{}-{:016x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn derivation_is_deterministic() {
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let a = derive(phrase).unwrap();
        let b = derive(phrase).unwrap();
        assert_eq!(a.address, b.address);
        assert_eq!(a.public_key, b.public_key);
    }

    #[test]
    fn different_phrases_produce_different_addresses() {
        let a = derive("abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about").unwrap();
        let b =
            derive("legal winner thank year wave sausage worth useful legal winner thank yellow")
                .unwrap();
        assert_ne!(a.address, b.address);
    }

    #[test]
    fn address_is_64_hex_chars() {
        let id = generate();
        assert_eq!(id.address.len(), 64);
        assert!(id.address.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_produces_valid_bip39() {
        let id = generate();
        assert!(validate_bip39(&id.seed_phrase).is_ok());
        let words: Vec<&str> = id.seed_phrase.split_whitespace().collect();
        assert_eq!(words.len(), 12);
    }

    #[test]
    fn bip39_with_typo_fails_validation() {
        // Swap a valid word for an invalid one
        let bad = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon notaword";
        assert!(validate_bip39(bad).is_err());
    }

    #[test]
    fn generated_phrase_restores_to_same_address() {
        let id1 = generate();
        let id2 = derive(&id1.seed_phrase).unwrap();
        assert_eq!(id1.address, id2.address);
        assert_eq!(id1.public_key, id2.public_key);
    }

    #[test]
    fn keyfile_materialization_preserves_imported_wallet_address_and_is_stable() {
        let dir = TestDir::new("materialize");
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let imported = derive(phrase).unwrap();
        let path = ensure_validator_keyfile(&dir.0, phrase, &imported.address).unwrap();
        let first = fs::read(&path).unwrap();
        let second_path = ensure_validator_keyfile(&dir.0, phrase, &imported.address).unwrap();
        assert_eq!(path, second_path);
        assert_eq!(fs::read(&path).unwrap(), first);
        assert!(!first
            .windows(phrase.len())
            .any(|window| window == phrase.as_bytes()));

        let parsed: serde_json::Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(parsed["scheme"], "ed25519");
        assert_eq!(parsed["address"], imported.address);
        assert_eq!(
            parsed["public_key"],
            imported.public_key.trim_start_matches("0x")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn existing_keyfile_is_never_replaced_by_another_phrase() {
        let dir = TestDir::new("no-replace");
        let first_phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let second_phrase =
            "legal winner thank year wave sausage worth useful legal winner thank yellow";
        let first_identity = derive(first_phrase).unwrap();
        let second_identity = derive(second_phrase).unwrap();
        let path = ensure_validator_keyfile(&dir.0, first_phrase, &first_identity.address).unwrap();
        let original = fs::read(&path).unwrap();

        let error =
            ensure_validator_keyfile(&dir.0, second_phrase, &second_identity.address).unwrap_err();
        assert!(error.contains("does not match"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn keyfile_materialization_rejects_symlink_target_without_touching_victim() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};
        let dir = TestDir::new("symlink");
        let identity_dir = dir.0.join(VALIDATOR_KEYFILE_DIR);
        arc_crypto::secret_file::create_new_private_directory(&identity_dir).unwrap();
        let victim = dir.0.join("victim");
        fs::write(&victim, b"do-not-touch").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&victim, identity_dir.join(VALIDATOR_KEYFILE_NAME)).unwrap();
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let imported = derive(phrase).unwrap();

        assert!(ensure_validator_keyfile(&dir.0, phrase, &imported.address).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"do-not-touch");
    }

    #[cfg(unix)]
    #[test]
    fn keyfile_materialization_rejects_insecure_existing_identity_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TestDir::new("directory-mode");
        let identity_dir = dir.0.join(VALIDATOR_KEYFILE_DIR);
        fs::create_dir(&identity_dir).unwrap();
        fs::set_permissions(&identity_dir, fs::Permissions::from_mode(0o750)).unwrap();
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let imported = derive(phrase).unwrap();

        let error = ensure_validator_keyfile(&dir.0, phrase, &imported.address).unwrap_err();
        assert!(
            error.contains("owner/permission/reparse validation"),
            "{error}"
        );
        assert!(!identity_dir.join(VALIDATOR_KEYFILE_NAME).exists());
    }
}
