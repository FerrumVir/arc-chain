use super::*;
use axum::{Router, body::Body, response::Response, routing::get};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::net::TcpListener;

struct Server {
    url: Url,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(router: Router) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server { url, task }
}

fn pin(data: &[u8]) -> SliceSpec {
    SliceSpec {
        digest: SliceDigest::Blake3(*blake3::hash(data).as_bytes()),
        bytes: data.len() as u64,
    }
}

fn config() -> SliceDistributionConfig {
    SliceDistributionConfig {
        host_slices: Some(true),
        download_bytes_per_second: 100_000_000,
        upload_bytes_per_second: 100_000_000,
        ..Default::default()
    }
}

fn store(dir: &TempDir, spec: &SliceSpec, config: &SliceDistributionConfig) -> SliceStore {
    SliceStore::new(dir.path().join("cache"), vec![spec.clone()], config).unwrap()
}

async fn seed(store: &SliceStore, spec: &SliceSpec, data: &[u8]) {
    fs::create_dir_all(&store.0.root).await.unwrap();
    fs::write(store.0.root.join(spec.digest.key()), data)
        .await
        .unwrap();
}

fn mirrors(url: &Url) -> SliceSources {
    SliceSources {
        mirrors: vec![url.clone()],
        peers: vec![],
    }
}

async fn mirror(data: Vec<u8>) -> (Server, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let server = server(Router::new().fallback(get(move || {
        let data = data.clone();
        count.fetch_add(1, Ordering::SeqCst);
        async move { data }
    })))
    .await;
    (server, hits)
}

#[tokio::test]
async fn missing_or_false_consent_never_contacts_a_source_or_creates_cache() {
    let data = b"assigned slice";
    let spec = pin(data);
    let (mirror, hits) = mirror(data.to_vec()).await;
    for consent in [None, Some(false)] {
        let dir = TempDir::new().unwrap();
        let mut cfg = config();
        cfg.host_slices = consent;
        let node = store(&dir, &spec, &cfg);
        assert_eq!(
            node.download(&spec.digest.key(), &mirrors(&mirror.url))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!node.0.root.exists());
        let peer = server(node.router()).await;
        let response = Client::new()
            .get(
                peer.url
                    .join(&format!("slices/{}", spec.digest.key()))
                    .unwrap(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let old: crate::config::NodeConfig = toml::from_str("").unwrap();
    assert_eq!(old.slice_distribution.host_slices, None);
    let cfg: crate::config::NodeConfig =
        toml::from_str("[slice_distribution]\nhost_slices = true\nupload_bytes_per_second = 123\n")
            .unwrap();
    assert_eq!(cfg.slice_distribution.host_slices, Some(true));
    assert_eq!(cfg.slice_distribution.upload_bytes_per_second, 123);
    assert!(
        toml::from_str::<crate::config::NodeConfig>("[slice_distribution]\nhost_slice = true")
            .is_err()
    );
}

#[tokio::test]
async fn mirrors_precede_peers_and_only_assigned_slices_are_downloaded() {
    let data = b"a small synthetic expert slice";
    let spec = pin(data);
    let dir = TempDir::new().unwrap();
    let node = store(&dir, &spec, &config());
    let (mirror, mirror_hits) = mirror(data.to_vec()).await;
    let (peer, peer_hits) = self::mirror(data.to_vec()).await;
    let sources = SliceSources {
        mirrors: vec![mirror.url.clone()],
        peers: vec![peer.url.clone()],
    };
    assert!(
        node.download(&pin(b"unassigned").digest.key(), &sources)
            .await
            .is_err()
    );
    assert_eq!(mirror_hits.load(Ordering::SeqCst), 0);
    let output = node.download(&spec.digest.key(), &sources).await.unwrap();
    assert_eq!(fs::read(&output).await.unwrap(), data);
    assert_eq!(mirror_hits.load(Ordering::SeqCst), 1);
    assert_eq!(peer_hits.load(Ordering::SeqCst), 0);
    node.download(&spec.digest.key(), &sources).await.unwrap();
    assert_eq!(mirror_hits.load(Ordering::SeqCst), 1); // verified cache hit
}

#[tokio::test]
async fn multi_peer_fallback_rejects_corruption_and_can_relay_verified_content() {
    let data = vec![42; 3 * BLOCK + 17];
    let spec = pin(&data);
    let dirs: Vec<_> = (0..4).map(|_| TempDir::new().unwrap()).collect();
    let nodes: Vec<_> = dirs.iter().map(|d| store(d, &spec, &config())).collect();
    seed(&nodes[0], &spec, &vec![11; data.len()]).await;
    seed(&nodes[1], &spec, &data).await;
    let bad_peer = server(nodes[0].router()).await;
    let good_peer = server(nodes[1].router()).await;
    let (bad_mirror, hits) = mirror(vec![7; data.len()]).await;
    let sources = SliceSources {
        mirrors: vec![bad_mirror.url.clone()],
        peers: vec![bad_peer.url.clone(), good_peer.url.clone()],
    };
    let output = nodes[2]
        .download(&spec.digest.key(), &sources)
        .await
        .unwrap();
    assert_eq!(fs::read(output).await.unwrap(), data);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let relay = server(nodes[2].router()).await;
    nodes[3]
        .download(
            &spec.digest.key(),
            &SliceSources {
                mirrors: vec![],
                peers: vec![relay.url.clone()],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fs::read(nodes[3].0.root.join(spec.digest.key()))
            .await
            .unwrap(),
        data
    );
}

#[tokio::test]
async fn truncated_network_body_resumes_after_reopening_store() {
    let data = b"resume from the exact persisted prefix";
    let spec = pin(data);
    let dir = TempDir::new().unwrap();
    let node = store(&dir, &spec, &config());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let partial = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "request ended before headers");
            request.extend_from_slice(&buffer[..count]);
            assert!(request.len() < 4096);
        }
        socket
            .write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", data.len()).as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(&data[..9]).await.unwrap();
        socket.flush().await.unwrap();
        // Ensure the client processes the prefix before EOF arrives.
        tokio::time::sleep(Duration::from_millis(50)).await;
        socket.shutdown().await.unwrap();
    });
    assert!(
        node.download(&spec.digest.key(), &mirrors(&url))
            .await
            .is_err()
    );
    partial.await.unwrap();
    assert!(!node.0.root.join(spec.digest.key()).exists());
    let part = node.0.root.join(format!("{}.part", spec.digest.key()));
    assert_eq!(fs::read(part).await.unwrap(), &data[..9]);
    drop(node);
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let captured = ranges.clone();
    let source = server(
        Router::new().fallback(get(move |headers: header::HeaderMap| {
            let ranges = captured.clone();
            async move {
                ranges
                    .lock()
                    .await
                    .push(headers[header::RANGE].to_str().unwrap().to_owned());
                Response::builder()
                    .status(206)
                    .header(
                        header::CONTENT_RANGE,
                        format!("bytes 9-{}/{}", data.len() - 1, data.len()),
                    )
                    .body(Body::from(data[9..].to_vec()))
                    .unwrap()
            }
        })),
    )
    .await;
    let resumed = store(&dir, &spec, &config());
    let output = resumed
        .download(&spec.digest.key(), &mirrors(&source.url))
        .await
        .unwrap();
    assert_eq!(fs::read(output).await.unwrap(), data);
    assert_eq!(*ranges.lock().await, vec!["bytes=9-"]);
}

#[tokio::test]
async fn ignored_ranges_restart_and_poisoned_prefixes_never_publish() {
    let data = b"the exact expected bytes";
    let spec = pin(data);
    let dir = TempDir::new().unwrap();
    let node = store(&dir, &spec, &config());
    fs::create_dir_all(&node.0.root).await.unwrap();
    let part = node.0.root.join(format!("{}.part", spec.digest.key()));
    fs::write(&part, b"poison").await.unwrap();
    let (source, _) = mirror(data.to_vec()).await;
    let output = node
        .download(&spec.digest.key(), &mirrors(&source.url))
        .await
        .unwrap();
    assert_eq!(fs::read(output).await.unwrap(), data);
    assert!(!part.exists());
}

#[tokio::test]
async fn invalid_ranges_lengths_encoded_and_corrupted_bodies_are_rejected() {
    let spec = pin(b"correct!");
    for case in 0..6 {
        let dir = TempDir::new().unwrap();
        let node = store(&dir, &spec, &config());
        let source = server(Router::new().fallback(get(move || async move {
            let mut builder = Response::builder();
            let data: &[u8] = match case {
                0 => b"corrupt!",
                1 => b"short",
                2 => b"way too long",
                3 => {
                    builder = builder
                        .status(206)
                        .header(header::CONTENT_RANGE, "bytes 1-7/8");
                    b"correct!"
                }
                4 => {
                    builder = builder.header(header::CONTENT_ENCODING, "gzip");
                    b"correct!"
                }
                _ => {
                    builder = builder.status(302).header(header::LOCATION, "/another");
                    b"correct!"
                }
            };
            builder.body(Body::from(data)).unwrap()
        })))
        .await;
        assert!(
            node.download(&spec.digest.key(), &mirrors(&source.url))
                .await
                .is_err(),
            "case {case}"
        );
        assert!(!node.0.root.join(spec.digest.key()).exists());
        let peer = server(node.router()).await;
        assert_eq!(
            Client::new()
                .get(
                    peer.url
                        .join(&format!("slices/{}", spec.digest.key()))
                        .unwrap()
                )
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn poisoned_resume_prefix_gets_one_clean_attempt_on_the_same_peer() {
    let data = b"trusted slice bytes";
    let spec = pin(data);
    let peer_dir = TempDir::new().unwrap();
    let peer = store(&peer_dir, &spec, &config());
    seed(&peer, &spec, data).await;
    let server = server(peer.router()).await;
    let dir = TempDir::new().unwrap();
    let node = store(&dir, &spec, &config());
    fs::create_dir_all(&node.0.root).await.unwrap();
    fs::write(
        node.0.root.join(format!("{}.part", spec.digest.key())),
        b"bad",
    )
    .await
    .unwrap();
    let output = node
        .download(
            &spec.digest.key(),
            &SliceSources {
                mirrors: vec![],
                peers: vec![server.url.clone()],
            },
        )
        .await
        .unwrap();
    assert_eq!(fs::read(output).await.unwrap(), data);
}

#[tokio::test]
async fn zero_upload_bandwidth_and_invalid_ranges_are_refused() {
    let data = b"range-test";
    let spec = pin(data);
    let dir = TempDir::new().unwrap();
    let node = store(&dir, &spec, &config());
    seed(&node, &spec, data).await;
    let peer = server(node.router()).await;
    let url = peer
        .url
        .join(&format!("slices/{}", spec.digest.key()))
        .unwrap();
    for range in [
        "bytes=10-",
        "bytes=-1",
        "bytes=0-1,3-4",
        "bytes=999999999999999999999-",
    ] {
        let response = Client::new()
            .get(url.clone())
            .header(header::RANGE, range)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }
    let mut cfg = config();
    cfg.upload_bytes_per_second = 0;
    let paused = store(&dir, &spec, &cfg);
    let server = server(paused.router()).await;
    let url = server
        .url
        .join(&format!("slices/{}", spec.digest.key()))
        .unwrap();
    assert_eq!(
        Client::new().get(url).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[test]
fn invalid_assignments_fail_before_io() {
    let cfg = config();
    let spec = pin(b"one");
    let mut wrong_size = spec.clone();
    wrong_size.bytes += 1;
    assert!(SliceStore::new(PathBuf::new(), vec![spec.clone(), wrong_size], &cfg).is_err());
    for bytes in [0, cfg.max_slice_bytes + 1] {
        let mut invalid = spec.clone();
        invalid.bytes = bytes;
        assert!(SliceStore::new(PathBuf::new(), vec![invalid], &cfg).is_err());
    }
}

#[tokio::test]
async fn stale_complete_cache_and_complete_resume_state_are_rehashed() {
    let data = b"verified object";
    let spec = pin(data);
    for cached in [true, false] {
        let dir = TempDir::new().unwrap();
        let node = store(&dir, &spec, &config());
        seed(&node, &spec, b"wrong cached bytes").await;
        if !cached {
            fs::rename(
                node.0.root.join(spec.digest.key()),
                node.0.root.join(format!("{}.part", spec.digest.key())),
            )
            .await
            .unwrap();
        }
        let (source, hits) = mirror(data.to_vec()).await;
        let output = node
            .download(&spec.digest.key(), &mirrors(&source.url))
            .await
            .unwrap();
        assert_eq!(fs::read(output).await.unwrap(), data);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
    let dir = TempDir::new().unwrap();
    let node = store(&dir, &spec, &config());
    fs::create_dir_all(&node.0.root).await.unwrap();
    fs::write(
        node.0.root.join(format!("{}.part", spec.digest.key())),
        data,
    )
    .await
    .unwrap();
    assert!(
        node.download(&spec.digest.key(), &SliceSources::default())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn sha256_identity_and_concurrent_downloads_use_one_verified_object() {
    let data = b"sha256 package slice";
    let spec = SliceSpec {
        digest: SliceDigest::Sha256(Sha256::digest(data).into()),
        bytes: data.len() as u64,
    };
    let dir = TempDir::new().unwrap();
    let node = store(&dir, &spec, &config());
    let (source, hits) = mirror(data.to_vec()).await;
    let key = spec.digest.key();
    let sources = mirrors(&source.url);
    let (a, b) = tokio::join!(node.download(&key, &sources), node.download(&key, &sources));
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn zero_bandwidth_blocks_and_revocation_cancels_a_throttled_download() {
    let data = vec![31; BLOCK];
    let spec = pin(&data);
    let (source, hits) = mirror(data).await;
    let dir = TempDir::new().unwrap();
    let mut cfg = config();
    cfg.download_bytes_per_second = 0;
    let node = store(&dir, &spec, &cfg);
    assert!(
        node.download(&spec.digest.key(), &mirrors(&source.url))
            .await
            .is_err()
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    cfg.download_bytes_per_second = 1;
    let node = store(&dir, &spec, &cfg);
    let consent = node.consent();
    let root = node.0.root.clone();
    let key = spec.digest.key();
    let key_copy = key.clone();
    let sources = mirrors(&source.url);
    let task = tokio::spawn(async move { node.download(&key_copy, &sources).await });
    timeout(Duration::from_secs(2), async {
        while hits.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    consent.set(false);
    consent.set(true); // revocation cannot be hidden by an immediate re-opt-in
    assert_eq!(
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(!root.join(key).exists());
}

#[tokio::test]
async fn upload_is_throttled_and_consent_revocation_stops_its_body() {
    let data = vec![19; 256];
    let spec = pin(&data);
    let dir = TempDir::new().unwrap();
    let mut cfg = config();
    cfg.upload_bytes_per_second = 1024;
    let node = store(&dir, &spec, &cfg);
    seed(&node, &spec, &data).await;
    let peer = server(node.router()).await;
    let url = peer
        .url
        .join(&format!("slices/{}", spec.digest.key()))
        .unwrap();
    let start = Instant::now();
    let (a, b) = tokio::join!(
        Client::new().get(url.clone()).send(),
        Client::new().get(url.clone()).send()
    );
    let (a, b) = tokio::join!(a.unwrap().bytes(), b.unwrap().bytes());
    assert_eq!(a.unwrap().as_ref(), data);
    assert_eq!(b.unwrap().as_ref(), data);
    assert!(start.elapsed() >= Duration::from_millis(490));
    let response = Client::new().get(url).send().await.unwrap();
    node.consent().set(false);
    assert!(response.bytes().await.is_err());
}

#[tokio::test]
async fn rate_limiter_accounts_for_concurrent_reservations() {
    let (tx, _) = watch::channel(true);
    let consent = SliceConsent(tx);
    let mut a = consent.permit().unwrap();
    let mut b = consent.permit().unwrap();
    let bandwidth = Bandwidth::new(1000);
    let start = Instant::now();
    let (a, b) = tokio::join!(bandwidth.charge(100, &mut a), bandwidth.charge(100, &mut b));
    a.unwrap();
    b.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(195));
}

#[tokio::test]
async fn cancelled_bandwidth_wait_does_not_delay_a_new_opt_in() {
    let (tx, _) = watch::channel(true);
    let consent = SliceConsent(tx);
    let bandwidth = Bandwidth::new(1000);
    let mut old = consent.permit().unwrap();
    consent.set(true); // saving the same answer must be idempotent
    old.check().unwrap();
    let revoke = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        consent.set(false);
        consent.set(true);
    };
    let (result, ()) = tokio::join!(bandwidth.charge(60_000, &mut old), revoke);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    // The old operation must remain revoked even after watch::changed was
    // consumed; a source fallback may not revive it.
    assert!(old.check().is_err());
    let mut new = consent.permit().unwrap();
    timeout(Duration::from_secs(1), bandwidth.charge(1, &mut new))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn consent_revocation_interrupts_a_source_that_never_sends_headers() {
    let spec = pin(b"test");
    let dir = TempDir::new().unwrap();
    let node = store(&dir, &spec, &config());
    let (tx, mut seen) = tokio::sync::mpsc::channel(1);
    let source = server(Router::new().fallback(get(move || {
        let tx = tx.clone();
        async move {
            tx.send(()).await.unwrap();
            std::future::pending::<String>().await
        }
    })))
    .await;
    let consent = node.consent();
    let sources = mirrors(&source.url);
    let key = spec.digest.key();
    let task = tokio::spawn(async move { node.download(&key, &sources).await });
    timeout(Duration::from_secs(2), seen.recv())
        .await
        .unwrap()
        .unwrap();
    consent.set(false);
    assert_eq!(
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_and_hardlink_cache_entries_cannot_overwrite_other_files() {
    let data = b"slice";
    let spec = pin(data);
    for hardlink in [false, true] {
        let dir = TempDir::new().unwrap();
        let node = store(&dir, &spec, &config());
        fs::create_dir_all(&node.0.root).await.unwrap();
        let victim = dir.path().join("unrelated");
        fs::write(&victim, b"keep me").await.unwrap();
        let part = node.0.root.join(format!("{}.part", spec.digest.key()));
        if hardlink {
            std::fs::hard_link(&victim, part).unwrap();
        } else {
            std::os::unix::fs::symlink(&victim, part).unwrap();
        }
        let (source, hits) = mirror(data.to_vec()).await;
        assert!(
            node.download(&spec.digest.key(), &mirrors(&source.url))
                .await
                .is_err()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert_eq!(fs::read(victim).await.unwrap(), b"keep me");
    }
}
