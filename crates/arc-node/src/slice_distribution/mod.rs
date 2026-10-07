//! Opt-in, content-addressed slice transfers. No listener or download starts
//! implicitly. The assignment layer supplies descriptors from an authenticated,
//! pinned manifest and explicit mirror/peer URLs; transport cannot assign work.
//!
//! One store owns a private cache directory (do not share it between processes).
//! Only verified objects are returned or served. `.part` files are untrusted
//! resume state, never model inputs. Consumers must keep the cache private.
//! The ENG-10 adapter and explicit local worker live in [`manifest`].

pub mod manifest;

use crate::config::SliceDistributionConfig;
use futures_util::stream;
use reqwest::{Client, StatusCode, Url, header};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore, watch};
use tokio::time::{Instant, sleep_until, timeout};

const BLOCK: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// An algorithm-qualified identity. The manifest chooses the algorithm;
/// filenames never contain a manifest-controlled path component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SliceDigest {
    Blake3([u8; 32]),
    Sha256([u8; 32]),
}

impl SliceDigest {
    pub fn key(&self) -> String {
        match self {
            Self::Blake3(hash) => format!("blake3-{}", hex::encode(hash)),
            Self::Sha256(hash) => format!("sha256-{}", hex::encode(hash)),
        }
    }
}

/// Trusted size and digest extracted by the caller from its pinned manifest.
/// This is a transport descriptor, not a replacement manifest schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceSpec {
    pub digest: SliceDigest,
    pub bytes: u64,
}

/// Mirrors name the entire object. Peer URLs are origins/base paths; the
/// content-addressed `/slices/<algorithm>-<hex>` endpoint is appended.
#[derive(Default)]
pub struct SliceSources {
    pub mirrors: Vec<Url>,
    pub peers: Vec<Url>,
}

/// Revocation interrupts pending requests, throttling and streamed bodies.
/// A transfer interrupted by revocation does not revive on a later opt-in.
#[derive(Clone)]
pub struct SliceConsent(watch::Sender<bool>);

impl SliceConsent {
    pub fn set(&self, enabled: bool) {
        self.0.send_if_modified(|current| {
            if *current == enabled {
                return false;
            }
            *current = enabled;
            true
        });
    }

    fn permit(&self) -> io::Result<Permit> {
        let mut receiver = self.0.subscribe();
        if !*receiver.borrow_and_update() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "slice hosting requires explicit opt-in",
            ));
        }
        Ok(Permit(receiver, false))
    }
}

struct Permit(watch::Receiver<bool>, bool);

impl Permit {
    fn check(&self) -> io::Result<()> {
        // Any change invalidates this operation, including off then on.
        if self.1 || self.0.has_changed().unwrap_or(true) || !*self.0.borrow() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "slice consent changed",
            ));
        }
        Ok(())
    }

    async fn run<T>(&mut self, future: impl Future<Output = io::Result<T>>) -> io::Result<T> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.0.changed() => {
                self.1 = true;
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "slice consent changed"))
            },
            result = future => result,
        }
    }
}

struct Bandwidth {
    rate: u64,
    gate: Mutex<()>,
}

impl Bandwidth {
    fn new(rate: u64) -> Self {
        Self {
            rate,
            gate: Mutex::new(()),
        }
    }

    fn enabled(&self) -> io::Result<()> {
        if self.rate == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "slice bandwidth is paused",
            ));
        }
        Ok(())
    }

    async fn charge(&self, bytes: usize, permit: &mut Permit) -> io::Result<()> {
        self.enabled()?;
        // Integer ceiling avoids zero-duration transfers at high limits.
        let nanos = ((bytes as u128 * 1_000_000_000).div_ceil(self.rate as u128)) as u64;
        // Serialize charges across streams. Holding this asynchronous guard
        // through the wait means cancellation drops the reservation instead
        // of leaving later transfers stuck behind a revoked slow transfer.
        let _guard = permit.run(async { Ok(self.gate.lock().await) }).await?;
        let deadline = Instant::now() + Duration::from_nanos(nanos);
        permit
            .run(async {
                sleep_until(deadline).await;
                Ok(())
            })
            .await
    }
}

struct Inner {
    root: PathBuf,
    assigned: BTreeMap<String, SliceSpec>,
    consent: SliceConsent,
    download: Bandwidth,
    upload: Bandwidth,
    downloading: Mutex<()>,
    serving: Arc<Semaphore>,
    client: Client,
}

#[derive(Clone)]
pub struct SliceStore(Arc<Inner>);

impl SliceStore {
    /// Construction is inert: no filesystem or network access. Assignment
    /// changes require a new store; never accept descriptors from a peer.
    pub fn new(
        root: PathBuf,
        assigned: Vec<SliceSpec>,
        config: &SliceDistributionConfig,
    ) -> io::Result<Self> {
        if !(1..=64).contains(&config.max_concurrent_serves) {
            return Err(invalid("max_concurrent_serves must be between 1 and 64"));
        }
        let mut pins = BTreeMap::new();
        for spec in assigned {
            if spec.bytes == 0 || spec.bytes > config.max_slice_bytes {
                return Err(invalid("slice size exceeds configured bound"));
            }
            if let Some(old) = pins.insert(spec.digest.key(), spec.clone())
                && old != spec
            {
                return Err(invalid("conflicting slice sizes"));
            }
        }
        let (sender, _) = watch::channel(config.host_slices == Some(true));
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(IO_TIMEOUT)
            .no_proxy()
            .build()
            .map_err(io::Error::other)?;
        Ok(Self(Arc::new(Inner {
            root,
            assigned: pins,
            consent: SliceConsent(sender),
            download: Bandwidth::new(config.download_bytes_per_second),
            upload: Bandwidth::new(config.upload_bytes_per_second),
            downloading: Mutex::new(()),
            serving: Arc::new(Semaphore::new(config.max_concurrent_serves)),
            client,
        })))
    }

    pub fn consent(&self) -> SliceConsent {
        self.0.consent.clone()
    }

    fn spec(&self, key: &str) -> io::Result<&SliceSpec> {
        self.0
            .assigned
            .get(key)
            .ok_or_else(|| invalid("slice is not assigned to this node"))
    }

    /// Try mirrors in order, then peers. An interrupted body leaves only
    /// untrusted resume state; a size/hash failure discards that state.
    /// Retry a failed call to resume even after restarting the process.
    pub async fn download(&self, key: &str, sources: &SliceSources) -> io::Result<PathBuf> {
        let mut permit = self.0.consent.permit()?;
        self.0.download.enabled()?;
        let spec = self.spec(key)?;
        let _guard = permit
            .run(async { Ok(self.0.downloading.lock().await) })
            .await?;
        permit.check()?;
        fs::create_dir_all(&self.0.root).await?;
        if fs::symlink_metadata(&self.0.root)
            .await?
            .file_type()
            .is_symlink()
        {
            return Err(invalid("slice cache must not be a symlink"));
        }
        let final_path = self.0.root.join(key);
        match regular(&final_path, false).await {
            Ok(mut file) => {
                if verify(&mut file, spec, &mut permit).await? {
                    return Ok(final_path);
                }
                fs::remove_file(&final_path).await?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let part_path = self.0.root.join(format!("{key}.part"));
        let mut part = regular(&part_path, true).await?;
        if part.metadata().await?.len() >= spec.bytes {
            if verify(&mut part, spec, &mut permit).await? {
                return self.publish(part, &part_path, final_path, &permit).await;
            }
            part.set_len(0).await?;
        }
        let mut urls = sources.mirrors.clone();
        for peer in &sources.peers {
            let mut url = peer.clone();
            let path = format!("{}/slices/{key}", peer.path().trim_end_matches('/'));
            url.set_path(&path);
            url.set_query(None);
            urls.push(url);
        }
        let mut last = invalid("no slice sources available");
        for url in urls {
            permit.check()?;
            let resumed = part.metadata().await?.len() > 0;
            // A corrupt prefix may predate this source. Give it one clean
            // attempt before attributing a digest mismatch to the source.
            for attempt in 0..=usize::from(resumed) {
                match self.fetch(&url, &mut part, spec, &mut permit).await {
                    Ok(()) => {
                        if verify(&mut part, spec, &mut permit).await? {
                            return self.publish(part, &part_path, final_path, &permit).await;
                        }
                        part.set_len(0).await?;
                        last = invalid("slice digest mismatch");
                        if resumed && attempt == 0 {
                            continue;
                        }
                    }
                    Err(e) => {
                        part.flush().await?;
                        permit.check()?;
                        // Protocol violations are not useful resume prefixes.
                        if e.kind() == io::ErrorKind::InvalidData {
                            part.set_len(0).await?;
                        }
                        last = e;
                    }
                }
                break;
            }
        }
        Err(last)
    }

    async fn publish(
        &self,
        file: File,
        part: &Path,
        final_path: PathBuf,
        permit: &Permit,
    ) -> io::Result<PathBuf> {
        file.sync_all().await?;
        drop(file);
        permit.check()?;
        fs::rename(part, &final_path).await?;
        // Revocation during rename must still deny consumption.
        permit.check()?;
        Ok(final_path)
    }

    async fn fetch(
        &self,
        url: &Url,
        part: &mut File,
        spec: &SliceSpec,
        permit: &mut Permit,
    ) -> io::Result<()> {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid("unsupported slice source URL"));
        }
        let mut offset = part.metadata().await?.len();
        let request = self
            .0
            .client
            .get(url.clone())
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::RANGE, format!("bytes={offset}-"));
        let mut response = permit
            .run(async {
                timeout(IO_TIMEOUT, request.send())
                    .await
                    .map_err(io::Error::other)?
                    .map_err(io::Error::other)
            })
            .await?;
        if response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return Err(invalid("encoded slice response"));
        }
        match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                let expected = format!("bytes {offset}-{}/{}", spec.bytes - 1, spec.bytes);
                if response
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    != Some(expected.as_str())
                {
                    return Err(invalid("invalid Content-Range"));
                }
            }
            StatusCode::OK => {
                // A mirror may ignore Range. Restart, never append its body.
                offset = 0;
                part.set_len(0).await?;
            }
            _ => {
                return Err(io::Error::other(format!(
                    "slice source returned {}",
                    response.status()
                )));
            }
        }
        if response
            .content_length()
            .is_some_and(|n| n != spec.bytes - offset)
        {
            return Err(invalid("slice response length mismatch"));
        }
        part.seek(io::SeekFrom::Start(offset)).await?;
        while let Some(chunk) = permit
            .run(async {
                timeout(IO_TIMEOUT, response.chunk())
                    .await
                    .map_err(io::Error::other)?
                    .map_err(io::Error::other)
            })
            .await?
        {
            if chunk.len() as u64 > spec.bytes - offset {
                return Err(invalid("oversized slice response"));
            }
            for block in chunk.chunks(BLOCK) {
                self.0.download.charge(block.len(), permit).await?;
                permit.check()?;
                part.write_all(block).await?;
                offset += block.len() as u64;
            }
        }
        part.flush().await?;
        if offset != spec.bytes {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete slice",
            ));
        }
        Ok(())
    }

    /// An explicitly mounted HTTP peer route. Only assigned, complete, hashed
    /// objects are exposed, and consent/bandwidth apply to every body chunk.
    pub fn router(&self) -> axum::Router {
        axum::Router::new()
            .route("/slices/{key}", axum::routing::get(serve))
            .with_state(self.clone())
    }
}

async fn regular(path: &Path, write: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .create(write)
        .truncate(false);
    #[cfg(unix)]
    options
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .mode(0o600);
    #[cfg(windows)]
    options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid("slice cache entry is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(invalid("slice cache entry has multiple links"));
        }
    }
    Ok(file)
}

async fn verify(file: &mut File, spec: &SliceSpec, permit: &mut Permit) -> io::Result<bool> {
    if file.metadata().await?.len() != spec.bytes {
        return Ok(false);
    }
    file.seek(io::SeekFrom::Start(0)).await?;
    let mut blake = blake3::Hasher::new();
    let mut sha = Sha256::new();
    let mut buffer = vec![0; BLOCK];
    let mut remaining = spec.bytes;
    while remaining > 0 {
        let count = remaining.min(BLOCK as u64) as usize;
        permit.run(file.read_exact(&mut buffer[..count])).await?;
        match spec.digest {
            SliceDigest::Blake3(_) => {
                blake.update(&buffer[..count]);
            }
            SliceDigest::Sha256(_) => sha.update(&buffer[..count]),
        }
        remaining -= count as u64;
    }
    permit.check()?;
    Ok(match spec.digest {
        SliceDigest::Blake3(hash) => *blake.finalize().as_bytes() == hash,
        SliceDigest::Sha256(hash) => <[u8; 32]>::from(sha.finalize()) == hash,
    })
}

async fn serve(
    axum::extract::State(store): axum::extract::State<SliceStore>,
    axum::extract::Path(key): axum::extract::Path<String>,
    headers: header::HeaderMap,
) -> Result<axum::response::Response, StatusCode> {
    let mut permit = store
        .0
        .consent
        .permit()
        .map_err(|_| StatusCode::FORBIDDEN)?;
    store
        .0
        .upload
        .enabled()
        .map_err(|_| StatusCode::FORBIDDEN)?;
    let spec = store.spec(&key).map_err(|_| StatusCode::NOT_FOUND)?;
    let start = match headers.get(header::RANGE) {
        None => 0,
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.strip_suffix('-'))
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v < spec.bytes)
            .ok_or(StatusCode::RANGE_NOT_SATISFIABLE)?,
    };
    // No admission queue: an untrusted peer cannot create arbitrarily many
    // pending disk/hash jobs. Router clones and Range requests share this cap.
    // The owned guard moves into the response body, releasing on EOF, error,
    // request cancellation or body drop, not merely when headers are returned.
    let admission = store
        .0
        .serving
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mut file = regular(&store.0.root.join(&key), false)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    // Retain full verification: metadata is not a safe integrity cache key.
    if !verify(&mut file, spec, &mut permit)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?
    {
        return Err(StatusCode::NOT_FOUND);
    }
    file.seek(io::SeekFrom::Start(start))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let remaining = spec.bytes - start;
    let status = if headers.contains_key(header::RANGE) {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut response = axum::response::Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, remaining)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::ACCEPT_RANGES, "bytes");
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", spec.bytes - 1, spec.bytes),
        );
    }
    let body = stream::try_unfold(
        (file, remaining, store, permit, admission),
        |(mut file, remaining, store, mut permit, admission)| async move {
            if remaining == 0 {
                return Ok::<_, io::Error>(None);
            }
            let mut bytes = vec![0; remaining.min(BLOCK as u64) as usize];
            store.0.upload.charge(bytes.len(), &mut permit).await?;
            permit.run(file.read_exact(&mut bytes)).await?;
            permit.check()?;
            let remaining = remaining - bytes.len() as u64;
            Ok(Some((bytes, (file, remaining, store, permit, admission))))
        },
    );
    response
        .body(axum::body::Body::from_stream(body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[cfg(test)]
mod tests;
