//! Production desktop update-channel selection.
//!
//! Public v0.7 headless timers consume GitHub's global `latest` release
//! without the v0.8 migration or signature contract. v0.8 desktop builds must
//! therefore discover releases independently while those timers are retired.
//! This module accepts only immutable releases created by ARC's protected
//! publisher, then gives Tauri an exact-tag `latest.json` endpoint. Tauri owns
//! the download resource and verifies the updater payload with the public key
//! embedded in `tauri.conf.json` before any installer bytes can execute.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use tauri::{Manager, Webview};
use tauri_plugin_updater::{Update, UpdaterExt};

const RELEASES_API: &str = "https://api.github.com/repos/FerrumVir/arc-chain/releases";
const RELEASE_DOWNLOAD_ROOT: &str = "https://github.com/FerrumVir/arc-chain/releases/download";
const RELEASE_PUBLISHER: &str = "github-actions[bot]";
const FIRST_CHANNEL_VERSION: (u64, u64, u64) = (0, 8, 0);
const RELEASES_PER_PAGE: usize = 100;
const MAX_RELEASE_PAGES: usize = 10;
const MAX_RELEASE_PAGE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_UPDATER_MANIFEST_BYTES: u64 = 256 * 1024;
const CHANNEL_TIMEOUT: Duration = Duration::from_secs(30);

const REQUIRED_DESKTOP_ASSETS: &[&str] = &[
    "latest.json",
    "arc-desktop-macos-arm64.app.tar.gz",
    "arc-desktop-macos-arm64.app.tar.gz.sig",
    "arc-desktop-macos-x86_64.app.tar.gz",
    "arc-desktop-macos-x86_64.app.tar.gz.sig",
    "arc-desktop-windows-x86_64-setup.exe",
    "arc-desktop-windows-x86_64-setup.exe.sig",
    "arc-desktop-linux-x86_64.AppImage",
    "arc-desktop-linux-x86_64.AppImage.sig",
];

#[derive(Clone, Debug, Deserialize)]
struct GithubActor {
    login: String,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubAsset {
    id: u64,
    name: String,
    size: u64,
    digest: Option<String>,
    state: String,
    uploader: Option<GithubActor>,
    browser_download_url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubRelease {
    id: u64,
    tag_name: String,
    target_commitish: String,
    name: String,
    draft: bool,
    prerelease: bool,
    immutable: bool,
    author: Option<GithubActor>,
    assets: Vec<GithubAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedRelease {
    id: u64,
    tag: String,
    version: Version,
    manifest_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMetadata {
    rid: tauri::ResourceId,
    current_version: String,
    version: String,
    date: Option<String>,
    body: Option<String>,
    raw_json: serde_json::Value,
}

fn strict_release_version(tag: &str) -> Option<Version> {
    let raw = tag.strip_prefix('v')?;
    if raw.is_empty()
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    let version = Version::parse(raw).ok()?;
    if !version.pre.is_empty() || !version.build.is_empty() || format!("v{version}") != tag {
        return None;
    }
    Some(version)
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn release_asset_url(tag: &str, name: &str) -> String {
    format!("{RELEASE_DOWNLOAD_ROOT}/{tag}/{name}")
}

fn trusted_digest(digest: Option<&str>) -> bool {
    digest
        .and_then(|value| value.strip_prefix("sha256:"))
        .is_some_and(|hash| is_lower_hex(hash, 64))
}

fn validate_required_asset(release: &GithubRelease, name: &str) -> Result<()> {
    let mut matching = release.assets.iter().filter(|asset| asset.name == name);
    let asset = matching.next().ok_or_else(|| {
        anyhow!(
            "release {} is missing required asset {name}",
            release.tag_name
        )
    })?;
    if matching.next().is_some() {
        bail!(
            "release {} contains duplicate required asset {name}",
            release.tag_name
        );
    }
    if asset.id == 0
        || asset.size == 0
        || asset.state != "uploaded"
        || asset.uploader.as_ref().map(|actor| actor.login.as_str()) != Some(RELEASE_PUBLISHER)
        || !trusted_digest(asset.digest.as_deref())
        || asset.browser_download_url != release_asset_url(&release.tag_name, name)
    {
        bail!(
            "release {} has untrusted metadata for required asset {name}",
            release.tag_name
        );
    }
    if name == "latest.json" && asset.size > MAX_UPDATER_MANIFEST_BYTES {
        bail!(
            "release {} updater manifest exceeds {} bytes",
            release.tag_name,
            MAX_UPDATER_MANIFEST_BYTES
        );
    }
    Ok(())
}

fn validate_channel_release(release: &GithubRelease, version: Version) -> Result<SelectedRelease> {
    if release.id == 0 {
        bail!("release {} has an invalid release id", release.tag_name);
    }
    if release.name != format!("ARC Chain {}", release.tag_name) {
        bail!(
            "release {} has an unexpected release name",
            release.tag_name
        );
    }
    if !is_lower_hex(&release.target_commitish, 40) {
        bail!(
            "release {} is not bound to an exact lowercase commit",
            release.tag_name
        );
    }

    let mut names = HashSet::with_capacity(release.assets.len());
    for asset in &release.assets {
        if !names.insert(asset.name.as_str()) {
            bail!(
                "release {} contains duplicate asset name {}",
                release.tag_name,
                asset.name
            );
        }
    }
    for name in REQUIRED_DESKTOP_ASSETS {
        validate_required_asset(release, name)?;
    }

    Ok(SelectedRelease {
        id: release.id,
        tag: release.tag_name.clone(),
        version,
        manifest_url: release_asset_url(&release.tag_name, "latest.json"),
    })
}

fn select_release(releases: &[GithubRelease]) -> Result<Option<SelectedRelease>> {
    let minimum = Version::new(
        FIRST_CHANNEL_VERSION.0,
        FIRST_CHANNEL_VERSION.1,
        FIRST_CHANNEL_VERSION.2,
    );
    let mut selected: Option<(&GithubRelease, Version)> = None;

    for release in releases {
        let Some(version) = strict_release_version(&release.tag_name) else {
            continue;
        };
        if version < minimum
            || release.draft
            || release.prerelease
            || !release.immutable
            || release.author.as_ref().map(|actor| actor.login.as_str()) != Some(RELEASE_PUBLISHER)
        {
            continue;
        }

        if let Some((current_release, current_version)) = selected.as_ref() {
            if version == *current_version && release.id != current_release.id {
                bail!(
                    "ambiguous immutable desktop releases advertise version {}",
                    version
                );
            }
            if version <= *current_version {
                continue;
            }
        }
        selected = Some((release, version));
    }

    selected
        .map(|(release, version)| validate_channel_release(release, version))
        .transpose()
}

async fn fetch_releases(client: &reqwest::Client) -> Result<Vec<GithubRelease>> {
    let mut releases = Vec::new();

    for page in 1..=MAX_RELEASE_PAGES {
        let mut response = client
            .get(RELEASES_API)
            .query(&[
                ("per_page", RELEASES_PER_PAGE.to_string()),
                ("page", page.to_string()),
            ])
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .with_context(|| format!("GitHub release discovery page {page} failed"))?;

        let status = response.status();
        if !status.is_success() {
            bail!("GitHub release discovery returned HTTP {status}");
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RELEASE_PAGE_BYTES)
        {
            bail!("GitHub release discovery response is too large");
        }

        let mut body = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or_default()
                .min(MAX_RELEASE_PAGE_BYTES) as usize,
        );
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("GitHub release discovery page {page} was truncated"))?
        {
            if chunk.len() > MAX_RELEASE_PAGE_BYTES as usize - body.len() {
                bail!("GitHub release discovery response is too large");
            }
            body.extend_from_slice(&chunk);
        }
        let page_releases: Vec<GithubRelease> = serde_json::from_slice(&body)
            .with_context(|| format!("GitHub release discovery page {page} was malformed"))?;
        let page_len = page_releases.len();
        if page_len > RELEASES_PER_PAGE {
            bail!("GitHub returned more releases than the requested page size");
        }
        releases.extend(page_releases);
        if page_len < RELEASES_PER_PAGE {
            return Ok(releases);
        }
    }

    bail!(
        "GitHub release history exceeds the bounded {}-page channel scan",
        MAX_RELEASE_PAGES
    )
}

fn expected_payload_name(target: &str) -> Option<&'static str> {
    match target {
        "darwin-aarch64" => Some("arc-desktop-macos-arm64.app.tar.gz"),
        "darwin-x86_64" => Some("arc-desktop-macos-x86_64.app.tar.gz"),
        "windows-x86_64" => Some("arc-desktop-windows-x86_64-setup.exe"),
        "linux-x86_64" => Some("arc-desktop-linux-x86_64.AppImage"),
        _ => None,
    }
}

fn validate_manifest_binding(
    advertised_version: &str,
    target: &str,
    download_url: &str,
    selected: &SelectedRelease,
) -> Result<()> {
    if advertised_version != selected.version.to_string() {
        bail!(
            "immutable release {} manifest advertises version {}",
            selected.tag,
            advertised_version
        );
    }
    let payload = expected_payload_name(target)
        .ok_or_else(|| anyhow!("unsupported desktop updater target {target}"))?;
    let expected_url = release_asset_url(&selected.tag, payload);
    if download_url != expected_url {
        bail!(
            "immutable release {} manifest does not bind target {} to its exact-tag payload",
            selected.tag,
            target
        );
    }
    Ok(())
}

fn validate_tauri_update(update: &Update, selected: &SelectedRelease) -> Result<()> {
    let target = tauri_plugin_updater::target()
        .ok_or_else(|| anyhow!("cannot determine the desktop updater target"))?;
    validate_manifest_binding(
        &update.version,
        &target,
        update.download_url.as_str(),
        selected,
    )
}

async fn check_arc_update_inner(webview: &Webview) -> Result<Option<UpdateMetadata>> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(CHANNEL_TIMEOUT)
        .user_agent(format!("ARC-Desktop/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to construct the desktop release-channel client")?;
    let releases = fetch_releases(&client).await?;
    let Some(selected) = select_release(&releases)? else {
        return Ok(None);
    };

    let endpoint = tauri::Url::parse(&selected.manifest_url)
        .context("selected updater manifest URL is invalid")?;
    let updater = webview
        .updater_builder()
        .endpoints(vec![endpoint])?
        .timeout(CHANNEL_TIMEOUT)
        .build()?;
    let Some(update) = updater.check().await? else {
        return Ok(None);
    };
    validate_tauri_update(&update, &selected)?;

    let metadata = UpdateMetadata {
        current_version: update.current_version.clone(),
        version: update.version.clone(),
        date: update.date.map(|date| date.to_string()),
        body: update.body.clone(),
        raw_json: update.raw_json.clone(),
        rid: webview.resources_table().add(update),
    };
    Ok(Some(metadata))
}

/// Discover the newest immutable ARC desktop release and return Tauri's own
/// signed-update resource. Download and install remain explicit frontend
/// actions; this command never fetches or executes a payload.
#[tauri::command]
pub async fn check_arc_update(webview: Webview) -> Result<Option<UpdateMetadata>, String> {
    check_arc_update_inner(&webview)
        .await
        .map_err(|error| format!("secure ARC update channel failed: {error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn actor(login: &str) -> GithubActor {
        GithubActor {
            login: login.into(),
        }
    }

    fn asset(tag: &str, name: &str) -> GithubAsset {
        GithubAsset {
            id: 10,
            name: name.into(),
            size: 123,
            digest: Some(DIGEST.into()),
            state: "uploaded".into(),
            uploader: Some(actor(RELEASE_PUBLISHER)),
            browser_download_url: release_asset_url(tag, name),
        }
    }

    fn release(version: &str) -> GithubRelease {
        let tag = format!("v{version}");
        GithubRelease {
            id: version
                .bytes()
                .filter(u8::is_ascii_digit)
                .fold(1_u64, |value, digit| value + u64::from(digit - b'0')),
            tag_name: tag.clone(),
            target_commitish: COMMIT.into(),
            name: format!("ARC Chain {tag}"),
            draft: false,
            prerelease: false,
            immutable: true,
            author: Some(actor(RELEASE_PUBLISHER)),
            assets: REQUIRED_DESKTOP_ASSETS
                .iter()
                .map(|name| asset(&tag, name))
                .collect(),
        }
    }

    #[test]
    fn selects_highest_immutable_v08_release_without_global_latest() {
        let old = release("0.7.99");
        let first = release("0.8.0");
        let newest = release("1.2.3");
        let selected = select_release(&[newest.clone(), old, first])
            .unwrap()
            .unwrap();
        assert_eq!(selected.tag, "v1.2.3");
        assert_eq!(
            selected.manifest_url,
            "https://github.com/FerrumVir/arc-chain/releases/download/v1.2.3/latest.json"
        );
        assert!(!selected.manifest_url.contains("/releases/latest/"));
    }

    #[test]
    fn ignores_untrusted_or_unpublished_releases() {
        let trusted = release("0.8.1");
        for mutate in [
            |release: &mut GithubRelease| release.draft = true,
            |release: &mut GithubRelease| release.prerelease = true,
            |release: &mut GithubRelease| release.immutable = false,
            |release: &mut GithubRelease| release.author = Some(actor("manual-publisher")),
        ] {
            let mut untrusted = release("9.9.9");
            mutate(&mut untrusted);
            let selected = select_release(&[untrusted, trusted.clone()])
                .unwrap()
                .unwrap();
            assert_eq!(selected.tag, "v0.8.1");
        }
    }

    #[test]
    fn rejects_tampered_required_asset_metadata() {
        let mut bad_digest = release("0.8.1");
        bad_digest.assets[0].digest = Some("sha256:not-a-digest".into());
        assert!(select_release(&[bad_digest]).is_err());

        let mut wrong_uploader = release("0.8.1");
        wrong_uploader.assets[0].uploader = Some(actor("attacker"));
        assert!(select_release(&[wrong_uploader]).is_err());

        let mut moving_url = release("0.8.1");
        moving_url.assets[0].browser_download_url =
            "https://github.com/FerrumVir/arc-chain/releases/latest/download/latest.json".into();
        assert!(select_release(&[moving_url]).is_err());

        let mut duplicate = release("0.8.1");
        duplicate.assets.push(duplicate.assets[0].clone());
        assert!(select_release(&[duplicate]).is_err());
    }

    #[test]
    fn rejects_non_strict_tags_and_ambiguous_versions() {
        for tag in [
            "0.8.1",
            "v0.8",
            "v00.8.1",
            "v0.8.1-rc.1",
            "v0.8.1+build",
            "v0.8.1/latest",
        ] {
            assert_eq!(strict_release_version(tag), None, "accepted {tag}");
        }

        let first = release("0.8.1");
        let mut duplicate = first.clone();
        duplicate.id += 100;
        assert!(select_release(&[first, duplicate]).is_err());
    }

    #[test]
    fn updater_config_has_no_global_or_fallback_endpoint() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let endpoints = config["plugins"]["updater"]["endpoints"]
            .as_array()
            .unwrap();
        assert!(endpoints.is_empty());
        assert!(!include_str!("../tauri.conf.json").contains("releases/latest"));
    }

    #[test]
    fn manifest_must_bind_version_target_and_exact_tag_payload() {
        let selected = validate_channel_release(&release("0.8.1"), Version::new(0, 8, 1)).unwrap();
        let exact = release_asset_url("v0.8.1", "arc-desktop-windows-x86_64-setup.exe");
        validate_manifest_binding("0.8.1", "windows-x86_64", &exact, &selected).unwrap();

        assert!(validate_manifest_binding("9.9.9", "windows-x86_64", &exact, &selected).is_err());
        assert!(validate_manifest_binding(
            "0.8.1",
            "windows-x86_64",
            "https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/arc-desktop-windows-x86_64-setup.exe",
            &selected
        )
        .is_err());
        assert!(validate_manifest_binding("0.8.1", "windows-aarch64", &exact, &selected).is_err());
    }
}
