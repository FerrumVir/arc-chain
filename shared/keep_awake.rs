/// Keeps the computer from idle-sleeping while a community job computes,
/// when the operator enabled `--prevent-sleep-during-jobs`. Dropping the
/// guard releases it. The machine may still sleep when the lid closes or
/// when it is idle between jobs.
///
/// macOS holds `caffeinate -i -w <arc-node pid>` and Linux holds
/// `systemd-inhibit ... tail --pid=<arc-node pid>`, so a crashed node can
/// never leave the machine awake. Windows sets `ES_SYSTEM_REQUIRED` on the
/// computing thread and clears it on drop; create and drop the guard on the
/// same thread.
#[must_use = "the computer may sleep again as soon as the guard is dropped"]
pub struct KeepAwake {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    helper: Option<std::process::Child>,
    #[cfg(windows)]
    thread_state_set: bool,
}

/// The helper command that keeps the machine awake while `pid` lives, on
/// platforms that use one.
pub fn keep_awake_helper_command(pid: u32) -> Option<(&'static str, Vec<String>)> {
    helper_command(std::env::consts::OS, pid, Purpose::Job)
}

#[derive(Clone, Copy)]
enum Purpose {
    Job,
    Sharing,
}

fn helper_command(os: &str, pid: u32, purpose: Purpose) -> Option<(&'static str, Vec<String>)> {
    if os == "macos" {
        Some((
            "/usr/bin/caffeinate",
            vec!["-i".to_string(), "-w".to_string(), pid.to_string()],
        ))
    } else if os == "linux" {
        Some((
            "systemd-inhibit",
            vec![
                // Sharing must allow explicit suspend. logind's idle lock only
                // prevents IdleAction; it takes no sleep/key/display inhibitor.
                match purpose {
                    Purpose::Job => "--what=idle:sleep",
                    Purpose::Sharing => "--what=idle",
                }
                .to_string(),
                "--who=ARC Node".to_string(),
                match purpose {
                    Purpose::Job => "--why=Running an ARC network job",
                    Purpose::Sharing => "--why=Available for ARC network jobs",
                }
                .to_string(),
                "--mode=block".to_string(),
                "tail".to_string(),
                format!("--pid={pid}"),
                "-f".to_string(),
                "/dev/null".to_string(),
            ],
        ))
    } else {
        None
    }
}

#[cfg(windows)]
mod windows_power {
    pub const ES_CONTINUOUS: u32 = 0x8000_0000;
    pub const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn SetThreadExecutionState(es_flags: u32) -> u32;
    }
}

impl KeepAwake {
    /// Start keeping the machine awake if `enabled`; otherwise inert.
    pub fn begin(enabled: bool) -> Self {
        Self::begin_for_process(enabled, std::process::id())
    }

    /// Same OS hold, released by Unix helpers if the watched worker exits.
    pub fn begin_for_process(enabled: bool, pid: u32) -> Self {
        Self::begin_with_purpose(enabled, pid, Purpose::Job)
    }

    /// Opt-in idle-system-sleep hold for an available worker. Unlike the legacy
    /// Linux per-job hold, this never inhibits an explicit suspend request.
    /// No platform requests a display/screen-saver hold.
    pub fn begin_sharing_for_process(pid: u32) -> Self {
        Self::begin_with_purpose(true, pid, Purpose::Sharing)
    }

    fn begin_with_purpose(enabled: bool, pid: u32, purpose: Purpose) -> Self {
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = (pid, purpose);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let command = if enabled {
                helper_command(std::env::consts::OS, pid, purpose)
            } else {
                None
            };
            let helper = command.and_then(|(program, args)| {
                let spawned = std::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                match spawned {
                    Ok(child) => Some(child),
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            program,
                            "could not keep the computer awake for a community job"
                        );
                        None
                    }
                }
            });
            Self { helper }
        }
        #[cfg(windows)]
        {
            let thread_state_set = enabled && {
                // SAFETY: SetThreadExecutionState only updates the calling
                // thread's power request flags; it takes no pointers.
                let previous = unsafe {
                    windows_power::SetThreadExecutionState(
                        windows_power::ES_CONTINUOUS | windows_power::ES_SYSTEM_REQUIRED,
                    )
                };
                previous != 0
            };
            Self { thread_state_set }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        {
            let _ = enabled;
            Self {}
        }
    }

    /// Whether the guard is currently holding the machine awake.
    pub fn is_active(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.helper.is_some()
        }
        #[cfg(windows)]
        {
            self.thread_state_set
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        {
            false
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(mut helper) = self.helper.take() {
            let _ = helper.kill();
            let _ = helper.wait();
        }
        #[cfg(windows)]
        if self.thread_state_set {
            // SAFETY: as in `begin`; ES_CONTINUOUS alone clears this
            // thread's system-required request.
            unsafe {
                windows_power::SetThreadExecutionState(windows_power::ES_CONTINUOUS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharing_helper_allows_manual_suspend_and_display_sleep() {
        // Check both Unix command contracts on every test host. This does not
        // claim runtime verification of Linux logind or a desktop environment.
        let (program, args) = helper_command("linux", 4242, Purpose::Sharing).unwrap();
        assert_eq!(program, "systemd-inhibit");
        assert_eq!(
            args,
            [
                "--what=idle",
                "--who=ARC Node",
                "--why=Available for ARC network jobs",
                "--mode=block",
                "tail",
                "--pid=4242",
                "-f",
                "/dev/null"
            ]
        );
        let (program, args) = helper_command("macos", 4242, Purpose::Sharing).unwrap();
        assert_eq!(program, "/usr/bin/caffeinate");
        assert_eq!(args, ["-i", "-w", "4242"]); // no -d display or -s forced sleep hold
    }

    #[test]
    fn per_job_helper_contract_is_unchanged() {
        let (program, args) = helper_command("linux", 4242, Purpose::Job).unwrap();
        assert_eq!(program, "systemd-inhibit");
        assert_eq!(
            args,
            [
                "--what=idle:sleep",
                "--who=ARC Node",
                "--why=Running an ARC network job",
                "--mode=block",
                "tail",
                "--pid=4242",
                "-f",
                "/dev/null"
            ]
        );
        assert_eq!(
            helper_command("macos", 4242, Purpose::Job),
            helper_command("macos", 4242, Purpose::Sharing)
        );
        assert_eq!(
            keep_awake_helper_command(4242),
            helper_command(std::env::consts::OS, 4242, Purpose::Job)
        );
        assert!(!KeepAwake::begin(false).is_active());
    }
}
