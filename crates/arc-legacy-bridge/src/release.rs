//! The pinned v0.8 release on disk: fetched, signature-checked, and probed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::exit;
use crate::fetch::Fetcher;
use crate::layout::{Layout, ensure_private_dir};
use crate::logging::Log;
use crate::manifest;
use crate::pins::{
    GENESIS_NAME, MANIFEST_NAME, MANIFEST_SIGNATURE_NAME, Pins, Platform, SEEDS_NAME, is_lower_hex,
};
use crate::sshsig;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRelease {
    pub dir: PathBuf,
    pub node: PathBuf,
    pub cli: PathBuf,
    pub genesis: PathBuf,
    pub seeds: PathBuf,
}

/// Fetch (or re-verify the cached copy of) every file the bridged node needs.
///
/// Order matches `install.sh`: the signed manifest is fetched and verified
/// with the pinned release key before any executable is downloaded, and every
/// asset must match both the signed manifest and the compiled-in pin.
pub fn ensure(
    layout: &Layout,
    pins: &Pins,
    platform: Platform,
    log: &mut Log,
) -> Result<VerifiedRelease> {
    let dir = layout.releases_dir().join(&pins.node_release.tag);
    ensure_private_dir(&dir)?;
    let fetcher = Fetcher::github_release(&pins.repository, &pins.node_release.tag);
    ensure_release_with(&fetcher, &dir, pins, platform, log)
}

pub(crate) fn ensure_release_with(
    fetcher: &Fetcher,
    dir: &Path,
    pins: &Pins,
    platform: Platform,
    log: &mut Log,
) -> Result<VerifiedRelease> {
    for pinned in [
        &pins.node_release.manifest,
        &pins.node_release.manifest_signature,
    ] {
        fetcher.ensure_file(
            &pinned.name,
            &pinned.sha256,
            pinned.size,
            &dir.join(&pinned.name),
            log,
        )?;
    }
    let manifest_bytes =
        fs::read(dir.join(MANIFEST_NAME)).context("cannot read the cached SHA256SUMS")?;
    let signature = fs::read_to_string(dir.join(MANIFEST_SIGNATURE_NAME))
        .context("cannot read SHA256SUMS.sig")?;
    sshsig::verify(
        &signature,
        &manifest_bytes,
        &pins.manifest_signer.namespace,
        &pins.signer_key()?,
    )
    .map_err(|error| {
        exit::refused(format!(
            "the pinned release manifest is not owner-signed: {error:#}"
        ))
    })?;
    let manifest = manifest::parse(&manifest_bytes).map_err(|error| {
        exit::refused(format!(
            "the signed release manifest is malformed: {error:#}"
        ))
    })?;
    manifest.check_pins(pins).map_err(|error| {
        exit::refused(format!(
            "the signed release manifest differs from the bridge pins: {error:#}"
        ))
    })?;

    let mut downloaded = Vec::new();
    for name in [
        platform.node_asset(),
        platform.cli_asset(),
        GENESIS_NAME,
        SEEDS_NAME,
    ] {
        let pinned = pins.asset(name)?;
        if fetcher.ensure_file(name, &pinned.sha256, pinned.size, &dir.join(name), log)? {
            downloaded.push(name);
        }
    }
    if downloaded.is_empty() {
        log.info(&format!(
            "reusing the verified {} release cache in {}",
            pins.node_release.tag,
            dir.display()
        ));
    } else {
        log.info(&format!(
            "downloaded and verified {} from {}: {}",
            pins.node_release.tag,
            pins.repository,
            downloaded.join(", ")
        ));
    }
    let release = VerifiedRelease {
        dir: dir.to_path_buf(),
        node: dir.join(platform.node_asset()),
        cli: dir.join(platform.cli_asset()),
        genesis: dir.join(GENESIS_NAME),
        seeds: dir.join(SEEDS_NAME),
    };
    make_executable(&release.node)?;
    make_executable(&release.cli)?;
    Ok(release)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot mark {} executable", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// A command for a helper binary that never flashes a console on Windows.
pub fn quiet_command(program: &Path) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Prove the pinned binary runs on this machine (architecture, C library,
/// OS version) before the bridge commits to starting it.
pub fn probe_version(binary: &Path, expected_version: &str) -> Result<()> {
    let output = quiet_command(binary)
        .arg("--version")
        .output()
        .map_err(|error| {
            exit::refused(format!(
                "the pinned {} cannot run on this machine: {error}",
                binary.display()
            ))
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reports_version = stdout
        .split_whitespace()
        .any(|token| token == expected_version);
    if !output.status.success() || !reports_version {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(exit::refused(format!(
            "the pinned {} did not report version {expected_version} on this machine (status {}): {}",
            binary.display(),
            output.status,
            stderr.lines().next().unwrap_or("no output")
        )));
    }
    Ok(())
}

/// Create (once) and self-check the bridged node's Ed25519 keyfile.
///
/// The identity is new: the v0.7 seed is never read. A v0.7 headless seed
/// was `community-<hostname>-<8 hex>`, about 32 bits of secret, so preserving
/// it would keep a key that can be brute-forced from its public address. The
/// keyfile is created by the pinned `arc-cli`, which writes it owner-only
/// (mode 0600 on Unix, a protected owner DACL on Windows) and never replaces
/// an existing file.
pub fn ensure_identity(layout: &Layout, cli: &Path, log: &mut Log) -> Result<String> {
    let keyfile = layout.keyfile();
    if fs::symlink_metadata(&keyfile).is_err() {
        let output = quiet_command(cli)
            .args(["keygen", "--scheme", "ed25519", "--output"])
            .arg(&keyfile)
            .output()
            .context("cannot run the pinned arc-cli")?;
        if !output.status.success() {
            return Err(exit::refused(format!(
                "the pinned arc-cli could not create {}: {}",
                keyfile.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        log.info(&format!(
            "created a new Ed25519 identity for the bridged node at {}",
            keyfile.display()
        ));
    }
    let output = quiet_command(cli)
        .arg("keygen")
        .arg("--verify-keyfile")
        .arg(&keyfile)
        .output()
        .context("cannot run the pinned arc-cli")?;
    let address = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || !is_lower_hex(&address, 64) {
        return Err(exit::refused(format!(
            "{} failed its self-check; it is kept unchanged for inspection: {}",
            keyfile.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::test_server::serve;
    use crate::hashing::sha256_bytes;
    use crate::layout::test_support::TempDir;
    use std::collections::BTreeMap;

    const MANIFEST: &[u8] = include_bytes!("../fixtures/v0.8.10/SHA256SUMS");
    const SIGNATURE: &[u8] = include_bytes!("../fixtures/v0.8.10/SHA256SUMS.sig");

    /// Pins whose asset digests describe small local stand-ins, while the
    /// manifest and its signature stay the real v0.8.10 bytes. This drives
    /// the full ordering and refusal logic without downloading 30 MB.
    fn stand_in_pins(platform: Platform) -> (Pins, BTreeMap<String, Vec<u8>>) {
        let mut pins = Pins::embedded().unwrap();
        let mut files = BTreeMap::new();
        files.insert(MANIFEST_NAME.to_string(), MANIFEST.to_vec());
        files.insert(MANIFEST_SIGNATURE_NAME.to_string(), SIGNATURE.to_vec());
        for name in [
            platform.node_asset(),
            platform.cli_asset(),
            GENESIS_NAME,
            SEEDS_NAME,
        ] {
            let body = format!("stand-in for {name}").into_bytes();
            let pinned = pins.node_release.assets.get_mut(name).unwrap();
            pinned.sha256 = sha256_bytes(&body);
            pinned.size = body.len() as u64;
            files.insert(name.to_string(), body);
        }
        (pins, files)
    }

    #[test]
    fn stand_in_assets_are_refused_because_the_signed_manifest_disagrees() {
        let platform = Platform::LinuxX86_64;
        let (pins, files) = stand_in_pins(platform);
        let server = serve(files, None);
        let fetcher = Fetcher::for_local_test_server(server.base.clone());
        let temp = TempDir::new("release-standin");
        let mut log = Log::stderr_only();
        let error =
            ensure_release_with(&fetcher, temp.path(), &pins, platform, &mut log).unwrap_err();
        assert_eq!(exit::code_for(&error), exit::EX_CONFIG);
        assert!(format!("{error:#}").contains("differs from the bridge pins"));
        // Nothing executable was fetched before the manifest check failed.
        assert!(!temp.path().join(platform.node_asset()).exists());
    }

    #[test]
    fn a_forged_manifest_signature_is_refused_before_any_asset_download() {
        let platform = Platform::MacosArm64;
        let pins = Pins::embedded().unwrap();
        let mut files = BTreeMap::new();
        let mut forged = MANIFEST.to_vec();
        forged[100] ^= 1;
        files.insert(MANIFEST_NAME.to_string(), forged.clone());
        files.insert(MANIFEST_SIGNATURE_NAME.to_string(), SIGNATURE.to_vec());
        let mut pins_for_forgery = pins;
        pins_for_forgery.node_release.manifest.sha256 = sha256_bytes(&forged);
        let server = serve(files, None);
        let fetcher = Fetcher::for_local_test_server(server.base.clone());
        let temp = TempDir::new("release-forged");
        let mut log = Log::stderr_only();
        let error =
            ensure_release_with(&fetcher, temp.path(), &pins_for_forgery, platform, &mut log)
                .unwrap_err();
        assert_eq!(exit::code_for(&error), exit::EX_CONFIG);
        assert!(format!("{error:#}").contains("not owner-signed"));
        let requested = server.requests.lock().unwrap().clone();
        assert!(
            requested.iter().all(|line| line.starts_with("SHA256SUMS")),
            "{requested:?}"
        );
    }

    #[test]
    fn a_manifest_that_does_not_match_its_pin_is_never_used() {
        let platform = Platform::WindowsX86_64;
        let pins = Pins::embedded().unwrap();
        let mut files = BTreeMap::new();
        files.insert(
            MANIFEST_NAME.to_string(),
            b"# ARC release manifest v1\n".to_vec(),
        );
        files.insert(MANIFEST_SIGNATURE_NAME.to_string(), SIGNATURE.to_vec());
        let server = serve(files, None);
        let fetcher = Fetcher::for_local_test_server(server.base.clone());
        let temp = TempDir::new("release-pinned-manifest");
        let mut log = Log::stderr_only();
        let error =
            ensure_release_with(&fetcher, temp.path(), &pins, platform, &mut log).unwrap_err();
        assert_eq!(exit::code_for(&error), exit::EX_UNAVAILABLE);
        assert!(!temp.path().join(MANIFEST_NAME).exists());
    }
}
