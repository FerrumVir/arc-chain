//! One question, asked once, when a stranded v0.7 install reaches this app.
//!
//! Released v0.7 desktops can only update by clicking "Install" on GitHub's
//! "Latest" release, which the v0.7 legacy bridge release points at this app. The
//! v0.7 app never asked whether the computer should contribute compute on the
//! new network, so the first launch here asks: "Your ARC node is upgrading to
//! the new network. Keep contributing compute?". Until the user answers, the
//! install runs as an observer without a model, even if v0.7 ran it as a
//! worker. The answer is recorded through `commands::set_compute_contribution`,
//! the same path as the Settings switch, so "Yes" downloads and verifies the
//! model if needed and promotes the node, and "Not now" keeps it an observer.
//!
//! The pending question lives in its own file beside `store.json`, so the
//! store and config formats are unchanged.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::store::Store;
use crate::types::{DataMigrationNotice, NodeConfig};
use crate::AppState;

pub const QUESTION_FILE: &str = "legacy-compute-question.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyComputeQuestion {
    /// The v0.7 role before the node was held as an observer.
    pub previous_role: String,
    /// The v0.7 model path before the node was held as an observer.
    pub previous_model_path: Option<String>,
    /// Why this launch counts as the first one after a v0.7 install.
    pub detected_by: String,
    pub recorded_unix_ms: u64,
}

/// What startup saw that can mark an install as coming from v0.7.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct V07Signals {
    /// The WAL fence created a migration notice on this launch.
    pub fenced_this_launch: bool,
    /// The saved configuration still uses the v0.7 data layout, read before
    /// the fence ran (`config_uses_v07_layout`).
    pub config_uses_v07_layout: bool,
    /// A migration notice kept from an earlier launch fenced the v0.7 root.
    pub fenced_v07_root_earlier: bool,
}

/// Fenced v0.7 chain data (on this launch, or kept from an earlier one), the
/// bridge's record of having run under the v0.7 app, or a saved
/// configuration that still uses the v0.7 data layout marks an install that
/// came from v0.7. The layout covers v0.7.10/v0.7.11 desktops whose node
/// never started, because their updater returned 404: they have no WAL to
/// fence and may have no bridge record, but their owner has still never been
/// asked. The earlier notice and the WAL kept at the `~/.arc` root cover a v0.7
/// install whose first v0.8 launch was a build without this question; the
/// WAL outlives the notice, which the user can dismiss.
pub fn detect_v07_origin(signals: V07Signals, home: &Path) -> Option<&'static str> {
    if signals.fenced_this_launch {
        Some("v0.7 chain data was fenced on this launch")
    } else if legacy_bridge_ran(home) {
        Some("the v0.7 legacy bridge release ran on this computer")
    } else if signals.config_uses_v07_layout {
        Some("the saved configuration still uses the v0.7 data layout")
    } else if signals.fenced_v07_root_earlier {
        Some("v0.7 chain data was fenced on an earlier launch")
    } else if v07_wal_kept_at_root(home) {
        Some("v0.7 chain data is still kept at the ~/.arc root")
    } else {
        None
    }
}

/// Every v0.6.0..=v0.7.11 desktop stored `dataDir` as the literal `"~/.arc"`
/// (`types.rs` default, `Onboarding.tsx`, `Dashboard.tsx`; no screen could
/// change it). Every v0.8 build defaults to a `data-v3*` child.
pub const V07_DATA_DIR: &str = "~/.arc";

/// Whether the saved configuration still uses the v0.7 desktop data layout:
/// `dataDir` at the `~/.arc` root. The WAL fence moves the pointer, so this
/// must be read before the fence runs.
pub fn config_uses_v07_layout(config: &NodeConfig, home: &Path) -> bool {
    config.data_dir == V07_DATA_DIR || expand_against(&config.data_dir, home) == v07_root(home)
}

/// Whether a kept migration notice fenced the v0.7 root itself, rather than a
/// malformed v0.8 `data-v3*` directory.
pub fn notice_fenced_v07_root(notice: Option<&DataMigrationNotice>, home: &Path) -> bool {
    notice.is_some_and(|notice| Path::new(&notice.legacy_data_dir) == v07_root(home))
}

/// Whether the `~/.arc` root still holds a v0.7 WAL: `state.wal` or `dag-wal`
/// without a valid `genesis.network-hash`. The fence keeps those bytes in
/// place and only moves the pointer, so they outlive a dismissed notice. A v0.8
/// install never writes there (its default is a `data-v3*` child), and a v0.8
/// node that did run at the root wrote a valid hash beside its WAL.
pub fn v07_wal_kept_at_root(home: &Path) -> bool {
    let root = v07_root(home);
    let has_wal = ["state.wal", "dag-wal"]
        .iter()
        .any(|name| fs::symlink_metadata(root.join(name)).is_ok());
    has_wal && !has_valid_network_hash(&root.join("genesis.network-hash"))
}

/// The same syntactic check the WAL fence applies to `genesis.network-hash`.
fn has_valid_network_hash(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && metadata.len() <= 1024 => {
            fs::read(path).ok().is_some_and(|bytes| {
                std::str::from_utf8(&bytes).ok().is_some_and(|value| {
                    let value = value.trim();
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
        }
        _ => false,
    }
}

fn v07_root(home: &Path) -> PathBuf {
    home.join(".arc")
}

/// `paths::expand_tilde` against an explicit home directory.
fn expand_against(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

/// The bridge writes `~/.arc/legacy-bridge/nodes/desktop-*/bridge-state.json`
/// when the v0.7 app starts it.
pub fn legacy_bridge_ran(home: &Path) -> bool {
    let nodes = home.join(".arc").join("legacy-bridge").join("nodes");
    fs::read_dir(nodes)
        .map(|entries| {
            entries.flatten().any(|entry| {
                entry.file_name().to_string_lossy().starts_with("desktop-")
                    && entry.path().join("bridge-state.json").is_file()
            })
        })
        .unwrap_or(false)
}

pub fn load(dir: &Path) -> Option<LegacyComputeQuestion> {
    let bytes = fs::read(dir.join(QUESTION_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save(dir: &Path, question: &LegacyComputeQuestion) -> anyhow::Result<()> {
    let temporary = dir.join(format!(".{QUESTION_FILE}.{}.tmp", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(question)?)?;
    fs::rename(&temporary, dir.join(QUESTION_FILE))?;
    Ok(())
}

pub fn clear(dir: &Path) -> anyhow::Result<()> {
    match fs::remove_file(dir.join(QUESTION_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// What `hold_until_answered` did.
#[derive(Debug, Default)]
pub struct Hold {
    /// The install is held as an observer without a model.
    pub held: bool,
    /// Holding it changed the config (the caller persists it).
    pub changed: bool,
    /// The question could not be recorded. The config is held anyway.
    pub record_error: Option<anyhow::Error>,
}

/// Startup: hold compute back until the question is answered, and record the
/// question the first time. The hold is applied before anything is written,
/// so a question that cannot be recorded still never lets an unasked v0.7
/// install start as a worker.
pub fn hold_until_answered(
    config: &mut NodeConfig,
    dir: &Path,
    origin: Option<&str>,
    now_unix_ms: u64,
) -> Hold {
    // An explicit answer, from this question or the Settings switch, wins.
    if config.compute_consent.is_some() {
        return Hold::default();
    }
    // Any file here, even one that no longer parses, is a pending question.
    let record_as = if dir.join(QUESTION_FILE).exists() {
        match load(dir) {
            Some(_) => None,
            None => Some("an earlier launch recorded this question, but its record is unreadable"),
        }
    } else {
        match origin {
            Some(origin) => Some(origin),
            None => return Hold::default(),
        }
    };
    let question = record_as.map(|detected_by| LegacyComputeQuestion {
        previous_role: config.role.clone(),
        previous_model_path: config.model_path.clone(),
        detected_by: detected_by.to_string(),
        recorded_unix_ms: now_unix_ms,
    });
    let changed = config.role != "observer" || config.model_path.is_some();
    config.role = "observer".to_string();
    config.model_path = None;
    Hold {
        held: true,
        changed,
        record_error: question.and_then(|question| save(dir, &question).err()),
    }
}

/// What `hold_at_startup` did, for the startup log.
#[derive(Debug, Default)]
pub struct StartupHold {
    pub hold: Hold,
    /// The held config could not be written to `store.json`.
    pub persist_error: Option<anyhow::Error>,
}

/// The startup step `lib.rs` runs after the WAL fence and before anything
/// can start the node: find a v0.7 origin, hold the stored config until the
/// question is answered, and persist the held config. `config_uses_v07_layout`
/// must have been read before the fence.
pub fn hold_at_startup(
    store: &mut Store,
    app_data_dir: &Path,
    home: &Path,
    fenced_this_launch: bool,
    config_uses_v07_layout: bool,
    now_unix_ms: u64,
) -> StartupHold {
    let signals = V07Signals {
        fenced_this_launch,
        config_uses_v07_layout,
        fenced_v07_root_earlier: notice_fenced_v07_root(store.data_migration_notice.as_ref(), home),
    };
    let origin = detect_v07_origin(signals, home);
    let Some(config) = store.config.as_mut() else {
        // No stored config: onboarding runs, and its model step asks.
        return StartupHold::default();
    };
    let hold = hold_until_answered(config, app_data_dir, origin, now_unix_ms);
    // Persist whenever the hold changed the config, and also when the
    // question could not be recorded: the stored observer config is then
    // what keeps a later launch from starting the old worker config.
    let persist_error = if hold.changed || hold.record_error.is_some() {
        store.save_to(app_data_dir).err()
    } else {
        None
    };
    StartupHold {
        hold,
        persist_error,
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The pending question, or `None` once the user has answered anywhere.
#[tauri::command]
pub async fn load_legacy_compute_question(
    state: State<'_, AppState>,
) -> Result<Option<LegacyComputeQuestion>, String> {
    let answered = state
        .store
        .lock()
        .await
        .config
        .as_ref()
        .and_then(|config| config.compute_consent)
        .is_some();
    if answered {
        return Ok(None);
    }
    let dir = state.data_dir.lock().await.clone();
    Ok(load(&dir))
}

/// Record the answer exactly as the Settings switch does, then retire the
/// question. The consent is persisted before any model download starts, so
/// a failed download still leaves "Yes" recorded and startup promotion
/// retries it.
#[tauri::command]
pub async fn answer_legacy_compute_question(
    app: AppHandle,
    state: State<'_, AppState>,
    contribute: bool,
) -> Result<NodeConfig, String> {
    let dir = state.data_dir.lock().await.clone();
    let outcome = crate::commands::set_compute_contribution(app, state.clone(), contribute).await;
    let recorded = state
        .store
        .lock()
        .await
        .config
        .as_ref()
        .and_then(|config| config.compute_consent)
        == Some(contribute);
    if recorded {
        clear(&dir).map_err(|error| error.to_string())?;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{compute_contribution_enabled, promotion_need, PromotionNeed};

    /// `store.json` as every released v0.6.0..=v0.7.11 desktop wrote it.
    /// `tests/legacy-bridge/check_v07_fixtures.py` checks it against each tag.
    const V07_DESKTOP_STORES: &str =
        include_str!("../../../tests/legacy-bridge/fixtures/v07-desktop-stores.json");

    /// A private home directory and app-data directory per test.
    struct Machine {
        root: tempfile::TempDir,
    }

    impl Machine {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
            }
        }

        fn home(&self) -> PathBuf {
            self.root.path().join("home")
        }

        fn arc_root(&self) -> PathBuf {
            self.home().join(".arc")
        }

        fn app_data(&self) -> PathBuf {
            self.root
                .path()
                .join("app-data")
                .join("network.arc.desktop")
        }

        /// Install a store exactly as an older desktop left it.
        fn install_store(&self, store: &Store) {
            fs::create_dir_all(self.arc_root()).unwrap();
            store.save_to(&self.app_data()).unwrap();
        }

        /// A v0.7 node that ran wrote its WAL at the `~/.arc` root.
        fn v07_node_ran(&self) {
            fs::create_dir_all(self.arc_root()).unwrap();
            fs::write(self.arc_root().join("state.wal"), b"v0.7 wal").unwrap();
        }

        /// What the bridge writes when the v0.7 app starts it.
        fn bridge_ran_under_the_v07_app(&self) {
            let node = self
                .arc_root()
                .join("legacy-bridge")
                .join("nodes")
                .join("desktop-0123456789ab");
            fs::create_dir_all(&node).unwrap();
            fs::write(node.join("bridge-state.json"), b"{}").unwrap();
        }

        /// The startup sequence of `lib.rs` from loading the store to the
        /// held config: layout read, WAL fence (persisted as `lib.rs` does),
        /// then `hold_at_startup`. Returns the store the node would start with.
        fn launch(&self, now_unix_ms: u64) -> (Store, StartupHold) {
            let mut store = Store::load_from(&self.app_data());
            let config_uses_v07_layout = store
                .config
                .as_ref()
                .is_some_and(|config| config_uses_v07_layout(config, &self.home()));
            let fenced_this_launch = match store.config.as_ref() {
                Some(config) => {
                    let legacy_dir = expand_against(&config.data_dir, &self.home());
                    store
                        .protect_legacy_v07_data_at(&legacy_dir)
                        .unwrap()
                        .is_some()
                }
                None => false,
            };
            if fenced_this_launch {
                store.save_to(&self.app_data()).unwrap();
            }
            let outcome = hold_at_startup(
                &mut store,
                &self.app_data(),
                &self.home(),
                fenced_this_launch,
                config_uses_v07_layout,
                now_unix_ms,
            );
            (store, outcome)
        }
    }

    fn v07_store(name: &str) -> Store {
        let fixture: serde_json::Value = serde_json::from_str(V07_DESKTOP_STORES).unwrap();
        serde_json::from_value(fixture["stores"][name]["store"].clone()).unwrap()
    }

    fn v07_worker() -> NodeConfig {
        v07_store("onboarding-worker").config.unwrap()
    }

    /// The node starts as an observer without a model, never promotes, and is
    /// waiting for the question.
    fn assert_held(store: &Store, machine: &Machine) {
        let config = store.config.as_ref().expect("config");
        assert_eq!(config.role, "observer");
        assert_eq!(config.model_path, None);
        assert_eq!(config.compute_consent, None);
        assert!(!compute_contribution_enabled(config));
        assert_eq!(promotion_need(config, 64), PromotionNeed::NoConsent);
        let launched = crate::node_manager::effective_launch_config(config);
        assert_eq!(launched.role, "observer");
        assert_eq!(launched.model_path, None);
        assert!(load(&machine.app_data()).is_some(), "question pending");
        // What the next launch reads back is held too.
        let persisted = Store::load_from(&machine.app_data()).config.unwrap();
        assert_eq!(persisted.role, "observer");
        assert_eq!(persisted.model_path, None);
    }

    #[test]
    fn every_released_v07_desktop_store_is_held_until_its_owner_answers() {
        let fixture: serde_json::Value = serde_json::from_str(V07_DESKTOP_STORES).unwrap();
        let tags = fixture["tags"].as_array().unwrap();
        assert_eq!(
            tags.len(),
            11,
            "v0.6.0 and v0.7.0..=v0.7.11 (no v0.7.8/9 tags)"
        );
        let shapes = fixture["stores"].as_object().unwrap();
        for tag in tags {
            for (shape, _) in shapes {
                for node_ran in [false, true] {
                    let label = format!("{tag} {shape} node_ran={node_ran}");
                    let machine = Machine::new();
                    let store = v07_store(shape);
                    assert_eq!(store.config.as_ref().unwrap().data_dir, V07_DATA_DIR);
                    machine.install_store(&store);
                    if node_ran {
                        machine.v07_node_ran();
                    }
                    let (store, outcome) = machine.launch(1);
                    assert!(outcome.hold.held, "{label}");
                    assert!(outcome.hold.record_error.is_none(), "{label}");
                    assert!(outcome.persist_error.is_none(), "{label}");
                    assert_held(&store, &machine);
                    let question = load(&machine.app_data()).unwrap();
                    let original = v07_store(shape).config.unwrap();
                    assert_eq!(question.previous_role, original.role, "{label}");
                    assert_eq!(question.previous_model_path, original.model_path, "{label}");
                }
            }
        }
    }

    #[test]
    fn a_v07_node_that_already_ran_is_fenced_and_held() {
        let machine = Machine::new();
        machine.install_store(&v07_store("onboarding-worker"));
        machine.v07_node_ran();
        let (store, outcome) = machine.launch(1);
        assert!(outcome.hold.changed);
        assert_held(&store, &machine);
        // The fence moved the pointer to a fresh v3 child.
        assert!(store.config.as_ref().unwrap().data_dir.contains("data-v3"));
        assert_eq!(
            load(&machine.app_data()).unwrap().detected_by,
            "v0.7 chain data was fenced on this launch"
        );
    }

    #[test]
    fn a_v07_node_that_never_ran_is_held() {
        // v0.7.10/v0.7.11: the updater returned 404, so no WAL and no bridge
        // record; the layout is the only signal.
        let machine = Machine::new();
        machine.install_store(&v07_store("onboarding-worker"));
        let (store, _) = machine.launch(1);
        assert_held(&store, &machine);
        assert_eq!(
            load(&machine.app_data()).unwrap().detected_by,
            "the saved configuration still uses the v0.7 data layout"
        );
    }

    #[test]
    fn a_desktop_the_bridge_ran_under_is_held() {
        let machine = Machine::new();
        machine.install_store(&v07_store("onboarding-worker"));
        machine.bridge_ran_under_the_v07_app();
        let (store, _) = machine.launch(1);
        assert_held(&store, &machine);
        assert_eq!(
            load(&machine.app_data()).unwrap().detected_by,
            "the v0.7 legacy bridge release ran on this computer"
        );
    }

    #[test]
    fn a_v07_install_first_opened_by_a_build_without_the_question_is_still_held() {
        // A v0.7 desktop updated to a v0.8 build without this question (for
        // example one marked Latest before the bridge): its WAL was fenced
        // there, and it ran as a worker without being asked. The kept notice
        // that fenced the v0.7 root marks it on the first launch here.
        let machine = Machine::new();
        machine.install_store(&v07_store("onboarding-worker"));
        machine.v07_node_ran();
        let mut store = Store::load_from(&machine.app_data());
        store
            .protect_legacy_v07_data_at(&machine.arc_root())
            .unwrap()
            .unwrap();
        store.save_to(&machine.app_data()).unwrap();
        let (store, _) = machine.launch(1);
        assert_held(&store, &machine);
        assert_eq!(
            load(&machine.app_data()).unwrap().detected_by,
            "v0.7 chain data was fenced on an earlier launch"
        );
    }

    #[test]
    fn a_v07_install_whose_earlier_notice_was_dismissed_is_still_held() {
        // As above, then the owner dismissed that build's migration notice
        // (`dismiss_data_migration_notice`). The v0.7 WAL the fence kept at
        // the ~/.arc root still marks the install.
        let machine = Machine::new();
        machine.install_store(&v07_store("onboarding-worker"));
        machine.v07_node_ran();
        let mut store = Store::load_from(&machine.app_data());
        store
            .protect_legacy_v07_data_at(&machine.arc_root())
            .unwrap()
            .unwrap();
        store.data_migration_notice = None;
        store.save_to(&machine.app_data()).unwrap();
        let (store, outcome) = machine.launch(1);
        assert!(outcome.hold.held);
        assert_held(&store, &machine);
        assert_eq!(
            load(&machine.app_data()).unwrap().detected_by,
            "v0.7 chain data is still kept at the ~/.arc root"
        );
        // And on every launch after that.
        let (store, _) = machine.launch(2);
        assert_held(&store, &machine);
    }

    #[test]
    fn only_a_v07_wal_at_the_root_counts() {
        let machine = Machine::new();
        let root = machine.arc_root();
        fs::create_dir_all(&root).unwrap();
        assert!(!v07_wal_kept_at_root(&machine.home()));
        // Binaries, models and a fresh v0.8 data-v3 child are not chain data.
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("models")).unwrap();
        fs::create_dir_all(root.join("data-v3")).unwrap();
        fs::write(root.join("data-v3").join("state.wal"), b"v3").unwrap();
        assert!(!v07_wal_kept_at_root(&machine.home()));
        for wal in ["state.wal", "dag-wal"] {
            let machine = Machine::new();
            let root = machine.arc_root();
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join(wal), b"v0.7").unwrap();
            assert!(v07_wal_kept_at_root(&machine.home()), "{wal}");
            // A malformed hash is not a v0.8 binding.
            fs::write(root.join("genesis.network-hash"), b"not a hash").unwrap();
            assert!(v07_wal_kept_at_root(&machine.home()), "{wal}");
            // A v0.8 node that ran at the root bound its WAL to the network.
            fs::write(
                root.join("genesis.network-hash"),
                format!("{}\n", "ab".repeat(32)),
            )
            .unwrap();
            assert!(!v07_wal_kept_at_root(&machine.home()), "{wal}");
        }
    }

    #[test]
    fn relaunching_before_answering_keeps_the_hold() {
        let machine = Machine::new();
        machine.install_store(&v07_store("onboarding-worker"));
        machine.v07_node_ran();
        machine.launch(7);
        for now in [8, 9] {
            let (store, outcome) = machine.launch(now);
            assert!(outcome.hold.held);
            assert!(!outcome.hold.changed, "already held and persisted");
            assert_held(&store, &machine);
        }
        assert_eq!(load(&machine.app_data()).unwrap().recorded_unix_ms, 7);
        assert_eq!(load(&machine.app_data()).unwrap().previous_role, "worker");
    }

    #[test]
    fn only_an_explicit_answer_ends_the_hold() {
        for contribute in [false, true] {
            let machine = Machine::new();
            machine.install_store(&v07_store("onboarding-worker"));
            let (mut store, _) = machine.launch(1);
            // `set_compute_contribution` records the answer under the store
            // lock, then the question is cleared.
            store.config.as_mut().unwrap().compute_consent = Some(contribute);
            store.save_to(&machine.app_data()).unwrap();
            clear(&machine.app_data()).unwrap();
            let (store, outcome) = machine.launch(2);
            assert!(!outcome.hold.held);
            let config = store.config.unwrap();
            assert_eq!(compute_contribution_enabled(&config), contribute);
            let expected = if contribute {
                PromotionNeed::Promote
            } else {
                PromotionNeed::NoConsent
            };
            assert_eq!(promotion_need(&config, 64), expected);
            assert!(load(&machine.app_data()).is_none());
        }
    }

    #[test]
    fn an_unrecordable_question_still_holds_the_node() {
        let machine = Machine::new();
        machine.install_store(&v07_store("onboarding-worker"));
        // A directory where the record goes: the rename cannot replace it.
        fs::create_dir_all(machine.app_data().join(QUESTION_FILE).join("occupied")).unwrap();
        let (store, outcome) = machine.launch(1);
        assert!(outcome.hold.held);
        assert!(outcome.hold.record_error.is_some());
        let config = store.config.as_ref().unwrap();
        assert_eq!(config.role, "observer");
        assert_eq!(config.model_path, None);
        assert_eq!(promotion_need(config, 64), PromotionNeed::NoConsent);
        let persisted = Store::load_from(&machine.app_data()).config.unwrap();
        assert_eq!(persisted.role, "observer");
        assert_eq!(persisted.model_path, None);
    }

    #[test]
    fn an_unreadable_question_record_is_still_pending() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(QUESTION_FILE), b"{ truncated").unwrap();
        let mut config = v07_worker();
        let hold = hold_until_answered(&mut config, dir.path(), None, 3);
        assert!(hold.held);
        assert!(hold.record_error.is_none());
        assert_eq!(config.role, "observer");
        assert_eq!(config.model_path, None);
        let question = load(dir.path()).expect("rewritten");
        assert_eq!(question.recorded_unix_ms, 3);
    }

    #[test]
    fn a_v07_install_that_never_finished_onboarding_is_left_to_onboarding() {
        // No stored config: onboarding's model step is the consent question.
        let machine = Machine::new();
        machine.install_store(&Store::default());
        machine.v07_node_ran();
        let (store, outcome) = machine.launch(1);
        assert!(!outcome.hold.held);
        assert!(store.config.is_none());
        assert!(load(&machine.app_data()).is_none());
    }

    #[test]
    fn v08_installs_are_never_asked() {
        // A fresh install.
        let machine = Machine::new();
        machine.install_store(&Store {
            config: Some(NodeConfig::default()),
            ..Store::default()
        });
        let (store, outcome) = machine.launch(1);
        assert!(!outcome.hold.held);
        assert_eq!(store.config.unwrap().role, NodeConfig::default().role);
        assert!(load(&machine.app_data()).is_none());

        // A v0.8.10 worker keeps #138's rule (a worker with a model from
        // before the question counts as opted in).
        let machine = Machine::new();
        let worker = NodeConfig {
            role: "worker".into(),
            model_path: Some("/Users/ada/.arc/models/standard.gguf".into()),
            ..NodeConfig::default()
        };
        machine.install_store(&Store {
            config: Some(worker.clone()),
            ..Store::default()
        });
        let (store, outcome) = machine.launch(1);
        assert!(!outcome.hold.held);
        assert_eq!(store.config.unwrap().model_path, worker.model_path);
    }

    #[test]
    fn a_notice_for_a_malformed_v3_directory_is_not_a_v07_origin() {
        let home = Path::new("/Users/ada");
        let notice = DataMigrationNotice {
            legacy_data_dir: "/Users/ada/.arc/data-v3".into(),
            active_data_dir: "/Users/ada/.arc/data-v3/data-v3".into(),
            migrated_at: 0,
            reason: "malformed genesis.network-hash".into(),
        };
        assert!(!notice_fenced_v07_root(Some(&notice), home));
        let v07 = DataMigrationNotice {
            legacy_data_dir: "/Users/ada/.arc".into(),
            ..notice
        };
        assert!(notice_fenced_v07_root(Some(&v07), home));
        assert!(!notice_fenced_v07_root(None, home));
    }

    #[test]
    fn only_the_v07_root_counts_as_the_v07_layout() {
        let home = Path::new("/Users/ada");
        let with = |data_dir: &str| NodeConfig {
            data_dir: data_dir.into(),
            ..NodeConfig::default()
        };
        assert!(config_uses_v07_layout(&with("~/.arc"), home));
        assert!(config_uses_v07_layout(&with("/Users/ada/.arc"), home));
        assert!(config_uses_v07_layout(&with("/Users/ada/.arc/"), home));
        assert!(!config_uses_v07_layout(&NodeConfig::default(), home));
        assert!(!config_uses_v07_layout(&with("~/.arc/data-v3-1"), home));
        assert!(!config_uses_v07_layout(&with("/Users/bob/.arc"), home));
    }
}
