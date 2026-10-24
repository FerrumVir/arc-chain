//! Node data left behind by an earlier ARC install whose app data is gone.
//!
//! Tauri's stock Windows uninstaller offers a "Delete the application data"
//! checkbox. Ticked during a manual "uninstall before installing" upgrade, it
//! removes `%APPDATA%\network.arc.desktop` (store.json with the identity and
//! config, and the validator keyfile) but leaves the node's data directory,
//! `~/.arc/data-v3`, in place with the previous install's desktop lifecycle
//! files. The new install then launches with no identity next to that data.
//! Launch-time reconciliation can still drain a node that is running there,
//! but when that node is already gone and its shutdown receipt is still
//! armed, the one recovery launch that would clear it needs an identity, so
//! reconciliation failed with "no identity - run onboarding" and every Start
//! was refused until ARC restarted.
//!
//! When the directory provably came from a desktop that managed a node there
//! and this install's app data did not exist at launch, nothing of this
//! install can own it: a node needs the stored identity to start. It is set
//! aside by renaming it beside itself (`data-v3.previous-install-<UTC>`),
//! never deleted, and the install starts fresh. The gate is the store's
//! absence, not an empty identity: a store that is present but could not be
//! read also loads empty (`store::StoreLoadOutcome::Failed`), and that
//! install owns its directory and must never have its store written over.
//! Every other guard stays: a directory a live node or another ARC desktop
//! holds, a linked path, a doubt of any kind leaves it in place and today's
//! reconciliation runs as before.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use fs2::FileExt as _;

use crate::node_manager;
use crate::paths;
use crate::store::StoreLoadOutcome;

/// Inserted between the directory's name and the UTC stamp of the set-aside:
/// `data-v3.previous-install-20261024T141500Z`.
pub const SET_ASIDE_INFIX: &str = ".previous-install-";
const SET_ASIDE_MAX_ATTEMPTS: u32 = 1_000;

/// What launch decided about the configured node data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Leave it: this install's app data existed at launch (readable or
    /// not), the directory is absent, or no desktop ever managed a node
    /// there.
    Keep,
    /// Evidence of a previous install, but a live holder or a doubt. Today's
    /// reconciliation runs unchanged; the reason is for the log.
    Held(String),
    /// Renamed from `from` to `to`, with every byte kept.
    SetAside { from: PathBuf, to: PathBuf },
}

/// Whether launch may set a previous install's directory aside, from how the
/// identity store loaded. Only a store that was absent qualifies: an empty
/// store that stands in for one that could not be read belongs to a live
/// install, and the notice such a move records would be written over the
/// unread identity.
pub fn app_data_was_absent(outcome: StoreLoadOutcome) -> bool {
    outcome == StoreLoadOutcome::Absent
}

/// Set the configured data directory aside when it provably belongs to a
/// previous install that no longer exists on this computer and nothing live
/// holds it. `app_data_absent` is [`app_data_was_absent`] for this launch's
/// store load; when it is false the directory is kept, whatever is in it.
pub fn set_aside_if_previous_install(
    data_dir: &Path,
    app_data_absent: bool,
    now: SystemTime,
) -> Decision {
    if !app_data_absent {
        return Decision::Keep;
    }
    match try_set_aside(data_dir, now) {
        Ok(decision) => decision,
        Err(reason) => Decision::Held(reason),
    }
}

fn try_set_aside(data_dir: &Path, now: SystemTime) -> Result<Decision, String> {
    match std::fs::symlink_metadata(data_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "the node data path is a link or not a directory: {}",
                    paths::display_with_home_redacted(data_dir)
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Decision::Keep),
        Err(error) => {
            return Err(format!(
                "cannot inspect the node data directory {}: {error}",
                paths::display_with_home_redacted(data_dir)
            ));
        }
    }
    // Links anywhere on the configured path are refused exactly as the
    // lifecycle lock refuses them; the rename below must never act through
    // one.
    node_manager::validate_configured_directory_ancestry_no_link(data_dir)
        .map_err(|error| error.to_string())?;
    if !left_by_a_desktop_managed_node(data_dir)? {
        return Ok(Decision::Keep);
    }

    // The same sibling lock every desktop takes before it publishes, moves
    // or rebarriers this directory name. Taken without waiting: a desktop
    // that is working on the directory right now is a live holder.
    let namespace = arc_crypto::secret_file::try_acquire_private_directory_namespace_lock(data_dir)
        .map_err(|error| {
            let shown = paths::display_with_home_redacted(data_dir);
            if error.kind() == std::io::ErrorKind::WouldBlock {
                format!(
                    "another ARC desktop is working on the node data directory {shown} right now"
                )
            } else {
                format!("cannot lock the node data directory namespace {shown}: {error}")
            }
        })?;
    namespace.restore_interrupted().map_err(|error| {
        format!(
            "cannot restore the node data directory namespace {}: {error}",
            paths::display_with_home_redacted(data_dir)
        )
    })?;
    let canonical = data_dir.canonicalize().map_err(|error| {
        format!(
            "cannot canonicalize the node data directory {}: {error}",
            paths::display_with_home_redacted(data_dir)
        )
    })?;
    ensure_nothing_live_holds(&canonical)?;

    let to = set_aside_target(data_dir, now)?;
    // Every probe handle above is closed: Windows refuses to rename a
    // directory while a file beneath it is open.
    std::fs::rename(data_dir, &to).map_err(|error| {
        format!(
            "could not set aside the node data directory {}: {error}",
            paths::display_with_home_redacted(data_dir)
        )
    })?;
    if let Err(error) = arc_crypto::secret_file::sync_parent_directory(&to) {
        tracing::warn!(
            %error,
            "the set-aside rename is visible but its parent directory could not be synced"
        );
    }
    drop(namespace);
    Ok(Decision::SetAside {
        from: data_dir.to_path_buf(),
        to,
    })
}

/// Only a desktop that spawned arc-node there writes the private shutdown
/// token and the exact executable identity. A fresh install's own launch
/// publishes the lifecycle locks and the namespace proof before onboarding,
/// so those are not evidence, and a node run by hand never has the control
/// directory at all.
fn left_by_a_desktop_managed_node(data_dir: &Path) -> Result<bool, String> {
    let control_dir = data_dir.join(arc_crypto::secret_file::DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
    match std::fs::symlink_metadata(&control_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "the desktop control path is a link or not a directory: {}",
                    paths::display_with_home_redacted(&control_dir)
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "cannot inspect the desktop control directory {}: {error}",
                paths::display_with_home_redacted(&control_dir)
            ));
        }
    }
    let mut found = false;
    for name in [
        node_manager::DESKTOP_SHUTDOWN_TOKEN_FILE_NAME,
        node_manager::DESKTOP_EXECUTABLE_IDENTITY_FILE_NAME,
    ] {
        let path = control_dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "a desktop control file is a link: {}",
                        paths::display_with_home_redacted(&path)
                    ));
                }
                found |= metadata.is_file();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect the desktop control file {}: {error}",
                    paths::display_with_home_redacted(&path)
                ));
            }
        }
    }
    Ok(found)
}

/// The three holders a live ARC leaves: another desktop's lifecycle owner
/// lock, arc-node's own exclusive data-directory lock, and a process whose
/// command line names this directory. All three must be absent.
fn ensure_nothing_live_holds(canonical: &Path) -> Result<(), String> {
    let shown = paths::display_with_home_redacted(canonical);
    let owner_lock = canonical
        .join(arc_crypto::secret_file::DESKTOP_SHUTDOWN_CONTROL_DIR_NAME)
        .join(arc_crypto::secret_file::DESKTOP_LIFECYCLE_OWNER_LOCK_FILE_NAME);
    match arc_crypto::secret_file::open_owned_private_read_write_repairing(&owner_lock) {
        Ok(file) => match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = fs2::FileExt::unlock(&file);
            }
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                return Err(format!(
                    "another ARC desktop currently owns the managed node lifecycle for {shown}"
                ));
            }
            Err(error) => {
                return Err(format!(
                    "cannot check the managed node lifecycle owner lock for {shown}: {error}"
                ));
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot open the managed node lifecycle owner lock for {shown}: {error}"
            ));
        }
    }
    match node_manager::node_data_directory_held(canonical) {
        Ok(false) => {}
        Ok(true) => return Err(format!("a node still holds the data directory {shown}")),
        Err(error) => {
            return Err(format!(
                "cannot check whether a node holds the data directory {shown}: {error}"
            ));
        }
    }
    if let Some(pid) = process_naming_directory(canonical) {
        return Err(format!(
            "process {pid} still names the data directory {shown} on its command line"
        ));
    }
    Ok(())
}

/// A process launched against this directory: arc-node's `--data-dir`, or
/// the desktop shutdown token beneath it.
fn process_naming_directory(canonical: &Path) -> Option<sysinfo::Pid> {
    let mut system = sysinfo::System::new();
    node_manager::refresh_process_command_metadata(&mut system);
    let data_flag = std::ffi::OsStr::new("--data-dir");
    let token_flag = std::ffi::OsStr::new("--desktop-shutdown-token-file");
    for process in system.processes().values() {
        for pair in process.cmd().windows(2) {
            if pair[0].as_os_str() != data_flag && pair[0].as_os_str() != token_flag {
                continue;
            }
            let Some(path) = node_manager::canonical_process_path(&pair[1], process.cwd()) else {
                continue;
            };
            if path == canonical || path.starts_with(canonical) {
                return Some(process.pid());
            }
        }
    }
    None
}

fn set_aside_target(data_dir: &Path, now: SystemTime) -> Result<PathBuf, String> {
    let parent = data_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "the node data directory has no parent".to_string())?;
    let leaf = data_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "the node data directory name is not valid UTF-8".to_string())?;
    let stamp = utc_stamp(now);
    for attempt in 1..=SET_ASIDE_MAX_ATTEMPTS {
        let name = if attempt == 1 {
            format!("{leaf}{SET_ASIDE_INFIX}{stamp}")
        } else {
            format!("{leaf}{SET_ASIDE_INFIX}{stamp}-{attempt}")
        };
        let candidate = parent.join(name);
        match std::fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(error) => {
                return Err(format!(
                    "cannot inspect the set-aside name {}: {error}",
                    paths::display_with_home_redacted(&candidate)
                ));
            }
        }
    }
    Err(format!(
        "too many set-aside directories already beside {}",
        paths::display_with_home_redacted(data_dir)
    ))
}

/// `20261024T141500Z`: sortable, no characters a file system refuses.
pub fn utc_stamp(now: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(now)
        .format("%Y%m%dT%H%M%SZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::types::{Identity, NodeConfig};
    use fs2::FileExt as _;
    use std::time::{Duration, UNIX_EPOCH};

    const CONTROL: &str = arc_crypto::secret_file::DESKTOP_SHUTDOWN_CONTROL_DIR_NAME;
    /// 2026-10-24T14:15:00Z.
    const NOW_SECS: u64 = 1_792_851_300;
    const STAMP: &str = "20261024T141500Z";
    /// The launch of an install whose app data was deleted.
    const APP_DATA_ABSENT: bool = true;
    /// The launch of an install whose app data exists, readable or not.
    const APP_DATA_PRESENT: bool = false;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(NOW_SECS)
    }

    /// A private root beneath the resolved temporary directory (macOS
    /// exposes it through the `/var` link, which production paths reject).
    fn test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "arc-desktop-previous-install-{label}-{}-{}",
            std::process::id(),
            {
                use rand::RngCore as _;
                rand::rngs::OsRng.next_u64()
            }
        ));
        arc_crypto::secret_file::secure_private_directory_tree(&root).unwrap();
        root
    }

    fn publish(path: &Path, contents: &[u8]) {
        arc_crypto::secret_file::durably_publish_new_private(path, contents).unwrap();
    }

    /// What a previous install leaves: the control directory with the
    /// shutdown token, the executable identity, an armed receipt and the
    /// lifecycle files, next to chain data.
    fn previous_install_data(root: &Path) -> PathBuf {
        let data = root.join("data-v3");
        let control = data.join(CONTROL);
        arc_crypto::secret_file::secure_private_directory_tree(&control).unwrap();
        publish(
            &control.join(node_manager::DESKTOP_SHUTDOWN_TOKEN_FILE_NAME),
            format!("{}\n", "ab".repeat(32)).as_bytes(),
        );
        publish(
            &control.join(node_manager::DESKTOP_EXECUTABLE_IDENTITY_FILE_NAME),
            b"arc.desktop.executable-path.v1\npath_utf8_hex=00\n",
        );
        publish(
            &control.join(arc_crypto::secret_file::DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME),
            b"arc.desktop.shutdown-unproven.v1\n",
        );
        publish(
            &control.join(arc_crypto::secret_file::DESKTOP_LIFECYCLE_OWNER_LOCK_FILE_NAME),
            arc_crypto::secret_file::desktop_lifecycle_owner_lock_payload(),
        );
        std::fs::create_dir_all(data.join("dag-wal")).unwrap();
        std::fs::write(data.join("dag-wal").join("segment-0001"), b"chain history").unwrap();
        data
    }

    fn assert_untouched(data: &Path) {
        assert!(data
            .join(CONTROL)
            .join(node_manager::DESKTOP_SHUTDOWN_TOKEN_FILE_NAME)
            .is_file());
        assert_eq!(
            std::fs::read(data.join("dag-wal").join("segment-0001")).unwrap(),
            b"chain history"
        );
    }

    fn identity_store() -> Store {
        Store {
            identity: Some(Identity {
                address: "11".repeat(32),
                public_key: format!("0x{}", "22".repeat(32)),
                seed_phrase: "a recovery phrase the launch must never erase".to_string(),
                created_at: 1,
            }),
            config: Some(NodeConfig::default()),
            data_migration_notice: None,
            previous_install_notice: None,
        }
    }

    #[test]
    fn the_utc_stamp_is_sortable_and_file_system_safe() {
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101T000000Z");
        assert_eq!(utc_stamp(now()), STAMP);
        assert!(STAMP.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    /// Only an absent store opens the gate. An empty store that stands in
    /// for one that could not be read is a live install's.
    #[test]
    fn only_an_absent_store_opens_the_gate() {
        assert!(app_data_was_absent(StoreLoadOutcome::Absent));
        assert!(!app_data_was_absent(StoreLoadOutcome::Loaded));
        assert!(!app_data_was_absent(StoreLoadOutcome::Failed));
    }

    #[test]
    fn an_absent_directory_is_kept() {
        let root = test_root("absent");
        assert_eq!(
            set_aside_if_previous_install(&root.join("data-v3"), APP_DATA_ABSENT, now()),
            Decision::Keep
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A fresh install's own first launch publishes the lifecycle locks and
    /// the namespace proof before onboarding; that is not a previous install.
    #[test]
    fn this_installs_own_pre_onboarding_directory_is_kept() {
        let root = test_root("own-fresh");
        let data = root.join("data-v3");
        let control = data.join(CONTROL);
        arc_crypto::secret_file::secure_private_directory_tree(&control).unwrap();
        publish(
            &control.join(arc_crypto::secret_file::DESKTOP_LIFECYCLE_LOCK_FILE_NAME),
            b"arc.desktop.lifecycle-lock.v1\n",
        );
        publish(
            &control.join(arc_crypto::secret_file::DESKTOP_LIFECYCLE_OWNER_LOCK_FILE_NAME),
            arc_crypto::secret_file::desktop_lifecycle_owner_lock_payload(),
        );
        publish(
            &control.join("lifecycle.namespace-proof.v3-00"),
            b"arc.desktop.lifecycle-namespace-proof.v3\n",
        );
        assert_eq!(
            set_aside_if_previous_install(&data, APP_DATA_ABSENT, now()),
            Decision::Keep
        );
        assert!(data.is_dir());
        // A node run by hand, with no desktop control directory, is not
        // ARC's to move either.
        let manual = root.join("manual");
        std::fs::create_dir_all(manual.join("dag-wal")).unwrap();
        std::fs::write(manual.join(".arc-node.lock"), b"").unwrap();
        assert_eq!(
            set_aside_if_previous_install(&manual, APP_DATA_ABSENT, now()),
            Decision::Keep
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_previous_installs_data_is_set_aside_with_every_byte_kept() {
        let root = test_root("set-aside");
        let data = previous_install_data(&root);

        let decision = set_aside_if_previous_install(&data, APP_DATA_ABSENT, now());
        let expected = root.join(format!("data-v3{SET_ASIDE_INFIX}{STAMP}"));
        assert_eq!(
            decision,
            Decision::SetAside {
                from: data.clone(),
                to: expected.clone()
            }
        );
        assert!(
            !data.exists(),
            "the configured path is free for a fresh start"
        );
        assert_untouched(&expected);
        assert!(expected
            .join(CONTROL)
            .join(arc_crypto::secret_file::DESKTOP_SHUTDOWN_RECEIPT_FILE_NAME)
            .is_file());

        // A second launch finds nothing at the configured path and keeps
        // the set-aside directory as it is.
        assert_eq!(
            set_aside_if_previous_install(&data, APP_DATA_ABSENT, now()),
            Decision::Keep
        );
        assert_untouched(&expected);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The field case end to end: the app-data directory is gone, the node
    /// data is not. The store loads as absent, and only then does the
    /// directory move.
    #[test]
    fn an_absent_store_beside_previous_install_data_sets_it_aside() {
        let root = test_root("store-absent");
        let app_data = root.join("app-data");
        let data = previous_install_data(&root);

        let (store, outcome) = Store::load_from_with_outcome(&app_data);
        assert_eq!(outcome, StoreLoadOutcome::Absent);
        assert!(store.identity.is_none());
        let decision = set_aside_if_previous_install(&data, app_data_was_absent(outcome), now());
        let expected = root.join(format!("data-v3{SET_ASIDE_INFIX}{STAMP}"));
        assert_eq!(
            decision,
            Decision::SetAside {
                from: data,
                to: expected.clone()
            }
        );
        assert_untouched(&expected);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The same-install case: a store with an identity loads, so this
    /// install may own the directory, whatever the control directory holds.
    #[test]
    fn a_store_with_an_identity_keeps_the_directory_and_the_store() {
        let root = test_root("store-identity");
        let app_data = root.join("app-data");
        identity_store().save_to(&app_data).unwrap();
        let store_bytes = std::fs::read(app_data.join("store.json")).unwrap();
        let data = previous_install_data(&root);

        let (store, outcome) = Store::load_from_with_outcome(&app_data);
        assert_eq!(outcome, StoreLoadOutcome::Loaded);
        assert_eq!(
            store
                .identity
                .as_ref()
                .map(|identity| identity.address.as_str()),
            Some("11".repeat(32).as_str())
        );
        assert_eq!(
            set_aside_if_previous_install(&data, app_data_was_absent(outcome), now()),
            Decision::Keep
        );
        assert_untouched(&data);
        assert_eq!(
            std::fs::read(app_data.join("store.json")).unwrap(),
            store_bytes
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A store that is present but cannot be read loads empty exactly like
    /// an absent one. It is a live install's wallet: the node data stays, and
    /// the store is not written.
    #[test]
    fn an_unreadable_store_keeps_the_directory_and_the_store() {
        let root = test_root("store-unreadable");
        let data = previous_install_data(&root);
        let fixtures: Vec<(&str, Vec<u8>)> = vec![
            // A UTF-8 byte-order mark in front of otherwise valid JSON.
            (
                "bom",
                [
                    b"\xEF\xBB\xBF".as_slice(),
                    br#"{"identity":null,"config":null}"#.as_slice(),
                ]
                .concat(),
            ),
            // Over the 1 MiB bound the reader enforces.
            ("oversize", vec![b' '; 1024 * 1024 + 1]),
            ("truncated", br#"{"identity":{"address":"11"#.to_vec()),
        ];
        for (label, bytes) in fixtures {
            let app_data = root.join(format!("app-data-{label}"));
            // The directory exists and is private; only the store is wrong.
            assert_eq!(
                Store::load_from_with_outcome(&app_data).1,
                StoreLoadOutcome::Absent
            );
            publish(&app_data.join("store.json"), &bytes);

            let (store, outcome) = Store::load_from_with_outcome(&app_data);
            assert_eq!(outcome, StoreLoadOutcome::Failed, "{label}");
            assert!(store.identity.is_none(), "{label}");
            assert!(!app_data_was_absent(outcome), "{label}");
            assert_eq!(
                set_aside_if_previous_install(&data, app_data_was_absent(outcome), now()),
                Decision::Keep,
                "{label}"
            );
            assert_untouched(&data);
            assert_eq!(
                std::fs::read(app_data.join("store.json")).unwrap(),
                bytes,
                "{label}: the store that could not be read is left exactly as it was"
            );
        }

        // A store the private opener refuses: a link in place of the file.
        #[cfg(unix)]
        {
            let app_data = root.join("app-data-linked");
            assert_eq!(
                Store::load_from_with_outcome(&app_data).1,
                StoreLoadOutcome::Absent
            );
            let real = root.join("real-store.json");
            identity_store().save_to(&root).unwrap();
            std::fs::rename(root.join("store.json"), &real).unwrap();
            std::os::unix::fs::symlink(&real, app_data.join("store.json")).unwrap();

            let (store, outcome) = Store::load_from_with_outcome(&app_data);
            assert_eq!(outcome, StoreLoadOutcome::Failed);
            assert!(store.identity.is_none());
            assert_eq!(
                set_aside_if_previous_install(&data, app_data_was_absent(outcome), now()),
                Decision::Keep
            );
            assert_untouched(&data);
            assert!(std::fs::symlink_metadata(app_data.join("store.json"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert!(std::fs::read_to_string(&real)
                .unwrap()
                .contains("a recovery phrase the launch must never erase"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn set_aside_names_never_collide() {
        let root = test_root("collide");
        let taken = root.join(format!("data-v3{SET_ASIDE_INFIX}{STAMP}"));
        std::fs::create_dir_all(&taken).unwrap();
        std::fs::write(taken.join("earlier"), b"an earlier set-aside").unwrap();
        let data = previous_install_data(&root);

        let decision = set_aside_if_previous_install(&data, APP_DATA_ABSENT, now());
        let expected = root.join(format!("data-v3{SET_ASIDE_INFIX}{STAMP}-2"));
        assert_eq!(
            decision,
            Decision::SetAside {
                from: data,
                to: expected.clone()
            }
        );
        assert_untouched(&expected);
        assert_eq!(
            std::fs::read(taken.join("earlier")).unwrap(),
            b"an earlier set-aside"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Present app data, readable or not, keeps the directory whatever the
    /// control directory holds.
    #[test]
    fn present_app_data_keeps_the_directory() {
        let root = test_root("same-install");
        let data = previous_install_data(&root);
        assert_eq!(
            set_aside_if_previous_install(&data, APP_DATA_PRESENT, now()),
            Decision::Keep
        );
        assert_untouched(&data);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_directory_a_live_node_holds_is_refused() {
        let root = test_root("node-holds");
        let data = previous_install_data(&root);
        let node_lock = data.join(".arc-node.lock");
        publish(&node_lock, b"");
        let held = arc_crypto::secret_file::open_private_read_write(&node_lock).unwrap();
        held.try_lock_exclusive().unwrap();

        match set_aside_if_previous_install(&data, APP_DATA_ABSENT, now()) {
            Decision::Held(reason) => assert!(
                reason.contains("a node still holds the data directory"),
                "{reason}"
            ),
            other => panic!("a held data directory must not move: {other:?}"),
        }
        assert_untouched(&data);
        fs2::FileExt::unlock(&held).unwrap();
        drop(held);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_directory_another_desktop_owns_is_refused() {
        let root = test_root("desktop-owns");
        let data = previous_install_data(&root);
        let owner_lock = data
            .join(CONTROL)
            .join(arc_crypto::secret_file::DESKTOP_LIFECYCLE_OWNER_LOCK_FILE_NAME);
        let held = arc_crypto::secret_file::open_private_read_write(&owner_lock).unwrap();
        held.try_lock_exclusive().unwrap();

        match set_aside_if_previous_install(&data, APP_DATA_ABSENT, now()) {
            Decision::Held(reason) => assert!(
                reason.contains("another ARC desktop currently owns"),
                "{reason}"
            ),
            other => panic!("an owned data directory must not move: {other:?}"),
        }
        assert_untouched(&data);
        fs2::FileExt::unlock(&held).unwrap();
        drop(held);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A desktop that is publishing or rebarriering the directory name holds
    /// the sibling namespace lock; the set-aside never waits for it.
    #[test]
    fn a_directory_whose_namespace_is_locked_is_refused() {
        let root = test_root("namespace-locked");
        let data = previous_install_data(&root);
        let namespace =
            arc_crypto::secret_file::acquire_private_directory_namespace_lock(&data).unwrap();

        match set_aside_if_previous_install(&data, APP_DATA_ABSENT, now()) {
            Decision::Held(reason) => assert!(
                reason.contains("another ARC desktop is working on"),
                "{reason}"
            ),
            other => panic!("a locked namespace must not move: {other:?}"),
        }
        assert_untouched(&data);
        drop(namespace);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed() {
        let root = test_root("links");
        let real = previous_install_data(&root);
        let linked = root.join("linked-data-v3");
        std::os::unix::fs::symlink(&real, &linked).unwrap();

        match set_aside_if_previous_install(&linked, APP_DATA_ABSENT, now()) {
            Decision::Held(reason) => assert!(reason.contains("link"), "{reason}"),
            other => panic!("a linked data path must not move: {other:?}"),
        }
        assert!(std::fs::symlink_metadata(&linked)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_untouched(&real);

        // A linked control directory inside a real data directory.
        let other = root.join("other-data-v3");
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(real.join(CONTROL), other.join(CONTROL)).unwrap();
        match set_aside_if_previous_install(&other, APP_DATA_ABSENT, now()) {
            Decision::Held(reason) => assert!(reason.contains("link"), "{reason}"),
            other => panic!("a linked control path must not move: {other:?}"),
        }
        assert!(other.is_dir());
        assert_untouched(&real);

        // A linked token file inside a real control directory.
        let third = root.join("third-data-v3");
        arc_crypto::secret_file::secure_private_directory_tree(&third.join(CONTROL)).unwrap();
        std::os::unix::fs::symlink(
            real.join(CONTROL)
                .join(node_manager::DESKTOP_SHUTDOWN_TOKEN_FILE_NAME),
            third
                .join(CONTROL)
                .join(node_manager::DESKTOP_SHUTDOWN_TOKEN_FILE_NAME),
        )
        .unwrap();
        match set_aside_if_previous_install(&third, APP_DATA_ABSENT, now()) {
            Decision::Held(reason) => assert!(reason.contains("link"), "{reason}"),
            other => panic!("a linked control file must not move: {other:?}"),
        }
        assert!(third.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_data_path_that_is_a_file_is_refused() {
        let root = test_root("file");
        let data = root.join("data-v3");
        std::fs::write(&data, b"not a directory").unwrap();
        match set_aside_if_previous_install(&data, APP_DATA_ABSENT, now()) {
            Decision::Held(reason) => assert!(reason.contains("not a directory"), "{reason}"),
            other => panic!("a file at the data path must not move: {other:?}"),
        }
        assert_eq!(std::fs::read(&data).unwrap(), b"not a directory");
        std::fs::remove_dir_all(root).unwrap();
    }
}
