//! Operator-selected ENG-10 slices. This is not a network assignment protocol.
use super::*;
use arc_inference::modern::mla::{
    package::StageSpec,
    slices::{self, SliceManifest, SliceRecord},
};
use std::collections::BTreeSet;
use std::net::SocketAddr;

/// A selection authenticated by a digest supplied separately from the manifest.
pub struct ManifestAssignment {
    manifest: SliceManifest,
    selected: BTreeMap<String, SliceRecord>,
}

impl ManifestAssignment {
    pub fn parse(bytes: &[u8], trusted_digest: &str, names: &[String]) -> io::Result<Self> {
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(invalid("slice manifest exceeds 16 MiB"));
        }
        let pin = decode_digest(trusted_digest)?;
        let manifest = SliceManifest::parse(bytes).map_err(io::Error::other)?;
        let recorded = manifest.value["manifest_blake3"]
            .as_str()
            .ok_or_else(|| invalid("missing manifest digest"))?;
        if decode_digest(recorded)? != pin {
            return Err(invalid("manifest differs from separately trusted digest"));
        }
        if names.is_empty() {
            return Err(invalid("explicit slice selection is required"));
        }
        let mut records = BTreeMap::new();
        for record in &manifest.slices {
            decode_digest(&record.blake3)?;
            if records
                .insert(record.name.clone(), record.clone())
                .is_some()
            {
                return Err(invalid("duplicate slice name"));
            }
        }
        let mut selected = BTreeMap::new();
        for name in names {
            let record = records
                .get(name)
                .ok_or_else(|| invalid(format!("unknown slice: {name}")))?;
            if selected.insert(name.clone(), record.clone()).is_some() {
                return Err(invalid("duplicate selected slice"));
            }
        }
        Ok(Self { manifest, selected })
    }

    pub fn specs(&self) -> io::Result<Vec<SliceSpec>> {
        self.selected
            .values()
            .map(|record| {
                Ok(SliceSpec {
                    digest: SliceDigest::Blake3(decode_digest(&record.blake3)?),
                    bytes: record.bytes,
                })
            })
            .collect()
    }

    /// ENG-10 mirrors use raw lowercase BLAKE3 filenames, not peer route keys.
    pub fn slice_path(&self, name: &str, directory: &Path) -> io::Result<PathBuf> {
        Ok(self.record(name)?.path(directory))
    }

    fn record(&self, name: &str) -> io::Result<&SliceRecord> {
        self.selected
            .get(name)
            .ok_or_else(|| invalid("slice is not selected"))
    }

    /// Assembly is supported only for complete stages. Pending profiles (e.g.
    /// Kimi YaRN) and incomplete expert selections cannot become model inputs.
    pub fn validate_stage(&self, stage: StageSpec) -> io::Result<()> {
        if self.manifest.value["pending"]
            .as_array()
            .is_none_or(|v| !v.is_empty())
            || self.manifest.value["complete"].as_bool() != Some(true)
            || self.manifest.value["model_root"].as_str().is_none()
        {
            return Err(invalid(
                "pending or incomplete manifest is unavailable for inference",
            ));
        }
        let config = self.manifest.config().map_err(io::Error::other)?;
        stage.validate(&config).map_err(io::Error::other)?;
        let mut required = BTreeSet::new();
        if stage.first_layer == 0 {
            required.insert("embed".to_owned());
        }
        for layer in stage.first_layer..stage.end_layer {
            required.insert(format!("layer.{layer}"));
        }
        if stage.end_layer == config.n_layers {
            required.insert("head".to_owned());
        }
        for segment in required {
            let records: Vec<_> = self
                .manifest
                .slices
                .iter()
                .filter(|r| r.segment == segment)
                .collect();
            if records.is_empty() || records.iter().any(|r| !self.selected.contains_key(&r.name)) {
                return Err(invalid(format!(
                    "complete segment {segment} required; partial-expert assembly is unsupported"
                )));
            }
        }
        Ok(())
    }

    /// Explicit offline assembly from an ENG-10 directory. The upstream
    /// assembler re-verifies files, tensor layout and segment hashes. The
    /// caller must publish its output only after this function succeeds.
    pub fn assemble_stage(
        &self,
        directory: &Path,
        stage: StageSpec,
        output: &Path,
    ) -> io::Result<serde_json::Value> {
        self.validate_stage(stage)?;
        slices::assemble_stage(&self.manifest, directory, stage, output).map_err(io::Error::other)
    }
}

fn decode_digest(value: &str) -> io::Result<[u8; 32]> {
    // Canonical filenames are necessary because SliceRecord::path uses the
    // original string. Never accept path syntax or case aliases from JSON.
    if value.len() != 64
        || value
            .bytes()
            .any(|b| !b.is_ascii_digit() && !(b'a'..=b'f').contains(&b))
    {
        return Err(invalid("expected lowercase 32-byte BLAKE3 digest"));
    }
    let mut digest = [0; 32];
    hex::decode_to_slice(value, &mut digest).map_err(io::Error::other)?;
    Ok(digest)
}

/// One explicitly configured local acquisition/peer worker. No inferred
/// selection, discovery, assignment messages, or inference activation.
pub struct SliceWorker {
    assignment: ManifestAssignment,
    store: SliceStore,
    mirrors: Vec<Url>,
    peers: Vec<Url>,
}

// Also revoke if an embedding caller cancels the run future. The HTTP server
// may already have connection tasks holding cloned router state.
struct RevokeOnDrop(SliceConsent);

impl Drop for RevokeOnDrop {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl SliceWorker {
    pub fn new(
        assignment: ManifestAssignment,
        cache: PathBuf,
        config: &SliceDistributionConfig,
        mirrors: Vec<Url>,
        peers: Vec<Url>,
    ) -> io::Result<Self> {
        let store = SliceStore::new(cache, assignment.specs()?, config)?;
        Ok(Self {
            assignment,
            store,
            mirrors,
            peers,
        })
    }

    pub fn consent(&self) -> SliceConsent {
        self.store.consent()
    }

    pub fn router(&self) -> axum::Router {
        self.store.router()
    }

    /// Explicit, synchronous offline promotion. Copies and re-hashes selected
    /// cache objects into a private ENG-10 directory; never hard-links mutable
    /// cache files. Only a successfully assembled package is published, using
    /// create-only persistence. The output's parent must already exist.
    pub fn assemble_cached_stage(
        &self,
        stage: StageSpec,
        output: &Path,
    ) -> io::Result<serde_json::Value> {
        let permit = self.consent().permit()?;
        self.assignment.validate_stage(stage)?;
        if std::fs::symlink_metadata(&self.store.0.root)?
            .file_type()
            .is_symlink()
        {
            return Err(invalid("slice cache must not be a symlink"));
        }
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let staging = tempfile::Builder::new()
            .prefix(".arc-slice-assembly-")
            .tempdir_in(parent)?;
        let mut copied = BTreeSet::new();
        for record in self.assignment.selected.values() {
            permit.check()?;
            if !copied.insert(&record.blake3) {
                continue;
            }
            copy_verified_cache(
                &self.store.0.root.join(format!("blake3-{}", record.blake3)),
                &record.path(staging.path()),
                record,
                &permit,
            )?;
        }
        let package = tempfile::NamedTempFile::new_in(staging.path())?;
        let report = self
            .assignment
            .assemble_stage(staging.path(), stage, package.path())?;
        package.as_file().sync_all()?;
        permit.check()?;
        // No partial/corrupt package or failed assembler output reaches this
        // name. Existing files and symlinks are never overwritten.
        package.persist_noclobber(output).map_err(|e| e.error)?;
        Ok(report)
    }

    /// Return verified cache paths, not assembled models. Mirrors are directory
    /// URLs; peers expose the algorithm-qualified transport endpoint.
    pub async fn download_selected(&self) -> io::Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for record in self.assignment.selected.values() {
            let file = format!("{}.slice", record.blake3);
            let mirrors = self
                .mirrors
                .iter()
                .map(|base| {
                    let mut url = base.clone();
                    url.path_segments_mut()
                        .map_err(|_| invalid("mirror must be a hierarchical URL"))?
                        .pop_if_empty()
                        .push(&file);
                    Ok(url)
                })
                .collect::<io::Result<Vec<_>>>()?;
            paths.push(
                self.store
                    .download(
                        &format!("blake3-{}", record.blake3),
                        &SliceSources {
                            mirrors,
                            peers: self.peers.clone(),
                        },
                    )
                    .await?,
            );
        }
        Ok(paths)
    }

    /// Bind only with explicit consent. Shutdown/revocation cancels downloads
    /// and active peer streams; all work is owned by this foreground future.
    pub async fn run(
        &self,
        listen: SocketAddr,
        shutdown: impl Future<Output = ()>,
    ) -> io::Result<()> {
        let mut permit = self.store.0.consent.permit()?;
        let _revoke_on_cancel = RevokeOnDrop(self.consent());
        let result = permit
            .run(async {
                let listener = tokio::net::TcpListener::bind(listen).await?;
                let download = async {
                    self.download_selected().await?;
                    std::future::pending::<io::Result<()>>().await
                };
                tokio::select! {
                    result = download => result,
                    result = axum::serve(listener, self.router()) => result,
                    _ = shutdown => Ok(()),
                }
            })
            .await;
        self.consent().set(false);
        result
    }
}

fn copy_verified_cache(
    source: &Path,
    destination: &Path,
    record: &SliceRecord,
    permit: &Permit,
) -> io::Result<()> {
    use std::io::{Read, Write};
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let mut source = options.open(source)?;
    let metadata = source.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != record.bytes {
        return Err(invalid("cache object is not a complete regular slice"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(invalid("cache object has multiple links"));
        }
    }
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = vec![0; BLOCK];
    let mut remaining = record.bytes;
    while remaining != 0 {
        permit.check()?;
        let count = remaining.min(BLOCK as u64) as usize;
        source.read_exact(&mut buffer[..count])?;
        hash.update(&buffer[..count]);
        destination.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    if source.read(&mut [0])? != 0 || hash.finalize().as_bytes() != &decode_digest(&record.blake3)?
    {
        return Err(invalid("cache object failed full BLAKE3 verification"));
    }
    permit.check()
}

#[cfg(test)]
mod tests;
