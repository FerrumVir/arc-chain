//! Startup pins for the bytes decoded into a shared resident bundle.

use super::invalid;
use crate::cached_integer_model::GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE;
use crate::tensor_parallel::{MAX_CANONICAL_ROW_FILE_BYTES, RowAssignment, TensorKey};
use arc_crypto::Hash256;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};

const MAX_MANIFEST_BYTES: u64 = 1 << 20;

#[derive(Deserialize)]
pub(super) struct FilePin {
    pub file: String,
    pub assignment: RowAssignment,
    pub bytes: usize,
    pub blake3: Hash256,
}

#[derive(Deserialize)]
struct Manifest {
    format: String,
    artifact_blake3: Hash256,
    execution_profile: String,
    worker_id: String,
    serialized_row_bytes: usize,
    files: Vec<FilePin>,
    row_partition: Option<Partition>,
}
#[derive(Deserialize)]
struct Partition {
    rank: u64,
    count: u64,
}

pub(super) struct PinnedManifest {
    pub hash: Hash256,
    pub files: BTreeMap<String, FilePin>,
}

pub(super) fn regular_file(path: &Path) -> io::Result<std::fs::File> {
    // NOFOLLOW rejects a replaced final symlink; NONBLOCK prevents a raced
    // FIFO from hanging startup. Read and hash this very descriptor only.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("row input is not a regular file"));
    }
    Ok(file)
}

impl PinnedManifest {
    pub fn read(path: &Path, hash: Hash256, artifact: Hash256) -> io::Result<Self> {
        if hash == Hash256::ZERO
            || artifact == Hash256::ZERO
            || !path.is_absolute()
            || path.components().any(|c| matches!(c, Component::ParentDir))
        {
            return Err(invalid("invalid manifest pin or path"));
        }
        let file = regular_file(path)?;
        if file.metadata()?.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("row manifest exceeds size bound"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES
            || Hash256(*blake3::hash(&bytes).as_bytes()) != hash
        {
            return Err(invalid("row manifest does not match pin"));
        }
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
        let partitioned = match manifest.format.as_str() {
            "arc.tensor-row-low-residency-bundle.v1" => false,
            "arc.tensor-row-offline-partition-bundle.v1" => true,
            _ => return Err(invalid("unsupported row manifest format")),
        };
        if partitioned
            && !manifest
                .row_partition
                .as_ref()
                .is_some_and(|p| p.count > 0 && p.count <= 32 && p.rank < p.count)
        {
            return Err(invalid("invalid manifest partition bounds"));
        }
        if manifest.artifact_blake3 != artifact
            || manifest.execution_profile != GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE
            || manifest.worker_id.is_empty()
            || manifest.worker_id.len() > 256
            || manifest.files.is_empty()
            || manifest.files.len() > super::MAX_FILES
        {
            return Err(invalid("row manifest identity or file count"));
        }
        let mut files = BTreeMap::new();
        let mut total = 0usize;
        for pin in manifest.files {
            let path = Path::new(&pin.file);
            let a = &pin.assignment;
            if pin.file.is_empty()
                || path.components().count() != 1
                || !matches!(path.components().next(), Some(Component::Normal(_)))
                || pin.blake3 == Hash256::ZERO
                || a.artifact_id != artifact
                || a.execution_profile != manifest.execution_profile
                || a.worker_id != manifest.worker_id
                || a.row_start >= a.row_end
                || (a.tensor == TensorKey::LmHead) != a.layer.is_none()
                || pin.bytes == 0
            {
                return Err(invalid("row manifest file path, identity or bounds"));
            }
            total = total
                .checked_add(pin.bytes)
                .ok_or_else(|| invalid("row size overflow"))?;
            if total > MAX_CANONICAL_ROW_FILE_BYTES || files.insert(pin.file.clone(), pin).is_some()
            {
                return Err(invalid("row manifest aggregate bound or duplicate file"));
            }
        }
        if total != manifest.serialized_row_bytes {
            return Err(invalid("row manifest aggregate differs from files"));
        }
        Ok(Self { hash, files })
    }
}

pub(super) struct DigestReader<R> {
    pub reader: R,
    pub digest: blake3::Hasher,
    pub enabled: bool,
}
impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.reader.read(buf)?;
        if self.enabled {
            self.digest.update(&buf[..count]);
        }
        Ok(count)
    }
}
