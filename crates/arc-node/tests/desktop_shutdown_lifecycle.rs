use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CONTROL_DIR: &str = ".arc-desktop-control";
const TOKEN_FILE: &str = "token";
const REQUEST_FILE: &str = "request";
const REQUEST_SCHEMA: &str = "arc.desktop.shutdown.v1";
// The lifecycle receipt deliberately re-hashes the exact arc-node executable
// when it is loaded, authenticated, and acknowledged.  An unstripped debug
// binary is hundreds of MiB on hosted runners, where those security checks can
// legitimately take longer than the old 60-second fixture deadline even
// though the node is making forward progress.  Keep this bounded far below
// the production graceful-drain budget while allowing the integrity checks to
// complete under slow CI I/O instead of weakening or bypassing them.
const STARTUP_SHUTDOWN_TEST_TIMEOUT: Duration = Duration::from_secs(180);

#[cfg(windows)]
fn acquire_managed_lifecycle_fixture(
    data_dir: &std::path::Path,
    session_nonce: &[u8; 32],
) -> std::fs::File {
    let data_namespace =
        arc_crypto::secret_file::acquire_private_directory_namespace_lock(data_dir).unwrap();
    data_namespace.restore_interrupted().unwrap();
    let control_dir = data_dir.join(CONTROL_DIR);
    let control_namespace =
        arc_crypto::secret_file::acquire_private_directory_namespace_lock(&control_dir).unwrap();
    control_namespace.restore_interrupted().unwrap();

    // Match the desktop's production handoff: prove the child name first,
    // then the parent name while no descendant handles are open, and retain
    // the stable sibling guard until the in-directory owner lease is locked.
    control_namespace.rebarrier_existing().unwrap();
    drop(control_namespace);
    data_namespace.rebarrier_existing().unwrap();

    let proof_path =
        arc_crypto::secret_file::desktop_lifecycle_namespace_proof_path(data_dir).unwrap();
    let proof = arc_crypto::secret_file::desktop_lifecycle_namespace_proof(data_dir, session_nonce)
        .unwrap();
    arc_crypto::secret_file::durably_replace_private(&proof_path, &proof).unwrap();

    let lifecycle_path =
        control_dir.join(arc_crypto::secret_file::DESKTOP_LIFECYCLE_LOCK_FILE_NAME);
    arc_crypto::secret_file::durably_replace_private(
        &lifecycle_path,
        &arc_crypto::secret_file::desktop_lifecycle_lock_payload(session_nonce),
    )
    .unwrap();
    let owner_path =
        control_dir.join(arc_crypto::secret_file::DESKTOP_LIFECYCLE_OWNER_LOCK_FILE_NAME);
    arc_crypto::secret_file::durably_replace_private(
        &owner_path,
        arc_crypto::secret_file::desktop_lifecycle_owner_lock_payload(),
    )
    .unwrap();
    let owner = arc_crypto::secret_file::open_private_read_write(&owner_path).unwrap();
    owner.try_lock().unwrap();
    drop(data_namespace);
    owner
}

fn publish_private_request(
    control_dir: &std::path::Path,
    pid: u32,
    token: &str,
    receipt_nonce: &[u8; 32],
) {
    let staging = control_dir.join(format!(
        ".request.integration.{}.{}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let request = control_dir.join(REQUEST_FILE);
    let mut file = arc_crypto::secret_file::create_new_private(&staging).unwrap();
    write!(
        file,
        "{REQUEST_SCHEMA}\npid={pid}\ntoken={token}\nnonce={}\n",
        hex::encode(receipt_nonce)
    )
    .unwrap();
    file.sync_all().unwrap();
    drop(file);
    std::fs::hard_link(&staging, &request).unwrap();
    arc_crypto::secret_file::sync_parent_directory(&request).unwrap();
    std::fs::remove_file(staging).unwrap();
}

/// Exercises the packaged Windows lifecycle channel as an actual process, not
/// only through cfg/type checks. There is no test-only startup pause: a sparse
/// model artifact supplies a production-shaped synchronous hash phase while a
/// private request arrives at the data-dir lock boundary. On Windows the
/// fixture supplies the exact desktop namespace proof and live owner lease so
/// the node exercises the same no-rename handoff as the packaged application.
/// The lifecycle worker must consume the request, replay/open persistent state,
/// complete the WAL durability barrier required by the prearmed receipt, and
/// exit before network admission.
#[test]
fn private_desktop_request_stops_node_during_initialization() {
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("node-data");
    arc_crypto::secret_file::secure_private_directory(&data_dir).unwrap();
    let control_dir = data_dir.join(CONTROL_DIR);
    arc_crypto::secret_file::secure_private_directory(&control_dir).unwrap();
    let token_file = control_dir.join(TOKEN_FILE);
    let token = "7a".repeat(32);
    let token_bytes = [0x7a; 32];
    let mut token_handle = arc_crypto::secret_file::create_new_private(&token_file).unwrap();
    writeln!(token_handle, "{token}").unwrap();
    token_handle.sync_all().unwrap();
    drop(token_handle);
    let genesis_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../genesis.toml")
        .canonicalize()
        .unwrap();
    let receipt = arc_crypto::secret_file::arm_desktop_shutdown_receipt(
        &data_dir,
        &token_bytes,
        std::path::Path::new(env!("CARGO_BIN_EXE_arc-node")),
        &genesis_path,
    )
    .unwrap();
    #[cfg(windows)]
    let desktop_lifecycle_nonce = [0x5au8; 32];
    #[cfg(windows)]
    let desktop_lifecycle_owner =
        acquire_managed_lifecycle_fixture(&data_dir, &desktop_lifecycle_nonce);
    let model_path = temp.path().join("startup-hash-fixture.gguf");
    let model = std::fs::File::create(&model_path).unwrap();
    model.set_len(256 * 1024 * 1024).unwrap();
    drop(model);

    let mut command = Command::new(env!("CARGO_BIN_EXE_arc-node"));
    command
        .args([
            "--rpc",
            "127.0.0.1:0",
            "--p2p-port",
            "0",
            "--eth-rpc-port",
            "0",
            "--stake",
            "0",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("--desktop-shutdown-token-file")
        .arg(&token_file)
        .arg("--genesis")
        .arg(&genesis_path)
        .arg("--model")
        .arg(&model_path)
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .env("RUST_LOG", "arc=info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command
            .arg("--desktop-lifecycle-nonce")
            .arg(hex::encode(desktop_lifecycle_nonce));
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().expect("spawn arc-node lifecycle fixture");
    let child_pid = child.id();

    let lock_file = data_dir.join(".arc-node.lock");
    let startup_deadline = Instant::now() + Duration::from_secs(15);
    while !lock_file.is_file() && Instant::now() < startup_deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "node exited before acquiring its data-directory lock"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        lock_file.is_file(),
        "node did not reach the locked startup edge"
    );

    publish_private_request(&control_dir, child_pid, &token, &receipt.nonce);

    let exit_deadline = Instant::now() + STARTUP_SHUTDOWN_TEST_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= exit_deadline {
            timed_out = true;
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut logs = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut logs)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut logs)
        .unwrap();
    assert!(
        !timed_out,
        "node did not honor the authenticated startup shutdown request: {logs}"
    );
    assert!(status.success(), "node shutdown was not clean: {logs}");
    assert!(
        logs.contains("authenticated local desktop shutdown requested")
            && logs
                .contains("shutdown requested during initialization; persistent state is durable"),
        "node did not take the receipt-bound WAL-safe startup shutdown path: {logs}"
    );
    assert!(
        !logs.contains("Starting P2P transport") && !logs.contains("Listening on"),
        "network admission started after the startup shutdown edge: {logs}"
    );
    assert!(!control_dir.join(REQUEST_FILE).exists());
    #[cfg(windows)]
    drop(desktop_lifecycle_owner);
}

#[cfg(unix)]
#[test]
fn sigterm_is_armed_before_synchronous_initialization() {
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("signal-node-data");
    std::fs::create_dir(&data_dir).unwrap();
    let model_path = temp.path().join("signal-startup-hash-fixture.gguf");
    let model = std::fs::File::create(&model_path).unwrap();
    model.set_len(256 * 1024 * 1024).unwrap();
    drop(model);

    let mut child = Command::new(env!("CARGO_BIN_EXE_arc-node"))
        .args([
            "--rpc",
            "127.0.0.1:0",
            "--p2p-port",
            "0",
            "--eth-rpc-port",
            "0",
            "--stake",
            "0",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("--model")
        .arg(&model_path)
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .env("RUST_LOG", "arc=info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn arc-node SIGTERM startup fixture");

    let lock_file = data_dir.join(".arc-node.lock");
    let startup_deadline = Instant::now() + Duration::from_secs(15);
    while !lock_file.is_file() && Instant::now() < startup_deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "node exited before the signal-safe startup boundary"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(lock_file.is_file(), "node did not reach locked startup");

    let delivered = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("invoke the platform SIGTERM utility");
    assert!(delivered.success(), "could not deliver startup SIGTERM");
    let exit_deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "node did not honor SIGTERM during synchronous initialization"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut logs = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut logs)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut logs)
        .unwrap();
    assert!(status.success(), "SIGTERM shutdown was not clean: {logs}");
    assert!(
        logs.contains("shutdown requested before persistent state opened"),
        "SIGTERM did not reach the early WAL-safe startup gate: {logs}"
    );
    assert!(
        !logs.contains("Starting P2P transport") && !logs.contains("Listening on"),
        "network admission started after startup SIGTERM: {logs}"
    );
}
