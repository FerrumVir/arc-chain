use std::future::Future;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::commands::PromotionNeed;

pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(20 * 60);
pub(crate) const AFTER_WAKE_DELAY: Duration = Duration::from_secs(90);

pub(crate) struct SingleFlight<T> {
    slot: StdMutex<Slot<T>>,
}

struct Slot<T> {
    next_run: u64,
    running: Option<(u64, watch::Receiver<Option<T>>)>,
}

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
        leader.release();
        let _ = leader.result.send(Some(value.clone()));
        value
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PowerSource {
    Mains,
    Battery,
    Unknown,
}

pub(crate) trait DownloadConditions {
    fn power_source(&self) -> PowerSource;
    fn network_metered(&self) -> Option<bool>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    User,
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipReason {
    OnBattery,
    MeteredNetwork,
    DiskFull {
        needed_bytes: u64,
    },
    InFlight,
}

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

    pub(super) fn power_source() -> PowerSource {
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

    pub(super) fn power_source() -> PowerSource {
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

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
struct PowerSupply {
    kind: String,
    scope: Option<String>,
    online: Option<String>,
    status: Option<String>,
}

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

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_networkmanager_metered(output: &str) -> Option<bool> {
    let value: u32 = output.trim().strip_prefix("u ")?.trim().parse().ok()?;
    match value {
        1 | 3 => Some(true),
        2 | 4 => Some(false),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    NoConsent,
    Ineligible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PromotionFacts {
    pub need: PromotionNeed,
    pub model_unavailable: bool,
    pub model_fits: bool,
    pub running_as_observer: bool,
    pub cached_model_ready: bool,
    pub disk_space_needed: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TickPlan {
    Stop(StopReason),
    Skip(SkipReason),
    Monitor,
    Promote,
}

pub(crate) fn plan_tick(facts: PromotionFacts, conditions: &impl DownloadConditions) -> TickPlan {
    let needs_download = match facts.need {
        PromotionNeed::NoConsent => return TickPlan::Stop(StopReason::NoConsent),
        PromotionNeed::Ineligible => return TickPlan::Stop(StopReason::Ineligible),
        PromotionNeed::Ready | PromotionNeed::Promote if !facts.model_fits => {
            return TickPlan::Stop(StopReason::Ineligible);
        }
        PromotionNeed::Ready if !facts.model_unavailable => {
            if !facts.running_as_observer {
                return TickPlan::Monitor;
            }
            false
        }
        PromotionNeed::Ready | PromotionNeed::Promote => !facts.cached_model_ready,
    };
    if !needs_download {
        return TickPlan::Promote;
    }
    if let Some(needed_bytes) = facts.disk_space_needed {
        return TickPlan::Skip(SkipReason::DiskFull { needed_bytes });
    }
    match download_gate(Trigger::Background, conditions) {
        Ok(()) => TickPlan::Promote,
        Err(reason) => TickPlan::Skip(reason),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetrySchedule {
    due: Instant,
}

impl RetrySchedule {
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

    pub(crate) fn on_power_change(
        &mut self,
        before: PowerSource,
        after: PowerSource,
        now: Instant,
    ) {
        if before == PowerSource::Battery && after == PowerSource::Mains {
            self.due = self.due.min(now + Duration::from_secs(5));
        }
    }

    pub(crate) fn after_tick(&mut self, now: Instant) {
        self.due = now + RETRY_INTERVAL;
    }
}

pub(crate) trait TickEnv {
    fn facts(&self) -> impl Future<Output = PromotionFacts> + Send;
    fn promote_unless_busy(&self) -> impl Future<Output = Option<Result<(), String>>> + Send;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TickOutcome {
    Stopped(StopReason),
    Skipped(SkipReason),
    Monitoring,
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
        TickPlan::Monitor => TickOutcome::Monitoring,
        TickPlan::Promote => match env.promote_unless_busy().await {
            None => TickOutcome::Skipped(SkipReason::InFlight),
            Some(Ok(())) => TickOutcome::Promoted,
            Some(Err(error)) => TickOutcome::Failed(error),
        },
    }
}

static DOWNLOAD_WAIT: StdMutex<Option<crate::types::ModelDownloadWait>> = StdMutex::new(None);

pub(crate) fn waiting_status() -> Option<crate::types::ModelDownloadWait> {
    DOWNLOAD_WAIT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn wait_for_outcome(outcome: &TickOutcome) -> Option<crate::types::ModelDownloadWait> {
    use crate::types::ModelDownloadWait;
    match outcome {
        TickOutcome::Skipped(SkipReason::OnBattery) => Some(ModelDownloadWait::OnBattery),
        TickOutcome::Skipped(SkipReason::MeteredNetwork) => Some(ModelDownloadWait::MeteredNetwork),
        TickOutcome::Skipped(SkipReason::DiskFull { needed_bytes }) => {
            Some(ModelDownloadWait::DiskFull {
                needed_bytes: *needed_bytes,
            })
        }
        TickOutcome::Failed(_) => Some(ModelDownloadWait::Failed),
        _ => None,
    }
}

pub(crate) fn record_outcome(outcome: &TickOutcome) {
    *DOWNLOAD_WAIT.lock().unwrap_or_else(|e| e.into_inner()) = wait_for_outcome(outcome);
}

pub(crate) async fn run_retry_loop(app: tauri::AppHandle) {
    use tauri::Manager as _;

    let mut wakes = crate::wake::subscribe();
    let mut wakes_open = true;
    let mut schedule = RetrySchedule::new(Instant::now());
    let mut power = SystemConditions.power_source();
    let mut power_samples = tokio::time::interval(Duration::from_secs(30));
    power_samples.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    power_samples.tick().await;
    if let Some(state) = app.try_state::<crate::AppState>() {
        let env = crate::commands::BackgroundPromotionEnv {
            app: &app,
            state: state.inner(),
        };
        if let TickPlan::Skip(reason) = plan_tick(env.facts().await, &SystemConditions) {
            record_outcome(&TickOutcome::Skipped(reason));
        }
    }
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(schedule.due_at())) => {}
            _ = power_samples.tick() => {
                let current = SystemConditions.power_source();
                schedule.on_power_change(power, current, Instant::now());
                power = current;
                continue;
            }
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
        record_outcome(&outcome);
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
            TickOutcome::Monitoring => {
                tracing::debug!(
                    "consented worker is ready; continuing bounded reconciliation monitoring"
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
            running_as_observer: false,
            cached_model_ready: false,
            disk_space_needed: None,
        }
    }

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
                        std::future::pending::<Result<u32, String>>,
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

    #[test]
    fn the_loop_monitors_ready_stops_without_consent_or_capacity_and_waits_for_disk() {
        assert_eq!(
            plan_tick(facts(PromotionNeed::Ready), &MAINS_UNMETERED),
            TickPlan::Monitor
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
                    disk_space_needed: Some(1024),
                    ..facts(PromotionNeed::Promote)
                },
                &MAINS_UNMETERED
            ),
            TickPlan::Skip(SkipReason::DiskFull { needed_bytes: 1024 })
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
                    disk_space_needed: Some(1024),
                    ..missing
                },
                &MAINS_UNMETERED
            ),
            TickPlan::Skip(SkipReason::DiskFull { needed_bytes: 1024 })
        );
    }

    #[tokio::test]
    async fn ready_monitoring_keeps_loop_alive_reconciles_later_observer_and_never_starts_stopped_node(
    ) {
        struct MonitorState {
            facts: PromotionFacts,
            restarts: usize,
        }
        struct MonitorEnv {
            state: StdMutex<MonitorState>,
        }
        impl TickEnv for MonitorEnv {
            async fn facts(&self) -> PromotionFacts {
                self.state.lock().unwrap().facts
            }

            async fn promote_unless_busy(&self) -> Option<Result<(), String>> {
                let mut state = self.state.lock().unwrap();
                state.restarts += 1;
                state.facts.running_as_observer = false;
                Some(Ok(()))
            }
        }

        let restricted = Conditions {
            power: PowerSource::Battery,
            metered: Some(true),
        };
        let env = MonitorEnv {
            state: StdMutex::new(MonitorState {
                facts: facts(PromotionNeed::Ready),
                restarts: 0,
            }),
        };

        let steady_or_stopped = background_tick(&env, &restricted).await;
        assert_eq!(steady_or_stopped, TickOutcome::Monitoring);
        assert!(!steady_or_stopped.ends_loop());
        assert_eq!(wait_for_outcome(&steady_or_stopped), None);
        assert_eq!(env.state.lock().unwrap().restarts, 0);

        env.state.lock().unwrap().facts.running_as_observer = true;
        let reconciled = background_tick(&env, &restricted).await;
        assert_eq!(reconciled, TickOutcome::Promoted);
        assert!(!reconciled.ends_loop());
        assert_eq!(env.state.lock().unwrap().restarts, 1);

        let after_reconcile = background_tick(&env, &restricted).await;
        assert_eq!(after_reconcile, TickOutcome::Monitoring);
        assert!(!after_reconcile.ends_loop());
        assert_eq!(env.state.lock().unwrap().restarts, 1);

        env.state.lock().unwrap().facts.need = PromotionNeed::NoConsent;
        env.state.lock().unwrap().facts.running_as_observer = true;
        let opted_out = background_tick(&env, &restricted).await;
        assert_eq!(opted_out, TickOutcome::Stopped(StopReason::NoConsent));
        assert!(opted_out.ends_loop());
        assert_eq!(env.state.lock().unwrap().restarts, 1);
    }

    #[tokio::test]
    async fn no_consent_and_ineligible_explicitly_end_monitoring_loop() {
        let no_consent = env(
            PromotionFacts {
                running_as_observer: true,
                cached_model_ready: true,
                ..facts(PromotionNeed::NoConsent)
            },
            Some(Ok(())),
        );
        let outcome = background_tick(&no_consent, &MAINS_UNMETERED).await;
        assert_eq!(outcome, TickOutcome::Stopped(StopReason::NoConsent));
        assert!(outcome.ends_loop());
        assert_eq!(no_consent.promotions.load(Ordering::SeqCst), 0);

        for ineligible_facts in [
            facts(PromotionNeed::Ineligible),
            PromotionFacts {
                model_fits: false,
                ..facts(PromotionNeed::Ready)
            },
        ] {
            let ineligible = env(ineligible_facts, Some(Ok(())));
            let outcome = background_tick(&ineligible, &MAINS_UNMETERED).await;
            assert_eq!(outcome, TickOutcome::Stopped(StopReason::Ineligible));
            assert!(outcome.ends_loop());
            assert_eq!(ineligible.promotions.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn full_disk_keeps_the_loop_alive_and_the_next_tick_can_download() {
        let mut environment = env(
            PromotionFacts {
                disk_space_needed: Some(4096),
                ..facts(PromotionNeed::Promote)
            },
            Some(Ok(())),
        );
        let outcome = background_tick(&environment, &MAINS_UNMETERED).await;
        assert!(!outcome.ends_loop());
        assert_eq!(
            wait_for_outcome(&outcome),
            Some(crate::types::ModelDownloadWait::DiskFull { needed_bytes: 4096 })
        );
        assert_eq!(environment.promotions.load(Ordering::SeqCst), 0);
        environment.facts.disk_space_needed = None;
        assert_eq!(
            background_tick(&environment, &MAINS_UNMETERED).await,
            TickOutcome::Promoted
        );
        assert_eq!(environment.promotions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mains_return_advances_retry_without_postponing_an_earlier_deadline() {
        let now = Instant::now();
        let mut schedule = RetrySchedule::new(now);
        schedule.on_power_change(PowerSource::Unknown, PowerSource::Unknown, now);
        assert_eq!(schedule.due_at(), now + RETRY_INTERVAL);
        schedule.on_power_change(PowerSource::Battery, PowerSource::Mains, now);
        assert_eq!(schedule.due_at(), now + Duration::from_secs(5));
        schedule.on_power_change(
            PowerSource::Battery,
            PowerSource::Mains,
            now + Duration::from_secs(4),
        );
        assert_eq!(schedule.due_at(), now + Duration::from_secs(5));
    }

    #[test]
    fn waiting_status_is_typed_and_clears_on_success_or_ineligibility() {
        use crate::types::ModelDownloadWait;
        assert_eq!(
            wait_for_outcome(&TickOutcome::Skipped(SkipReason::OnBattery)),
            Some(ModelDownloadWait::OnBattery)
        );
        assert_eq!(
            wait_for_outcome(&TickOutcome::Failed("private path".into())),
            Some(ModelDownloadWait::Failed)
        );
        assert_eq!(wait_for_outcome(&TickOutcome::Promoted), None);
        assert_eq!(
            wait_for_outcome(&TickOutcome::Stopped(StopReason::NoConsent)),
            None
        );
        assert_eq!(
            serde_json::to_value(ModelDownloadWait::DiskFull { needed_bytes: 4096 }).unwrap(),
            serde_json::json!({"reason": "diskFull", "neededBytes": 4096})
        );
    }

    #[test]
    fn ticks_come_every_twenty_minutes_and_soon_after_a_wake() {
        let start = Instant::now();
        let mut schedule = RetrySchedule::new(start);
        assert_eq!(schedule.due_at(), start + Duration::from_secs(20 * 60));

        let tick = schedule.due_at();
        schedule.after_tick(tick);
        assert_eq!(schedule.due_at(), tick + Duration::from_secs(20 * 60));

        let woke = tick + Duration::from_secs(60);
        schedule.on_wake(woke);
        assert_eq!(schedule.due_at(), woke + AFTER_WAKE_DELAY);
        schedule.on_wake(woke + Duration::from_secs(30));
        assert_eq!(schedule.due_at(), woke + AFTER_WAKE_DELAY);

        let mut close = RetrySchedule::new(start);
        let almost = close.due_at() - Duration::from_secs(10);
        close.on_wake(almost);
        assert_eq!(close.due_at(), start + RETRY_INTERVAL);
    }

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

        let ready = env(facts(PromotionNeed::Ready), Some(Ok(())));
        let outcome = background_tick(&ready, &MAINS_UNMETERED).await;
        assert_eq!(outcome, TickOutcome::Monitoring);
        assert!(!outcome.ends_loop());
        assert_eq!(ready.promotions.load(Ordering::SeqCst), 0);

        for need in [PromotionNeed::NoConsent, PromotionNeed::Ineligible] {
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
