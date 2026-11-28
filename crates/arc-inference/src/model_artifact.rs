use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use arc_crypto::Hash256;
use thiserror::Error;

use crate::InferenceError;

const HASH_BUFFER_BYTES: usize = 1024 * 1024;
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

#[derive(Error, Debug)]
pub enum ModelArtifactLoadError {
    #[error("model artifact hashing cancelled")]
    Cancelled,

    #[error(transparent)]
    Inference(#[from] InferenceError),
}

impl ModelArtifactLoadError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArtifactCommitment {
    path: PathBuf,
    model_id: Hash256,
    size_bytes: u64,
}

impl ModelArtifactCommitment {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, InferenceError> {
        match Self::from_path_cancellable(path, &NEVER_CANCELLED) {
            Ok(commitment) => Ok(commitment),
            Err(ModelArtifactLoadError::Inference(error)) => Err(error),
            Err(ModelArtifactLoadError::Cancelled) => Err(InferenceError::Runtime(
                "model artifact hashing cancelled".to_string(),
            )),
        }
    }

    pub fn from_path_cancellable(
        path: impl AsRef<Path>,
        cancel: &AtomicBool,
    ) -> Result<Self, ModelArtifactLoadError> {
        if cancel.load(Ordering::Acquire) {
            return Err(ModelArtifactLoadError::Cancelled);
        }
        let path = path.as_ref();
        let file = File::open(path).map_err(|error| {
            ModelArtifactLoadError::Inference(InferenceError::Runtime(format!(
                "failed to open model artifact {}: {error}",
                path.display()
            )))
        })?;
        let (model_id, size_bytes) = hash_reader_cancellable(file, cancel).map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted && cancel.load(Ordering::Acquire) {
                ModelArtifactLoadError::Cancelled
            } else {
                ModelArtifactLoadError::Inference(InferenceError::Runtime(format!(
                    "failed to hash model artifact {}: {error}",
                    path.display()
                )))
            }
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            model_id,
            size_bytes,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn model_id(&self) -> Hash256 {
        self.model_id
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub fn verify_unchanged(&self) -> Result<(), InferenceError> {
        match self.verify_unchanged_cancellable(&NEVER_CANCELLED) {
            Ok(()) => Ok(()),
            Err(ModelArtifactLoadError::Inference(error)) => Err(error),
            Err(ModelArtifactLoadError::Cancelled) => Err(InferenceError::Runtime(
                "model artifact verification cancelled".to_string(),
            )),
        }
    }

    pub fn verify_unchanged_cancellable(
        &self,
        cancel: &AtomicBool,
    ) -> Result<(), ModelArtifactLoadError> {
        let current = Self::from_path_cancellable(&self.path, cancel)?;
        if current.model_id != self.model_id || current.size_bytes != self.size_bytes {
            return Err(ModelArtifactLoadError::Inference(InferenceError::Runtime(
                format!(
                    "model artifact {} changed after its identity was committed",
                    self.path.display()
                ),
            )));
        }
        Ok(())
    }
}

fn hash_reader_cancellable(
    mut reader: impl Read,
    cancel: &AtomicBool,
) -> io::Result<(Hash256, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    let mut size_bytes = 0u64;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "model artifact hashing cancelled",
            ));
        }
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "model artifact hashing cancelled",
            ));
        }
        hasher.update(&buffer[..read]);
        size_bytes = size_bytes
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("model artifact size overflow"))?;
    }
    Ok((Hash256(*hasher.finalize().as_bytes()), size_bytes))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor, Read};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::{
        HASH_BUFFER_BYTES, ModelArtifactCommitment, ModelArtifactLoadError, NEVER_CANCELLED,
        hash_reader_cancellable,
    };

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(label: &str) -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "arc-model-artifact-{}-{label}-{seq}.bin",
                std::process::id()
            )))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    struct CancelAfterFirstChunk<'a> {
        inner: Cursor<&'a [u8]>,
        cancel: &'a AtomicBool,
        chunks_read: usize,
    }

    impl Read for CancelAfterFirstChunk<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buf)?;
            if read > 0 {
                self.chunks_read += 1;
                if self.chunks_read == 1 {
                    self.cancel.store(true, Ordering::Release);
                }
            }
            Ok(read)
        }
    }

    #[test]
    fn same_shape_with_mutated_weight_bytes_has_a_different_model_id() {
        let shape_header = b"GGUF:layers=32,width=4096,heads=32,vocab=32000;weights=";
        let mut model_a = shape_header.to_vec();
        model_a.resize(shape_header.len() + 3 * HASH_BUFFER_BYTES, 7u8);
        let mut model_b = model_a.clone();
        model_b[shape_header.len() + HASH_BUFFER_BYTES + 17] ^= 1;

        let (id_a, size_a) =
            hash_reader_cancellable(Cursor::new(model_a.as_slice()), &NEVER_CANCELLED)
                .expect("hash model A");
        let (id_b, size_b) =
            hash_reader_cancellable(Cursor::new(model_b.as_slice()), &NEVER_CANCELLED)
                .expect("hash model B");

        assert_eq!(size_a, size_b);
        assert_ne!(id_a, id_b);
        assert_eq!(id_a.0, *blake3::hash(&model_a).as_bytes());
        assert_eq!(id_b.0, *blake3::hash(&model_b).as_bytes());
    }

    #[test]
    fn identical_artifact_bytes_have_one_identity_for_every_shard() {
        let artifact = b"GGUF:layers=4;weights=the exact shared artifact bytes";
        let shard_ids: Vec<_> = (0..4)
            .map(|_| {
                hash_reader_cancellable(Cursor::new(artifact), &NEVER_CANCELLED)
                    .expect("hash shard artifact")
                    .0
            })
            .collect();

        assert!(shard_ids.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn cancellable_hashing_matches_ordinary_hash_and_aborts_mid_stream() {
        let payload = vec![0x5Au8; 3 * HASH_BUFFER_BYTES + 19];
        let file = TempFile::new("cancellable");
        std::fs::write(&file.0, &payload).unwrap();

        let ordinary = ModelArtifactCommitment::from_path(&file.0).unwrap();
        let uncancelled = AtomicBool::new(false);
        let cancellable =
            ModelArtifactCommitment::from_path_cancellable(&file.0, &uncancelled).unwrap();
        assert_eq!(cancellable, ordinary);
        cancellable
            .verify_unchanged_cancellable(&uncancelled)
            .unwrap();

        let preset_cancel = AtomicBool::new(true);
        let missing = file.0.with_extension("missing");
        let preset_err =
            ModelArtifactCommitment::from_path_cancellable(&missing, &preset_cancel).unwrap_err();
        assert!(preset_err.is_cancelled());
        let verify_cancelled = cancellable
            .verify_unchanged_cancellable(&preset_cancel)
            .unwrap_err();
        assert!(verify_cancelled.is_cancelled());

        let mid_cancel = AtomicBool::new(false);
        let mut reader = CancelAfterFirstChunk {
            inner: Cursor::new(payload.as_slice()),
            cancel: &mid_cancel,
            chunks_read: 0,
        };
        let interrupted = hash_reader_cancellable(&mut reader, &mid_cancel).unwrap_err();
        assert_eq!(interrupted.kind(), io::ErrorKind::Interrupted);
        assert_eq!(reader.chunks_read, 1);

        let mut mutated = payload;
        mutated[HASH_BUFFER_BYTES + 7] ^= 0x01;
        std::fs::write(&file.0, &mutated).unwrap();
        let changed = cancellable
            .verify_unchanged_cancellable(&uncancelled)
            .unwrap_err();
        assert!(matches!(changed, ModelArtifactLoadError::Inference(_)));
    }
}
