use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;

use crate::sshsig;

pub const EMBEDDED_PINS: &str = include_str!("../pins/active.json");

pub const PINS_SCHEMA: &str = "arc.legacy-bridge.pins.v1";
pub const REPOSITORY: &str = "FerrumVir/arc-chain";
pub const MANIFEST_NAME: &str = "SHA256SUMS";
pub const MANIFEST_SIGNATURE_NAME: &str = "SHA256SUMS.sig";
pub const GENESIS_NAME: &str = "genesis.toml";
pub const SEEDS_NAME: &str = "testnet-seeds.txt";
pub const SIGNER_PRINCIPAL: &str = "arc-release";
pub const SIGNER_NAMESPACE: &str = "arc-release-manifest-v1";
pub const SHARD_REQUIRED_ADMITTED_LAYERS_FLOOR: usize = 6;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedFile {
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedPinnedFile {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRelease {
    pub tag: String,
    pub version: String,
    pub commit: String,
    pub manifest: NamedPinnedFile,
    pub manifest_signature: NamedPinnedFile,
    pub worker_names_privacy_safe: bool,
    pub assets: BTreeMap<String, PinnedFile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestSigner {
    pub principal: String,
    pub namespace: String,
    pub public_key: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPin {
    pub file_name: String,
    pub sha256: String,
    pub size: u64,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardWorkerPin {
    pub coordinator_url: String,
    pub coordinator_id: String,
    pub transaction_domain: String,
    pub expected_model_id: String,
    pub required_admitted_layers: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pins {
    pub schema: String,
    pub bridge_version: String,
    pub repository: String,
    pub node_release: NodeRelease,
    pub manifest_signer: ManifestSigner,
    pub community_rpc_origins: Vec<String>,
    pub model: ModelPin,
    #[serde(default)]
    pub shard_worker: Option<ShardWorkerPin>,
}

impl Pins {
    pub fn embedded() -> Result<Pins> {
        Pins::parse(EMBEDDED_PINS)
    }

    pub fn parse(text: &str) -> Result<Pins> {
        let pins: Pins = serde_json::from_str(text).context("bridge pin set is not valid JSON")?;
        pins.validate()?;
        Ok(pins)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == PINS_SCHEMA,
            "unsupported bridge pin schema {:?}",
            self.schema
        );
        ensure!(
            is_strict_version(&self.bridge_version),
            "bridge_version must be MAJOR.MINOR.PATCH"
        );
        ensure!(
            self.repository == REPOSITORY,
            "bridge pins target an unexpected repository"
        );

        let release = &self.node_release;
        ensure!(
            is_strict_version(&release.version),
            "node release version must be MAJOR.MINOR.PATCH"
        );
        ensure!(
            release.tag == format!("v{}", release.version),
            "node release tag must be exactly v<version>"
        );
        ensure!(
            version_at_least(&release.version, (0, 8, 0)),
            "the bridge only installs v0.8.0 or newer nodes"
        );
        ensure!(
            is_lower_hex(&release.commit, 40),
            "node release commit must be 40 lowercase hex characters"
        );
        ensure!(
            release.manifest.name == MANIFEST_NAME,
            "manifest pin must name SHA256SUMS"
        );
        ensure!(
            release.manifest_signature.name == MANIFEST_SIGNATURE_NAME,
            "manifest signature pin must name SHA256SUMS.sig"
        );
        for file in [&release.manifest, &release.manifest_signature] {
            check_digest_and_size(&file.name, &file.sha256, file.size)?;
        }
        for (name, file) in &release.assets {
            ensure!(
                is_asset_name(name),
                "pinned asset name is not a plain release asset name"
            );
            check_digest_and_size(name, &file.sha256, file.size)?;
        }
        for platform in Platform::ALL {
            for name in [platform.node_asset(), platform.cli_asset()] {
                ensure!(
                    release.assets.contains_key(name),
                    "pins are missing the {name} asset"
                );
            }
        }
        for name in [GENESIS_NAME, SEEDS_NAME] {
            ensure!(release.assets.contains_key(name), "pins are missing {name}");
        }

        ensure!(
            self.manifest_signer.principal == SIGNER_PRINCIPAL,
            "unexpected manifest signer principal"
        );
        ensure!(
            self.manifest_signer.namespace == SIGNER_NAMESPACE,
            "unexpected manifest signature namespace"
        );
        sshsig::parse_ed25519_public_key(&self.manifest_signer.public_key)
            .context("pinned release public key is invalid")?;

        ensure!(
            !self.community_rpc_origins.is_empty(),
            "pins must list the community RPC origins"
        );
        let mut seen = std::collections::BTreeSet::new();
        for origin in &self.community_rpc_origins {
            ensure!(
                origin.starts_with("https://") && !origin.ends_with('/') && origin.len() > 8,
                "community RPC origins must be https:// origins without a path"
            );
            ensure!(
                seen.insert(origin.as_str()),
                "community RPC origins must be unique"
            );
        }

        check_digest_and_size(&self.model.file_name, &self.model.sha256, self.model.size)?;
        ensure!(
            self.model.url.starts_with("https://"),
            "the model URL must use https"
        );
        if let Some(shard) = &self.shard_worker {
            ensure!(
                is_valid_https_origin(&shard.coordinator_url),
                "shard_worker.coordinator_url must be a strict https:// origin without path, query, fragment, or userinfo"
            );
            ensure!(
                is_nonzero_lower_hex(&shard.coordinator_id, 64),
                "shard_worker.coordinator_id must be a nonzero 64-character lowercase hex digest"
            );
            ensure!(
                is_nonzero_lower_hex(&shard.transaction_domain, 64),
                "shard_worker.transaction_domain must be a nonzero 64-character lowercase hex digest"
            );
            ensure!(
                is_nonzero_lower_hex(&shard.expected_model_id, 64),
                "shard_worker.expected_model_id must be a nonzero 64-character lowercase hex digest"
            );
            ensure!(
                shard.required_admitted_layers >= SHARD_REQUIRED_ADMITTED_LAYERS_FLOOR,
                "shard_worker.required_admitted_layers must be at least {SHARD_REQUIRED_ADMITTED_LAYERS_FLOOR}"
            );
        }
        Ok(())
    }

    pub fn asset(&self, name: &str) -> Result<&PinnedFile> {
        self.node_release
            .assets
            .get(name)
            .ok_or_else(|| anyhow!("{name} is not pinned"))
    }

    pub fn signer_key(&self) -> Result<[u8; 32]> {
        sshsig::parse_ed25519_public_key(&self.manifest_signer.public_key)
    }
}

fn check_digest_and_size(name: &str, sha256: &str, size: u64) -> Result<()> {
    ensure!(
        is_lower_hex(sha256, 64),
        "{name} must be pinned by a lowercase SHA-256"
    );
    ensure!(size > 0, "{name} must be pinned with a positive size");
    Ok(())
}

pub fn is_asset_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

pub fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn is_nonzero_lower_hex(value: &str, len: usize) -> bool {
    is_lower_hex(value, len) && value.bytes().any(|byte| byte != b'0')
}

pub fn is_valid_https_origin(origin: &str) -> bool {
    let Some(authority) = origin.strip_prefix("https://") else {
        return false;
    };
    if authority.is_empty()
        || authority.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte.is_ascii_whitespace()
                || matches!(byte, b'/' | b'?' | b'#' | b'@' | b'\\')
        })
    {
        return false;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((address, after)) = rest.split_once(']') else {
            return false;
        };
        if address.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        return match after {
            "" => true,
            _ => after.strip_prefix(':').is_some_and(is_valid_port),
        };
    }
    match authority.split_once(':') {
        Some((host, port)) => !port.contains(':') && is_valid_host(host) && is_valid_port(port),
        None => is_valid_host(authority),
    }
}

fn is_valid_port(port: &str) -> bool {
    !port.is_empty()
        && !port.starts_with('0')
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.starts_with('.') || host.ends_with('.') || host.contains("..") {
        return false;
    }
    if host.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return host.parse::<std::net::Ipv4Addr>().is_ok();
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

pub fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let mut parts = value.split('.');
    let major = parse_component(parts.next()?)?;
    let minor = parse_component(parts.next()?)?;
    let patch = parse_component(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn parse_component(part: &str) -> Option<u64> {
    if part.is_empty() || part.len() > 9 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if part.len() > 1 && part.starts_with('0') {
        return None;
    }
    part.parse().ok()
}

pub fn is_strict_version(value: &str) -> bool {
    parse_version(value).is_some()
}

pub fn version_at_least(value: &str, minimum: (u64, u64, u64)) -> bool {
    parse_version(value).is_some_and(|version| version >= minimum)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    MacosArm64,
    MacosX86_64,
    LinuxX86_64,
    LinuxArm64,
    WindowsX86_64,
}

impl Platform {
    pub const ALL: [Platform; 5] = [
        Platform::MacosArm64,
        Platform::MacosX86_64,
        Platform::LinuxX86_64,
        Platform::LinuxArm64,
        Platform::WindowsX86_64,
    ];

    pub fn current() -> Option<Platform> {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(Platform::MacosArm64)
        } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            Some(Platform::MacosX86_64)
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Platform::LinuxX86_64)
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Some(Platform::LinuxArm64)
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some(Platform::WindowsX86_64)
        } else {
            None
        }
    }

    pub fn node_asset(self) -> &'static str {
        match self {
            Platform::MacosArm64 => "arc-node-macos-arm64",
            Platform::MacosX86_64 => "arc-node-macos-x86_64",
            Platform::LinuxX86_64 => "arc-node-linux-x86_64",
            Platform::LinuxArm64 => "arc-node-linux-arm64",
            Platform::WindowsX86_64 => "arc-node-windows-x86_64.exe",
        }
    }

    pub fn cli_asset(self) -> &'static str {
        match self {
            Platform::MacosArm64 => "arc-cli-macos-arm64",
            Platform::MacosX86_64 => "arc-cli-macos-x86_64",
            Platform::LinuxX86_64 => "arc-cli-linux-x86_64",
            Platform::LinuxArm64 => "arc-cli-linux-arm64",
            Platform::WindowsX86_64 => "arc-cli-windows-x86_64.exe",
        }
    }

    pub fn legacy_launcher_asset(self) -> &'static str {
        match self {
            Platform::MacosArm64 => "arc-node-macos-arm64",
            Platform::MacosX86_64 => "arc-node-macos-x86_64",
            Platform::LinuxX86_64 => "arc-node-linux-x86_64",
            Platform::LinuxArm64 => "arc-node-linux-aarch64",
            Platform::WindowsX86_64 => "arc-node-windows-x86_64.exe",
        }
    }

    pub fn is_windows(self) -> bool {
        self == Platform::WindowsX86_64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;

    const FIXTURE_MANIFEST: &[u8] = include_bytes!("../fixtures/v0.8.27/SHA256SUMS");
    const FIXTURE_SIGNATURE: &[u8] = include_bytes!("../fixtures/v0.8.27/SHA256SUMS.sig");

    #[test]
    fn embedded_pins_parse_and_validate() {
        let pins = Pins::embedded().unwrap();
        assert_eq!(pins.node_release.tag, "v0.8.27");
        assert!(pins.node_release.worker_names_privacy_safe);
        assert_eq!(pins.community_rpc_origins.len(), 6);
    }

    #[test]
    fn bridge_version_matches_crate_version() {
        let pins = Pins::embedded().unwrap();
        assert_eq!(pins.bridge_version, env!("CARGO_PKG_VERSION"));
        assert!(!version_at_least(&pins.bridge_version, (0, 8, 0)));
        assert!(version_at_least(&pins.bridge_version, (0, 7, 12)));
    }

    #[test]
    fn every_pinned_digest_matches_the_signed_release_manifest() {
        let pins = Pins::embedded().unwrap();
        assert_eq!(
            crate::hashing::sha256_bytes(FIXTURE_MANIFEST),
            pins.node_release.manifest.sha256
        );
        assert_eq!(
            FIXTURE_MANIFEST.len() as u64,
            pins.node_release.manifest.size
        );
        assert_eq!(
            crate::hashing::sha256_bytes(FIXTURE_SIGNATURE),
            pins.node_release.manifest_signature.sha256
        );
        assert_eq!(
            FIXTURE_SIGNATURE.len() as u64,
            pins.node_release.manifest_signature.size
        );
        let signature = std::str::from_utf8(FIXTURE_SIGNATURE).unwrap();
        sshsig::verify(
            signature,
            FIXTURE_MANIFEST,
            SIGNER_NAMESPACE,
            &pins.signer_key().unwrap(),
        )
        .unwrap();
        let parsed = manifest::parse(FIXTURE_MANIFEST).unwrap();
        parsed.check_pins(&pins).unwrap();
    }

    #[test]
    fn pins_reject_tampering() {
        let text = EMBEDDED_PINS.replace("\"v0.8.27\"", "\"v0.8.9\"");
        assert!(
            Pins::parse(&text).is_err(),
            "tag/version mismatch must fail"
        );
        let text = EMBEDDED_PINS.replace("\"0.7.15\"", "\"0.7.x\"");
        assert!(
            Pins::parse(&text).is_err(),
            "non-strict bridge version must fail"
        );
        let text = EMBEDDED_PINS.replace("https://149.28.32.76", "http://149.28.32.76");
        assert!(Pins::parse(&text).is_err(), "plain-http origin must fail");
        let text = EMBEDDED_PINS.replacen("\"schema\"", "\"unexpected\": 1, \"schema\"", 1);
        assert!(Pins::parse(&text).is_err(), "unknown fields must fail");
    }

    #[test]
    fn strict_versions() {
        assert_eq!(parse_version("0.8.10"), Some((0, 8, 10)));
        assert_eq!(parse_version("0.7.12"), Some((0, 7, 12)));
        for bad in [
            "",
            "0.8",
            "0.8.10.1",
            "v0.8.10",
            "0.08.1",
            "0.8.-1",
            "0.8.1-rc1",
            "a.b.c",
        ] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert!(version_at_least("0.8.0", (0, 8, 0)));
        assert!(!version_at_least("0.7.99", (0, 8, 0)));
    }

    #[test]
    fn platform_asset_names_match_v07_and_v08_conventions() {
        assert_eq!(
            Platform::LinuxArm64.legacy_launcher_asset(),
            "arc-node-linux-aarch64"
        );
        assert_eq!(Platform::LinuxArm64.node_asset(), "arc-node-linux-arm64");
        for platform in Platform::ALL {
            assert!(platform.node_asset().starts_with("arc-node-"));
            assert!(platform.cli_asset().starts_with("arc-cli-"));
            assert_eq!(
                platform.is_windows(),
                platform.node_asset().ends_with(".exe")
            );
        }
    }

    fn pins_json_with_shard(shard: serde_json::Value) -> String {
        let mut value: serde_json::Value = serde_json::from_str(EMBEDDED_PINS).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("shard_worker".to_string(), shard);
        serde_json::to_string(&value).unwrap()
    }

    fn valid_shard_json() -> serde_json::Value {
        serde_json::json!({
            "coordinator_url": "https://149.28.153.31",
            "coordinator_id": "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b",
            "transaction_domain": "3b9147e44d878646caf2d0729d06bd2e082b265e0525760595bd10605f37a10f",
            "expected_model_id": "934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67",
            "required_admitted_layers": 6,
        })
    }

    #[test]
    fn shard_worker_pin_is_optional_and_strictly_validated() {
        let mut without_shard: serde_json::Value = serde_json::from_str(EMBEDDED_PINS).unwrap();
        without_shard
            .as_object_mut()
            .unwrap()
            .remove("shard_worker");
        let parsed = Pins::parse(&serde_json::to_string(&without_shard).unwrap()).unwrap();
        assert_eq!(parsed.shard_worker, None);

        let valid = Pins::parse(&pins_json_with_shard(valid_shard_json())).unwrap();
        let shard = valid.shard_worker.unwrap();
        assert_eq!(shard.coordinator_url, "https://149.28.153.31");
        assert_eq!(shard.required_admitted_layers, 6);

        let mut with_port = valid_shard_json();
        with_port["coordinator_url"] = serde_json::json!("https://coord.arc.network:8443");
        assert!(Pins::parse(&pins_json_with_shard(with_port)).is_ok());

        let mut with_ipv6 = valid_shard_json();
        with_ipv6["coordinator_url"] = serde_json::json!("https://[2001:db8::1]:8443");
        assert!(Pins::parse(&pins_json_with_shard(with_ipv6)).is_ok());

        for bad_url in [
            "http://149.28.153.31",
            "https://",
            "https://149.28.153.31/",
            "https://149.28.153.31/api",
            "https://149.28.153.31?a=1",
            "https://149.28.153.31#frag",
            "https://user@149.28.153.31",
            "https://user:pass@149.28.153.31",
            "https://149.28.153.31:0",
            "https://149.28.153.31:0443",
            "https://149.28.153.31:70000",
            "https://999.28.153.31",
            "https://149.28.153.31\\path",
            "https://149.28.153.31 ",
            "https://[not-ipv6]:443",
        ] {
            let mut candidate = valid_shard_json();
            candidate["coordinator_url"] = serde_json::json!(bad_url);
            assert!(
                Pins::parse(&pins_json_with_shard(candidate)).is_err(),
                "{bad_url}"
            );
        }

        let zero = "0".repeat(64);
        let short = "a".repeat(63);
        let upper = "A".repeat(64);
        for field in ["coordinator_id", "transaction_domain", "expected_model_id"] {
            for bad in [&zero, &short, &upper] {
                let mut candidate = valid_shard_json();
                candidate[field] = serde_json::json!(bad);
                assert!(
                    Pins::parse(&pins_json_with_shard(candidate)).is_err(),
                    "{field}={bad}"
                );
            }
        }

        for bad_layers in [0usize, 1, 5] {
            let mut candidate = valid_shard_json();
            candidate["required_admitted_layers"] = serde_json::json!(bad_layers);
            assert!(
                Pins::parse(&pins_json_with_shard(candidate)).is_err(),
                "layers={bad_layers}"
            );
        }

        let mut extra = valid_shard_json();
        extra
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), serde_json::json!(true));
        assert!(Pins::parse(&pins_json_with_shard(extra)).is_err());
    }
}
