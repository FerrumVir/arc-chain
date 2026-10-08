//! Islands of separate processes on one host: the `arc-island stage`
//! processes joined by TCP, as the multi-process tests and the benchmark run
//! them. Each stage process prints one JSON line with the address it listens
//! on; stages start from the last to the first so each knows its next hop.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;

use serde_json::Value;

use super::coordinator::Coordinator;
use super::transport::{TcpTransport, Transport};
use crate::modern::ModernError;
use crate::modern::mla::config::MlaConfig;

/// One `arc-island stage` process.
pub struct StageProcess {
    pub child: Child,
    /// Where it listens.
    pub address: String,
    /// What it reported when it came up.
    pub hello: Value,
    exe: PathBuf,
    args: Vec<String>,
    stdout: BufReader<ChildStdout>,
}

impl StageProcess {
    /// Start `exe stage <args> --listen <listen>` and wait for its hello.
    pub fn spawn(exe: &Path, args: Vec<String>, listen: &str) -> Result<Self, ModernError> {
        Self::spawn_command(exe, "stage", args, listen)
    }

    /// A finite replica server or relay, with the same readiness/lifecycle
    /// contract as a stage. `restart` is only supported for stage processes.
    pub fn spawn_command(
        exe: &Path,
        subcommand: &str,
        args: Vec<String>,
        listen: &str,
    ) -> Result<Self, ModernError> {
        let mut command = Command::new(exe);
        command
            .arg(subcommand)
            .args(&args)
            .args(["--listen", listen])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|e| ModernError::Io(format!("{}: {e}", exe.display())))?;
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .map_err(|e| ModernError::Io(format!("stage hello: {e}")))?;
        let hello: Value = serde_json::from_str(line.trim()).map_err(|_| {
            let _ = child.kill();
            ModernError::Io(format!("the stage process did not come up: {line:?}"))
        })?;
        let address = hello["listening"]
            .as_str()
            .ok_or_else(|| ModernError::Io(format!("stage hello without an address: {line}")))?
            .to_string();
        Ok(Self {
            child,
            address,
            hello,
            exe: exe.to_path_buf(),
            args,
            stdout,
        })
    }

    /// Kill the process without warning (SIGKILL on Unix): a crash.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Start the same stage again on the same address (a restart after a
    /// crash; with a log directory it replays its log first).
    pub fn restart(&mut self) -> Result<(), ModernError> {
        let again = Self::spawn(&self.exe, self.args.clone(), &self.address.clone())?;
        *self = again;
        Ok(())
    }

    /// Wait for the process to exit and return its last stdout line (the
    /// stage's statistics).
    pub fn finish(mut self) -> Result<Value, ModernError> {
        let mut last = Value::Null;
        let mut line = String::new();
        while self.stdout.read_line(&mut line).unwrap_or(0) > 0 {
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                last = v;
            }
            line.clear();
        }
        let _ = self.child.wait();
        Ok(last)
    }
}

/// An island of stage processes on this host.
pub struct ProcessIsland {
    pub stages: Vec<StageProcess>,
    pub coordinator: Coordinator,
}

impl ProcessIsland {
    /// Start one `arc-island stage` process per range of `cuts`, all opening
    /// `package` (which must hold every layer; each process reads only its
    /// own), and a coordinator in this process. `extra(s)` adds arguments
    /// for stage `s` (log directory, WAN emulation, faults).
    pub fn launch(
        exe: &Path,
        package: &Path,
        cuts: &[usize],
        config: &MlaConfig,
        extra: impl Fn(usize) -> Vec<String>,
    ) -> Result<Self, ModernError> {
        let transport: Arc<dyn Transport> = Arc::new(TcpTransport);
        let listener = transport
            .listen("127.0.0.1:0")
            .map_err(|e| ModernError::Io(format!("coordinator listen: {e}")))?;
        let mut next = listener.address();
        let mut stages = Vec::new();
        for s in (0..cuts.len() - 1).rev() {
            let mut args = vec![
                "--package".to_string(),
                package.display().to_string(),
                "--layers".into(),
                format!("{}:{}", cuts[s], cuts[s + 1]),
                "--next".into(),
                next.clone(),
            ];
            args.extend(extra(s));
            let stage = StageProcess::spawn(exe, args, "127.0.0.1:0")?;
            next = stage.address.clone();
            stages.push(stage);
        }
        stages.reverse();
        let coordinator = Coordinator::new(transport, next, listener, config.clone());
        Ok(Self {
            stages,
            coordinator,
        })
    }

    /// Shut every stage down and collect their statistics lines.
    pub fn shutdown(mut self) -> Result<Vec<Value>, ModernError> {
        self.coordinator.shutdown()?;
        self.stages.into_iter().map(StageProcess::finish).collect()
    }
}

impl Drop for StageProcess {
    fn drop(&mut self) {
        // Never leave a stage behind (a failed test, an early return).
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
