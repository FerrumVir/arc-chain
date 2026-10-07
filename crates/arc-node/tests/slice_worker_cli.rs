//! Exercise the actual operator dispatch before node/chain startup.
use std::{net::TcpListener, path::Path, process::Command};

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
