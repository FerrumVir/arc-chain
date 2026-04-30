use crate::types::{LogEntry, NodeConfig};
use std::collections::VecDeque;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const LOG_RING_SIZE: usize = 2000;

pub struct NodeManager {
    child: Option<Child>,
    started_at: Option<Instant>,
    pub rpc_port: u16,
    pub logs: Arc<Mutex<VecDeque<LogEntry>>>,
    /// Set by the reaper when the child exits unexpectedly (we didn't call stop()).
    /// Surfaced through node_status.last_error so the UI can show a crash banner.
    pub crash_info: Arc<Mutex<Option<CrashInfo>>>,
    /// Intentional-stop flag - `stop()` sets this before killing, so the reaper
    /// doesn't misreport a clean shutdown as a crash.
    stopping: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashInfo {
    pub exit_code: Option<i32>,
    pub message: String,
    pub at_millis: i64,
}

/// Paths to the testnet bootstrap config bundled with the app.
/// Callers resolve these from the Tauri resource dir via AppHandle, then
/// hand them to `start()` so we don't carry Tauri types deep in NodeManager.
#[derive(Clone, Debug, Default)]
pub struct TestnetResources {
    pub seeds_file: Option<PathBuf>,
    pub genesis_file: Option<PathBuf>,
}

impl NodeManager {
    pub fn new() -> Self {
        Self {
            child: None,
            started_at: None,
            // Defaults to 9090 (community installer convention). Real value
            // is set from NodeConfig.rpc_port on start().
            rpc_port: 9090,
            logs: Arc::new(Mutex::new(VecDeque::with_capacity(LOG_RING_SIZE))),
            crash_info: Arc::new(Mutex::new(None)),
            stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub async fn clear_crash(&self) {
        *self.crash_info.lock().await = None;
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0)
    }

    pub fn is_running(&mut self) -> bool {
        // tokio::process::Child::try_wait doesn't exist on async Child;
        // checking id() is enough - if we haven't called wait() it's still alive.
        self.child.is_some()
    }

    /// Spawn arc-node with the CLI flags the chain actually accepts
    /// (verified against crates/arc-node/src/main.rs). Pass:
    ///   --rpc <ip>:<port>            bind the HTTP RPC server
    ///   --p2p-port <port>            QUIC P2P listener
    ///   --data-dir <dir>             where WAL + state live
    ///   --validator-seed <string>    deterministic keypair seed
    ///                                (the BIP-39 phrase from identity.rs)
    ///   --seeds-file <path>          testnet peer bootstrap list
    ///   --genesis <path>             testnet genesis.toml
    ///   --eth-rpc-port 0             disable the extra EVM RPC port
    ///   --community-mode             (worker role only) register with seed
    ///                                gateways as a volunteer inference worker
    ///   --model <path>               (optional) GGUF weights for local inference
    ///
    /// Any of these missing would leave the node either bound to wrong
    /// ports, isolated from the testnet, identity-mismatched, or silent
    /// as a worker. All are required for the "download → run → earn"
    /// operator flow.
    pub async fn start(
        &mut self,
        config: &NodeConfig,
        validator_seed: &str,
        resources: &TestnetResources,
    ) -> anyhow::Result<()> {
        if self.is_running() {
            return Ok(());
        }

        let binary = resolve_binary()?;
        let data_dir = resolve_data_dir(&config.data_dir);
        std::fs::create_dir_all(&data_dir).ok();

        // Probe for an available port pair. First preference is the configured
        // port; fall back in 10-port increments up to 5 tries. This catches the
        // common case of an old node process still bound or Jupyter stealing 9090.
        let (rpc_port, p2p_port) = choose_port_pair(config.rpc_port, config.p2p_port)?;
        if rpc_port != config.rpc_port {
            push_log(
                &self.logs,
                "warn",
                format!(
                    "port {} busy - using {} instead (p2p {} → {})",
                    config.rpc_port, rpc_port, config.p2p_port, p2p_port
                ),
            )
            .await;
        }

        let mut cmd = Command::new(&binary);
        cmd.arg("--rpc")
            .arg(format!("127.0.0.1:{}", rpc_port))
            .arg("--p2p-port")
            .arg(p2p_port.to_string())
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--validator-seed")
            .arg(validator_seed)
            .arg("--eth-rpc-port")
            .arg("0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Bundled testnet bootstrap config. Fall back to arc-node's built-in
        // defaults if the resource wasn't found (shouldn't happen in a
        // shipped build, but don't crash dev-mode).
        if let Some(seeds) = &resources.seeds_file {
            cmd.arg("--seeds-file").arg(seeds);
        } else {
            push_log(
                &self.logs,
                "warn",
                "testnet-seeds.txt not bundled - node will start isolated".into(),
            )
            .await;
        }
        if let Some(genesis) = &resources.genesis_file {
            cmd.arg("--genesis").arg(genesis);
        } else {
            push_log(
                &self.logs,
                "warn",
                "genesis.toml not bundled - node validator set will be empty".into(),
            )
            .await;
        }

        // Only register as a community inference worker if we actually
        // have a model to serve. role="worker" without model_path is
        // nonsense - the gateway would forward requests the node can't
        // answer. Default role is "observer" which just joins consensus,
        // validates blocks, and helps the network without requiring a
        // 4 GB model download.
        if config.role == "worker" && config.model_path.is_some() {
            cmd.arg("--community-mode");
        }

        if let Some(model) = &config.model_path {
            cmd.arg("--model").arg(model);
        }

        push_log(
            &self.logs,
            "info",
            format!(
                "spawning {} --rpc 127.0.0.1:{} --p2p-port {} --validator-seed <{}…> {}{}",
                binary.display(),
                rpc_port,
                p2p_port,
                &validator_seed
                    .chars()
                    .take(8)
                    .collect::<String>(),
                if config.role == "worker" {
                    "--community-mode "
                } else {
                    ""
                },
                config
                    .model_path
                    .as_deref()
                    .map(|p| format!("--model {}", p))
                    .unwrap_or_else(|| "(observer, no --model)".into()),
            ),
        )
        .await;

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "Failed to start arc-node at {}: {}. If this is your first launch, the binary should have been auto-downloaded. Check ~/.arc/bin/arc-node exists and is executable.",
                binary.display(),
                e
            )
        })?;

        self.rpc_port = rpc_port;
        self.started_at = Some(Instant::now());

        // Drain stdout/stderr into the log ring.
        if let Some(stdout) = child.stdout.take() {
            let logs = self.logs.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    push_log(&logs, "info", line).await;
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let logs = self.logs.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let level = classify_level(&line);
                    push_log(&logs, level, line).await;
                }
            });
        }

        self.stopping.store(false, std::sync::atomic::Ordering::SeqCst);
        *self.crash_info.lock().await = None;
        self.child = Some(child);

        push_log(
            &self.logs,
            "info",
            format!("arc-node started on 127.0.0.1:{}", rpc_port),
        )
        .await;
        Ok(())
    }

    /// Called on every node_status poll. If our child has exited and we didn't
    /// ask it to, record a CrashInfo. Non-blocking; safe to call frequently.
    pub async fn try_reap_if_crashed(&mut self) {
        if self.child.is_none() {
            return;
        }
        let Some(child) = self.child.as_mut() else { return };
        match child.try_wait() {
            Ok(Some(status)) => {
                let was_stopping = self
                    .stopping
                    .swap(false, std::sync::atomic::Ordering::SeqCst);
                self.child = None;
                self.started_at = None;
                if !was_stopping {
                    let code = status.code();
                    let message = format!(
                        "arc-node exited unexpectedly{}",
                        code.map(|c| format!(" (code {})", c)).unwrap_or_default()
                    );
                    push_log(&self.logs, "error", message.clone()).await;
                    *self.crash_info.lock().await = Some(CrashInfo {
                        exit_code: code,
                        message,
                        at_millis: chrono::Utc::now().timestamp_millis(),
                    });
                }
            }
            Ok(None) => { /* still running */ }
            Err(_) => { /* treat as still running; will re-check next tick */ }
        }
    }

    pub async fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            // Tell the crash reaper this is intentional so it doesn't fire.
            self.stopping
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = child.kill().await;
            let _ = child.wait().await;
            self.started_at = None;
            *self.crash_info.lock().await = None;
            push_log(&self.logs, "info", "arc-node stopped".into()).await;
        }
        Ok(())
    }

    pub async fn restart(
        &mut self,
        config: &NodeConfig,
        validator_seed: &str,
        resources: &TestnetResources,
    ) -> anyhow::Result<()> {
        self.stop().await?;
        self.start(config, validator_seed, resources).await
    }

    pub async fn logs_snapshot(&self, limit: usize) -> Vec<LogEntry> {
        let guard = self.logs.lock().await;
        let n = guard.len().min(limit);
        guard
            .iter()
            .rev()
            .take(n)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

async fn push_log(logs: &Arc<Mutex<VecDeque<LogEntry>>>, level: &str, message: String) {
    let entry = LogEntry {
        id: format!("log-{}", uuid_like()),
        timestamp: chrono::Utc::now().timestamp_millis(),
        level: level.into(),
        message,
    };
    let mut guard = logs.lock().await;
    if guard.len() == LOG_RING_SIZE {
        guard.pop_front();
    }
    guard.push_back(entry);
}

fn uuid_like() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 6];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn classify_level(line: &str) -> &'static str {
    let lower = line.to_ascii_lowercase();
    if lower.contains("error") || lower.contains("panic") {
        "error"
    } else if lower.contains("warn") {
        "warn"
    } else if lower.contains(" ok ") || lower.contains("✓") {
        "ok"
    } else {
        "info"
    }
}

fn resolve_binary() -> anyhow::Result<PathBuf> {
    // Allow override for tests
    if let Ok(p) = std::env::var("ARC_NODE_BINARY") {
        return Ok(PathBuf::from(p));
    }

    // Canonical app-managed path. Auto-download (commands::ensure_binary)
    // writes here on first launch.
    let p = managed_binary_path();
    if p.exists() {
        return Ok(p);
    }

    // Fall back to PATH lookup for devs who built arc-node themselves
    if let Ok(p) = which_on_path("arc-node") {
        return Ok(p);
    }

    Err(anyhow::anyhow!(
        "arc-node binary not found. Expected at {} or on PATH. The app should download it on first launch - check Settings → Diagnostics.",
        p.display()
    ))
}

/// Canonical location for the auto-downloaded arc-node binary.
/// `~/.arc/bin/arc-node` (or `.exe` on Windows). Public so commands.rs can
/// write to the same path during auto-download.
pub fn managed_binary_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".arc").join("bin").join(if cfg!(windows) {
        "arc-node.exe"
    } else {
        "arc-node"
    })
}

fn which_on_path(name: &str) -> anyhow::Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| anyhow::anyhow!("no PATH"))?;
    let exe = if cfg!(windows) {
        format!("{}.exe", name)
    } else {
        name.into()
    };
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(&exe);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(anyhow::anyhow!("{} not found on PATH", name))
}

fn resolve_data_dir(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        return home.join(rest);
    }
    PathBuf::from(s)
}

fn port_available(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpListener::bind(addr).is_ok()
}

fn choose_port_pair(preferred_rpc: u16, preferred_p2p: u16) -> anyhow::Result<(u16, u16)> {
    // Try up to 5 offsets in +10 increments. Both RPC and P2P must be free.
    for i in 0..5 {
        let rpc = preferred_rpc + (i * 10);
        let p2p = preferred_p2p + (i * 10);
        if port_available(rpc) && port_available(p2p) {
            return Ok((rpc, p2p));
        }
    }
    Err(anyhow::anyhow!(
        "ports {}/{} and 5 fallbacks all busy. Free a port and retry, or change RPC port in Settings.",
        preferred_rpc,
        preferred_p2p
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_binary_is_under_home_bin() {
        let p = managed_binary_path();
        let s = p.to_string_lossy();
        assert!(s.contains(".arc"), "path should contain .arc: {s}");
        assert!(s.ends_with("arc-node") || s.ends_with("arc-node.exe"));
    }

    #[test]
    fn resolve_data_dir_expands_tilde() {
        let p = resolve_data_dir("~/foo");
        assert!(!p.starts_with("~"));
    }

    #[test]
    fn port_pair_probes_fallback() {
        // Hold one unprivileged port busy. The probe tries offsets +0,+10,
        // +20,+30,+40; with only one port taken, one of those falls
        // through to an available slot.
        let l1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = l1.local_addr().unwrap().port();
        // Pair the busy RPC with a separate unprivileged p2p seed so the
        // probe has a free-port window to land in.
        let l2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let p2p_seed = l2.local_addr().unwrap().port();
        // Release l2 - we only wanted its assigned port number to feed in.
        drop(l2);
        let result = choose_port_pair(busy, p2p_seed);
        assert!(result.is_ok(), "probe should find a free pair");
        let (rpc, _p2p) = result.unwrap();
        assert_ne!(rpc, busy, "should have picked a fallback rpc port");
    }
}

impl Default for NodeManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
fn _path_sanity(_: &Path) {}
