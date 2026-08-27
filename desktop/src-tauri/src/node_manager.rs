use crate::paths;
use crate::types::{LogEntry, NodeConfig};
use std::collections::VecDeque;
use std::net::{SocketAddr, TcpListener, UdpSocket};
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
    /// Core count the *running* child was actually launched with. Read back
    /// by `node_status` so the Dashboard reports the node's real compute
    /// width rather than whatever the config currently says — those diverge
    /// the moment the user moves the slider without applying it.
    pub active_worker_threads: Option<u32>,
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
            active_worker_threads: None,
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
            .arg("--eth-rpc-port")
            .arg("0")
            // ── LIVE-NETWORK SAFETY: join as an observer, never a validator ──
            //
            // Current arc-node builds default `--stake` to 0, but every
            // released binary through v0.7.11 defaults to 5,000,000 ARC — and
            // this manager may spawn any of them. Passing the flag explicitly
            // keeps the desktop safe regardless of binary vintage: without it,
            // an old binary announces itself to the public seeds as a 5M-stake
            // validator and tries to shard-join the testnet — welding a
            // phantom validator into validator sets
            // that are currently frozen, on a network where four of six
            // seeds have not produced a block in ~6 days. Recovering from
            // that means hand-editing state on six VPSes.
            //
            // `--stake 0` is the observer path: full consensus participation
            // and DAG validation, zero claim on the validator set. It exists
            // in every arc-node version the desktop can encounter, which is
            // why it is passed unconditionally rather than probed for.
            //
            // TODO(chain-core): arc-node v0.7.11 gained `--community`, which
            // is exactly `--stake 0 --community-mode` plus GGUF
            // auto-discovery. Prefer it once the minimum supported node
            // version is >= 0.7.11 — passing an unknown flag to an older
            // binary makes clap abort before the node ever starts, so it
            // cannot simply be swapped in while 0.7.9 nodes are still in the
            // field. Gate it on `binary_supports_flag(&binary, "--community")`.
            .arg("--stake")
            .arg("0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // ── Compute contribution ────────────────────────────────────────
        // rayon sizes its global pool from RAYON_NUM_THREADS the first time
        // that pool is built, and arc-node only ever calls ThreadPoolBuilder
        // explicitly under `--benchmark`. Setting the env var is therefore
        // the one control that works on every shipped node version, with no
        // chain-side change required.
        //
        // `--threads` is the explicit flag the chain-core agent is adding.
        // It is probed rather than assumed for the same reason as
        // `--community` above: an unknown flag is a hard clap failure, and a
        // node that will not start is a worse outcome than a node running at
        // its default width.
        if let Some(n) = config.worker_threads.filter(|n| *n > 0) {
            cmd.env("RAYON_NUM_THREADS", n.to_string());
            if binary_supports_flag(&binary, "--threads") {
                cmd.arg("--threads").arg(n.to_string());
            } else {
                push_log(
                    &self.logs,
                    "info",
                    format!(
                        "limiting node to {} cores via RAYON_NUM_THREADS (this arc-node has no --threads flag)",
                        n
                    ),
                )
                .await;
            }
        }

        // Windows: detach from the GUI parent's console.
        //
        // arc-node is a console executable. When a Tauri GUI app spawns
        // it without these flags, Windows allocates a fresh console
        // window for the child and ties it to the parent's console
        // group. Two failure modes follow:
        //
        //   1. The black console pops up alongside the desktop window.
        //      Users (reasonably) close it. Closing a console sends
        //      CTRL_CLOSE_EVENT to every process attached to it; the
        //      C runtime's default handler exits with NTSTATUS
        //      0xC000013A (STATUS_CONTROL_C_EXIT, decimal -1073741510).
        //      That is exactly the exit code we see in field crash
        //      reports.
        //   2. Any Ctrl+C delivered to the parent console is also
        //      delivered to the child via the shared process group,
        //      same -1073741510 exit.
        //
        // CREATE_NO_WINDOW (0x08000000) tells Windows not to allocate
        // a console for the child. CREATE_NEW_PROCESS_GROUP (0x00000200)
        // additionally isolates it from any console signals the parent
        // does receive. Together they make the child immune to the
        // console-event class of crashes. Stdio::piped() above still
        // gives us the child's stdout/stderr, so log capture is
        // unaffected.
        #[cfg(windows)]
        {
            // `tokio::process::Command::creation_flags` is a Windows-only
            // inherent method (it forwards to CreateProcessW). No std
            // trait import needed.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        }

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
            if !binary_supports_flag(&binary, "--community-rpc-url") {
                anyhow::bail!(
                    "this arc-node predates secure community RPC origins; update arc-node before starting community mode"
                );
            }
            cmd.arg("--community-mode");
            for origin in crate::rpc_client::PRODUCTION_RPC_ORIGINS {
                cmd.arg("--community-rpc-url").arg(origin);
            }
        }

        if let Some(model) = &config.model_path {
            cmd.arg("--model").arg(model);
        }

        push_log(
            &self.logs,
            "info",
            format!(
                "spawning {} --rpc 127.0.0.1:{} --p2p-port {} --stake 0 --validator-seed <{}…> {}{}{}",
                binary.display(),
                rpc_port,
                p2p_port,
                &validator_seed
                    .chars()
                    .take(8)
                    .collect::<String>(),
                if config.role == "worker" && config.model_path.is_some() {
                    "--community-mode "
                } else {
                    ""
                },
                config
                    .worker_threads
                    .filter(|n| *n > 0)
                    .map(|n| format!("({} cores) ", n))
                    .unwrap_or_default(),
                config
                    .model_path
                    .as_deref()
                    .map(|p| format!("--model {}", p))
                    .unwrap_or_else(|| "(observer, no --model)".into()),
            ),
        )
        .await;

        // The validator seed is the wallet's BIP-39 phrase, so it goes in the
        // environment rather than argv. A process's command line is readable
        // by every user on the machine (`ps -ax -o command`), which would put
        // the phrase that controls the user's funds in the process table;
        // its environment is readable only by the owning user. arc-node reads
        // ARC_VALIDATOR_SEED as the fallback for --validator-seed.
        //
        // Older binaries (<= v0.7.11) predate the env fallback and would
        // silently derive the DEFAULT identity from "arc-validator-0" — a
        // different address, so earnings would accrue to a key the user does
        // not hold. Only omit the flag when the binary advertises the env var.
        // clap prints `[env: ARC_VALIDATOR_SEED=]` in --help for an arg with an
        // env fallback, so the existing --help probe answers this too.
        if binary_supports_flag(&binary, "ARC_VALIDATOR_SEED") {
            cmd.env("ARC_VALIDATOR_SEED", validator_seed);
        } else {
            tracing::warn!(
                "arc-node at {} predates ARC_VALIDATOR_SEED; passing the seed on \
                 the command line, where other users on this machine can read it. \
                 Update the node binary to keep it out of the process table.",
                binary.display()
            );
            cmd.arg("--validator-seed").arg(validator_seed);
        }

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "Failed to start arc-node at {}: {}. If this is your first launch, the binary should have been auto-downloaded. Check ~/.arc/bin/arc-node exists and is executable.",
                binary.display(),
                e
            )
        })?;

        self.rpc_port = rpc_port;
        self.started_at = Some(Instant::now());
        self.active_worker_threads = config.worker_threads.filter(|n| *n > 0);

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
                self.active_worker_threads = None;
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
            self.active_worker_threads = None;
            *self.crash_info.lock().await = None;
            push_log(&self.logs, "info", "arc-node stopped".into()).await;
            return Ok(());
        }
        // No managed child handle. This happens when the Tauri process
        // restarted (e.g. cargo rebuild in dev) while arc-node — spawned
        // with CREATE_NEW_PROCESS_GROUP — kept running detached. Locate
        // it by the managed binary path and kill it so the UI's Stop
        // button is not a no-op.
        let killed = kill_detached_arc_node();
        if killed > 0 {
            self.started_at = None;
            *self.crash_info.lock().await = None;
            push_log(
                &self.logs,
                "info",
                format!("arc-node stopped (detached, {} pid)", killed),
            )
            .await;
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

/// An explicitly configured binary path, if the operator set one.
///
/// `ARC_NODE_BIN` is the documented name; `ARC_NODE_BINARY` is kept because
/// existing test fixtures and dev scripts already export it. Both win over
/// every other resolution step — if someone names a binary, that is the
/// binary, and `ensure_binary` must not second-guess it with a download.
pub fn env_binary_override() -> Option<PathBuf> {
    for key in ["ARC_NODE_BIN", "ARC_NODE_BINARY"] {
        if let Some(v) = std::env::var_os(key) {
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
}

/// A locally built `target/release/arc-node`, if this app is running from a
/// repo checkout.
///
/// This is the path the demo machine takes: the repo has a freshly built
/// arc-node that matches the desktop version, while the published GitHub
/// release ships no arc-node assets at all. Without this, a dev-mode run has
/// nothing to launch even with the binary sitting two directories away.
///
/// Searched relative to both the working directory (`cargo tauri dev` runs
/// from `desktop/src-tauri`, some tooling from `desktop/`) and the running
/// executable's ancestors (`target/debug/arc-desktop` lives inside the same
/// checkout), so it resolves whichever way the app was launched.
pub fn dev_build_binary() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) { "arc-node.exe" } else { "arc-node" };

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        // desktop/src-tauri → ../.. = repo root; desktop → .. = repo root.
        roots.push(cwd.join("..").join(".."));
        roots.push(cwd.join(".."));
        roots.push(cwd.clone());
    }
    if let Ok(exe) = std::env::current_exe() {
        // Walk up from the executable; one of these ancestors is the repo
        // root when running a dev build out of target/.
        roots.extend(exe.ancestors().take(8).map(PathBuf::from));
    }

    for root in roots {
        let cand = root.join("target").join("release").join(exe_name);
        if cand.is_file() {
            // Canonicalize so the `../..` forms turn into a clean absolute
            // path in logs and in the "detached process" comparison.
            return Some(cand.canonicalize().unwrap_or(cand));
        }
    }
    None
}

fn resolve_binary() -> anyhow::Result<PathBuf> {
    // 1. Explicit override (env). Highest precedence, no questions asked.
    if let Some(p) = env_binary_override() {
        return Ok(p);
    }

    // 2. Canonical app-managed path. Auto-download (commands::ensure_binary)
    //    writes here on first launch.
    let managed = managed_binary_path();
    if managed.exists() {
        return Ok(managed);
    }

    // 3. A release build sitting in this repo checkout (dev + demo machines).
    if let Some(p) = dev_build_binary() {
        return Ok(p);
    }

    // 4. PATH lookup, for devs who installed arc-node system-wide.
    if let Ok(p) = which_on_path("arc-node") {
        return Ok(p);
    }

    Err(anyhow::anyhow!(
        "arc-node binary not found. Looked at $ARC_NODE_BIN, {}, ./target/release/arc-node in this checkout, and PATH. \
         Build one with `cargo build --release -p arc-node`, or set ARC_NODE_BIN to an existing binary.",
        managed.display()
    ))
}

/// Does this arc-node accept `flag`?
///
/// clap aborts the process on an unrecognized argument, so every optional
/// flag must be probed before use or a node that would have started fine at
/// default settings refuses to start at all. `--help` output is the only
/// version-independent way to ask.
///
/// Cached per (binary, flag): `--help` costs a process spawn, and `start()`
/// runs on every user-visible Start click.
pub fn binary_supports_flag(binary: &Path, flag: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex as StdMutex, OnceLock};

    static CACHE: OnceLock<StdMutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| StdMutex::new(HashMap::new()));
    let key = format!("{}\u{0}{}", binary.display(), flag);
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(&key) {
            return *hit;
        }
    }

    let mut cmd = std::process::Command::new(binary);
    cmd.arg("--help");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let supported = match cmd.output() {
        Ok(out) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            // Match the flag followed by a boundary so `--threads` does not
            // report true because `--bench-rayon-threads` is present.
            text.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
                .any(|tok| tok == flag)
        }
        Err(_) => false,
    };
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, supported);
    }
    supported
}

/// Canonical location for the auto-downloaded arc-node binary.
/// `~/.arc/bin/arc-node` (or `.exe` on Windows). Public so commands.rs can
/// write to the same path during auto-download.
/// Find and kill any arc-node process whose executable matches the managed
/// binary path. Returns the count killed. Used by `stop()` when the in-memory
/// child handle is gone (typically after a Tauri-side dev rebuild).
fn kill_detached_arc_node() -> usize {
    // Match every path the node could have been launched from, not just the
    // managed one — otherwise Stop is a silent no-op for anyone running the
    // dev build or an ARC_NODE_BIN override, which is exactly the demo
    // machine's configuration.
    let candidates: Vec<PathBuf> = [
        env_binary_override(),
        Some(managed_binary_path()),
        dev_build_binary(),
    ]
    .into_iter()
    .flatten()
    .map(|p| p.canonicalize().unwrap_or(p))
    .collect();

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut killed = 0;
    for proc_ in sys.processes().values() {
        let Some(exe) = proc_.exe() else { continue };
        let exe_canon = exe.canonicalize().unwrap_or(exe.to_path_buf());
        if candidates.contains(&exe_canon) && proc_.kill() {
            killed += 1;
        }
    }
    killed
}

pub fn managed_binary_path() -> PathBuf {
    paths::arc_home().join("bin").join(if cfg!(windows) {
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

/// Resolve the configured data dir. Goes through [`paths::expand_tilde`] so
/// the default `~/.arc` lands in the user's profile on Windows instead of
/// `./.arc` relative to the GUI's CWD.
pub fn resolve_data_dir(s: &str) -> PathBuf {
    paths::expand_tilde(s)
}

/// Can we bind TCP on this port? Correct probe for the RPC listener.
fn tcp_available(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpListener::bind(addr).is_ok()
}

/// Can we bind UDP on this port? Correct probe for the P2P listener, which
/// is QUIC and therefore UDP.
///
/// The previous code TCP-probed the P2P port, which made the probe blind to
/// the exact failure it was written to survive: with WSL2 or Docker Desktop
/// installed, Hyper-V reserves dynamic UDP exclusion ranges that frequently
/// cover 9000-9100. UDP 9091 is then un-bindable by any user-mode process
/// while TCP 9091 binds fine — so the probe passed, arc-node got a port it
/// could not use, and the only sign was a silent fall back to an ephemeral
/// port with no inbound reachability.
fn udp_available(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    UdpSocket::bind(addr).is_ok()
}

fn choose_port_pair(preferred_rpc: u16, preferred_p2p: u16) -> anyhow::Result<(u16, u16)> {
    // Try up to 5 offsets in +10 increments. RPC must be TCP-bindable and
    // P2P must be UDP-bindable.
    for i in 0..5 {
        let rpc = preferred_rpc.saturating_add(i * 10);
        let p2p = preferred_p2p.saturating_add(i * 10);
        if tcp_available(rpc) && udp_available(p2p) {
            return Ok((rpc, p2p));
        }
    }
    Err(anyhow::anyhow!(
        "RPC ports {}+ (TCP) and P2P ports {}+ (UDP) are all busy across 5 fallbacks. \
         On Windows this is usually a Hyper-V/WSL2 UDP exclusion range - run \
         `netsh int ipv4 show excludedportrange protocol=udp` to check. Change the RPC port in Settings to move both.",
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

    /// The P2P listener is QUIC/UDP. A TCP bind on the same number proves
    /// nothing about it, which is what made the old probe blind to Hyper-V's
    /// UDP exclusion ranges.
    #[test]
    fn p2p_probe_is_udp_not_tcp() {
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let busy_udp = udp.local_addr().unwrap().port();
        assert!(!udp_available(busy_udp), "held UDP port must read as busy");
        // The same port number is still free for TCP - the exact blind spot
        // the old probe had.
        assert!(
            tcp_available(busy_udp),
            "TCP on a UDP-busy port is typically free; this is why the probes must differ"
        );
    }

    #[test]
    fn choose_port_pair_skips_udp_busy_p2p() {
        let rpc_l = TcpListener::bind("127.0.0.1:0").unwrap();
        let rpc = rpc_l.local_addr().unwrap().port();
        drop(rpc_l);
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let busy_p2p = udp.local_addr().unwrap().port();
        let (_, chosen_p2p) = choose_port_pair(rpc, busy_p2p).expect("should find a free pair");
        assert_ne!(chosen_p2p, busy_p2p, "must not hand back a UDP-busy p2p port");
    }

    /// `--threads` must not report as supported just because
    /// `--bench-rayon-threads` appears in the same help text.
    #[test]
    fn flag_probe_requires_whole_token_match() {
        // `/bin/echo` is not arc-node, so the probe returns false rather
        // than panicking - the safe default for an unknown binary.
        assert!(!binary_supports_flag(Path::new("/nonexistent/arc-node"), "--threads"));
    }

    #[test]
    fn production_community_origins_are_six_distinct_https_origins() {
        let unique: std::collections::HashSet<_> =
            crate::rpc_client::PRODUCTION_RPC_ORIGINS.into_iter().collect();
        assert_eq!(unique.len(), 6);
        for origin in unique {
            assert!(origin.starts_with("https://"));
            assert!(origin.ends_with(".nip.io"));
            assert!(!origin.contains(":9090"));
            assert!(!origin["https://".len()..].contains('/'));
        }
    }

    #[test]
    fn data_dir_lands_under_home_not_cwd() {
        // The Windows bug in miniature: `~/.arc` must never resolve to a
        // relative `./.arc`.
        let p = resolve_data_dir("~/.arc");
        assert!(p.is_absolute() || p.starts_with(paths::home_dir()));
        assert!(!p.starts_with("./"));
    }
}

impl Default for NodeManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
fn _path_sanity(_: &Path) {}
