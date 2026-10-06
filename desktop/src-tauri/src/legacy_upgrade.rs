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
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::types::NodeConfig;
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

/// Fenced v0.7 chain data on this launch, or the bridge's record of having
/// run under the v0.7 app, marks an install that came from v0.7.
pub fn detect_v07_origin(created_migration_notice: bool, home: &Path) -> Option<&'static str> {
    if created_migration_notice {
        Some("v0.7 chain data was fenced on this launch")
    } else if legacy_bridge_ran(home) {
        Some("the v0.7 legacy bridge release ran on this computer")
    } else {
        None
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

/// Startup: hold compute back until the question is answered. Records the
/// question the first time and returns whether `config` changed (the caller
/// persists it before the node auto-starts).
pub fn hold_until_answered(
    config: &mut NodeConfig,
    dir: &Path,
    origin: Option<&str>,
    now_unix_ms: u64,
) -> anyhow::Result<bool> {
    // An explicit answer, from this question or the Settings switch, wins.
    if config.compute_consent.is_some() {
        return Ok(false);
    }
    if load(dir).is_none() {
        let Some(origin) = origin else {
            return Ok(false);
        };
        save(
            dir,
            &LegacyComputeQuestion {
                previous_role: config.role.clone(),
                previous_model_path: config.model_path.clone(),
                detected_by: origin.to_string(),
                recorded_unix_ms: now_unix_ms,
            },
        )?;
    }
    let changed = config.role != "observer" || config.model_path.is_some();
    config.role = "observer".to_string();
    config.model_path = None;
    Ok(changed)
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
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arc-legacy-upgrade-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn v07_worker() -> NodeConfig {
        NodeConfig {
            role: "worker".to_string(),
            model_path: Some("/Users/ada/.arc/models/standard.gguf".to_string()),
            ..NodeConfig::default()
        }
    }

    #[test]
    fn a_v07_worker_is_held_as_an_observer_until_it_answers() {
        let dir = temp_dir("hold");
        let mut config = v07_worker();
        let changed =
            hold_until_answered(&mut config, &dir, Some("v0.7 chain data"), 7).unwrap();
        assert!(changed);
        assert_eq!(config.role, "observer");
        assert_eq!(config.model_path, None);
        let question = load(&dir).expect("question recorded");
        assert_eq!(question.previous_role, "worker");
        assert_eq!(
            question.previous_model_path.as_deref(),
            Some("/Users/ada/.arc/models/standard.gguf")
        );
        assert_eq!(question.recorded_unix_ms, 7);

        // Later launches keep holding it even without a fresh origin signal.
        let mut relaunch = v07_worker();
        assert!(hold_until_answered(&mut relaunch, &dir, None, 9).unwrap());
        assert_eq!(relaunch.role, "observer");
        assert_eq!(load(&dir).unwrap().recorded_unix_ms, 7);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_answer_ends_the_hold() {
        let dir = temp_dir("answered");
        let mut config = v07_worker();
        hold_until_answered(&mut config, &dir, Some("bridge"), 1).unwrap();
        let mut answered = NodeConfig {
            compute_consent: Some(true),
            ..v07_worker()
        };
        assert!(!hold_until_answered(&mut answered, &dir, Some("bridge"), 2).unwrap());
        assert_eq!(answered.role, "worker");
        clear(&dir).unwrap();
        clear(&dir).unwrap();
        assert!(load(&dir).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn installs_that_did_not_come_from_v07_are_never_asked() {
        let dir = temp_dir("fresh");
        let mut config = v07_worker();
        assert!(!hold_until_answered(&mut config, &dir, None, 1).unwrap());
        assert_eq!(config.role, "worker");
        assert!(load(&dir).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_bridge_record_marks_a_v07_origin() {
        let home = temp_dir("bridge-home");
        assert_eq!(detect_v07_origin(false, &home), None);
        assert!(detect_v07_origin(true, &home).is_some());
        let node = home
            .join(".arc")
            .join("legacy-bridge")
            .join("nodes")
            .join("desktop-0123456789ab");
        fs::create_dir_all(&node).unwrap();
        assert_eq!(detect_v07_origin(false, &home), None);
        fs::write(node.join("bridge-state.json"), b"{}").unwrap();
        assert!(detect_v07_origin(false, &home).is_some());
        fs::remove_dir_all(home).unwrap();
    }
}
