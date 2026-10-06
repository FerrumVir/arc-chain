//! Start the pinned v0.8 node exactly as `install.sh` would, at stake 0.
//!
//! The argument list mirrors `install.sh` `NODE_ARGS` (RPC on loopback, the
//! release's seeds and genesis, `--stake 0 --min-stake 0 --eth-rpc-port 0`, a
//! dedicated data directory and keyfile, community mode with the six HTTPS
//! origins, and `--model … --full-integer-worker` only for a consented
//! worker). It never passes `--shard-range`, a validator seed, or any
//! insecure development flag.
//!
//! A stake-zero v0.8 node with no `--model` loads any canonical GGUF it finds
//! at `./llama2-7b.gguf`, `./llama-2-7b.Q4_K_M.gguf`, `$HOME/.arc-models/…`,
//! `/opt/arc/llama2-7b.gguf`, or `/var/lib/arc/llama2-7b.gguf`
//! (`arc-node` `auto_discover_model`). With compute off, the bridge therefore
//! runs the node from a private working directory with a private `HOME`, and
//! refuses to start if one of the absolute paths exists.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::argv::LegacyInvocation;
use crate::consent::Compute;
use crate::exit;
use crate::layout::Layout;
use crate::pins::Pins;
use crate::release::VerifiedRelease;

pub const RELATIVE_MODEL_AUTODISCOVERY: &[&str] = &["llama2-7b.gguf", "llama-2-7b.Q4_K_M.gguf"];
pub const HOME_MODEL_AUTODISCOVERY: &[&str] = &[
    ".arc-models/llama2-7b.gguf",
    ".arc-models/llama-2-7b.Q4_K_M.gguf",
];
pub const ABSOLUTE_MODEL_AUTODISCOVERY: &[&str] =
    &["/opt/arc/llama2-7b.gguf", "/var/lib/arc/llama2-7b.gguf"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub community: bool,
    pub compute_model: Option<PathBuf>,
}

pub fn plan(
    layout: &Layout,
    invocation: &LegacyInvocation,
    pins: &Pins,
    release: &VerifiedRelease,
    compute: &Compute,
) -> LaunchPlan {
    let community = pins.node_release.worker_names_privacy_safe;
    let mut args: Vec<OsString> = vec![
        "--rpc".into(),
        format!("127.0.0.1:{}", invocation.rpc.port()).into(),
        "--p2p-port".into(),
        invocation.p2p_port.to_string().into(),
        "--seeds-file".into(),
        release.seeds.clone().into_os_string(),
        "--genesis".into(),
        release.genesis.clone().into_os_string(),
        "--stake".into(),
        "0".into(),
        "--min-stake".into(),
        "0".into(),
        "--eth-rpc-port".into(),
        "0".into(),
        "--data-dir".into(),
        layout.data_dir().into_os_string(),
        "--validator-key-file".into(),
        layout.keyfile().into_os_string(),
    ];
    if community {
        args.push("--community-mode".into());
        for origin in &pins.community_rpc_origins {
            args.push("--community-rpc-url".into());
            args.push(origin.into());
        }
    } else {
        // Read-only: no registration, heartbeat, or claims, so a pin that
        // would publish the hostname never announces this computer.
        args.push("--no-community".into());
    }
    let compute_model = match compute {
        Compute::On(model) if community => Some(model.clone()),
        _ => None,
    };
    if let Some(model) = &compute_model {
        args.push("--model".into());
        args.push(model.clone().into_os_string());
        args.push("--full-integer-worker".into());
    }
    LaunchPlan {
        program: release.node.clone(),
        args,
        cwd: layout.node_dir.clone(),
        home: layout.node_home(),
        community,
        compute_model,
    }
}

/// Refuse to start an observer that the node would silently turn into a worker.
pub fn check_autodiscovery(plan: &LaunchPlan) -> Result<()> {
    if plan.compute_model.is_some() {
        return Ok(());
    }
    let mut hazards: Vec<PathBuf> = Vec::new();
    for name in RELATIVE_MODEL_AUTODISCOVERY {
        hazards.push(plan.cwd.join(name));
    }
    for relative in HOME_MODEL_AUTODISCOVERY {
        hazards.push(plan.home.join(relative));
    }
    for absolute in ABSOLUTE_MODEL_AUTODISCOVERY {
        hazards.push(PathBuf::from(absolute));
    }
    match hazards.iter().find(|path| path.exists()) {
        Some(path) => Err(exit::refused(format!(
            "{} exists, and the pinned arc-node loads it automatically, which would contribute compute without consent; move it or opt in to compute",
            path.display()
        ))),
        None => Ok(()),
    }
}

fn command_for(plan: &LaunchPlan) -> Command {
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .current_dir(&plan.cwd)
        .env("HOME", &plan.home);
    command
}

/// Replace this process with the node, so the supervisor's PID, signals,
/// and stdio now belong to the node itself. Returns only on failure.
#[cfg(unix)]
pub fn exec(plan: &LaunchPlan) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let error = command_for(plan).exec();
    Err(anyhow::Error::new(error).context(format!("could not start {}", plan.program.display())))
}

/// Windows cannot replace a process image. Run the node as a child in a job
/// object that kills it when this process ends, so the v0.7 desktop's
/// "stop" (which terminates its direct child) still stops the node.
#[cfg(windows)]
pub fn exec(plan: &LaunchPlan) -> Result<i32> {
    use anyhow::{Context, anyhow};
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // SAFETY: null security attributes and a null name create an unnamed job
    // with default security; the call has no other preconditions.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(anyhow!(
            "could not create a job object for the node: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: an all-zero bit pattern is a valid value of this plain C struct.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
        .map_err(|_| anyhow!("job limit structure size does not fit in u32"))?;
    // SAFETY: `job` is a live job handle and `limits` is a live, correctly
    // sized JOBOBJECT_EXTENDED_LIMIT_INFORMATION for this information class.
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            size,
        )
    };
    if configured == 0 {
        return Err(anyhow!(
            "could not configure the node job object: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut command = command_for(plan);
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {}", plan.program.display()))?;
    // SAFETY: both handles are live; `child` owns its process handle.
    let assigned = unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) };
    if assigned == 0 {
        let error = std::io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        return Err(anyhow!(
            "could not place the node in its job object: {error}"
        ));
    }
    let status = child.wait().context("could not wait for the node")?;
    // The job handle is deliberately left open: it closes, killing anything
    // still in the job, only when this launcher process exits.
    Ok(status.code().unwrap_or(1))
}

/// Whether `path` would be found by the node's model auto-discovery.
pub fn is_autodiscovery_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    ABSOLUTE_MODEL_AUTODISCOVERY
        .iter()
        .any(|candidate| text == *candidate)
        || HOME_MODEL_AUTODISCOVERY
            .iter()
            .any(|candidate| text.ends_with(&candidate.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::argv::LegacyKind;
    use crate::layout::test_support::{TempDir, fake_arc_dir};
    use crate::layout::{prepare, resolve};
    use std::net::SocketAddr;

    fn fixture(temp: &TempDir) -> (Layout, LegacyInvocation, VerifiedRelease) {
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);
        let invocation = LegacyInvocation {
            kind: LegacyKind::Desktop,
            rpc: "127.0.0.1:9954".parse::<SocketAddr>().unwrap(),
            p2p_port: 9955,
            data_dir: root.clone(),
            seeds_file: None,
            genesis: None,
            model: None,
            community_mode: true,
        };
        let layout = resolve(&launcher, &invocation, temp.path()).unwrap();
        prepare(&layout).unwrap();
        let dir = layout.releases_dir().join("v0.8.10");
        let release = VerifiedRelease {
            node: dir.join("arc-node-linux-x86_64"),
            cli: dir.join("arc-cli-linux-x86_64"),
            genesis: dir.join("genesis.toml"),
            seeds: dir.join("testnet-seeds.txt"),
            dir,
        };
        (layout, invocation, release)
    }

    fn strings(plan: &LaunchPlan) -> Vec<String> {
        plan.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn value_after(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1).cloned())
    }

    #[test]
    fn observer_plan_is_stake_zero_fresh_and_never_registers_on_an_unsafe_pin() {
        let temp = TempDir::new("launch-observer");
        let (layout, invocation, release) = fixture(&temp);
        let pins = Pins::embedded().unwrap();
        assert!(!pins.node_release.worker_names_privacy_safe);
        let plan = plan(
            &layout,
            &invocation,
            &pins,
            &release,
            &Compute::On(PathBuf::from("/m.gguf")),
        );
        let args = strings(&plan);
        assert_eq!(value_after(&args, "--stake").as_deref(), Some("0"));
        assert_eq!(value_after(&args, "--min-stake").as_deref(), Some("0"));
        assert_eq!(value_after(&args, "--eth-rpc-port").as_deref(), Some("0"));
        assert_eq!(
            value_after(&args, "--rpc").as_deref(),
            Some("127.0.0.1:9954")
        );
        assert_eq!(value_after(&args, "--p2p-port").as_deref(), Some("9955"));
        assert_eq!(
            value_after(&args, "--data-dir").map(PathBuf::from),
            Some(layout.data_dir())
        );
        assert!(!layout.data_dir().starts_with(layout.root.join("data")));
        assert!(args.contains(&"--no-community".to_string()));
        assert!(!args.contains(&"--community-mode".to_string()));
        assert!(
            !args.contains(&"--model".to_string()),
            "unsafe pin never computes"
        );
        for forbidden in [
            "--validator-seed",
            "--insecure-dev-validator-seed",
            "--shard-range",
            "--community",
        ] {
            assert!(!args.contains(&forbidden.to_string()), "{forbidden}");
        }
        assert_eq!(plan.cwd, layout.node_dir);
        assert_eq!(plan.home, layout.node_home());
    }

    #[test]
    fn privacy_safe_pin_registers_and_only_consent_adds_the_worker_flags() {
        let temp = TempDir::new("launch-worker");
        let (layout, invocation, release) = fixture(&temp);
        let mut pins = Pins::embedded().unwrap();
        pins.node_release.worker_names_privacy_safe = true;
        let observer = strings(&plan(
            &layout,
            &invocation,
            &pins,
            &release,
            &Compute::Off("no consent".to_string()),
        ));
        assert!(observer.contains(&"--community-mode".to_string()));
        assert_eq!(
            observer
                .iter()
                .filter(|arg| *arg == "--community-rpc-url")
                .count(),
            pins.community_rpc_origins.len()
        );
        assert!(!observer.contains(&"--model".to_string()));
        assert!(!observer.contains(&"--full-integer-worker".to_string()));

        let worker = strings(&plan(
            &layout,
            &invocation,
            &pins,
            &release,
            &Compute::On(PathBuf::from("/models/llama.gguf")),
        ));
        assert_eq!(
            value_after(&worker, "--model").as_deref(),
            Some("/models/llama.gguf")
        );
        assert!(worker.contains(&"--full-integer-worker".to_string()));
        assert_eq!(value_after(&worker, "--stake").as_deref(), Some("0"));
    }

    #[test]
    fn model_autodiscovery_paths_block_a_compute_off_start() {
        let temp = TempDir::new("launch-discovery");
        let (layout, invocation, release) = fixture(&temp);
        let pins = Pins::embedded().unwrap();
        let plan = plan(
            &layout,
            &invocation,
            &pins,
            &release,
            &Compute::Off("off".to_string()),
        );
        check_autodiscovery(&plan).unwrap();
        std::fs::create_dir_all(plan.home.join(".arc-models")).unwrap();
        std::fs::write(plan.home.join(".arc-models").join("llama2-7b.gguf"), b"x").unwrap();
        assert_eq!(
            exit::code_for(&check_autodiscovery(&plan).unwrap_err()),
            exit::EX_CONFIG
        );
        assert!(is_autodiscovery_path(
            &plan.home.join(".arc-models").join("llama2-7b.gguf")
        ));
    }
}
