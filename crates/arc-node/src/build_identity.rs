use sha2::{Digest, Sha256};
use std::sync::OnceLock;

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
