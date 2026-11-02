//! Node Care: opt-out anonymous error reports and a daily heartbeat.
//!
//! The WebView decides what to report, builds and scrubs every payload, and
//! keeps the queue and rate limits (`src/lib/node-care.ts`, unit-tested
//! there). This module does three small things the WebView cannot:
//!
//! 1. Keep that state in `node-care.json` in the app-data dir.
//! 2. Report the platform (`os`, `arch`) and app version.
//! 3. POST one payload to the endpoint baked in at build time through
//!    `ARC_NODE_CARE_ENDPOINT`. Unset or empty means reporting is disabled:
//!    nothing is ever sent, whatever the WebView asks.
//!
//! It refuses anything but the two published schemas with their exact field
//! lists, refuses when the saved switch is not explicitly on, and never
//! surfaces a delivery error: the caller only learns delivered or not.

use crate::AppState;
use serde::Serialize;
use std::path::Path;
use std::time::Duration;
use tauri::State;

const ENDPOINT: Option<&str> = option_env!("ARC_NODE_CARE_ENDPOINT");
const STATE_FILE: &str = "node-care.json";
const MAX_STATE_BYTES: usize = 256 * 1024;
const MAX_PAYLOAD_BYTES: usize = 8 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const REPORT_SCHEMA: &str = "arc.node-care.report.v1";
pub(crate) const HEARTBEAT_SCHEMA: &str = "arc.node-care.heartbeat.v1";

const REPORT_FIELDS: &[&str] = &[
    "schema",
    "install_id",
    "event",
    "app_version",
    "node_version",
    "os",
    "arch",
    "stage",
    "error_code",
    "message",
    "uptime_s",
    "occurred_at",
];
const HEARTBEAT_FIELDS: &[&str] = &[
    "schema",
    "install_id",
    "app_version",
    "node_version",
    "status",
    "os",
];

/// Serializes state-file writes; two IPC saves must not share a temp file.
static STATE_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The build-time endpoint, HTTPS only, without a trailing slash.
fn endpoint() -> Option<&'static str> {
    normalize_endpoint(ENDPOINT?)
}

fn normalize_endpoint(raw: &str) -> Option<&str> {
    let trimmed = raw.trim().trim_end_matches('/');
    let host = trimmed.strip_prefix("https://")?;
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return None;
    }
    Some(trimmed)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeCareInfo {
    /// False when this build has no report endpoint: nothing can be sent.
    pub endpoint_configured: bool,
    pub app_version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    /// Used only on this machine, to replace the home folder with `~` in a
    /// message before it is sent. Never part of a payload.
    pub home_dir: String,
}

#[tauri::command]
pub fn node_care_info() -> NodeCareInfo {
    NodeCareInfo {
        endpoint_configured: endpoint().is_some(),
        app_version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        home_dir: crate::paths::home_dir().to_string_lossy().into_owned(),
    }
}

#[tauri::command]
pub async fn node_care_load_state(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let dir = state.data_dir.lock().await.clone();
    Ok(read_state(&dir))
}

#[tauri::command]
pub async fn node_care_save_state(state: State<'_, AppState>, json: String) -> Result<(), String> {
    if json.len() > MAX_STATE_BYTES {
        return Err("node care state is too large".to_string());
    }
    match serde_json::from_str::<serde_json::Value>(&json) {
        Ok(value) if value.is_object() => {}
        _ => return Err("node care state must be a JSON object".to_string()),
    }
    let dir = state.data_dir.lock().await.clone();
    write_state(&dir, &json).map_err(|error| error.to_string())
}

/// POST one payload. `kind` is `report` or `heartbeat`. Resolves `true` only
/// on a 2xx answer; every refusal and failure is `false`, never an error.
#[tauri::command]
pub async fn node_care_send(
    state: State<'_, AppState>,
    kind: String,
    body: String,
) -> Result<bool, String> {
    let Some(endpoint) = endpoint() else {
        return Ok(false);
    };
    let Some((route, schema, fields)) = route_for(&kind) else {
        return Ok(false);
    };
    if body.len() > MAX_PAYLOAD_BYTES || !payload_shape_ok(&body, schema, fields) {
        return Ok(false);
    }
    let dir = state.data_dir.lock().await.clone();
    if !reporting_enabled(&dir) {
        return Ok(false);
    }
    let client = match reqwest::Client::builder()
        .timeout(SEND_TIMEOUT)
        .connect_timeout(SEND_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => return Ok(false),
    };
    let result = client
        .post(format!("{endpoint}{route}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await;
    Ok(matches!(result, Ok(response) if response.status().is_success()))
}

fn route_for(kind: &str) -> Option<(&'static str, &'static str, &'static [&'static str])> {
    match kind {
        "report" => Some(("/v1/report", REPORT_SCHEMA, REPORT_FIELDS)),
        "heartbeat" => Some(("/v1/heartbeat", HEARTBEAT_SCHEMA, HEARTBEAT_FIELDS)),
        _ => None,
    }
}

/// A flat JSON object with the expected schema and no field outside the
/// published list, so this command cannot carry anything else.
fn payload_shape_ok(body: &str, schema: &str, fields: &[&str]) -> bool {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    map.get("schema").and_then(|value| value.as_str()) == Some(schema)
        && map.keys().all(|key| fields.contains(&key.as_str()))
        && map
            .values()
            .all(|value| !value.is_object() && !value.is_array())
}

/// Sending needs the saved switch to be explicitly on.
fn reporting_enabled(dir: &Path) -> bool {
    read_state(dir)
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("enabled").and_then(|enabled| enabled.as_bool()))
        == Some(true)
}

fn read_state(dir: &Path) -> Option<String> {
    let path = dir.join(STATE_FILE);
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES as u64 {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    if value.is_object() {
        Some(text)
    } else {
        None
    }
}

fn write_state(dir: &Path, json: &str) -> std::io::Result<()> {
    let _guard = STATE_WRITE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".{STATE_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&temp, json)?;
    std::fs::rename(&temp, dir.join(STATE_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_must_be_https_and_non_empty() {
        assert_eq!(normalize_endpoint(""), None);
        assert_eq!(normalize_endpoint("   "), None);
        assert_eq!(normalize_endpoint("https://"), None);
        assert_eq!(normalize_endpoint("http://reports.example"), None);
        assert_eq!(
            normalize_endpoint(" https://reports.example/ "),
            Some("https://reports.example")
        );
    }

    #[test]
    fn only_the_published_shapes_pass() {
        let report = r#"{"schema":"arc.node-care.report.v1","install_id":"00","event":"crash","app_version":"0.8.14","node_version":"0.8.14","os":"macos","arch":"aarch64","stage":"node","error_code":"exited-unexpectedly","message":"","uptime_s":5,"occurred_at":"2026-10-22T00:00:00Z"}"#;
        assert!(payload_shape_ok(report, REPORT_SCHEMA, REPORT_FIELDS));
        assert!(!payload_shape_ok(
            report,
            HEARTBEAT_SCHEMA,
            HEARTBEAT_FIELDS
        ));

        let extra = r#"{"schema":"arc.node-care.report.v1","address":"x"}"#;
        assert!(!payload_shape_ok(extra, REPORT_SCHEMA, REPORT_FIELDS));
        let nested = r#"{"schema":"arc.node-care.report.v1","message":{"a":1}}"#;
        assert!(!payload_shape_ok(nested, REPORT_SCHEMA, REPORT_FIELDS));
        assert!(!payload_shape_ok("[]", REPORT_SCHEMA, REPORT_FIELDS));

        let heartbeat = r#"{"schema":"arc.node-care.heartbeat.v1","install_id":"00","app_version":"0.8.14","node_version":"0.8.14","status":"earning","os":"linux"}"#;
        assert!(payload_shape_ok(
            heartbeat,
            HEARTBEAT_SCHEMA,
            HEARTBEAT_FIELDS
        ));
        assert!(route_for("other").is_none());
    }

    #[test]
    fn sending_needs_the_switch_explicitly_on() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!reporting_enabled(dir.path()));
        write_state(dir.path(), r#"{"enabled":false}"#).unwrap();
        assert!(!reporting_enabled(dir.path()));
        write_state(dir.path(), r#"{"enabled":true,"installId":"ab"}"#).unwrap();
        assert!(reporting_enabled(dir.path()));
        assert_eq!(
            read_state(dir.path()).as_deref(),
            Some(r#"{"enabled":true,"installId":"ab"}"#)
        );
    }
}
