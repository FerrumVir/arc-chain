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
    pub previous_role: String,
    pub previous_model_path: Option<String>,
    pub detected_by: String,
    pub recorded_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct V07Signals {
    pub fenced_this_launch: bool,
    pub config_uses_v07_layout: bool,
    pub fenced_v07_root_earlier: bool,
}

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

pub const V07_DATA_DIR: &str = "~/.arc";

pub fn config_uses_v07_layout(config: &NodeConfig, home: &Path) -> bool {
    config.data_dir == V07_DATA_DIR || expand_against(&config.data_dir, home) == v07_root(home)
}

pub fn notice_fenced_v07_root(notice: Option<&DataMigrationNotice>, home: &Path) -> bool {
    notice.is_some_and(|notice| Path::new(&notice.legacy_data_dir) == v07_root(home))
}

pub fn v07_wal_kept_at_root(home: &Path) -> bool {
    let root = v07_root(home);
    let has_wal = ["state.wal", "dag-wal"]
        .iter()
        .any(|name| fs::symlink_metadata(root.join(name)).is_ok());
    has_wal && !has_valid_network_hash(&root.join("genesis.network-hash"))
}

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

fn expand_against(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

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

#[derive(Debug, Default)]
pub struct Hold {
    pub held: bool,
    pub changed: bool,
    pub record_error: Option<anyhow::Error>,
}

pub fn hold_until_answered(
    config: &mut NodeConfig,
    dir: &Path,
    origin: Option<&str>,
    now_unix_ms: u64,
) -> Hold {
    if config.compute_consent.is_some() {
        return Hold::default();
    }
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

#[derive(Debug, Default)]
pub struct StartupHold {
    pub hold: Hold,
    pub persist_error: Option<anyhow::Error>,
}

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
        return StartupHold::default();
    };
    let hold = hold_until_answered(config, app_data_dir, origin, now_unix_ms);
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
        crate::commands::note_compute_question_answered(&dir);
        clear(&dir).map_err(|error| error.to_string())?;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{compute_contribution_enabled, promotion_need, PromotionNeed};

    const V07_DESKTOP_STORES: &str =
        include_str!("../../../tests/legacy-bridge/fixtures/v07-desktop-stores.json");

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

        fn install_store(&self, store: &Store) {
            fs::create_dir_all(self.arc_root()).unwrap();
            store.save_to(&self.app_data()).unwrap();
        }

        fn v07_node_ran(&self) {
            fs::create_dir_all(self.arc_root()).unwrap();
            fs::write(self.arc_root().join("state.wal"), b"v0.7 wal").unwrap();
        }

        fn bridge_ran_under_the_v07_app(&self) {
            let node = self
                .arc_root()
                .join("legacy-bridge")
                .join("nodes")
                .join("desktop-0123456789ab");
            fs::create_dir_all(&node).unwrap();
            fs::write(node.join("bridge-state.json"), b"{}").unwrap();
        }

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
        assert!(store.config.as_ref().unwrap().data_dir.contains("data-v3"));
        assert_eq!(
            load(&machine.app_data()).unwrap().detected_by,
            "v0.7 chain data was fenced on this launch"
        );
    }

    #[test]
    fn a_v07_node_that_never_ran_is_held() {
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
        let (store, _) = machine.launch(2);
        assert_held(&store, &machine);
    }

    #[test]
    fn only_a_v07_wal_at_the_root_counts() {
        let machine = Machine::new();
        let root = machine.arc_root();
        fs::create_dir_all(&root).unwrap();
        assert!(!v07_wal_kept_at_root(&machine.home()));
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
            fs::write(root.join("genesis.network-hash"), b"not a hash").unwrap();
            assert!(v07_wal_kept_at_root(&machine.home()), "{wal}");
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
        let machine = Machine::new();
        machine.install_store(&Store {
            config: Some(NodeConfig::default()),
            ..Store::default()
        });
        let (store, outcome) = machine.launch(1);
        assert!(!outcome.hold.held);
        assert_eq!(store.config.unwrap().role, NodeConfig::default().role);
        assert!(load(&machine.app_data()).is_none());

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
