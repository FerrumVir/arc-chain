//! Keep a consented install contributing without a relaunch.
//!
//! An install whose user said yes to compute becomes a worker once its model
//! is on disk. Before this module, a model download that stalled or failed was
//! retried only at the next launch or on a click, so a computer that is never
//! restarted could stay an observer for weeks. A worker whose model file went
//! missing was never repaired on its own either.
//!
//! While the install still needs its model, a background loop calls the same
//! promotion path every [`RETRY_INTERVAL`], and soon after the computer wakes
//! from sleep. It ends once the install is a worker with a readable model,
//! the user turns compute off, the computer is below the memory floor, or the
//! disk lacks the room the download needs.
//!
//! The pieces are kept apart so they can be tested without hardware or real
//! time:
//! - [`SingleFlight`]: one promotion at a time in this process. A user call
//!   that finds one running waits for it and gets its result; a background
//!   tick that finds one running skips.
//! - [`DownloadConditions`]: mains power and metered network, behind one
//!   trait. Background ticks never pull the model on battery or on a network
//!   the OS reports as metered; a download the user starts is never gated.
//! - [`plan_tick`] and [`RetrySchedule`]: pure decisions over facts and times.
//! - [`background_tick`]: one tick, generic over its environment.

use std::future::Future;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::commands::PromotionNeed;

/// How often the loop retries while the install still needs its model.
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(20 * 60);
/// How soon after a wake from sleep the loop retries. Long enough for Wi-Fi
/// to reconnect.
pub(crate) const AFTER_WAKE_DELAY: Duration = Duration::from_secs(90);

// ── Single-flight ─────────────────────────────────────────────────────────

/// At most one run of an async operation at a time, in this process.
///
/// Callers that join a run in flight receive a clone of its result. If the
/// running caller is dropped before it finishes, joined callers receive
/// `interrupted()` from their own call, and the next call starts afresh.
pub(crate) struct SingleFlight<T> {
    slot: StdMutex<Slot<T>>,
}

struct Slot<T> {
    /// Numbers runs, so a finished run never clears a newer one.
    next_run: u64,
    running: Option<(u64, watch::Receiver<Option<T>>)>,
}

/// Clears the slot when the leading run ends, finished or dropped.
struct Leader<'a, T> {
    flight: &'a SingleFlight<T>,
    run: u64,
    result: watch::Sender<Option<T>>,
}

impl<T> Leader<'_, T> {
    fn release(&self) {
        let mut slot = self
            .flight
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot
            .running
            .as_ref()
            .is_some_and(|(run, _)| *run == self.run)
        {
            slot.running = None;
        }
    }
}

impl<T> Drop for Leader<'_, T> {
    fn drop(&mut self) {
        self.release();
    }
}

enum Role<'a, T> {
    Lead(Leader<'a, T>),
    Join(watch::Receiver<Option<T>>),
}

impl<T: Clone + Send + Sync> SingleFlight<T> {
    pub(crate) const fn new() -> Self {
        Self {
            slot: StdMutex::new(Slot {
                next_run: 0,
                running: None,
            }),
        }
    }

    /// Whether a run is in flight now.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> bool {
        self.slot
            .lock()
            .map(|slot| slot.running.is_some())
            .unwrap_or(false)
    }

    fn claim(&self, may_join: bool) -> Option<Role<'_, T>> {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, running)) = slot.running.as_ref() {
            return may_join.then(|| Role::Join(running.clone()));
        }
        let run = slot.next_run;
        slot.next_run = slot.next_run.wrapping_add(1);
        let (result, receiver) = watch::channel(None);
        slot.running = Some((run, receiver));
        Some(Role::Lead(Leader {
            flight: self,
            run,
            result,
        }))
    }

    async fn lead<F, Fut>(leader: Leader<'_, T>, run: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let value = run().await;
        // Free the slot before publishing: a caller arriving after this point
        // starts a new run instead of receiving a finished one.
        leader.release();
        let _ = leader.result.send(Some(value.clone()));
        value
    }

    /// Run `run`, or wait for the run in flight and return its result.
    pub(crate) async fn run_or_join<F, Fut>(&self, run: F, interrupted: impl FnOnce() -> T) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        match self.claim(true) {
            Some(Role::Lead(leader)) => Self::lead(leader, run).await,
            Some(Role::Join(mut running)) => {
                match running.wait_for(|value| value.is_some()).await {
                    Ok(value) => (*value).clone().unwrap_or_else(interrupted),
                    Err(_) => interrupted(),
                }
            }
            None => unreachable!("a joining claim always returns a role"),
        }
    }

    /// Run `run` unless a run is in flight; `None` means one was, and nothing
    /// ran. Never waits for the other run.
    pub(crate) async fn run_unless_busy<F, Fut>(&self, run: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        match self.claim(false) {
            Some(Role::Lead(leader)) => Some(Self::lead(leader, run).await),
            _ => None,
        }
    }
}

// ── Power and network ─────────────────────────────────────────────────────

/// Where the computer draws power from, as far as the OS reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PowerSource {
    /// Mains, or a computer with no battery.
    Mains,
    Battery,
    /// The OS could not establish mains power. Downloads may proceed; idle holds may not.
    Unknown,
}

/// What a background download checks before it pulls gigabytes. One trait,
/// so the scheduler tests need no real hardware.
pub(crate) trait DownloadConditions {
    fn power_source(&self) -> PowerSource;
    /// `Some(true)` when the OS reports the network as metered, `Some(false)`
    /// when it reports it as unmetered, `None` when it cannot be read.
    fn network_metered(&self) -> Option<bool>;
}

/// Who asked for the download.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// Onboarding, Settings, the observer banner, the one-time question:
    /// never gated.
    User,
    /// The retry loop.
    Background,
}

/// Why a background tick did nothing this time and will try again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipReason {
    OnBattery,
    MeteredNetwork,
    /// Another promotion is running in this process.
    InFlight,
}

/// Whether a download for `trigger` may start under `conditions`. An unknown
/// metered state proceeds.
pub(crate) fn download_gate(
    trigger: Trigger,
    conditions: &impl DownloadConditions,
) -> Result<(), SkipReason> {
    if trigger == Trigger::User {
        return Ok(());
    }
    if conditions.power_source() == PowerSource::Battery {
        return Err(SkipReason::OnBattery);
    }
    if conditions.network_metered() == Some(true) {
        return Err(SkipReason::MeteredNetwork);
    }
    Ok(())
}

/// The OS readings.
///
/// - Power: Windows `GetSystemPowerStatus`, macOS IOKit power sources, Linux
///   `/sys/class/power_supply`. A computer with no battery counts as mains.
/// - Metered: Linux NetworkManager's `Metered` property, read through
///   `busctl`. Windows and macOS report it only through WinRT and
///   Network.framework, which this app does not link, so there it is unknown
///   and treated as unmetered.
pub(crate) struct SystemConditions;

impl DownloadConditions for SystemConditions {
    fn power_source(&self) -> PowerSource {
        platform::power_source()
    }

    fn network_metered(&self) -> Option<bool> {
        platform::network_metered()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::PowerSource;
    use std::ffi::{c_char, c_void, CStr};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        fn IOPSGetProvidingPowerSourceType(snapshot: CFTypeRef) -> CFStringRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: CFTypeRef);
        fn CFStringGetCString(
            string: CFStringRef,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> u8;
    }

    /// `IOPSGetProvidingPowerSourceType` names the source: "AC Power",
    /// "Battery Power" or "UPS Power". A Mac without a battery reports
    /// "AC Power". Unknown/UPS retain the download gate's permissive policy
    /// but cannot acquire an opt-in mains-only idle hold.
    pub(super) fn power_source() -> PowerSource {
        // SAFETY: the snapshot is a CF object we own and release once; the
        // providing type is borrowed from it (the "Get" rule) and read before
        // the release.
        unsafe {
            let snapshot = IOPSCopyPowerSourcesInfo();
            if snapshot.is_null() {
                return PowerSource::Unknown;
            }
            let kind = IOPSGetProvidingPowerSourceType(snapshot);
            let mut buffer = [0 as c_char; 64];
            let read = !kind.is_null()
                && CFStringGetCString(
                    kind,
                    buffer.as_mut_ptr(),
                    buffer.len() as isize,
                    K_CF_STRING_ENCODING_UTF8,
                ) != 0;
            let source = if !read {
                PowerSource::Unknown
            } else {
                match CStr::from_ptr(buffer.as_ptr()).to_bytes() {
                    b"AC Power" => PowerSource::Mains,
                    b"Battery Power" => PowerSource::Battery,
                    _ => PowerSource::Unknown,
                }
            };
            CFRelease(snapshot);
            source
        }
    }

    pub(super) fn network_metered() -> Option<bool> {
        None
    }
}

#[cfg(windows)]
mod platform {
    use super::PowerSource;
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    /// `ACLineStatus` 0 is offline (on battery), 1 online, 255 unknown;
    /// `BatteryFlag` 128 means the computer has no battery.
    pub(super) fn power_source() -> PowerSource {
        // SAFETY: GetSystemPowerStatus fills the struct we pass and nothing
        // else.
        let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
            return PowerSource::Unknown;
        }
        super::windows_power_source(status.ACLineStatus, status.BatteryFlag)
    }

    pub(super) fn network_metered() -> Option<bool> {
        None
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{PowerSource, PowerSupply};
    use std::path::Path;

    fn read_trimmed(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|value| value.trim().to_string())
    }

    pub(super) fn power_source() -> PowerSource {
        let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
            return PowerSource::Unknown;
        };
        let supplies: Vec<PowerSupply> = entries
            .flatten()
            .map(|entry| {
                let dir = entry.path();
                PowerSupply {
                    kind: read_trimmed(&dir.join("type")).unwrap_or_default(),
                    scope: read_trimmed(&dir.join("scope")),
                    online: read_trimmed(&dir.join("online")),
                    status: read_trimmed(&dir.join("status")),
                }
            })
            .collect();
        super::linux_power_source(&supplies)
    }

    /// NetworkManager's global `Metered` property, read with `busctl`, which
    /// ships with systemd. Missing tools or a missing NetworkManager mean
    /// unknown.
    pub(super) fn network_metered() -> Option<bool> {
        let output = std::process::Command::new("busctl")
            .args([
                "--system",
                "--timeout=2",
                "get-property",
                "org.freedesktop.NetworkManager",
                "/org/freedesktop/NetworkManager",
                "org.freedesktop.NetworkManager",
                "Metered",
            ])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        super::parse_networkmanager_metered(&String::from_utf8_lossy(&output.stdout))
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod platform {
    use super::PowerSource;

    pub(super) fn power_source() -> PowerSource {
        PowerSource::Mains
    }

    pub(super) fn network_metered() -> Option<bool> {
        None
    }
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn windows_power_source(ac_line_status: u8, battery_flag: u8) -> PowerSource {
    const AC_OFFLINE: u8 = 0;
    const NO_SYSTEM_BATTERY: u8 = 128;
    if ac_line_status == AC_OFFLINE && battery_flag & NO_SYSTEM_BATTERY == 0 {
        PowerSource::Battery
    } else if ac_line_status == 1 || battery_flag == NO_SYSTEM_BATTERY {
        PowerSource::Mains
    } else {
        PowerSource::Unknown
    }
}

/// One entry of `/sys/class/power_supply`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
struct PowerSupply {
    kind: String,
    scope: Option<String>,
    online: Option<String>,
    status: Option<String>,
}

/// A computer is on battery when it has a system battery (not a mouse or a
/// headset: `scope` "Device"), no external supply is online, and a battery is
/// discharging.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn linux_power_source(supplies: &[PowerSupply]) -> PowerSource {
    let system_batteries: Vec<&PowerSupply> = supplies
        .iter()
        .filter(|supply| supply.kind == "Battery" && supply.scope.as_deref() != Some("Device"))
        .collect();
    if system_batteries.is_empty() {
        return PowerSource::Mains;
    }
    let external_online = supplies
        .iter()
        .filter(|supply| supply.kind != "Battery")
        .any(|supply| supply.online.as_deref() == Some("1"));
    if external_online {
        return PowerSource::Mains;
    }
    let discharging = system_batteries
        .iter()
        .any(|battery| battery.status.as_deref() == Some("Discharging"));
    if discharging {
        PowerSource::Battery
    } else {
        PowerSource::Unknown
    }
}

/// `busctl get-property` prints `u <n>`. NetworkManager's `NMMetered`: 0
/// unknown, 1 yes, 2 no, 3 guessed yes, 4 guessed no.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_networkmanager_metered(output: &str) -> Option<bool> {
    let value: u32 = output.trim().strip_prefix("u ")?.trim().parse().ok()?;
    match value {
        1 | 3 => Some(true),
        2 | 4 => Some(false),
        _ => None,
    }
}

// ── The plan for one tick ─────────────────────────────────────────────────

/// Why the loop ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    NoConsent,
    Ineligible,
    /// A worker with a readable model.
    Ready,
    /// The disk lacks the room the download needs (the bytes still to fetch
    /// plus the 512 MiB margin).
    DiskFull,
}

/// What the loop knows at a tick, apart from power and network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PromotionFacts {
    pub need: PromotionNeed,
    /// A worker's configured model file is missing or unreadable.
    pub model_unavailable: bool,
    /// The computer meets the memory floor for the model.
    pub model_fits: bool,
    /// Downloading the model would not fit on the disk.
    pub disk_full: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TickPlan {
    Stop(StopReason),
    Skip(SkipReason),
    Promote,
}

/// Decide a background tick.
pub(crate) fn plan_tick(facts: PromotionFacts, conditions: &impl DownloadConditions) -> TickPlan {
    match facts.need {
        PromotionNeed::NoConsent => return TickPlan::Stop(StopReason::NoConsent),
        PromotionNeed::Ineligible => return TickPlan::Stop(StopReason::Ineligible),
        PromotionNeed::Ready if !facts.model_unavailable => {
            return TickPlan::Stop(StopReason::Ready)
        }
        // A worker whose model is gone: download it again, if it still fits.
        PromotionNeed::Ready if !facts.model_fits => return TickPlan::Stop(StopReason::Ineligible),
        PromotionNeed::Ready | PromotionNeed::Promote => {}
    }
    if facts.disk_full {
        return TickPlan::Stop(StopReason::DiskFull);
    }
    match download_gate(Trigger::Background, conditions) {
        Ok(()) => TickPlan::Promote,
        Err(reason) => TickPlan::Skip(reason),
    }
}

/// When the next tick is due. Times are monotonic; on platforms where the
/// monotonic clock stops during sleep, a deadline set before sleep is late
/// after it, which is why a wake moves it forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetrySchedule {
    due: Instant,
}

impl RetrySchedule {
    /// The first background tick comes one interval after the loop starts;
    /// the launch has already tried once.
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            due: now + RETRY_INTERVAL,
        }
    }

    pub(crate) fn due_at(&self) -> Instant {
        self.due
    }

    pub(crate) fn on_wake(&mut self, now: Instant) {
        self.due = self.due.min(now + AFTER_WAKE_DELAY);
    }

    pub(crate) fn after_tick(&mut self, now: Instant) {
        self.due = now + RETRY_INTERVAL;
    }
}

// ── One tick ──────────────────────────────────────────────────────────────

/// What a tick reads and runs. Production reads the store and hardware and
/// runs the shared promotion; tests script both.
pub(crate) trait TickEnv {
    fn facts(&self) -> impl Future<Output = PromotionFacts> + Send;
    /// Run the shared promotion unless one is in flight; `None` means one
    /// was, and nothing ran.
    fn promote_unless_busy(&self) -> impl Future<Output = Option<Result<(), String>>> + Send;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TickOutcome {
    Stopped(StopReason),
    Skipped(SkipReason),
    Promoted,
    Failed(String),
}

impl TickOutcome {
    pub(crate) fn ends_loop(&self) -> bool {
        matches!(self, TickOutcome::Stopped(_))
    }
}

pub(crate) async fn background_tick<E: TickEnv>(
    env: &E,
    conditions: &impl DownloadConditions,
) -> TickOutcome {
    match plan_tick(env.facts().await, conditions) {
        TickPlan::Stop(reason) => TickOutcome::Stopped(reason),
        TickPlan::Skip(reason) => TickOutcome::Skipped(reason),
        TickPlan::Promote => match env.promote_unless_busy().await {
            None => TickOutcome::Skipped(SkipReason::InFlight),
            Some(Ok(())) => TickOutcome::Promoted,
            Some(Err(error)) => TickOutcome::Failed(error),
        },
    }
}

// ── The loop ──────────────────────────────────────────────────────────────

/// Retry promotion in the background until [`TickOutcome::ends_loop`]. Started
/// once, after startup reconciliation succeeded. Installs with node auto-start
/// off are included: the model downloads and the configuration becomes a
/// worker, and the node stays stopped until the user starts it.
pub(crate) async fn run_retry_loop(app: tauri::AppHandle) {
    use tauri::Manager as _;

    let mut wakes = crate::wake::subscribe();
    let mut wakes_open = true;
    let mut schedule = RetrySchedule::new(Instant::now());
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(schedule.due_at())) => {}
            changed = wakes.changed(), if wakes_open => {
                match changed {
                    Ok(()) => schedule.on_wake(Instant::now()),
                    Err(_) => wakes_open = false,
                }
                continue;
            }
        }
        let Some(state) = app.try_state::<crate::AppState>() else {
            return;
        };
        let env = crate::commands::BackgroundPromotionEnv {
            app: &app,
            state: state.inner(),
        };
        let outcome = background_tick(&env, &SystemConditions).await;
        match &outcome {
            TickOutcome::Stopped(reason) => {
                tracing::info!(?reason, "background model download retry finished");
            }
            TickOutcome::Skipped(reason) => {
                tracing::info!(
                    ?reason,
                    "background model download retry skipped; trying again later"
                );
            }
            TickOutcome::Promoted => {
                tracing::info!("compute contribution enabled in the background: worker mode with a verified model");
            }
            TickOutcome::Failed(error) => {
                tracing::warn!(%error, "background model download retry failed; trying again later");
            }
        }
        if outcome.ends_loop() {
            return;
        }
        schedule.after_tick(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Conditions {
        power: PowerSource,
        metered: Option<bool>,
    }

    impl DownloadConditions for Conditions {
        fn power_source(&self) -> PowerSource {
            self.power
        }
        fn network_metered(&self) -> Option<bool> {
            self.metered
        }
    }

    const MAINS_UNMETERED: Conditions = Conditions {
        power: PowerSource::Mains,
        metered: Some(false),
    };

    fn facts(need: PromotionNeed) -> PromotionFacts {
        PromotionFacts {
            need,
            model_unavailable: false,
            model_fits: true,
            disk_full: false,
        }
    }

    // ── single-flight ──

    #[tokio::test]
    async fn a_user_call_joins_the_run_in_flight_and_gets_its_result() {
        let flight: Arc<SingleFlight<Result<u32, String>>> = Arc::new(SingleFlight::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let (release, released) = tokio::sync::oneshot::channel::<()>();

        let leader = {
            let flight = flight.clone();
            let runs = runs.clone();
            tokio::spawn(async move {
                flight
                    .run_or_join(
                        || async move {
                            runs.fetch_add(1, Ordering::SeqCst);
                            let _ = released.await;
                            Ok(7)
                        },
                        || Err("interrupted".into()),
                    )
                    .await
            })
        };
        while !flight.in_flight() {
            tokio::task::yield_now().await;
        }
        let joiner = flight.run_or_join(
            || async {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(99)
            },
            || Err("interrupted".into()),
        );
        tokio::pin!(joiner);
        // Polled once, it joins the run and waits.
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut joiner)
            .await
            .is_err());
        release.send(()).unwrap();

        assert_eq!(leader.await.unwrap(), Ok(7));
        assert_eq!(
            joiner.await,
            Ok(7),
            "the joining call returns the run's result"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1, "only one run happened");
        assert!(!flight.in_flight());
    }

    #[tokio::test]
    async fn a_background_tick_skips_while_a_run_is_in_flight_and_never_queues() {
        let flight: Arc<SingleFlight<Result<u32, String>>> = Arc::new(SingleFlight::new());
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let leader = {
            let flight = flight.clone();
            tokio::spawn(async move {
                flight
                    .run_or_join(
                        || async move {
                            let _ = released.await;
                            Ok(1)
                        },
                        || Err("interrupted".into()),
                    )
                    .await
            })
        };
        while !flight.in_flight() {
            tokio::task::yield_now().await;
        }
        let ran = Cell::new(false);
        let skipped = flight
            .run_unless_busy(|| async {
                ran.set(true);
                Ok(2)
            })
            .await;
        assert_eq!(skipped, None, "a background tick returns at once");
        assert!(!ran.get());

        release.send(()).unwrap();
        assert_eq!(leader.await.unwrap(), Ok(1));
        // Once the run finished, the next tick runs.
        assert_eq!(
            flight.run_unless_busy(|| async { Ok(3) }).await,
            Some(Ok(3))
        );
    }

    #[tokio::test]
    async fn a_dropped_run_frees_the_slot_and_tells_joiners() {
        let flight: Arc<SingleFlight<Result<u32, String>>> = Arc::new(SingleFlight::new());
        let leader = {
            let flight = flight.clone();
            tokio::spawn(async move {
                flight
                    .run_or_join(
                        || std::future::pending::<Result<u32, String>>(),
                        || Err("interrupted".into()),
                    )
                    .await
            })
        };
        while !flight.in_flight() {
            tokio::task::yield_now().await;
        }
        let joiner = flight.run_or_join(|| async { Ok(5) }, || Err("interrupted".into()));
        tokio::pin!(joiner);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut joiner)
            .await
            .is_err());
        leader.abort();
        let _ = leader.await;
        assert_eq!(joiner.await, Err("interrupted".into()));
        assert!(!flight.in_flight());
        assert_eq!(
            flight.run_unless_busy(|| async { Ok(6) }).await,
            Some(Ok(6))
        );
    }

    // ── power and network ──

    #[test]
    fn a_background_tick_on_battery_skips() {
        let battery = Conditions {
            power: PowerSource::Battery,
            metered: Some(false),
        };
        assert_eq!(
            plan_tick(facts(PromotionNeed::Promote), &battery),
            TickPlan::Skip(SkipReason::OnBattery)
        );
    }

    #[test]
    fn a_background_tick_on_a_metered_network_skips() {
        let metered = Conditions {
            power: PowerSource::Mains,
            metered: Some(true),
        };
        assert_eq!(
            plan_tick(facts(PromotionNeed::Promote), &metered),
            TickPlan::Skip(SkipReason::MeteredNetwork)
        );
    }

    #[test]
    fn an_unknown_metered_state_proceeds() {
        let unknown = Conditions {
            power: PowerSource::Mains,
            metered: None,
        };
        assert_eq!(
            plan_tick(facts(PromotionNeed::Promote), &unknown),
            TickPlan::Promote
        );
    }

    #[test]
    fn a_user_started_download_proceeds_on_battery_and_on_a_metered_network() {
        for conditions in [
            Conditions {
                power: PowerSource::Battery,
                metered: Some(false),
            },
            Conditions {
                power: PowerSource::Mains,
                metered: Some(true),
            },
            Conditions {
                power: PowerSource::Battery,
                metered: Some(true),
            },
        ] {
            assert_eq!(download_gate(Trigger::User, &conditions), Ok(()));
            assert!(download_gate(Trigger::Background, &conditions).is_err());
        }
    }

    #[test]
    fn windows_power_status_maps_to_mains_or_battery() {
        assert_eq!(windows_power_source(0, 0), PowerSource::Battery);
        assert_eq!(windows_power_source(0, 8), PowerSource::Battery);
        assert_eq!(windows_power_source(1, 8), PowerSource::Mains);
        assert_eq!(
            windows_power_source(255, 1),
            PowerSource::Unknown,
            "unknown must not acquire an idle hold"
        );
        assert_eq!(
            windows_power_source(0, 128),
            PowerSource::Mains,
            "no system battery"
        );
    }

    fn supply(
        kind: &str,
        scope: Option<&str>,
        online: Option<&str>,
        status: Option<&str>,
    ) -> PowerSupply {
        PowerSupply {
            kind: kind.into(),
            scope: scope.map(Into::into),
            online: online.map(Into::into),
            status: status.map(Into::into),
        }
    }

    #[test]
    fn linux_power_supplies_map_to_mains_or_battery() {
        assert_eq!(
            linux_power_source(&[]),
            PowerSource::Mains,
            "a desktop has no battery"
        );
        assert_eq!(
            linux_power_source(&[supply("Battery", Some("Device"), None, Some("Discharging"))]),
            PowerSource::Mains,
            "a wireless mouse battery is not the computer's"
        );
        assert_eq!(
            linux_power_source(&[
                supply("Mains", None, Some("0"), None),
                supply("Battery", None, None, Some("Discharging")),
            ]),
            PowerSource::Battery
        );
        assert_eq!(
            linux_power_source(&[
                supply("Mains", None, Some("1"), None),
                supply("Battery", None, None, Some("Charging")),
            ]),
            PowerSource::Mains
        );
        assert_eq!(
            linux_power_source(&[
                supply("USB", None, Some("1"), None),
                supply("Battery", None, None, Some("Discharging")),
            ]),
            PowerSource::Mains
        );
    }

    #[test]
    fn networkmanager_metered_values_parse() {
        assert_eq!(parse_networkmanager_metered("u 1\n"), Some(true));
        assert_eq!(parse_networkmanager_metered("u 3"), Some(true));
        assert_eq!(parse_networkmanager_metered("u 2"), Some(false));
        assert_eq!(parse_networkmanager_metered("u 4"), Some(false));
        assert_eq!(parse_networkmanager_metered("u 0"), None);
        assert_eq!(parse_networkmanager_metered(""), None);
        assert_eq!(parse_networkmanager_metered("s \"yes\""), None);
    }

    // ── the plan ──

    #[test]
    fn the_loop_stops_on_ready_no_consent_ineligible_and_disk_full() {
        assert_eq!(
            plan_tick(facts(PromotionNeed::Ready), &MAINS_UNMETERED),
            TickPlan::Stop(StopReason::Ready)
        );
        assert_eq!(
            plan_tick(facts(PromotionNeed::NoConsent), &MAINS_UNMETERED),
            TickPlan::Stop(StopReason::NoConsent)
        );
        assert_eq!(
            plan_tick(facts(PromotionNeed::Ineligible), &MAINS_UNMETERED),
            TickPlan::Stop(StopReason::Ineligible)
        );
        assert_eq!(
            plan_tick(
                PromotionFacts {
                    disk_full: true,
                    ..facts(PromotionNeed::Promote)
                },
                &MAINS_UNMETERED
            ),
            TickPlan::Stop(StopReason::DiskFull)
        );
    }

    #[test]
    fn a_worker_whose_model_is_missing_downloads_it_again() {
        let missing = PromotionFacts {
            model_unavailable: true,
            ..facts(PromotionNeed::Ready)
        };
        assert_eq!(plan_tick(missing, &MAINS_UNMETERED), TickPlan::Promote);
        assert_eq!(
            plan_tick(
                PromotionFacts {
                    model_fits: false,
                    ..missing
                },
                &MAINS_UNMETERED
            ),
            TickPlan::Stop(StopReason::Ineligible)
        );
        assert_eq!(
            plan_tick(
                PromotionFacts {
                    disk_full: true,
                    ..missing
                },
                &MAINS_UNMETERED
            ),
            TickPlan::Stop(StopReason::DiskFull)
        );
    }

    // ── timing ──

    #[test]
    fn ticks_come_every_twenty_minutes_and_soon_after_a_wake() {
        let start = Instant::now();
        let mut schedule = RetrySchedule::new(start);
        assert_eq!(schedule.due_at(), start + Duration::from_secs(20 * 60));

        let tick = schedule.due_at();
        schedule.after_tick(tick);
        assert_eq!(schedule.due_at(), tick + Duration::from_secs(20 * 60));

        // A wake brings the next tick forward to shortly after it...
        let woke = tick + Duration::from_secs(60);
        schedule.on_wake(woke);
        assert_eq!(schedule.due_at(), woke + AFTER_WAKE_DELAY);
        // ...and a second wake never pushes it back.
        schedule.on_wake(woke + Duration::from_secs(30));
        assert_eq!(schedule.due_at(), woke + AFTER_WAKE_DELAY);

        // A wake just before a tick that is already due sooner keeps it.
        let mut close = RetrySchedule::new(start);
        let almost = close.due_at() - Duration::from_secs(10);
        close.on_wake(almost);
        assert_eq!(close.due_at(), start + RETRY_INTERVAL);
    }

    // ── one tick, end to end over a scripted environment ──

    struct ScriptedEnv {
        facts: PromotionFacts,
        result: Option<Result<(), String>>,
        promotions: AtomicUsize,
    }

    impl TickEnv for ScriptedEnv {
        async fn facts(&self) -> PromotionFacts {
            self.facts
        }
        async fn promote_unless_busy(&self) -> Option<Result<(), String>> {
            self.promotions.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn env(facts: PromotionFacts, result: Option<Result<(), String>>) -> ScriptedEnv {
        ScriptedEnv {
            facts,
            result,
            promotions: AtomicUsize::new(0),
        }
    }

    #[tokio::test]
    async fn a_tick_promotes_retries_after_failure_skips_in_flight_and_stops() {
        let promote = env(facts(PromotionNeed::Promote), Some(Ok(())));
        assert_eq!(
            background_tick(&promote, &MAINS_UNMETERED).await,
            TickOutcome::Promoted
        );
        assert_eq!(promote.promotions.load(Ordering::SeqCst), 1);

        let failing = env(facts(PromotionNeed::Promote), Some(Err("stalled".into())));
        let outcome = background_tick(&failing, &MAINS_UNMETERED).await;
        assert_eq!(outcome, TickOutcome::Failed("stalled".into()));
        assert!(
            !outcome.ends_loop(),
            "a failed download is retried at the next tick"
        );

        let busy = env(facts(PromotionNeed::Promote), None);
        let outcome = background_tick(&busy, &MAINS_UNMETERED).await;
        assert_eq!(outcome, TickOutcome::Skipped(SkipReason::InFlight));
        assert!(!outcome.ends_loop());

        let repair = env(
            PromotionFacts {
                model_unavailable: true,
                ..facts(PromotionNeed::Ready)
            },
            Some(Ok(())),
        );
        assert_eq!(
            background_tick(&repair, &MAINS_UNMETERED).await,
            TickOutcome::Promoted
        );
        assert_eq!(
            repair.promotions.load(Ordering::SeqCst),
            1,
            "the missing model is fetched again"
        );

        for need in [
            PromotionNeed::Ready,
            PromotionNeed::NoConsent,
            PromotionNeed::Ineligible,
        ] {
            let done = env(facts(need), Some(Ok(())));
            let outcome = background_tick(&done, &MAINS_UNMETERED).await;
            assert!(outcome.ends_loop(), "{need:?} ends the loop");
            assert_eq!(done.promotions.load(Ordering::SeqCst), 0);
        }

        let battery = env(facts(PromotionNeed::Promote), Some(Ok(())));
        let outcome = background_tick(
            &battery,
            &Conditions {
                power: PowerSource::Battery,
                metered: None,
            },
        )
        .await;
        assert_eq!(outcome, TickOutcome::Skipped(SkipReason::OnBattery));
        assert_eq!(
            battery.promotions.load(Ordering::SeqCst),
            0,
            "nothing downloads on battery"
        );
    }
}
