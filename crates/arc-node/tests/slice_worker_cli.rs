//! Exercise the actual operator dispatch before node/chain startup.
use std::{net::TcpListener, path::Path, process::Command};

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eng10-tiny")
}

fn stage_command(scratch: &Path, operation: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arc-node"));
    command
        .arg(operation)
        .arg("--node-config")
        .arg(scratch.join("node.toml"))
        .arg("--manifest")
        .arg(fixture().join("manifest.json"))
        .arg("--manifest-blake3")
        .arg(
            std::fs::read_to_string(fixture().join("trusted-manifest-blake3.txt"))
                .unwrap()
                .trim(),
        )
        .arg("--cache")
        .arg(scratch.join("cache"))
        .current_dir(scratch);
    for name in [
        "layer.1.core",
        "layer.1.experts.0",
        "layer.1.experts.1",
        "layer.1.experts.2",
        "layer.1.experts.3",
    ] {
        command.args(["--slice", name]);
    }
    command
}

#[test]
fn offline_cli_promotes_cache_and_preserves_existing_output() {
    let scratch = tempfile::tempdir().unwrap();
    std::fs::write(
        scratch.path().join("node.toml"),
        "[slice_distribution]\nhost_slices = true\n",
    )
    .unwrap();
    let cache = scratch.path().join("cache");
    std::fs::create_dir(&cache).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture().join("manifest.json")).unwrap()).unwrap();
    for record in manifest["slices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["segment"] == "layer.1")
    {
        let hash = record["blake3"].as_str().unwrap();
        std::fs::copy(
            fixture().join(format!("{hash}.slice")),
            cache.join(format!("blake3-{hash}")),
        )
        .unwrap();
    }
    let package = scratch.path().join("stage.arcspkg");
    for success in [true, false] {
        let output = stage_command(scratch.path(), "slice-assemble")
            .args(["--stage", "1:2", "--output"])
            .arg(&package)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let header = arc_inference::modern::mla::package::read_header_file(&package).unwrap();
        assert_eq!(
            header.stage,
            arc_inference::modern::mla::package::StageSpec {
                first_layer: 1,
                end_layer: 2
            }
        );
    }
}

/// SIGINT is the executable's graceful owner stop on Unix. Windows retains
/// the portable startup/assembly tests; this test never starts a chain node.
#[cfg(unix)]
#[tokio::test]
async fn real_worker_sigint_closes_port_and_restart_enforces_saved_consent() {
    use axum::{Router, extract::Path as UrlPath, routing::get};
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            // Only the child started by this test, never a process-name kill.
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    struct ServerGuard(tokio::task::JoinHandle<()>);
    impl Drop for ServerGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let scratch = tempfile::tempdir().unwrap();
    let config = scratch.path().join("node.toml");
    std::fs::write(
        &config,
        "[slice_distribution]\nhost_slices = true\nupload_bytes_per_second = 1\n",
    )
    .unwrap();
    let mirror = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mirror_url = format!("http://{}", mirror.local_addr().unwrap());
    let router = Router::new().route(
        "/{file}",
        get(|UrlPath(file): UrlPath<String>| async move {
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(fixture().join("manifest.json")).unwrap())
                    .unwrap();
            assert!(
                manifest["slices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["segment"] == "layer.1"
                        && file == format!("{}.slice", r["blake3"].as_str().unwrap()))
            );
            tokio::fs::read(fixture().join(file)).await.unwrap()
        }),
    );
    let _server = ServerGuard(tokio::spawn(async move {
        axum::serve(mirror, router).await.unwrap();
    }));
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let stderr = scratch.path().join("worker.stderr");
    let mut child = ChildGuard(
        stage_command(scratch.path(), "slice-worker")
            .args(["--mirror", &mirror_url, "--listen", &address.to_string()])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture().join("manifest.json")).unwrap()).unwrap();
    let hash = manifest["slices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "layer.1.experts.0")
        .unwrap()["blake3"]
        .as_str()
        .unwrap();
    let url = format!("http://{address}/slices/blake3-{hash}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let active = loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(&stderr).unwrap()
        );
        if let Ok(response) = client.get(&url).send().await
            && response.status().is_success()
        {
            break response;
        }
        assert!(Instant::now() < deadline, "worker failed to become ready");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        std::fs::read(scratch.path().join("cache").join(format!("blake3-{hash}"))).unwrap(),
        std::fs::read(fixture().join(format!("{hash}.slice"))).unwrap()
    );
    // Saving false alone is NOT a live toggle. The running command continues
    // serving until the owner stops it, then restart must deny the saved setting.
    std::fs::write(&config, "[slice_distribution]\nhost_slices = false\n").unwrap();
    let active_range = client
        .get(&url)
        .header("Range", "bytes=1-")
        .send()
        .await
        .unwrap();
    assert_eq!(active_range.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    // SAFETY: this is our still-owned, unreaped child PID, not a daemon/node.
    assert_eq!(
        unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "SIGINT did not stop worker");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    assert!(active.bytes().await.is_err());
    assert!(active_range.bytes().await.is_err());
    let restarted = stage_command(scratch.path(), "slice-worker")
        .args(["--listen", &address.to_string()])
        .output()
        .unwrap();
    assert!(!restarted.status.success());
    assert!(String::from_utf8_lossy(&restarted.stderr).contains("explicit opt-in"));
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[test]
fn slice_worker_cli_requires_separate_pin_and_explicit_config_consent() {
    let scratch = tempfile::tempdir().unwrap();
    let config = scratch.path().join("node.toml");
    std::fs::write(&config, "").unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eng10-tiny");
    let cache = scratch.path().join("cache");
    // Occupied endpoint distinguishes consent rejection from listener startup.
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = occupied.local_addr().unwrap().to_string();
    let trusted = std::fs::read_to_string(fixture.join("trusted-manifest-blake3.txt")).unwrap();
    for (pin, error) in [
        ("00".repeat(32), "separately trusted digest"),
        (trusted.trim().to_owned(), "explicit opt-in"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_arc-node"))
            .args(["slice-worker", "--node-config"])
            .arg(&config)
            .arg("--manifest")
            .arg(fixture.join("manifest.json"))
            .args([
                "--manifest-blake3",
                &pin,
                "--slice",
                "layer.1.experts.0",
                "--cache",
            ])
            .arg(&cache)
            .args(["--listen", &address])
            .current_dir(scratch.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(error), "{stderr}");
        assert!(!cache.exists());
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 1);
    }
}
