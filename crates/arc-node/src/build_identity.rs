//! What this process is: the SHA-256 of its own executable and the cargo
//! features it was built with. A version string is shared by many builds; the
//! digest is what a build-provenance record (`scripts/arc-build-provenance.sh`)
//! names, so evidence from a running node can be tied to one exact build (R4).
//!
//! A node reports its own digest, so this catches the wrong binary running,
//! not a node that lies about itself.

use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// Behaviour-changing cargo features compiled into this binary.
pub fn features() -> Vec<&'static str> {
    let mut features = Vec::new();
    if cfg!(feature = "native-test-executor") {
        features.push("native-test-executor");
    }
    if cfg!(feature = "benchmark-tools") {
        features.push("benchmark-tools");
    }
    if cfg!(feature = "candle") {
        features.push("candle");
    }
    if cfg!(feature = "stwo-prover") {
        features.push("stwo-prover");
    }
    features
}

static DIGEST: OnceLock<Option<String>> = OnceLock::new();

/// Start hashing this process's executable, off the startup path.
///
/// The file is OPENED here, so the digest describes the bytes this process
/// was started from even if the path is replaced later by an upgrade. The
/// hashing itself runs on its own thread and logs the result: a debug binary
/// is about 190 MB and unoptimised SHA-256 takes roughly 14 s, which used to
/// sit between a node's first log line and everything else. That was long
/// enough for a restarted validator to fall outside its peers' DAG retention
/// window before it asked them for history, so it never rejoined.
pub fn begin() {
    let opened = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::File::open(path).ok());
    let Some(mut file) = opened else {
        let _ = DIGEST.set(None);
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("arc-build-identity".into())
        .spawn(move || {
            let mut hasher = Sha256::new();
            let digest = std::io::copy(&mut file, &mut hasher)
                .ok()
                .map(|_| hex::encode(hasher.finalize()));
            let readable = digest.is_some();
            let _ = DIGEST.set(digest);
            if readable {
                tracing::info!(
                    binary_sha256 = executable_sha256().unwrap_or("unreadable"),
                    features = ?features(),
                    "build identity"
                );
            } else {
                tracing::warn!("this node cannot read its own executable; it reports no digest");
            }
        });
    if spawned.is_err() {
        let _ = DIGEST.set(None);
    }
}

/// SHA-256 (hex) of the executable this process was started from, or `None`
/// while [`begin`] is still hashing it, or if it could not be read.
pub fn executable_sha256() -> Option<&'static str> {
    DIGEST.get().and_then(|digest| digest.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_is_the_running_executable_s_and_is_stable() {
        let path = std::env::current_exe().unwrap();
        let expected = hex::encode(Sha256::digest(std::fs::read(path).unwrap()));
        // Nothing is reported until the background hash finishes, and the
        // startup path never waits for it.
        assert_eq!(executable_sha256(), None, "not published before begin()");
        begin();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while executable_sha256().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(executable_sha256(), Some(expected.as_str()));
        assert_eq!(executable_sha256(), executable_sha256());
    }
}
