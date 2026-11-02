//! Where the legacy install lives and where the bridge writes.
//!
//! The launcher always runs as `<ARC dir>/bin/arc-node` because that is the
//! only file v0.7 updaters replace. Everything the bridge creates lives under
//! `<ARC dir>/legacy-bridge/`:
//!
//! ```text
//! legacy-bridge/
//!   bridge.log                       append-only, no secrets
//!   releases/<tag>/...               pinned, verified v0.8 assets (shared cache)
//!   nodes/<kind>-<fingerprint>/      one per v0.7 data directory
//!     identity/validator-key.json    fresh Ed25519 identity, created by arc-cli
//!     data/                          fresh v0.8 data directory
//!     home/                          HOME for the node: hides model auto-discovery
//!     v0.7-data-archive-NNNN.json    stat-only record of the untouched v0.7 data
//!     bridge-state.json              what was started, and why
//!     compute-consent, compute-model.json  (headless only, operator-written)
//! ```
//!
//! For a headless install the v0.7 data directory is `<ARC dir>/data`, a
//! sibling of `legacy-bridge/`. The desktop's v0.7 data directory is the ARC
//! dir itself, so `bin/`, `models/`, `legacy-bridge/`, and the v0.8 desktop's
//! own `data-v3*` directories are excluded from its archive record.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::argv::{LegacyInvocation, LegacyKind};
use crate::exit;
use crate::hashing::sha256_bytes;

pub const BRIDGE_DIR_NAME: &str = "legacy-bridge";
pub const DESKTOP_EXCLUDED_TOP_LEVEL: &[&str] = &["bin", "models", BRIDGE_DIR_NAME];
pub const DESKTOP_EXCLUDED_PREFIX: &str = "data-v3";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub kind: LegacyKind,
    pub root: PathBuf,
    pub launcher: PathBuf,
    pub legacy_data_dir: PathBuf,
    pub legacy_data_dir_is_root: bool,
    pub bridge_dir: PathBuf,
    pub node_dir: PathBuf,
}

impl Layout {
    pub fn nodes_dir(&self) -> PathBuf {
        self.bridge_dir.join("nodes")
    }

    pub fn releases_dir(&self) -> PathBuf {
        self.bridge_dir.join("releases")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.bridge_dir.join("models")
    }

    pub fn log_file(&self) -> PathBuf {
        self.bridge_dir.join("bridge.log")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.bridge_dir.join(".bridge.lock")
    }

    pub fn identity_dir(&self) -> PathBuf {
        self.node_dir.join("identity")
    }

    pub fn keyfile(&self) -> PathBuf {
        self.identity_dir().join("validator-key.json")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.node_dir.join("data")
    }

    pub fn node_home(&self) -> PathBuf {
        self.node_dir.join("home")
    }

    pub fn state_file(&self) -> PathBuf {
        self.node_dir.join("bridge-state.json")
    }

    pub fn consent_file(&self) -> PathBuf {
        self.node_dir.join("compute-consent")
    }

    pub fn model_record(&self) -> PathBuf {
        self.node_dir.join("compute-model.json")
    }

    /// Top-level names inside the v0.7 data directory that are not v0.7 node
    /// state and are therefore left out of the archive record.
    pub fn is_excluded_top_level(&self, name: &str) -> bool {
        self.legacy_data_dir_is_root
            && (DESKTOP_EXCLUDED_TOP_LEVEL.contains(&name)
                || name.starts_with(DESKTOP_EXCLUDED_PREFIX))
    }
}

/// `<ARC dir>` for a launcher at `<ARC dir>/bin/arc-node[.exe]`.
pub fn root_for_launcher(launcher: &Path) -> Result<PathBuf> {
    let launcher = canonical_or_lexical(launcher);
    let file_name = launcher
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if file_name != "arc-node" && file_name != "arc-node.exe" {
        return Err(exit::refused(format!(
            "the bridge only runs as <ARC dir>/bin/arc-node, not as {file_name:?}"
        )));
    }
    let bin = launcher
        .parent()
        .ok_or_else(|| exit::refused("the bridge executable has no parent directory"))?;
    if bin.file_name().and_then(|name| name.to_str()) != Some("bin") {
        return Err(exit::refused(
            "the bridge only runs from a bin/ directory inside the ARC directory",
        ));
    }
    bin.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| exit::refused("the bin/ directory has no parent ARC directory"))
}

pub fn resolve(launcher: &Path, invocation: &LegacyInvocation, cwd: &Path) -> Result<Layout> {
    let root = root_for_launcher(launcher)?;
    let given = if invocation.data_dir.is_absolute() {
        invocation.data_dir.clone()
    } else {
        // v0.7 resolved a relative --data-dir against its working directory.
        cwd.join(&invocation.data_dir)
    };
    let legacy = canonical_or_lexical(&given);
    let legacy_data_dir_is_root = legacy == root;
    match invocation.kind {
        LegacyKind::Headless => {
            if cfg!(windows) {
                return Err(exit::refused(
                    "released v0.7 had no Windows headless installer; refusing an unrecognized layout",
                ));
            }
            if legacy != root.join("data") {
                return Err(exit::refused(format!(
                    "a v0.7 headless node keeps its data in <ARC dir>/data; {} is not {}",
                    legacy.display(),
                    root.join("data").display()
                )));
            }
        }
        LegacyKind::Desktop => {
            // The desktop default is "~/.arc". Windows builds without HOME set
            // resolved it to ".arc" relative to the app's working directory.
            let default_name = legacy.file_name().and_then(|name| name.to_str()) == Some(".arc");
            if !legacy_data_dir_is_root && !default_name {
                return Err(exit::refused(format!(
                    "{} is not the v0.7 desktop data directory",
                    legacy.display()
                )));
            }
        }
    }
    let bridge_dir = root.join(BRIDGE_DIR_NAME);
    let fingerprint = sha256_bytes(legacy.to_string_lossy().as_bytes());
    let node_dir = bridge_dir.join("nodes").join(format!(
        "{}-{}",
        invocation.kind.label(),
        &fingerprint[..12]
    ));
    Ok(Layout {
        kind: invocation.kind,
        root,
        launcher: canonical_or_lexical(launcher),
        legacy_data_dir: legacy,
        legacy_data_dir_is_root,
        bridge_dir,
        node_dir,
    })
}

/// Rebuild the layout of an already bridged node for an operator command.
pub fn from_existing_node(
    launcher: &Path,
    node_dir: &Path,
    legacy_data_dir: &Path,
) -> Result<Layout> {
    let root = root_for_launcher(launcher)?;
    let name = node_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("bridged node directory name is not UTF-8"))?;
    let kind = name
        .split_once('-')
        .and_then(|(label, _)| LegacyKind::from_label(label))
        .ok_or_else(|| anyhow!("{name} is not a bridged node directory"))?;
    let legacy = canonical_or_lexical(legacy_data_dir);
    Ok(Layout {
        kind,
        legacy_data_dir_is_root: legacy == root,
        bridge_dir: root.join(BRIDGE_DIR_NAME),
        launcher: canonical_or_lexical(launcher),
        legacy_data_dir: legacy,
        node_dir: node_dir.to_path_buf(),
        root,
    })
}

pub fn canonical_or_lexical(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return without_verbatim_prefix(canonical);
    }
    path.parent()
        .zip(path.file_name())
        .and_then(|(parent, name)| {
            fs::canonicalize(parent)
                .ok()
                .map(|parent| without_verbatim_prefix(parent).join(name))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

/// Windows canonicalization returns `\\?\C:\...`. Use the ordinary `C:\...`
/// spelling for drive paths so the node, arc-cli, and messages see the same
/// paths the v0.7 desktop used. Other platforms are unchanged.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy().into_owned();
        if let Some(rest) = text
            .strip_prefix(r"\\?\")
            .filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
        {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Create every directory the bridge owns, private to the current user.
///
/// The fresh v0.8 data directory is deliberately not created here: the node
/// creates it itself with its own durable, owner-only directory routine and
/// binds it to the network on first use (`genesis.network-hash`).
pub fn prepare(layout: &Layout) -> Result<()> {
    for dir in [
        layout.bridge_dir.clone(),
        layout.nodes_dir(),
        layout.releases_dir(),
        layout.node_dir.clone(),
        layout.identity_dir(),
        layout.node_home(),
    ] {
        ensure_private_dir(&dir)?;
    }
    Ok(())
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => match create_dir_private(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot create {}", path.display()));
            }
        },
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
        }
    }
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(exit::refused(format!(
            "{} must be a real directory, not a link or file",
            path.display()
        )));
    }
    check_owner_and_mode(path, &metadata)
}

#[cfg(unix)]
fn create_dir_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_dir_private(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn check_owner_and_mode(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        return Err(exit::refused(format!(
            "{} is owned by another user",
            path.display()
        )));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(exit::refused(format!(
            "{} is writable by other users",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_owner_and_mode(_path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

/// Write a file by renaming a fully written sibling over it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let temporary = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        io::Write::write_all(&mut file, bytes)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot flush {}", temporary.display()))?;
    }
    fs::rename(&temporary, path).with_context(|| format!("cannot replace {}", path.display()))
}

/// Holds the bridge-wide lock; released on drop (and on Unix exec, because
/// Rust opens files close-on-exec).
pub struct BridgeLock {
    _file: File,
}

pub fn lock(layout: &Layout, wait: Duration) -> Result<BridgeLock> {
    let path = layout.lock_file();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(BridgeLock { _file: file }),
            Err(fs::TryLockError::WouldBlock) => {
                if started.elapsed() >= wait {
                    return Err(exit::temporary(
                        "another bridge instance is still preparing this node",
                    ));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("cannot lock {}", path.display()));
            }
        }
    }
}

fn data_dir_spellings(layout: &Layout, invocation: &LegacyInvocation) -> Vec<String> {
    let mut spellings = vec![
        invocation.data_dir.to_string_lossy().into_owned(),
        layout.legacy_data_dir.to_string_lossy().into_owned(),
    ];
    spellings.dedup();
    spellings
}

/// True for a released v0.7 `arc-node` command line using one of `data_dirs`.
/// v0.7 always passed `--validator-seed`; the v0.8 node the bridge starts
/// never does, so a bridged process can never match.
pub fn is_legacy_node_command(args: &[String], data_dirs: &[String]) -> bool {
    let Some(program) = args.first() else {
        return false;
    };
    let base = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !base.starts_with("arc-node") {
        return false;
    }
    let has_seed = args
        .iter()
        .any(|arg| arg == "--validator-seed" || arg.starts_with("--validator-seed="));
    if !has_seed {
        return false;
    }
    let separate = args
        .windows(2)
        .any(|pair| pair[0] == "--data-dir" && data_dirs.contains(&pair[1]));
    let joined = args.iter().any(|arg| {
        arg.strip_prefix("--data-dir=")
            .is_some_and(|value| data_dirs.iter().any(|dir| dir == value))
    });
    separate || joined
}

/// Best-effort check that no v0.7 node still runs on the legacy data
/// directory. Supervisors stop the old process before they start the bridge
/// in its place; this catches a second, unsupervised copy.
#[cfg(target_os = "linux")]
pub fn live_legacy_node(layout: &Layout, invocation: &LegacyInvocation) -> Option<u32> {
    let own = std::process::id();
    let data_dirs = data_dir_spellings(layout, invocation);
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        if is_legacy_node_command(&args, &data_dirs) {
            return Some(pid);
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub fn live_legacy_node(layout: &Layout, invocation: &LegacyInvocation) -> Option<u32> {
    let own = std::process::id();
    let data_dirs = data_dir_spellings(layout, invocation);
    let output = std::process::Command::new("/bin/ps")
        .args(["-axww", "-o", "pid=", "-o", "command="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let line = line.trim_start();
        let Some((pid, command)) = line.split_once(' ') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if pid == own {
            continue;
        }
        let args: Vec<String> = command.split_whitespace().map(str::to_string).collect();
        if is_legacy_node_command(&args, &data_dirs) {
            return Some(pid);
        }
    }
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn live_legacy_node(layout: &Layout, invocation: &LegacyInvocation) -> Option<u32> {
    // The v0.7 desktop stops its child before starting another, and released
    // v0.7 had no Windows service. Nothing else can hold this data directory.
    let _ = data_dir_spellings(layout, invocation);
    None
}

#[cfg(test)]
pub mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A unique, canonical temporary directory removed on drop.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> TempDir {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "arc-legacy-bridge-{label}-{}-{sequence}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(std::fs::canonicalize(&path).unwrap())
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `<root>/bin/arc-node[.exe]` with an empty launcher file.
    pub fn fake_arc_dir(root: &Path) -> PathBuf {
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let launcher = bin.join(if cfg!(windows) {
            "arc-node.exe"
        } else {
            "arc-node"
        });
        std::fs::write(&launcher, b"launcher").unwrap();
        launcher
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{TempDir, fake_arc_dir};
    use super::*;
    use crate::exit::{EX_CONFIG, EX_TEMPFAIL, code_for};
    use std::net::SocketAddr;

    fn invocation(kind: LegacyKind, data_dir: &Path) -> LegacyInvocation {
        LegacyInvocation {
            kind,
            rpc: "127.0.0.1:9944".parse::<SocketAddr>().unwrap(),
            p2p_port: 9945,
            data_dir: data_dir.to_path_buf(),
            seeds_file: None,
            genesis: None,
            model: None,
            community_mode: false,
        }
    }

    #[test]
    fn desktop_layout_uses_the_arc_dir_and_excludes_non_state() {
        let temp = TempDir::new("layout-desktop");
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);
        let layout = resolve(
            &launcher,
            &invocation(LegacyKind::Desktop, &root),
            temp.path(),
        )
        .unwrap();
        assert_eq!(layout.root, canonical_or_lexical(&root));
        assert!(layout.legacy_data_dir_is_root);
        assert!(
            layout
                .node_dir
                .starts_with(layout.root.join("legacy-bridge").join("nodes"))
        );
        assert!(
            layout
                .node_dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("desktop-")
        );
        assert!(layout.is_excluded_top_level("bin"));
        assert!(layout.is_excluded_top_level("legacy-bridge"));
        assert!(layout.is_excluded_top_level("data-v3-1"));
        assert!(!layout.is_excluded_top_level("state.wal"));
        assert!(!layout.is_excluded_top_level("dag-wal"));
    }

    #[test]
    fn relative_desktop_data_dir_resolves_against_the_working_directory() {
        let temp = TempDir::new("layout-relative");
        let root = temp.path().join("home").join(".arc");
        let launcher = fake_arc_dir(&root);
        let cwd = temp.path().join("app");
        fs::create_dir_all(cwd.join(".arc")).unwrap();
        let layout = resolve(
            &launcher,
            &invocation(LegacyKind::Desktop, Path::new(".arc")),
            &cwd,
        )
        .unwrap();
        assert_eq!(
            layout.legacy_data_dir,
            canonical_or_lexical(&cwd.join(".arc"))
        );
        assert!(!layout.legacy_data_dir_is_root);
        assert!(!layout.is_excluded_top_level("bin"));
    }

    #[cfg(unix)]
    #[test]
    fn headless_layout_requires_the_installer_data_dir() {
        let temp = TempDir::new("layout-headless");
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);
        fs::create_dir_all(root.join("data")).unwrap();
        let layout = resolve(
            &launcher,
            &invocation(LegacyKind::Headless, &root.join("data")),
            temp.path(),
        )
        .unwrap();
        assert!(!layout.legacy_data_dir_is_root);
        assert!(!layout.node_dir.starts_with(&layout.legacy_data_dir));
        let other = resolve(
            &launcher,
            &invocation(LegacyKind::Headless, &temp.path().join("elsewhere")),
            temp.path(),
        )
        .unwrap_err();
        assert_eq!(code_for(&other), EX_CONFIG);
    }

    #[test]
    fn launcher_must_be_bin_arc_node() {
        let temp = TempDir::new("layout-launcher");
        let stray = temp.path().join("arc-node");
        fs::write(&stray, b"x").unwrap();
        let error = resolve(
            &stray,
            &invocation(LegacyKind::Desktop, temp.path()),
            temp.path(),
        )
        .unwrap_err();
        assert_eq!(code_for(&error), EX_CONFIG);
        let renamed = temp.path().join("bin").join("arc-node-new");
        fs::create_dir_all(renamed.parent().unwrap()).unwrap();
        fs::write(&renamed, b"x").unwrap();
        assert!(root_for_launcher(&renamed).is_err());
    }

    #[test]
    fn prepare_creates_private_dirs_and_lock_excludes_a_second_holder() {
        let temp = TempDir::new("layout-prepare");
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);
        let layout = resolve(
            &launcher,
            &invocation(LegacyKind::Desktop, &root),
            temp.path(),
        )
        .unwrap();
        prepare(&layout).unwrap();
        prepare(&layout).unwrap();
        for dir in [layout.identity_dir(), layout.node_home()] {
            assert!(dir.is_dir(), "{}", dir.display());
        }
        assert!(
            !layout.data_dir().exists(),
            "the node creates its own data dir"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&layout.node_dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let held = lock(&layout, Duration::from_secs(1)).unwrap();
        let second = lock(&layout, Duration::from_millis(300)).err().unwrap();
        assert_eq!(code_for(&second), EX_TEMPFAIL);
        drop(held);
        assert!(lock(&layout, Duration::from_secs(1)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_refuses_a_symlinked_bridge_dir() {
        let temp = TempDir::new("layout-symlink");
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(BRIDGE_DIR_NAME)).unwrap();
        let layout = resolve(
            &launcher,
            &invocation(LegacyKind::Desktop, &root),
            temp.path(),
        )
        .unwrap();
        assert_eq!(code_for(&prepare(&layout).unwrap_err()), EX_CONFIG);
    }

    #[test]
    fn legacy_process_matching() {
        let dirs = vec!["/home/ops/.arc/data".to_string()];
        let v07 = |extra: &[&str]| -> Vec<String> {
            let mut args = vec![
                "/home/ops/.arc/bin/arc-node".to_string(),
                "--validator-seed".to_string(),
                "community-x".to_string(),
            ];
            args.extend(extra.iter().map(|value| value.to_string()));
            args
        };
        assert!(is_legacy_node_command(
            &v07(&["--data-dir", "/home/ops/.arc/data"]),
            &dirs
        ));
        assert!(is_legacy_node_command(
            &v07(&["--data-dir=/home/ops/.arc/data"]),
            &dirs
        ));
        assert!(!is_legacy_node_command(
            &v07(&["--data-dir", "/home/ops/.arc/other"]),
            &dirs
        ));
        let bridged = vec![
            "/home/ops/.arc/legacy-bridge/releases/v0.8.10/arc-node-linux-x86_64".to_string(),
            "--data-dir".to_string(),
            "/home/ops/.arc/data".to_string(),
        ];
        assert!(!is_legacy_node_command(&bridged, &dirs));
        let other_program = vec![
            "/usr/bin/vim".to_string(),
            "--validator-seed".to_string(),
            "--data-dir".to_string(),
            "/home/ops/.arc/data".to_string(),
        ];
        assert!(!is_legacy_node_command(&other_program, &dirs));
    }

    #[test]
    fn atomic_write_replaces() {
        let temp = TempDir::new("layout-atomic");
        let path = temp.path().join("state.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
    }
}
