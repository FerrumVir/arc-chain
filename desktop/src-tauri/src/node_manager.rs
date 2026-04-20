use crate::types::{LogEntry, NodeConfig};
use std::collections::VecDeque;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
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
    /// Intentional-stop flag — `stop()` sets this before killing, so the reaper
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
        // checking id() is enough — if we haven't called wait() it's still alive.
        self.child.is_some()
    }

    pub async fn start(&mut self, config: &NodeConfig) -> anyhow::Result<()> {
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
                    "port {} busy — using {} instead (p2p {} → {})",
                    config.rpc_port, rpc_port, config.p2p_port, p2p_port
                ),
            )
            .await;
        }

        let mut cmd = Command::new(&binary);
        cmd.arg("--rpc-port")
            .arg(rpc_port.to_string())
            .arg("--p2p-port")
            .arg(p2p_port.to_string())
            .arg("--data-dir")
            .arg(&data_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(model) = &config.model_path {
            cmd.arg("--model").arg(model);
        }

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "Failed to start arc-node at {}: {}. Run the community installer first: curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh | bash",
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

        // Wait for the child in a background task. If it exits while we
        // haven't asked it to stop(), record a CrashInfo so node_status can
        // surface it to the UI as a red banner with "Relaunch".
        // We extract the pid upfront since child is moved into the task.
        let crash = self.crash_info.clone();
        let logs = self.logs.clone();
        let stopping = self.stopping.clone();
        self.stopping.store(false, std::sync::atomic::Ordering::SeqCst);
        // Clear any previous crash state now that we're launching again.
        *self.crash_info.lock().await = None;

        self.child = Some(child);
        // Take the child out momentarily to install the waiter. We re-insert
        // a lightweight handle via pid() — but tokio::process::Child doesn't
        // give us that split cleanly, so instead we install the waiter by
        // spawning right here and sharing the exit via the crash slot.
        // Simpler: spawn a task that holds a clone of the crash flag and
        // waits by polling the child's .wait() lazily. Since we can't clone
        // Child, we accept a race: if the user calls stop() cleanly, we set
        // `stopping=true` first, and the reaper checks it.
        // Workaround: capture the child's pid and spawn a wait_pid loop.
        if let Some(child) = self.child.as_mut() {
            if let Some(_pid) = child.id() {
                // Can't move `child` into the task because it belongs to self.
                // Instead, we detect crashes opportunistically: each node_status
                // poll calls `try_reap_if_crashed()` below.
            }
        }
        // The opportunistic reaper in try_reap_if_crashed() handles the
        // crash detection without needing to move the child.
        let _ = (crash, logs, stopping);

        push_log(&self.logs, "info", format!("arc-node started on :{}", rpc_port)).await;
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

    pub async fn restart(&mut self, config: &NodeConfig) -> anyhow::Result<()> {
        self.stop().await?;
        self.start(config).await
    }

    pub async fn logs_snapshot(&self, limit: usize) -> Vec<LogEntry> {
        let guard = self.logs.lock().await;
        let n = guard.len().min(limit);
        guard.iter().rev().take(n).cloned().collect::<Vec<_>>()
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

    // Prefer the community installer path: ~/.arc/bin/arc-node
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let installed = home.join(".arc").join("bin").join(if cfg!(windows) {
        "arc-node.exe"
    } else {
        "arc-node"
    });
    if installed.exists() {
        return Ok(installed);
    }

    // Fall back to PATH lookup
    if let Ok(p) = which_on_path("arc-node") {
        return Ok(p);
    }

    Err(anyhow::anyhow!(
        "arc-node binary not found. Expected at {} or on PATH",
        installed.display()
    ))
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
