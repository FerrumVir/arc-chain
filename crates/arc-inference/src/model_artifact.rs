//! Exact, content-addressed identity for inference model artifacts.
//!
//! Model dimensions are metadata, not identity: two artifacts can have the
//! same layer/width/vocabulary shape and completely different weights.  ARC
//! therefore identifies a model by streaming every byte of its source
//! artifact through BLAKE3.  The bounded buffer keeps memory usage constant
//! even for multi-gigabyte GGUF files.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use arc_crypto::Hash256;

use crate::InferenceError;

const HASH_BUFFER_BYTES: usize = 1024 * 1024;

/// A BLAKE3 commitment to the exact bytes of one source model artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArtifactCommitment {
    path: PathBuf,
    model_id: Hash256,
    size_bytes: u64,
}

impl ModelArtifactCommitment {
    /// Stream the complete artifact at `path` into BLAKE3.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, InferenceError> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|error| {
            InferenceError::Runtime(format!(
                "failed to open model artifact {}: {error}",
                path.display()
            ))
        })?;
        let (model_id, size_bytes) = hash_reader(file).map_err(|error| {
            InferenceError::Runtime(format!(
                "failed to hash model artifact {}: {error}",
                path.display()
            ))
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

    /// Re-read the artifact and fail if its bytes changed after commitment.
    /// Model loading is a startup-only operation, so correctness is worth the
    /// additional bounded-memory pass over the file.
    pub fn verify_unchanged(&self) -> Result<(), InferenceError> {
        let current = Self::from_path(&self.path)?;
        if current.model_id != self.model_id || current.size_bytes != self.size_bytes {
            return Err(InferenceError::Runtime(format!(
                "model artifact {} changed after its identity was committed",
                self.path.display()
            )));
        }
        Ok(())
    }
}

fn hash_reader(mut reader: impl Read) -> io::Result<(Hash256, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    let mut size_bytes = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
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
    use std::io::Cursor;

    use super::{HASH_BUFFER_BYTES, hash_reader};

    #[test]
    fn same_shape_with_mutated_weight_bytes_has_a_different_model_id() {
        let shape_header = b"GGUF:layers=32,width=4096,heads=32,vocab=32000;weights=";
        let mut model_a = shape_header.to_vec();
        // Larger than the legacy first-1MiB + last-1MiB sampler. Mutate only
        // the unsampled middle so this test fails if full streaming ever
        // regresses back to endpoint sampling.
        model_a.resize(shape_header.len() + 3 * HASH_BUFFER_BYTES, 7u8);
        let mut model_b = model_a.clone();
        model_b[shape_header.len() + HASH_BUFFER_BYTES + 17] ^= 1;

        let (id_a, size_a) = hash_reader(Cursor::new(model_a.as_slice())).expect("hash model A");
        let (id_b, size_b) = hash_reader(Cursor::new(model_b.as_slice())).expect("hash model B");

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
                hash_reader(Cursor::new(artifact))
                    .expect("hash shard artifact")
                    .0
            })
            .collect();

        assert!(shard_ids.windows(2).all(|pair| pair[0] == pair[1]));
    }
}
