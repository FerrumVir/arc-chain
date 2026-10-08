//! `bridge-state.json`: what the bridge started and why. No secrets.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::write_atomic;

pub const STATE_SCHEMA: &str = "arc.legacy-bridge.state.v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeState {
    pub schema: String,
    pub bridge_version: String,
    pub legacy_kind: String,
    pub legacy_data_dir: String,
    pub legacy_rpc_port: u16,
    pub legacy_p2p_port: u16,
    pub legacy_model: Option<String>,
    pub archive_record: String,
    pub archive_generation: u32,
    pub node_release_tag: String,
    pub node_binary: String,
    pub node_data_dir: String,
    pub node_keyfile: String,
    pub node_address: String,
    pub node_rpc: String,
    pub stake: u64,
    pub community_registration: bool,
    pub compute: String,
    pub supervisor: String,
    pub updated_unix: u64,
}

impl BridgeState {
    pub fn write(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn read(path: &Path) -> Result<BridgeState> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("{} is not a bridge state record", path.display()))
    }
}

/// Which supervisor restarts this node, from the environment it set.
pub fn detect_supervisor(desktop: bool) -> String {
    if desktop {
        return "desktop-app".to_string();
    }
    let launchd_label = std::env::var("XPC_SERVICE_NAME")
        .ok()
        .filter(|label| !label.is_empty() && label != "0");
    if let Some(label) = launchd_label {
        return format!("launchd:{label}");
    }
    if std::env::var_os("INVOCATION_ID").is_some() {
        return "systemd".to_string();
    }
    "unsupervised".to_string()
}

/// The command an operator runs to restart the bridged node.
pub fn restart_hint(supervisor: &str) -> String {
    if let Some(label) = supervisor.strip_prefix("launchd:") {
        format!("launchctl kickstart -k gui/$(id -u)/{label}")
    } else if supervisor == "systemd" {
        "sudo systemctl restart arc-node".to_string()
    } else if supervisor == "desktop-app" {
        "restart the node from the ARC Node app".to_string()
    } else {
        "stop arc-node and start it again with the same command".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::test_support::TempDir;

    #[test]
    fn state_round_trips_and_hints_name_the_supervisor() {
        let temp = TempDir::new("state");
        let state = BridgeState {
            schema: STATE_SCHEMA.to_string(),
            bridge_version: "0.7.12".to_string(),
            legacy_kind: "headless".to_string(),
            legacy_data_dir: "/home/ops/.arc/data".to_string(),
            legacy_rpc_port: 9944,
            legacy_p2p_port: 9945,
            legacy_model: None,
            archive_record: "v0.7-data-archive-0001.json".to_string(),
            archive_generation: 1,
            node_release_tag: "v0.8.10".to_string(),
            node_binary: "/home/ops/.arc/legacy-bridge/releases/v0.8.10/arc-node-linux-x86_64".to_string(),
            node_data_dir: "/home/ops/.arc/legacy-bridge/nodes/headless-0123456789ab/data".to_string(),
            node_keyfile: "/home/ops/.arc/legacy-bridge/nodes/headless-0123456789ab/identity/validator-key.json"
                .to_string(),
            node_address: "ab".repeat(32),
            node_rpc: "127.0.0.1:9944".to_string(),
            stake: 0,
            community_registration: false,
            compute: "off: no compute consent has been recorded".to_string(),
            supervisor: "systemd".to_string(),
            updated_unix: 1,
        };
        let path = temp.path().join("bridge-state.json");
        state.write(&path).unwrap();
        assert_eq!(BridgeState::read(&path).unwrap(), state);
        assert_eq!(restart_hint("systemd"), "sudo systemctl restart arc-node");
        assert!(restart_hint("launchd:com.arc.inference").ends_with("/com.arc.inference"));
    }
}
