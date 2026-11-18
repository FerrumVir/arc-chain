//! Per-user desktop lease and bounded, authenticated argument handoff.
//! Registered first, before any plugin or application setup can mutate state.
//! The lease is a Tauri resource: cleanup_before_exit releases it even when
//! restart skips RunEvent::Exit. No persistent socket pathname is unlinked.
use fs2::FileExt;
use std::{
    fs::File,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tauri::{Emitter, Manager};

const TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_CONNECTIONS: usize = 8;
const MAX_MESSAGE: u64 = 64 * 1024;
static CLOSING: AtomicBool = AtomicBool::new(false);

pub fn closing() -> bool {
    CLOSING.load(Ordering::SeqCst)
}
pub fn begin_closing(app: &tauri::AppHandle) {
    CLOSING.store(true, Ordering::SeqCst);
    let _ = app.emit("desktop-closing", true);
}

pub fn should_show(args: &[String]) -> bool {
    !args.iter().any(|arg| arg == "--minimized")
}

/// All explicit reopen routes use this order and the same wake signal.
pub fn show_main(app: &tauri::AppHandle) {
    if app.try_state::<StartupFailure>().is_some() {
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        crate::wake::emit_signal(app, crate::wake::SignalReason::Shown);
        if closing() {
            let _ = app.emit("desktop-closing", true);
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Endpoint {
    port: u16,
    token: String,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Handoff {
    token: String,
    args: Vec<String>,
}

struct Lease {
    _file: File,
    stopped: Arc<AtomicBool>,
}
impl tauri::Resource for Lease {}
impl Drop for Lease {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}
enum Entry {
    Owner(Lease, TcpListener, String),
    Forwarded,
}

fn enter(app_data_dir: &Path, args: &[String]) -> anyhow::Result<Entry> {
    use arc_crypto::secret_file as private;
    // Tauri's per-user app-data ancestry can legitimately contain symlinks
    // (e.g. /var on macOS or a relocated home). Resolve only that ancestry,
    // including on first launch when the app-data directory does not exist.
    // Never canonicalize desktop-session itself: its no-link, owner and
    // private-permission checks must run before any capability is published.
    std::fs::create_dir_all(app_data_dir)?;
    let dir = app_data_dir.canonicalize()?.join("desktop-session");
    private::secure_private_directory_tree(&dir)?;
    let path = dir.join("desktop-instance.lock");
    private::durably_publish_new_private(&path, b"")?;
    let file = private::open_private_read_write(&path)?;
    let endpoint_path = dir.join("desktop-instance.json");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
                listener.set_nonblocking(true)?;
                let token = hex::encode(rand::random::<[u8; 32]>());
                let endpoint = Endpoint {
                    port: listener.local_addr()?.port(),
                    token: token.clone(),
                };
                // Windows mandatory byte locks also deny reads: publish the
                // handoff endpoint separately from the locked lease file.
                private::durably_replace_private(&endpoint_path, &serde_json::to_vec(&endpoint)?)?;
                return Ok(Entry::Owner(
                    Lease {
                        _file: file,
                        stopped: Arc::new(AtomicBool::new(false)),
                    },
                    listener,
                    token,
                ));
            }
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                // The owner may still be publishing its endpoint, or exiting.
                // Only an authenticated acknowledgement permits this copy to exit 0.
                let mut bytes = Vec::new();
                if let Ok(endpoint_file) = private::open_private(&endpoint_path) {
                    let _ = endpoint_file.take(MAX_MESSAGE).read_to_end(&mut bytes);
                }
                if let Ok(endpoint) = serde_json::from_slice::<Endpoint>(&bytes) {
                    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
                    if let Ok(mut stream) =
                        TcpStream::connect_timeout(&address, Duration::from_millis(200))
                    {
                        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
                        let message = serde_json::to_vec(&Handoff {
                            token: endpoint.token,
                            args: args.to_vec(),
                        })?;
                        anyhow::ensure!(
                            message.len() < MAX_MESSAGE as usize,
                            "desktop launch arguments are too large"
                        );
                        if stream.write_all(&message).is_ok()
                            && stream.shutdown(std::net::Shutdown::Write).is_ok()
                        {
                            let mut ack = [0];
                            if stream.read_exact(&mut ack).is_ok() && ack == [1] {
                                return Ok(Entry::Forwarded);
                            }
                        }
                    }
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "ARC is still opening or closing. Try opening it again in a few seconds."
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn receive(
    mut stream: TcpStream,
    token: &str,
    mut callback: impl FnMut(Vec<String>),
) -> anyhow::Result<()> {
    // Accepted sockets can inherit the listener's nonblocking flag.
    stream.set_nonblocking(false)?;
    let deadline = Instant::now() + CONNECTION_TIMEOUT;
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow::anyhow!("desktop handoff deadline exceeded"))?;
        stream.set_read_timeout(Some(remaining))?;
        let mut chunk = [0u8; 4096];
        let size = stream.read(&mut chunk)?;
        if size == 0 {
            break;
        }
        anyhow::ensure!(
            bytes.len() + size < MAX_MESSAGE as usize,
            "desktop handoff too large"
        );
        bytes.extend_from_slice(&chunk[..size]);
    }
    anyhow::ensure!(
        Instant::now() < deadline,
        "desktop handoff deadline exceeded"
    );
    let message: Handoff = serde_json::from_slice(&bytes)?;
    // BLAKE3 Hash equality uses constant_time_eq_32. The protocol token is
    // fixed-size ASCII hex; reject other lengths before hashing/comparing.
    anyhow::ensure!(
        message.token.len() == 64 && token.len() == 64,
        "invalid desktop handoff"
    );
    anyhow::ensure!(
        blake3::hash(message.token.as_bytes()) == blake3::hash(token.as_bytes()),
        "invalid desktop handoff"
    );
    callback(message.args);
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| anyhow::anyhow!("desktop handoff deadline exceeded"))?;
    stream.set_write_timeout(Some(remaining))?;
    stream.write_all(&[1])?;
    Ok(())
}

/// A slow unauthenticated peer cannot monopolize the accept loop. Both the
/// number of workers and each worker's total connection lifetime are bounded.
fn serve(
    listener: TcpListener,
    token: String,
    stopped: Arc<AtomicBool>,
    callback: impl Fn(Vec<String>) + Send + Sync + 'static,
) {
    let active = Arc::new(AtomicUsize::new(0));
    let callback = Arc::new(callback);
    while !stopped.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    drop(stream);
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let active = active.clone();
                let callback = callback.clone();
                let token = token.clone();
                let stopped = stopped.clone();
                std::thread::spawn(move || {
                    let _ = receive(stream, &token, |args| {
                        if !stopped.load(Ordering::SeqCst) {
                            callback(args);
                        }
                    });
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

struct StartupFailure(String);

/// Called by application setup only after the dialog plugin is initialized.
/// Return before loading the store, installing a tray or reconciling a node.
/// Use an asynchronous native dialog so the main event loop remains available.
pub fn show_startup_failure(app: &tauri::AppHandle) -> bool {
    use tauri_plugin_dialog::DialogExt;
    let Some(failure) = app.try_state::<StartupFailure>() else {
        return false;
    };
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    let handle = app.clone();
    app.dialog()
        .message(&failure.0)
        .title("ARC could not open")
        .kind(tauri_plugin_dialog::MessageDialogKind::Error)
        .show(move |_| handle.exit(1));
    true
}

pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("single-instance")
        .setup(|app, _| {
            let args: Vec<String> = std::env::args_os()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            let entry = app
                .path()
                .app_data_dir()
                .map_err(anyhow::Error::from)
                .and_then(|dir| enter(&dir, &args));
            match entry {
                Ok(Entry::Forwarded) => std::process::exit(0),
                Ok(Entry::Owner(lease, listener, token)) => {
                    let stopped = lease.stopped.clone();
                    app.resources_table().add(lease);
                    let app = app.clone();
                    std::thread::spawn(move || {
                        serve(listener, token, stopped, move |args| {
                            if should_show(&args) {
                                let handle = app.clone();
                                let _ = app.run_on_main_thread(move || show_main(&handle));
                            }
                        })
                    });
                }
                Err(error) => {
                    // Propagating this through plugin setup would panic in
                    // Builder::build, silently in Windows GUI builds.
                    app.manage(StartupFailure(crate::paths::redact_home_text(
                        &error.to_string(),
                    )));
                }
            }
            Ok(())
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_launch_does_not_raise_window() {
        assert!(!should_show(&["ARC".into(), "--minimized".into()]));
        assert!(should_show(&["ARC".into()]));
    }
    #[test]
    fn handoff_preserves_arguments_and_lease_reopens_after_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let Entry::Owner(lease, listener, token) = enter(dir.path(), &[]).unwrap() else {
            panic!()
        };
        let stopped = lease.stopped.clone();
        let (send, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve(listener, token, stopped, move |args| {
                send.send(args).unwrap();
            })
        });
        let args = vec![
            "ARC".into(),
            "--minimized".into(),
            "argument with spaces".into(),
        ];
        assert!(matches!(
            enter(dir.path(), &args).unwrap(),
            Entry::Forwarded
        ));
        assert_eq!(received.recv_timeout(TIMEOUT).unwrap(), args);
        // A lease has no owner outside Tauri's resource table. Dropping
        // that table exercises the same resource drop as its exit cleanup.
        let mut resources = tauri::ResourceTable::default();
        resources.add(lease);
        drop(resources);
        worker.join().unwrap();
        assert!(matches!(enter(dir.path(), &[]).unwrap(), Entry::Owner(..)));
    }
    #[test]
    fn child_handoff() {
        let Ok(dir) = std::env::var("ARC_INSTANCE_TEST_DIRECTORY") else {
            return;
        };
        assert!(matches!(
            enter(Path::new(&dir), &["ARC".into(), "--minimized".into()]).unwrap(),
            Entry::Forwarded
        ));
    }

    #[test]
    fn second_process_exits_successfully_without_taking_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let Entry::Owner(lease, listener, token) = enter(dir.path(), &[]).unwrap() else {
            panic!()
        };
        let stopped = lease.stopped.clone();
        let (send, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve(listener, token, stopped, move |args| {
                send.send(args).unwrap();
            })
        });
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "desktop_instance::tests::child_handoff",
                "--nocapture",
            ])
            .env("ARC_INSTANCE_TEST_DIRECTORY", dir.path())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("handoff timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            received.recv_timeout(TIMEOUT).unwrap(),
            vec!["ARC", "--minimized"]
        );
        drop(lease);
        worker.join().unwrap();
    }

    #[test]
    fn slow_and_unauthenticated_peers_do_not_block_legitimate_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let Entry::Owner(lease, listener, token) = enter(dir.path(), &[]).unwrap() else {
            panic!()
        };
        let address = listener.local_addr().unwrap();
        let stopped = lease.stopped.clone();
        let (send, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve(listener, token, stopped, move |args| {
                send.send(args).unwrap();
            })
        });
        let mut invalid = TcpStream::connect(address).unwrap();
        invalid
            .write_all(
                &serde_json::to_vec(&Handoff {
                    token: "0".repeat(64),
                    args: vec!["unauthenticated".into()],
                })
                .unwrap(),
            )
            .unwrap();
        invalid.shutdown(std::net::Shutdown::Write).unwrap();
        invalid.set_read_timeout(Some(TIMEOUT)).unwrap();
        assert!(invalid.read_exact(&mut [0]).is_err());
        let mut slow = TcpStream::connect(address).unwrap();
        slow.write_all(b"{").unwrap();
        let drip = std::thread::spawn(move || {
            let started = Instant::now();
            slow.set_write_timeout(Some(CONNECTION_TIMEOUT)).unwrap();
            slow.set_read_timeout(Some(TIMEOUT)).unwrap();
            // All gaps are below the old per-read timeout. The overall
            // deadline must close the connection despite continued input.
            for _ in 0..8 {
                std::thread::sleep(Duration::from_millis(100));
                if slow.write_all(b" ").is_err() {
                    break;
                }
            }
            assert!(slow.read_exact(&mut [0]).is_err());
            assert!(started.elapsed() < Duration::from_secs(2));
        });
        assert!(matches!(
            enter(dir.path(), &["legitimate".into()]).unwrap(),
            Entry::Forwarded
        ));
        assert_eq!(received.recv_timeout(TIMEOUT).unwrap(), vec!["legitimate"]);
        drip.join().unwrap();
        assert!(received.try_recv().is_err());
        drop(lease);
        worker.join().unwrap();
    }

    #[test]
    fn first_launch_creates_private_session_under_missing_app_data() {
        let root = tempfile::tempdir().unwrap();
        eprintln!("OS temp directory: {}", std::env::temp_dir().display());
        eprintln!(
            "temp ancestry contains symlink: {}",
            root.path().ancestors().any(|path| {
                std::fs::symlink_metadata(path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
            })
        );
        let app_data = root.path().join("new-home/app-data/ARC");
        let _owner = enter(&app_data, &[]).unwrap();
        let session = app_data.join("desktop-session");
        arc_crypto::secret_file::validate_private_directory(&session).unwrap();
        for name in ["desktop-instance.lock", "desktop-instance.json"] {
            // Windows denies reads of the held lock; its creation/open path
            // already validates it. Inspect the endpoint on every platform.
            if cfg!(windows) && name.ends_with(".lock") {
                continue;
            }
            arc_crypto::secret_file::open_private(&session.join(name)).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn linked_home_and_app_data_share_the_canonical_owners_lease() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("real-home");
        std::fs::create_dir(&home).unwrap();
        let linked_home = root.path().join("home");
        symlink(&home, &linked_home).unwrap();
        let app_data = home.join("app-data/ARC");
        // First launch through a symlinked home, with missing app-data dirs.
        let Entry::Owner(lease, listener, token) =
            enter(&linked_home.join("app-data/ARC"), &[]).unwrap()
        else {
            panic!()
        };
        let linked_app_data = root.path().join("linked-app-data");
        symlink(&app_data, &linked_app_data).unwrap();
        let stopped = lease.stopped.clone();
        let (send, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve(listener, token, stopped, move |args| {
                send.send(args).unwrap();
            })
        });
        for path in [&app_data, &linked_app_data] {
            assert!(matches!(
                enter(path, &["reopen".into()]).unwrap(),
                Entry::Forwarded
            ));
            assert_eq!(received.recv_timeout(TIMEOUT).unwrap(), vec!["reopen"]);
        }
        drop(lease);
        worker.join().unwrap();
        assert!(matches!(enter(&app_data, &[]).unwrap(), Entry::Owner(..)));
    }

    #[cfg(unix)]
    #[test]
    fn linked_private_session_is_rejected_without_touching_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(outside.join("sentinel"), b"unchanged").unwrap();
        symlink(&outside, root.path().join("desktop-session")).unwrap();
        assert!(enter(root.path(), &[]).is_err());
        assert_eq!(
            std::fs::read(outside.join("sentinel")).unwrap(),
            b"unchanged"
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn non_directory_private_session_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("desktop-session");
        std::fs::write(&session, b"unchanged").unwrap();
        assert!(enter(root.path(), &[]).is_err());
        assert_eq!(std::fs::read(session).unwrap(), b"unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn existing_private_session_is_tightened_and_linked_lock_is_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("desktop-session");
        std::fs::create_dir(&session).unwrap();
        std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o755)).unwrap();
        let outside = root.path().join("outside-lock");
        std::fs::write(&outside, b"unchanged").unwrap();
        let mode = std::fs::metadata(&outside).unwrap().permissions().mode();
        symlink(&outside, session.join("desktop-instance.lock")).unwrap();
        assert!(enter(root.path(), &[]).is_err());
        arc_crypto::secret_file::validate_private_directory(&session).unwrap();
        assert_eq!(
            std::fs::metadata(&session).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode(),
            mode
        );
        assert!(!session.join("desktop-instance.json").exists());
    }

    #[test]
    fn different_profiles_do_not_share_a_lease() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _first = enter(a.path(), &[]).unwrap();
        assert!(matches!(enter(b.path(), &[]).unwrap(), Entry::Owner(..)));
    }
}
