//! Resumable, size-capped, digest-checked HTTPS downloads.
//!
//! A cached file is reused only if its size and SHA-256 equal the pin; a
//! mismatching cache entry is deleted and fetched again. Downloads land in a
//! `.partial` sibling, resume with a byte range after an interruption, and
//! are renamed into place only after the full pinned digest matches.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::exit;
use crate::hashing::sha256_file;
use crate::logging::Log;

pub const USER_AGENT: &str = concat!("arc-legacy-bridge/", env!("CARGO_PKG_VERSION"));
const ATTEMPTS: u32 = 3;

pub struct Fetcher {
    agent: ureq::Agent,
    base: String,
    retry_pause: Duration,
}

impl Fetcher {
    /// Assets of one exact GitHub release tag, HTTPS only.
    pub fn github_release(repository: &str, tag: &str) -> Fetcher {
        Fetcher::with_base(
            format!("https://github.com/{repository}/releases/download/{tag}"),
            true,
        )
    }

    /// Files under one fixed HTTPS URL prefix.
    pub fn https_prefix(base: &str) -> Fetcher {
        Fetcher::with_base(base.trim_end_matches('/').to_string(), true)
    }

    fn with_base(base: String, https_only: bool) -> Fetcher {
        let agent = ureq::AgentBuilder::new()
            .https_only(https_only)
            .redirects(8)
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(60))
            .user_agent(USER_AGENT)
            .build();
        Fetcher {
            agent,
            base,
            retry_pause: Duration::from_secs(2),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_local_test_server(base: String) -> Fetcher {
        let mut fetcher = Fetcher::with_base(base, false);
        fetcher.retry_pause = Duration::from_millis(10);
        fetcher
    }

    pub fn url(&self, name: &str) -> String {
        format!("{}/{name}", self.base)
    }

    /// Make `dest` hold exactly the pinned bytes. Returns true if it downloaded.
    pub fn ensure_file(
        &self,
        name: &str,
        sha256: &str,
        size: u64,
        dest: &Path,
        log: &mut Log,
    ) -> Result<bool> {
        if fs::symlink_metadata(dest).is_ok() {
            if file_matches(dest, sha256, size)? {
                return Ok(false);
            }
            log.warn(&format!(
                "cached {name} does not match its pinned digest; fetching it again"
            ));
            fs::remove_file(dest)
                .with_context(|| format!("cannot remove mismatching {}", dest.display()))?;
        }
        let partial = partial_path(dest)?;
        let mut last_error = anyhow!("no download attempt was made");
        for attempt in 1..=ATTEMPTS {
            match self.download_once(name, size, &partial) {
                Ok(()) => {
                    if file_matches(&partial, sha256, size)? {
                        fs::rename(&partial, dest).with_context(|| {
                            format!("cannot move {} into place", dest.display())
                        })?;
                        return Ok(true);
                    }
                    let _ = fs::remove_file(&partial);
                    last_error = anyhow!("downloaded {name} does not match its pinned SHA-256");
                    log.warn(&format!(
                        "downloaded {name} does not match its pinned SHA-256 (attempt {attempt}/{ATTEMPTS}); discarded"
                    ));
                }
                Err(error) => {
                    log.warn(&format!(
                        "download of {name} failed (attempt {attempt}/{ATTEMPTS}): {error:#}"
                    ));
                    last_error = error;
                }
            }
            if attempt < ATTEMPTS {
                std::thread::sleep(self.retry_pause * attempt);
            }
        }
        Err(exit::unavailable(format!(
            "could not fetch the pinned {name} from {}: {last_error:#}",
            self.url(name)
        )))
    }

    fn download_once(&self, name: &str, size: u64, partial: &Path) -> Result<()> {
        let offset = match fs::symlink_metadata(partial) {
            Ok(metadata) if metadata.is_file() && metadata.len() <= size => metadata.len(),
            Ok(_) => {
                fs::remove_file(partial)
                    .with_context(|| format!("cannot reset {}", partial.display()))?;
                0
            }
            Err(_) => 0,
        };
        if offset == size {
            return Ok(());
        }
        let mut request = self
            .agent
            .get(&self.url(name))
            .set("Accept-Encoding", "identity");
        if offset > 0 {
            request = request.set("Range", &format!("bytes={offset}-"));
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(416, _)) => {
                let _ = fs::remove_file(partial);
                bail!("the server rejected resuming {name}; the next attempt starts over");
            }
            Err(ureq::Error::Status(status, _)) => bail!("GET {name} returned HTTP {status}"),
            Err(error) => bail!("GET {name} failed: {error}"),
        };
        let status = response.status();
        let (mut file, mut written) = if status == 206 && offset > 0 {
            let expected = format!("bytes {offset}-");
            let range_ok = response
                .header("Content-Range")
                .is_some_and(|value| value.starts_with(&expected));
            if !range_ok {
                let _ = fs::remove_file(partial);
                bail!("GET {name} returned an unexpected byte range");
            }
            let file = OpenOptions::new()
                .append(true)
                .open(partial)
                .with_context(|| format!("cannot append to {}", partial.display()))?;
            (file, offset)
        } else if status == 200 {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(partial)
                .with_context(|| format!("cannot write {}", partial.display()))?;
            (file, 0)
        } else {
            bail!("GET {name} returned HTTP {status}");
        };

        // Read at most one byte past the pin so an oversized body is caught.
        let mut reader = response.into_reader().take(size - written + 1);
        let mut buffer = vec![0_u8; 256 * 1024];
        loop {
            let read = reader
                .read(&mut buffer)
                .with_context(|| format!("reading {name} was interrupted"))?;
            if read == 0 {
                break;
            }
            written += read as u64;
            if written > size {
                drop(file);
                let _ = fs::remove_file(partial);
                bail!("{name} is larger than its pinned size");
            }
            file.write_all(&buffer[..read])
                .with_context(|| format!("cannot write {}", partial.display()))?;
        }
        file.sync_all()
            .with_context(|| format!("cannot flush {}", partial.display()))?;
        if written != size {
            bail!("{name} ended after {written} of {size} bytes; the next attempt resumes");
        }
        Ok(())
    }
}

/// True when `path` is a regular file of exactly `size` bytes and `sha256`.
pub fn file_matches(path: &Path, sha256: &str, size: u64) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };
    if !metadata.is_file() || metadata.len() != size {
        return Ok(false);
    }
    let digest = sha256_file(path).with_context(|| format!("cannot hash {}", path.display()))?;
    Ok(digest == sha256)
}

fn partial_path(dest: &Path) -> Result<PathBuf> {
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("{} has no file name", dest.display()))?;
    Ok(dest.with_file_name(format!("{name}.partial")))
}

#[cfg(test)]
pub(crate) mod test_server {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A tiny HTTP/1.1 file server for download tests. `cut_after` makes the
    /// first full-body response stop early, simulating a dropped connection.
    pub struct TestServer {
        pub base: String,
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    pub fn serve(files: BTreeMap<String, Vec<u8>>, cut_after: Option<usize>) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            let mut cut = cut_after;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut range_start: Option<usize> = None;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                    let lower = header.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("range: bytes=") {
                        range_start = value.trim().trim_end_matches('-').parse().ok();
                    }
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/')
                    .to_string();
                seen.lock()
                    .unwrap()
                    .push(format!("{path} range={range_start:?}"));
                let Some(body) = files.get(&path) else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                };
                let (status, start) = match range_start {
                    Some(start) if start < body.len() => ("206 Partial Content", start),
                    _ => ("200 OK", 0),
                };
                let mut slice = &body[start..];
                let mut header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    slice.len()
                );
                if start > 0 {
                    header.push_str(&format!(
                        "Content-Range: bytes {start}-{}/{}\r\n",
                        body.len() - 1,
                        body.len()
                    ));
                }
                header.push_str("\r\n");
                let _ = stream.write_all(header.as_bytes());
                if let Some(limit) = cut.take() {
                    slice = &slice[..limit.min(slice.len())];
                }
                let _ = stream.write_all(slice);
            }
        });
        TestServer { base, requests }
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::serve;
    use super::*;
    use crate::hashing::sha256_bytes;
    use crate::layout::test_support::TempDir;
    use std::collections::BTreeMap;

    fn payload() -> Vec<u8> {
        (0..700_000_u32).map(|value| (value % 253) as u8).collect()
    }

    fn files(body: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        files.insert("asset".to_string(), body.to_vec());
        files
    }

    #[test]
    fn downloads_verifies_and_then_reuses_the_cache() {
        let body = payload();
        let server = serve(files(&body), None);
        let fetcher = Fetcher::for_local_test_server(server.base.clone());
        let temp = TempDir::new("fetch-cache");
        let dest = temp.path().join("asset");
        let mut log = Log::stderr_only();
        let digest = sha256_bytes(&body);
        assert!(
            fetcher
                .ensure_file("asset", &digest, body.len() as u64, &dest, &mut log)
                .unwrap()
        );
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(
            !fetcher
                .ensure_file("asset", &digest, body.len() as u64, &dest, &mut log)
                .unwrap()
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_interrupted_download_resumes_with_a_range() {
        let body = payload();
        let server = serve(files(&body), Some(123_457));
        let fetcher = Fetcher::for_local_test_server(server.base.clone());
        let temp = TempDir::new("fetch-resume");
        let dest = temp.path().join("asset");
        let mut log = Log::stderr_only();
        fetcher
            .ensure_file(
                "asset",
                &sha256_bytes(&body),
                body.len() as u64,
                &dest,
                &mut log,
            )
            .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        let requests = server.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests[1].contains("range=Some(123457)"), "{requests:?}");
    }

    #[test]
    fn a_tampered_cache_is_replaced_and_a_wrong_payload_is_rejected() {
        let body = payload();
        let server = serve(files(&body), None);
        let fetcher = Fetcher::for_local_test_server(server.base.clone());
        let temp = TempDir::new("fetch-tamper");
        let dest = temp.path().join("asset");
        let mut log = Log::stderr_only();
        let mut tampered = body.clone();
        tampered[10] ^= 1;
        fs::write(&dest, &tampered).unwrap();
        fetcher
            .ensure_file(
                "asset",
                &sha256_bytes(&body),
                body.len() as u64,
                &dest,
                &mut log,
            )
            .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);

        let other = temp.path().join("other");
        let error = fetcher
            .ensure_file(
                "asset",
                &"0".repeat(64),
                body.len() as u64,
                &other,
                &mut log,
            )
            .unwrap_err();
        assert_eq!(exit::code_for(&error), exit::EX_UNAVAILABLE);
        assert!(!other.exists());
        assert!(!temp.path().join("other.partial").exists());
    }

    #[test]
    fn an_oversized_or_missing_asset_is_rejected() {
        let body = payload();
        let server = serve(files(&body), None);
        let fetcher = Fetcher::for_local_test_server(server.base.clone());
        let temp = TempDir::new("fetch-oversize");
        let mut log = Log::stderr_only();
        let short = temp.path().join("short");
        assert!(
            fetcher
                .ensure_file(
                    "asset",
                    &sha256_bytes(&body[..1000]),
                    1000,
                    &short,
                    &mut log
                )
                .is_err()
        );
        assert!(!short.exists());
        let missing = temp.path().join("missing");
        assert!(
            fetcher
                .ensure_file("absent", &sha256_bytes(b""), 1, &missing, &mut log)
                .is_err()
        );
    }
}
