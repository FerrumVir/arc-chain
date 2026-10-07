use super::*;
use axum::{
    Router, body::Body, extract::Path as UrlPath, http::HeaderMap, response::Response, routing::get,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

const MANIFEST: &[u8] = include_bytes!("../../../tests/fixtures/eng10-tiny/manifest.json");
const PIN: &str = include_str!("../../../tests/fixtures/eng10-tiny/trusted-manifest-blake3.txt");
const NAME: &str = "layer.1.experts.0";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eng10-tiny")
}
fn assignment(names: &[&str]) -> ManifestAssignment {
    ManifestAssignment::parse(
        MANIFEST,
        PIN.trim(),
        &names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
    .unwrap()
}
fn config() -> SliceDistributionConfig {
    SliceDistributionConfig {
        host_slices: Some(true),
        download_bytes_per_second: 100_000_000,
        upload_bytes_per_second: 100_000_000,
        ..Default::default()
    }
}
fn worker(
    dir: &TempDir,
    cfg: &SliceDistributionConfig,
    mirrors: Vec<Url>,
    peers: Vec<Url>,
) -> SliceWorker {
    SliceWorker::new(
        assignment(&[NAME]),
        dir.path().join("cache"),
        cfg,
        mirrors,
        peers,
    )
    .unwrap()
}
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server { url, task }
}
async fn mirror(mode: u8) -> (Server, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let ranges = Arc::new(AtomicUsize::new(0));
    let (count, resumed) = (hits.clone(), ranges.clone());
    let router = Router::new().route(
        "/fixture/{file}",
        get(move |UrlPath(file): UrlPath<String>, headers: HeaderMap| {
            let (count, resumed) = (count.clone(), resumed.clone());
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                // Only the requested selection may cross this integration boundary.
                let selected = assignment(&[NAME]);
                let path = selected.slice_path(NAME, &fixture()).unwrap();
                assert_eq!(file, path.file_name().unwrap().to_str().unwrap());
                let mut data = fs::read(path).await.unwrap();
                if mode == 1 {
                    data[0] ^= 1;
                }
                if mode == 2 {
                    data.truncate(data.len() / 2);
                }
                if mode == 0
                    && let Some(range) = headers.get(header::RANGE)
                {
                    resumed.fetch_add(1, Ordering::SeqCst);
                    let range = range.to_str().unwrap();
                    let start: usize = range
                        .strip_prefix("bytes=")
                        .unwrap()
                        .strip_suffix('-')
                        .unwrap()
                        .parse()
                        .unwrap();
                    return Response::builder()
                        .status(206)
                        .header(
                            header::CONTENT_RANGE,
                            format!("bytes {start}-{}/{}", data.len() - 1, data.len()),
                        )
                        .body(Body::from(data[start..].to_vec()))
                        .unwrap();
                }
                Response::new(Body::from(data))
            }
        }),
    );
    let mut server = server(router).await;
    server.url.set_path("/fixture"); // No trailing slash: adapter appends safely.
    (server, hits, ranges)
}

#[test]
fn independently_pinned_manifest_and_exact_selection() {
    assert!(ManifestAssignment::parse(MANIFEST, &"00".repeat(32), &[NAME.into()]).is_err());
    assert!(ManifestAssignment::parse(MANIFEST, PIN.trim(), &[]).is_err());
    assert!(ManifestAssignment::parse(MANIFEST, PIN.trim(), &["layer.1.experts".into()]).is_err());
    assert!(ManifestAssignment::parse(MANIFEST, PIN.trim(), &[NAME.into(), NAME.into()]).is_err());
    let a = assignment(&[NAME]);
    let spec = &a.specs().unwrap()[0];
    let bytes = std::fs::read(a.slice_path(NAME, &fixture()).unwrap()).unwrap();
    assert_eq!(spec.bytes, bytes.len() as u64);
    assert_eq!(
        spec.digest,
        SliceDigest::Blake3(*blake3::hash(&bytes).as_bytes())
    );
    assert!(a.slice_path("embed", &fixture()).is_err());
    // Self-consistent re-hashing of a changed manifest still cannot change trust.
    let mut changed: serde_json::Value = serde_json::from_slice(MANIFEST).unwrap();
    changed["source"] = serde_json::json!({"untrusted": true});
    changed["manifest_blake3"] = arc_inference::model_package::manifest_body_blake3(&changed)
        .unwrap()
        .into();
    let encoded = serde_json::to_vec(&changed).unwrap();
    assert!(SliceManifest::parse(&encoded).is_ok());
    assert!(ManifestAssignment::parse(&encoded, PIN.trim(), &[NAME.into()]).is_err());
}

#[test]
fn assembly_rejects_pending_and_partial_experts_and_accepts_complete_stage() {
    let stage = StageSpec {
        first_layer: 1,
        end_layer: 2,
    };
    let output = TempDir::new().unwrap();
    let path = output.path().join("model.arcspkg");
    assert!(
        assignment(&[NAME])
            .assemble_stage(&fixture(), stage, &path)
            .is_err()
    );
    assert!(!path.exists());
    let names = [
        "layer.1.core",
        NAME,
        "layer.1.experts.1",
        "layer.1.experts.2",
        "layer.1.experts.3",
    ];
    assignment(&names)
        .assemble_stage(&fixture(), stage, &path)
        .unwrap();
    assert!(path.exists());
    let pending = ManifestAssignment::parse(
        include_bytes!("../../../tests/fixtures/eng10-tiny/pending-yarn-manifest.json"),
        include_str!("../../../tests/fixtures/eng10-tiny/trusted-yarn-manifest-blake3.txt").trim(),
        &names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
    .unwrap();
    let pending_path = output.path().join("pending.arcspkg");
    assert!(
        pending
            .assemble_stage(&fixture(), stage, &pending_path)
            .is_err()
    );
    assert!(!pending_path.exists());
}

#[tokio::test]
async fn selected_download_resumes_and_maps_mirror_paths() {
    let dir = TempDir::new().unwrap();
    let (source, hits, ranges) = mirror(0).await;
    let w = worker(&dir, &config(), vec![source.url.clone()], vec![]);
    let spec = &w.assignment.specs().unwrap()[0];
    let bytes = fs::read(w.assignment.slice_path(NAME, &fixture()).unwrap())
        .await
        .unwrap();
    fs::create_dir_all(&w.store.0.root).await.unwrap();
    fs::write(
        w.store.0.root.join(format!("{}.part", spec.digest.key())),
        &bytes[..1234],
    )
    .await
    .unwrap();
    let paths = w.download_selected().await.unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(fs::read(&paths[0]).await.unwrap(), bytes);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(ranges.load(Ordering::SeqCst), 1);
    let mut entries = fs::read_dir(&w.store.0.root).await.unwrap();
    assert!(entries.next_entry().await.unwrap().is_some());
    assert!(entries.next_entry().await.unwrap().is_none());
}

#[tokio::test]
async fn corrupt_and_partial_sources_cannot_publish_then_peer_fallback_works() {
    let (good, _, _) = mirror(0).await;
    let peer_dir = TempDir::new().unwrap();
    let peer = worker(&peer_dir, &config(), vec![good.url.clone()], vec![]);
    peer.download_selected().await.unwrap();
    let served = server(peer.router()).await;
    for mode in [1, 2] {
        let (bad, _, _) = mirror(mode).await;
        let dir = TempDir::new().unwrap();
        let w = worker(&dir, &config(), vec![bad.url.clone()], vec![]);
        assert!(w.download_selected().await.is_err());
        let key = w.assignment.specs().unwrap()[0].digest.key();
        assert!(!w.store.0.root.join(key).exists());
        let fallback = worker(
            &dir,
            &config(),
            vec![bad.url.clone()],
            vec![served.url.clone()],
        );
        assert_eq!(fallback.download_selected().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn worker_consent_and_bandwidth_apply_to_downloads_and_peer_router() {
    let (source, hits, _) = mirror(0).await;
    for consent in [None, Some(false)] {
        let dir = TempDir::new().unwrap();
        let mut cfg = config();
        cfg.host_slices = consent;
        let w = worker(&dir, &cfg, vec![source.url.clone()], vec![]);
        assert_eq!(
            w.download_selected().await.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            w.run("127.0.0.1:0".parse().unwrap(), std::future::pending())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!w.store.0.root.exists());
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let dir = TempDir::new().unwrap();
    let mut cfg = config();
    cfg.download_bytes_per_second = 69_120;
    cfg.upload_bytes_per_second = 69_120;
    let w = worker(&dir, &cfg, vec![source.url.clone()], vec![]);
    let start = Instant::now();
    w.download_selected().await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(100));
    let served = server(w.router()).await;
    let url = served
        .url
        .join(&format!(
            "slices/{}",
            w.assignment.specs().unwrap()[0].digest.key()
        ))
        .unwrap();
    let start = Instant::now();
    assert_eq!(
        Client::new()
            .get(url.clone())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .len(),
        6912
    );
    assert!(start.elapsed() >= Duration::from_millis(100));
    w.consent().set(false);
    assert_eq!(
        Client::new().get(url).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert!(w.download_selected().await.is_err());
    let paused_dir = TempDir::new().unwrap();
    cfg.download_bytes_per_second = 0;
    assert!(
        worker(&paused_dir, &cfg, vec![source.url.clone()], vec![])
            .download_selected()
            .await
            .is_err()
    );
    let slow_dir = TempDir::new().unwrap();
    cfg.download_bytes_per_second = 1;
    let slow = worker(&slow_dir, &cfg, vec![source.url.clone()], vec![]);
    let revoke = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        slow.consent().set(false);
    };
    let (result, ()) = tokio::join!(slow.download_selected(), revoke);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    assert!(
        !slow
            .store
            .0
            .root
            .join(slow.assignment.specs().unwrap()[0].digest.key())
            .exists()
    );
}

#[tokio::test]
async fn foreground_worker_startup_downloads_and_shutdown_revokes() {
    let (source, hits, _) = mirror(0).await;
    let dir = TempDir::new().unwrap();
    let w = worker(&dir, &config(), vec![source.url.clone()], vec![]);
    let published = w
        .store
        .0
        .root
        .join(w.assignment.specs().unwrap()[0].digest.key());
    w.run("127.0.0.1:0".parse().unwrap(), async {
        timeout(Duration::from_secs(10), async {
            while !fs::try_exists(&published).await.unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    })
    .await
    .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(w.consent().permit().is_err());
}
