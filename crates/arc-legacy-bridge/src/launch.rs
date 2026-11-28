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
use crate::shard_policy::WorkerLaunchRole;

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
    pub shard_worker: bool,
    pub compute_model: Option<PathBuf>,
}

pub fn plan(
    layout: &Layout,
    invocation: &LegacyInvocation,
    pins: &Pins,
    release: &VerifiedRelease,
    compute: &Compute,
) -> LaunchPlan {
    let role = WorkerLaunchRole::from_compute(compute);
    plan_for_role(layout, invocation, pins, release, &role)
}

pub fn plan_for_role(
    layout: &Layout,
    invocation: &LegacyInvocation,
    pins: &Pins,
    release: &VerifiedRelease,
    role: &WorkerLaunchRole,
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
    let mut shard_worker = false;
    let compute_model = if !community {
        args.push("--no-community".into());
        None
    } else {
        match role {
            WorkerLaunchRole::Observer(_) => {
                args.push("--community-mode".into());
                for origin in &pins.community_rpc_origins {
                    args.push("--community-rpc-url".into());
                    args.push(origin.into());
                }
                None
            }
            WorkerLaunchRole::FullIntegerWorker { model, .. } => {
                args.push("--community-mode".into());
                for origin in &pins.community_rpc_origins {
                    args.push("--community-rpc-url".into());
                    args.push(origin.into());
                }
                args.push("--model".into());
                args.push(model.clone().into_os_string());
                args.push("--full-integer-worker".into());
                Some(model.clone())
            }
            WorkerLaunchRole::CommunityShardWorker {
                model,
                pin,
                max_working_set_bytes,
                ..
            } => {
                shard_worker = true;
                args.push("--threads".into());
                args.push("1".into());
                args.push("--community-shard-worker".into());
                args.push("--community-shard-coordinator-url".into());
                args.push(pin.coordinator_url.as_str().into());
                args.push("--community-shard-coordinator-id".into());
                args.push(pin.coordinator_id.as_str().into());
                args.push("--community-shard-transaction-domain".into());
                args.push(pin.transaction_domain.as_str().into());
                args.push("--community-shard-expected-model-id".into());
                args.push(pin.expected_model_id.as_str().into());
                args.push("--community-shard-max-working-set-bytes".into());
                args.push(max_working_set_bytes.to_string().into());
                args.push("--model".into());
                args.push(model.clone().into_os_string());
                Some(model.clone())
            }
        }
    };
    LaunchPlan {
        program: release.node.clone(),
        args,
        cwd: layout.node_dir.clone(),
        home: layout.node_home(),
        community,
        shard_worker,
        compute_model,
    }
}

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
    if plan.shard_worker {
        command.env_remove("ARC_COMMUNITY_RPC_URLS");
    }
    command
}

#[cfg(unix)]
pub fn exec(plan: &LaunchPlan) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let error = command_for(plan).exec();
    Err(anyhow::Error::new(error).context(format!("could not start {}", plan.program.display())))
}

#[cfg(windows)]
pub(crate) mod windows_job {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, ExitStatus};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_BAD_LENGTH, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    #[cfg(test)]
    use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_JOB_MEMORY;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    struct OwnedWinHandle(HANDLE);

    impl Drop for OwnedWinHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(crate) struct WindowsJob {
        handle: HANDLE,
    }

    impl WindowsJob {
        fn create_with_limits(limits: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> io::Result<Self> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self { handle };
            let size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .map_err(|_| {
                io::Error::other("job limit structure size does not fit in u32")
            })?;
            let configured = unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(limits).cast(),
                    size,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        pub(crate) fn create_kill_on_close() -> io::Result<Self> {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            Self::create_with_limits(&limits)
        }

        #[cfg(test)]
        pub(crate) fn create_with_job_memory_limit(limit_bytes: usize) -> io::Result<Self> {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.JobMemoryLimit = limit_bytes;
            Self::create_with_limits(&limits)
        }

        pub(crate) fn assign_process(&self, process_handle: HANDLE) -> io::Result<()> {
            let assigned = unsafe { AssignProcessToJobObject(self.handle, process_handle) };
            if assigned == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(crate) fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.handle, 1);
            }
        }
    }

    impl Drop for WindowsJob {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    pub(crate) fn resume_suspended_process(pid: u32) -> io::Result<()> {
        let snapshot = {
            let mut attempts = 0u32;
            loop {
                let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
                if raw != INVALID_HANDLE_VALUE && !raw.is_null() {
                    break OwnedWinHandle(raw);
                }
                let error = io::Error::last_os_error();
                attempts += 1;
                if error.raw_os_error() == Some(ERROR_BAD_LENGTH as i32) && attempts < 8 {
                    continue;
                }
                return Err(error);
            }
        };

        let entry_size = u32::try_from(std::mem::size_of::<THREADENTRY32>())
            .map_err(|_| io::Error::other("THREADENTRY32 size does not fit in u32"))?;
        let min_owner_size = u32::try_from(
            std::mem::offset_of!(THREADENTRY32, th32OwnerProcessID) + std::mem::size_of::<u32>(),
        )
        .map_err(|_| io::Error::other("THREADENTRY32 owner field offset does not fit in u32"))?;

        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = entry_size;
        let mut ok = unsafe { Thread32First(snapshot.0, &mut entry) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut resumed_threads = 0usize;
        while ok != 0 {
            if entry.dwSize >= min_owner_size && entry.th32OwnerProcessID == pid {
                let raw_thread =
                    unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if raw_thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let thread = OwnedWinHandle(raw_thread);
                let previous_count = unsafe { ResumeThread(thread.0) };
                if previous_count == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                if previous_count > 0 {
                    resumed_threads += 1;
                }
            }
            entry.dwSize = entry_size;
            ok = unsafe { Thread32Next(snapshot.0, &mut entry) };
        }
        let end_error = io::Error::last_os_error();
        if end_error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
            return Err(end_error);
        }
        if resumed_threads == 0 {
            return Err(io::Error::other(format!(
                "no suspended primary thread found for spawned process {pid}"
            )));
        }
        Ok(())
    }

    pub(crate) fn wait_for_child_exit_bounded(
        child: &mut Child,
        timeout: Duration,
    ) -> io::Result<Option<ExitStatus>> {
        let started = Instant::now();
        let poll_interval = Duration::from_millis(5);
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            if started.elapsed() >= timeout {
                return Ok(None);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            thread::sleep(remaining.min(poll_interval));
        }
    }

    #[cfg(test)]
    pub(crate) fn spawn_suspended(command: &mut Command) -> io::Result<Child> {
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        command.spawn()
    }

    pub(crate) fn spawn_in_kill_on_close_job(
        command: &mut Command,
    ) -> io::Result<(WindowsJob, Child)> {
        let job = WindowsJob::create_kill_on_close()?;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        if let Err(error) = job.assign_process(child.as_raw_handle()) {
            let _ = child.kill();
            let _ = wait_for_child_exit_bounded(&mut child, Duration::from_secs(5));
            return Err(error);
        }
        if let Err(error) = resume_suspended_process(child.id()) {
            job.terminate();
            let _ = child.kill();
            let _ = wait_for_child_exit_bounded(&mut child, Duration::from_secs(5));
            return Err(error);
        }
        Ok((job, child))
    }
}

#[cfg(windows)]
pub fn exec(plan: &LaunchPlan) -> Result<i32> {
    use anyhow::Context;

    let mut command = command_for(plan);
    let (job, mut child) = windows_job::spawn_in_kill_on_close_job(&mut command)
        .with_context(|| format!("could not start {}", plan.program.display()))?;
    let status = child.wait().context("could not wait for the node")?;
    drop(job);
    Ok(status.code().unwrap_or(1))
}

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
        let mut pins = Pins::embedded().unwrap();
        pins.node_release.worker_names_privacy_safe = false;
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

    #[test]
    fn community_shard_worker_plan_emits_exclusive_shard_flags_and_respects_privacy_gate() {
        use crate::pins::ShardWorkerPin;

        let temp = TempDir::new("launch-shard");
        let (layout, invocation, release) = fixture(&temp);
        let pin = ShardWorkerPin {
            coordinator_url: "https://149.28.153.31".to_string(),
            coordinator_id: "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b"
                .to_string(),
            transaction_domain: "3b9147e44d878646caf2d0729d06bd2e082b265e0525760595bd10605f37a10f"
                .to_string(),
            expected_model_id: "934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67"
                .to_string(),
            required_admitted_layers: 6,
        };
        let role = WorkerLaunchRole::CommunityShardWorker {
            model: PathBuf::from("/models/llama.gguf"),
            pin: Box::new(pin.clone()),
            max_working_set_bytes: 8 * 1024 * 1024 * 1024,
            admitted_max_resident_layers: 6,
        };

        let mut safe_pins = Pins::embedded().unwrap();
        safe_pins.node_release.worker_names_privacy_safe = true;
        let shard_plan = plan_for_role(&layout, &invocation, &safe_pins, &release, &role);
        assert!(shard_plan.community);
        assert!(shard_plan.shard_worker);
        assert_eq!(
            shard_plan.compute_model,
            Some(PathBuf::from("/models/llama.gguf"))
        );
        let args = strings(&shard_plan);
        assert_eq!(value_after(&args, "--stake").as_deref(), Some("0"));
        assert_eq!(value_after(&args, "--threads").as_deref(), Some("1"));
        assert!(args.contains(&"--community-shard-worker".to_string()));
        assert_eq!(
            value_after(&args, "--community-shard-coordinator-url").as_deref(),
            Some(pin.coordinator_url.as_str())
        );
        assert_eq!(
            value_after(&args, "--community-shard-coordinator-id").as_deref(),
            Some(pin.coordinator_id.as_str())
        );
        assert_eq!(
            value_after(&args, "--community-shard-transaction-domain").as_deref(),
            Some(pin.transaction_domain.as_str())
        );
        assert_eq!(
            value_after(&args, "--community-shard-expected-model-id").as_deref(),
            Some(pin.expected_model_id.as_str())
        );
        assert_eq!(
            value_after(&args, "--community-shard-max-working-set-bytes").as_deref(),
            Some("8589934592")
        );
        assert_eq!(
            value_after(&args, "--model").as_deref(),
            Some("/models/llama.gguf")
        );
        for forbidden in [
            "--community-mode",
            "--community-rpc-url",
            "--no-community",
            "--full-integer-worker",
            "--community",
            "--validator-seed",
            "--shard-range",
        ] {
            assert!(!args.contains(&forbidden.to_string()), "{forbidden}");
        }

        let mut unsafe_pins = Pins::embedded().unwrap();
        unsafe_pins.node_release.worker_names_privacy_safe = false;
        let unsafe_plan = plan_for_role(&layout, &invocation, &unsafe_pins, &release, &role);
        assert!(!unsafe_plan.community);
        assert!(!unsafe_plan.shard_worker);
        assert_eq!(unsafe_plan.compute_model, None);
        let args = strings(&unsafe_plan);
        assert!(args.contains(&"--no-community".to_string()));
        assert!(!args.contains(&"--community-shard-worker".to_string()));
        assert!(!args.contains(&"--model".to_string()));
    }
}
