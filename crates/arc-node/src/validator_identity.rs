//! Validator signing-key loading and explicit insecure development fallback.
//!
//! Production validators load the JSON format written by `arc keygen` from a
//! mode-0600 regular file. Secret-derived identity is retained only for
//! stake-zero clients and deliberately opted-in local development networks.

use anyhow::{Context, Result, bail, ensure};
use arc_crypto::{Hash256, KeyPair};
use serde::Deserialize;
use std::fs::{self, File};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use zeroize::{Zeroize, Zeroizing};

const MAX_KEYFILE_BYTES: u64 = 16 * 1024;
const DEFAULT_OBSERVER_SEED: &str = "arc-stake-zero-observer";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArcKeyfile {
    scheme: String,
    secret_key: String,
    public_key: String,
    address: String,
}

impl Drop for ArcKeyfile {
    fn drop(&mut self) {
        self.secret_key.zeroize();
    }
}

/// Describes the non-secret source of the loaded validator identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentitySource {
    Keyfile,
    InsecureDevelopmentSeed,
    StakeZeroSeed,
}

pub struct LoadedIdentity {
    pub keypair: KeyPair,
    pub source: IdentitySource,
}

/// Load an ARC CLI-compatible Ed25519 keyfile after enforcing its filesystem
/// and self-consistency contract.
pub fn load_ed25519_keyfile(path: &Path) -> Result<KeyPair> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect validator keyfile {}", path.display()))?;
    ensure!(
        !path_metadata.file_type().is_symlink(),
        "validator keyfile must not be a symbolic link: {}",
        path.display()
    );
    ensure!(
        path_metadata.is_file(),
        "validator keyfile is not a regular file: {}",
        path.display()
    );

    let mut file = File::open(path)
        .with_context(|| format!("failed to open validator keyfile {}", path.display()))?;
    let metadata = file.metadata().with_context(|| {
        format!(
            "failed to inspect open validator keyfile {}",
            path.display()
        )
    })?;

    #[cfg(unix)]
    ensure!(
        path_metadata.dev() == metadata.dev() && path_metadata.ino() == metadata.ino(),
        "validator keyfile changed while it was being opened; retry with a stable regular file"
    );

    ensure!(
        metadata.len() <= MAX_KEYFILE_BYTES,
        "validator keyfile is unexpectedly large ({} bytes; maximum {})",
        metadata.len(),
        MAX_KEYFILE_BYTES
    );

    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        ensure!(
            mode == 0o600,
            "validator keyfile {} must have mode 0600 (found {:04o}); run: chmod 600 {}",
            path.display(),
            mode,
            path.display()
        );
    }
    #[cfg(not(unix))]
    bail!(
        "production validator keyfile permission validation is not implemented on this OS; run a stake-zero node or use a supported Unix validator host"
    );

    let mut encoded = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.by_ref()
        .take(MAX_KEYFILE_BYTES + 1)
        .read_to_end(&mut encoded)
        .with_context(|| format!("failed to read validator keyfile {}", path.display()))?;
    ensure!(
        encoded.len() as u64 <= MAX_KEYFILE_BYTES,
        "validator keyfile exceeded the {} byte maximum while being read",
        MAX_KEYFILE_BYTES
    );
    let parsed = serde_json::from_slice::<ArcKeyfile>(&encoded)
        .with_context(|| format!("failed to parse validator keyfile {}", path.display()));
    let keyfile = parsed?;

    ensure!(
        keyfile.scheme == "ed25519",
        "validator keyfile scheme must be ed25519 (found {})",
        keyfile.scheme
    );

    let secret = Zeroizing::new(
        hex::decode(&keyfile.secret_key)
            .context("validator keyfile secret_key is not valid hex")?,
    );
    ensure!(
        secret.len() == 32,
        "validator keyfile Ed25519 secret_key must be 32 bytes (found {})",
        secret.len()
    );
    let mut secret_bytes: [u8; 32] = secret
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("validator keyfile has an invalid Ed25519 secret key"))?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&secret_bytes);
    secret_bytes.zeroize();
    let derived_public = signing_key.verifying_key().to_bytes();
    let declared_public = hex::decode(&keyfile.public_key)
        .context("validator keyfile public_key is not valid hex")?;
    ensure!(
        declared_public.as_slice() == derived_public.as_slice(),
        "validator keyfile public_key does not match its secret_key"
    );

    let keypair = KeyPair::Ed25519(signing_key);
    let derived_address = keypair.address();
    let declared_address = parse_address(&keyfile.address)
        .context("validator keyfile address is not a 64-character hexadecimal ARC address")?;
    ensure!(
        declared_address == derived_address,
        "validator keyfile address does not match its Ed25519 public key (declared {}, derived {})",
        declared_address,
        derived_address
    );
    Ok(keypair)
}

/// Select a validator identity under the production/dev policy.
pub fn resolve_identity(
    keyfile: Option<&Path>,
    seed: Option<&str>,
    stake: u64,
    allow_insecure_dev_seed: bool,
) -> Result<LoadedIdentity> {
    ensure!(
        keyfile.is_none() || seed.is_none(),
        "validator identity is ambiguous: configure either validator_key_file or validator_seed, never both"
    );

    if stake > 0 && seed.is_some() && !allow_insecure_dev_seed {
        bail!(
            "a staked validator cannot accept --validator-seed, ARC_VALIDATOR_SEED, or [validator].seed. Remove the seed and provide --validator-key-file <mode-0600 arc keygen JSON>; use --insecure-dev-validator-seed only for a disposable local network"
        );
    }

    if let Some(path) = keyfile {
        return Ok(LoadedIdentity {
            keypair: load_ed25519_keyfile(path)?,
            source: IdentitySource::Keyfile,
        });
    }

    if stake > 0 && !allow_insecure_dev_seed {
        bail!(
            "staked validators require --validator-key-file <path> (or ARC_VALIDATOR_KEY_FILE) pointing to a mode-0600 Ed25519 JSON file created by `arc keygen --scheme ed25519`; seed/argv identities are forbidden"
        );
    }

    let (seed, source) = if stake > 0 {
        let seed = seed.ok_or_else(|| {
            anyhow::anyhow!(
                "--insecure-dev-validator-seed also requires an explicit --validator-seed or [validator].seed"
            )
        })?;
        (seed, IdentitySource::InsecureDevelopmentSeed)
    } else {
        (
            seed.unwrap_or(DEFAULT_OBSERVER_SEED),
            IdentitySource::StakeZeroSeed,
        )
    };

    Ok(LoadedIdentity {
        keypair: derive_insecure_seed_keypair(seed),
        source,
    })
}

/// Legacy deterministic derivation, retained only behind the explicit policy
/// above and for incomplete local-development genesis files.
pub fn derive_insecure_seed_keypair(seed: &str) -> KeyPair {
    let mut secret = blake3::derive_key("ARC-chain-validator-keypair-v1", seed.as_bytes());
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&secret);
    secret.zeroize();
    KeyPair::Ed25519(signing_key)
}

pub fn parse_address(value: &str) -> Result<Hash256> {
    ensure!(
        value.len() == 64,
        "ARC address must contain exactly 64 hexadecimal characters"
    );
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(value, &mut bytes).context("ARC address contains invalid hexadecimal")?;
    Ok(Hash256(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use serde_json::json;
    #[cfg(unix)]
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(unix)]
    use std::path::PathBuf;

    #[cfg(unix)]
    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "arc-validator-identity-{}-{}-{}.json",
            std::process::id(),
            label,
            rand::random::<u64>()
        ))
    }

    #[cfg(unix)]
    fn write_keyfile(path: &Path, secret: [u8; 32], mutate: impl FnOnce(&mut serde_json::Value)) {
        let keypair = KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&secret));
        let public = match &keypair {
            KeyPair::Ed25519(key) => key.verifying_key().to_bytes(),
            _ => unreachable!(),
        };
        let mut value = json!({
            "scheme": "ed25519",
            "secret_key": hex::encode(secret),
            "public_key": hex::encode(public),
            "address": keypair.address().to_hex(),
        });
        mutate(&mut value);
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path).unwrap();
        file.write_all(&bytes).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn loads_arc_cli_compatible_ed25519_keyfile() {
        let path = temp_path("valid");
        write_keyfile(&path, [7u8; 32], |_| {});
        let loaded = load_ed25519_keyfile(&path).unwrap();
        let expected = KeyPair::Ed25519(ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]));
        assert_eq!(loaded.address(), expected.address());
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_public_key_or_address_mismatch() {
        for field in ["public_key", "address"] {
            let path = temp_path(field);
            write_keyfile(&path, [9u8; 32], |value| {
                value[field] = serde_json::Value::String("00".repeat(32));
            });
            let error = load_ed25519_keyfile(&path).err().unwrap().to_string();
            assert!(error.contains("does not match"), "{error}");
            fs::remove_file(path).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_or_world_permissions() {
        let path = temp_path("mode");
        write_keyfile(&path, [11u8; 32], |_| {});
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let error = load_ed25519_keyfile(&path).err().unwrap().to_string();
        assert!(error.contains("mode 0600"), "{error}");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn production_stake_rejects_seed_and_requires_keyfile() {
        let seed_error = resolve_identity(None, Some("public-seed"), 5_000_000, false)
            .err()
            .unwrap()
            .to_string();
        assert!(seed_error.contains("cannot accept"), "{seed_error}");

        let missing_error = resolve_identity(None, None, 5_000_000, false)
            .err()
            .unwrap()
            .to_string();
        assert!(
            missing_error.contains("require --validator-key-file"),
            "{missing_error}"
        );
    }

    #[test]
    fn explicit_insecure_flag_preserves_local_dev_seed() {
        let loaded = resolve_identity(None, Some("local-dev-only"), 5_000_000, true).unwrap();
        assert_eq!(loaded.source, IdentitySource::InsecureDevelopmentSeed);
        assert_eq!(
            loaded.keypair.address(),
            derive_insecure_seed_keypair("local-dev-only").address()
        );
    }

    #[test]
    fn rejects_ambiguous_keyfile_and_seed_configuration() {
        let error = resolve_identity(
            Some(Path::new("unused-keyfile.json")),
            Some("dev-seed"),
            0,
            true,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("ambiguous"), "{error}");
    }
}
