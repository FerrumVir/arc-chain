//! Streaming SHA-256 helpers.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    sha256_file_with_progress(path, |_| {})
}

/// Hash a file in 1 MiB chunks, reporting the running byte count.
pub fn sha256_file_with_progress(path: &Path, mut progress: impl FnMut(u64)) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
        progress(total);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answers() {
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn file_hash_matches_bytes_hash() {
        let dir = crate::layout::test_support::TempDir::new("hashing");
        let path = dir.path().join("payload");
        let payload: Vec<u8> = (0..3_000_000_u32)
            .map(|value| (value % 251) as u8)
            .collect();
        std::fs::write(&path, &payload).unwrap();
        let mut last = 0;
        let digest = sha256_file_with_progress(&path, |total| last = total).unwrap();
        assert_eq!(digest, sha256_bytes(&payload));
        assert_eq!(last, payload.len() as u64);
    }
}
