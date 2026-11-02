//! "Archive in place": the v0.7 data directory stays exactly where and what it
//! is. The bridge never opens a v0.7 file for writing, never renames or
//! deletes one, and never follows a link inside it. It only records a
//! stat-level manifest (path, type, size, modification time) so anyone can
//! later check that nothing changed. Content hashes are left to explicit
//! verification (`--legacy-bridge-verify-archive`, and the CI acceptance
//! test) so a multi-gigabyte WAL does not delay the first v0.8 start past the
//! v0.7 updater's 30-second health window.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::consent::modified_nanos;
use crate::exit;
use crate::layout::{Layout, write_atomic};
use crate::logging::{Log, unix_now};

pub const ARCHIVE_SCHEMA: &str = "arc.legacy-bridge.v07-data-archive.v1";
const MAX_ENTRIES: usize = 2_000_000;
const FILE_PREFIX: &str = "v0.7-data-archive-";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub modified_unix_nanos: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveRecord {
    pub schema: String,
    pub generation: u32,
    pub legacy_kind: String,
    pub legacy_data_dir: String,
    pub recorded_unix: u64,
    pub excluded_top_level: Vec<String>,
    pub truncated: bool,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveSummary {
    pub path: PathBuf,
    pub generation: u32,
    pub entries: usize,
}

/// Stat every entry below the v0.7 data directory without following links.
pub fn snapshot(layout: &Layout) -> Result<(Vec<Entry>, bool)> {
    let mut entries = Vec::new();
    let root = &layout.legacy_data_dir;
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(exit::refused(format!(
                "the v0.7 data path {} is not a directory",
                root.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((entries, false)),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", root.display()));
        }
    }
    let mut truncated = false;
    let mut pending: Vec<(PathBuf, String)> = vec![(root.clone(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        let mut children = Vec::new();
        for child in fs::read_dir(&dir).with_context(|| format!("cannot list {}", dir.display()))? {
            let child = child.with_context(|| format!("cannot list {}", dir.display()))?;
            children.push((
                child.file_name().to_string_lossy().into_owned(),
                child.path(),
            ));
        }
        children.sort();
        for (name, path) in children {
            if prefix.is_empty() && layout.is_excluded_top_level(&name) {
                continue;
            }
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("cannot inspect {}", path.display()))?;
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            };
            entries.push(Entry {
                path: relative.clone(),
                kind: kind.to_string(),
                size: if file_type.is_file() {
                    metadata.len()
                } else {
                    0
                },
                modified_unix_nanos: modified_nanos(&metadata),
            });
            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            if kind == "dir" {
                pending.push((path, relative));
            }
        }
        if truncated {
            break;
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((entries, truncated))
}

fn excluded_names(layout: &Layout) -> Vec<String> {
    if layout.legacy_data_dir_is_root {
        let mut names: Vec<String> = crate::layout::DESKTOP_EXCLUDED_TOP_LEVEL
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        names.push(format!("{}*", crate::layout::DESKTOP_EXCLUDED_PREFIX));
        names
    } else {
        Vec::new()
    }
}

/// The newest archive record for this node, if any.
pub fn latest(layout: &Layout) -> Result<Option<(PathBuf, ArchiveRecord)>> {
    let mut best: Option<(u32, PathBuf)> = None;
    let listing = match fs::read_dir(&layout.node_dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot list {}", layout.node_dir.display()));
        }
    };
    for child in listing.flatten() {
        let name = child.file_name().to_string_lossy().into_owned();
        let Some(number) = name
            .strip_prefix(FILE_PREFIX)
            .and_then(|rest| rest.strip_suffix(".json"))
            .and_then(|digits| digits.parse::<u32>().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(current, _)| number > *current) {
            best = Some((number, child.path()));
        }
    }
    let Some((_, path)) = best else {
        return Ok(None);
    };
    let text =
        fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let record: ArchiveRecord = serde_json::from_str(&text)
        .with_context(|| format!("{} is not an archive record", path.display()))?;
    Ok(Some((path, record)))
}

/// Record the v0.7 data directory, writing a new generation only if it
/// changed since the newest record.
pub fn ensure(layout: &Layout, log: &mut Log) -> Result<ArchiveSummary> {
    let (entries, truncated) = snapshot(layout)?;
    let previous = latest(layout)?;
    if let Some((path, record)) = previous
        .as_ref()
        .filter(|(_, record)| record.entries == entries && record.truncated == truncated)
    {
        return Ok(ArchiveSummary {
            path: path.clone(),
            generation: record.generation,
            entries: entries.len(),
        });
    }
    let generation = previous
        .as_ref()
        .map_or(1, |(_, record)| record.generation + 1);
    let record = ArchiveRecord {
        schema: ARCHIVE_SCHEMA.to_string(),
        generation,
        legacy_kind: layout.kind.label().to_string(),
        legacy_data_dir: layout.legacy_data_dir.to_string_lossy().into_owned(),
        recorded_unix: unix_now(),
        excluded_top_level: excluded_names(layout),
        truncated,
        entries,
    };
    let path = layout
        .node_dir
        .join(format!("{FILE_PREFIX}{generation:04}.json"));
    write_atomic(&path, &serde_json::to_vec_pretty(&record)?)?;
    if generation == 1 {
        log.info(&format!(
            "archived the v0.7 data in place ({} entries, left unchanged): {}",
            record.entries.len(),
            layout.legacy_data_dir.display()
        ));
    } else {
        log.warn(&format!(
            "the v0.7 data changed since archive record {} (the v0.7 node ran again, usually after a rollback); wrote record {generation}. The bridge itself never writes v0.7 data.",
            generation - 1
        ));
    }
    Ok(ArchiveSummary {
        path,
        generation,
        entries: record.entries.len(),
    })
}

/// Compare the current tree with a record: (added, removed, changed) paths.
pub fn diff(record: &ArchiveRecord, current: &[Entry]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let before: std::collections::BTreeMap<&str, &Entry> = record
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let after: std::collections::BTreeMap<&str, &Entry> = current
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let added: Vec<String> = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .map(|path| (*path).to_string())
        .collect();
    let removed: Vec<String> = before
        .keys()
        .filter(|path| !after.contains_key(*path))
        .map(|path| (*path).to_string())
        .collect();
    let changed: Vec<String> = after
        .iter()
        .filter(|(path, entry)| before.get(*path).is_some_and(|old| old != *entry))
        .map(|(path, _)| (*path).to_string())
        .collect();
    (added, removed, changed)
}

/// Hash every regular file under `dir` (used by the explicit verify command).
pub fn content_digests(dir: &Path, entries: &[Entry]) -> Result<Vec<(String, String)>> {
    let mut digests = Vec::new();
    for entry in entries.iter().filter(|entry| entry.kind == "file") {
        let path = dir.join(&entry.path);
        let digest = crate::hashing::sha256_file(&path)
            .with_context(|| format!("cannot hash {}", path.display()))?;
        digests.push((entry.path.clone(), digest));
    }
    Ok(digests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::argv::{LegacyInvocation, LegacyKind};
    use crate::layout::test_support::{TempDir, fake_arc_dir};
    use crate::layout::{prepare, resolve};
    use std::net::SocketAddr;

    fn desktop_layout(temp: &TempDir) -> Layout {
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);
        let invocation = LegacyInvocation {
            kind: LegacyKind::Desktop,
            rpc: "127.0.0.1:9944".parse::<SocketAddr>().unwrap(),
            p2p_port: 9945,
            data_dir: root.clone(),
            seeds_file: None,
            genesis: None,
            model: None,
            community_mode: false,
        };
        let layout = resolve(&launcher, &invocation, temp.path()).unwrap();
        prepare(&layout).unwrap();
        layout
    }

    #[test]
    fn the_archive_records_state_only_and_is_written_once_while_unchanged() {
        let temp = TempDir::new("archive-once");
        let layout = desktop_layout(&temp);
        fs::write(layout.legacy_data_dir.join("state.wal"), b"v0.7 state").unwrap();
        fs::create_dir_all(layout.legacy_data_dir.join("dag-wal")).unwrap();
        fs::write(
            layout.legacy_data_dir.join("dag-wal").join("segment-0.wal"),
            b"dag",
        )
        .unwrap();
        fs::create_dir_all(layout.legacy_data_dir.join("models")).unwrap();
        fs::write(
            layout.legacy_data_dir.join("models").join("m.gguf"),
            b"model",
        )
        .unwrap();
        let before = fs::read(layout.legacy_data_dir.join("state.wal")).unwrap();

        let mut log = Log::stderr_only();
        let first = ensure(&layout, &mut log).unwrap();
        assert_eq!(first.generation, 1);
        let (_, record) = latest(&layout).unwrap().unwrap();
        let paths: Vec<&str> = record
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, vec!["dag-wal", "dag-wal/segment-0.wal", "state.wal"]);

        let second = ensure(&layout, &mut log).unwrap();
        assert_eq!(second.generation, 1, "an unchanged tree is not re-recorded");
        assert_eq!(
            fs::read(layout.legacy_data_dir.join("state.wal")).unwrap(),
            before
        );

        fs::write(
            layout.legacy_data_dir.join("state.wal"),
            b"v0.7 state, longer after a rollback",
        )
        .unwrap();
        let third = ensure(&layout, &mut log).unwrap();
        assert_eq!(third.generation, 2);
        let (_, record_two) = latest(&layout).unwrap().unwrap();
        let (added, removed, changed) = diff(&record, &record_two.entries);
        assert!(added.is_empty() && removed.is_empty());
        assert_eq!(changed, vec!["state.wal".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_the_v07_data_are_recorded_not_followed() {
        let temp = TempDir::new("archive-links");
        let layout = desktop_layout(&temp);
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), b"not v0.7 data").unwrap();
        std::os::unix::fs::symlink(&outside, layout.legacy_data_dir.join("linked")).unwrap();
        let (entries, _) = snapshot(&layout).unwrap();
        let linked: Vec<&Entry> = entries
            .iter()
            .filter(|entry| entry.path.starts_with("linked"))
            .collect();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].kind, "symlink");
    }

    #[test]
    fn content_digests_cover_every_file() {
        let temp = TempDir::new("archive-digests");
        let layout = desktop_layout(&temp);
        fs::write(layout.legacy_data_dir.join("state.wal"), b"abc").unwrap();
        let (entries, _) = snapshot(&layout).unwrap();
        let digests = content_digests(&layout.legacy_data_dir, &entries).unwrap();
        assert_eq!(
            digests,
            vec![(
                "state.wal".to_string(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string()
            )]
        );
    }
}
