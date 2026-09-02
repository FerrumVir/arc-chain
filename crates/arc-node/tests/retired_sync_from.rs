use std::net::TcpListener;
use std::process::Command;

#[test]
fn retired_sync_from_fails_before_disk_or_network_side_effects() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local observation socket");
    listener
        .set_nonblocking(true)
        .expect("make observation socket nonblocking");
    let peer = format!("http://{}", listener.local_addr().unwrap());
    let data_dir = std::env::temp_dir().join(format!(
        "arc-retired-sync-from-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    assert!(!data_dir.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_arc-node"))
        .args(["--sync-from", &peer, "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("run arc-node preflight");

    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("retired unauthenticated snapshot protocol"),
        "unexpected failure: {combined}"
    );
    assert!(
        !data_dir.exists(),
        "retired sync preflight must not create or bind the requested data directory"
    );
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "retired sync preflight must not contact the supplied peer"
    );
}
