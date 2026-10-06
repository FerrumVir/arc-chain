//! The owner-signed `SHA256SUMS` release manifest.
//!
//! Format and rules mirror the release publisher (`release.yml`, "sealed
//! release manifest") and `install.sh`: four fixed metadata lines naming the
//! schema, repository, tag, and source commit, then one `<sha256>  <name>`
//! record per asset with no duplicates and no other metadata.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow, ensure};

use crate::pins::{Pins, is_asset_name, is_lower_hex};

pub const SCHEMA_LINE: &str = "# ARC release manifest v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub repository: String,
    pub tag: String,
    pub commit: String,
    pub digests: BTreeMap<String, String>,
}

pub fn parse(bytes: &[u8]) -> Result<ReleaseManifest> {
    let text = std::str::from_utf8(bytes).map_err(|_| anyhow!("release manifest is not UTF-8"))?;
    ensure!(
        !text.contains('\r'),
        "release manifest must use LF line endings"
    );
    ensure!(
        text.ends_with('\n'),
        "release manifest must end with a newline"
    );
    let lines: Vec<&str> = text.lines().collect();
    ensure!(lines.len() > 4, "release manifest is truncated");
    ensure!(
        lines[0] == SCHEMA_LINE,
        "release manifest has the wrong schema"
    );
    let repository = header(lines[1], "# repository=")?;
    let tag = header(lines[2], "# tag=")?;
    let commit = header(lines[3], "# commit=")?;
    ensure!(
        is_lower_hex(&commit, 40),
        "release manifest commit is not 40 lowercase hex characters"
    );

    let mut digests = BTreeMap::new();
    for line in &lines[4..] {
        ensure!(
            !line.starts_with('#'),
            "release manifest contains unexpected metadata"
        );
        let (digest, name) = line
            .split_once("  ")
            .ok_or_else(|| anyhow!("release manifest record is malformed"))?;
        ensure!(
            is_lower_hex(digest, 64),
            "release manifest record has an invalid SHA-256"
        );
        ensure!(
            is_asset_name(name),
            "release manifest record has an invalid asset name"
        );
        ensure!(
            digests
                .insert(name.to_string(), digest.to_string())
                .is_none(),
            "release manifest lists {name} twice"
        );
    }
    Ok(ReleaseManifest {
        repository,
        tag,
        commit,
        digests,
    })
}

fn header(line: &str, prefix: &str) -> Result<String> {
    line.strip_prefix(prefix)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("release manifest is missing its {prefix} line"))
}

impl ReleaseManifest {
    /// The signed manifest must name the pinned repository, tag, and commit,
    /// and list exactly the pinned digest for every asset the bridge uses.
    pub fn check_pins(&self, pins: &Pins) -> Result<()> {
        ensure!(
            self.repository == pins.repository,
            "signed manifest targets another repository"
        );
        ensure!(
            self.tag == pins.node_release.tag,
            "signed manifest targets another tag"
        );
        ensure!(
            self.commit == pins.node_release.commit,
            "signed manifest targets another source commit"
        );
        for (name, pinned) in &pins.node_release.assets {
            let digest = self
                .digests
                .get(name)
                .ok_or_else(|| anyhow!("signed manifest does not list {name}"))?;
            ensure!(
                digest == &pinned.sha256,
                "signed manifest digest for {name} differs from the pinned digest"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &[u8] = include_bytes!("../fixtures/v0.8.10/SHA256SUMS");

    #[test]
    fn real_v0810_manifest_parses_and_matches_pins() {
        let manifest = parse(MANIFEST).unwrap();
        assert_eq!(manifest.repository, "FerrumVir/arc-chain");
        assert_eq!(manifest.tag, "v0.8.10");
        assert_eq!(manifest.commit, "fd4e8cd2f76b97d08221a622397f513ad04000ad");
        assert_eq!(manifest.digests.len(), 28);
        manifest.check_pins(&Pins::embedded().unwrap()).unwrap();
    }

    #[test]
    fn manifest_shape_violations_are_rejected() {
        let text = std::str::from_utf8(MANIFEST).unwrap();
        let crlf = text.replace('\n', "\r\n");
        assert!(parse(crlf.as_bytes()).is_err());
        let extra_header = text.replacen("# commit=", "# signer=someone\n# commit=", 1);
        assert!(parse(extra_header.as_bytes()).is_err());
        let duplicated = format!(
            "{text}1fd253cd09520549534bb8b3be64b81022f4adcf66c24884967c621305097724  arc-node-linux-x86_64\n"
        );
        assert!(parse(duplicated.as_bytes()).is_err());
        let path_name = format!(
            "{text}1fd253cd09520549534bb8b3be64b81022f4adcf66c24884967c621305097724  ../arc-node\n"
        );
        assert!(parse(path_name.as_bytes()).is_err());
        assert!(parse(text.trim_end().as_bytes()).is_err());
    }

    #[test]
    fn pin_mismatch_is_rejected() {
        let pins = Pins::embedded().unwrap();
        let text = std::str::from_utf8(MANIFEST)
            .unwrap()
            .replace("# tag=v0.8.10", "# tag=v0.8.11");
        assert!(parse(text.as_bytes()).unwrap().check_pins(&pins).is_err());
        let mut manifest = parse(MANIFEST).unwrap();
        manifest
            .digests
            .insert("arc-node-linux-x86_64".to_string(), "0".repeat(64));
        assert!(manifest.check_pins(&pins).is_err());
        manifest.digests.remove("arc-node-linux-x86_64");
        assert!(manifest.check_pins(&pins).is_err());
    }
}
