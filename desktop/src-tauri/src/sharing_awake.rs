//! Desktop-owned opt-in hold. All OS guards are created and dropped on one
//! native thread (required by Windows), independently of per-job holds.
use crate::background_promotion::{DownloadConditions, PowerSource, SystemConditions};
use crate::keep_awake::KeepAwake;
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

#[derive(Default)]
struct Hold<G> {
    guard: Option<(u32, G)>,
}

impl<G> Hold<G> {
    fn reconcile(
        &mut self,
        opted_in: bool,
        worker: Option<u32>,
        power: PowerSource,
        acquire: impl FnOnce(u32) -> G,
    ) {
        let wanted = worker.filter(|_| opted_in && power == PowerSource::Mains);
        if self.guard.as_ref().map(|(pid, _)| *pid) != wanted {
            // Drop before acquiring: Windows requests belong to this thread,
            // and dropping the old guard after acquisition would clear the new one.
            self.guard = None;
            self.guard = wanted.map(|pid| (pid, acquire(pid)));
        }
    }
}

enum Change {
    Enabled(bool),
    Worker(Option<u32>),
    Power,
}
struct Request {
    change: Change,
    done: Option<mpsc::Sender<()>>,
}

// macOS notifies immediately; the slow sample recovers lost notifications and
// detects a dead worker. Other platforms have no notification source here.
const POWER_CHECK_INTERVAL: Duration = if cfg!(target_os = "macos") {
    Duration::from_secs(3)
} else {
    Duration::from_secs(1)
};

fn run_controller<G>(
    rx: mpsc::Receiver<Request>,
    interval: Duration,
    mut power_source: impl FnMut() -> PowerSource,
    mut process_running: impl FnMut(u32) -> bool,
    mut acquire: impl FnMut(u32) -> G,
) {
    let mut enabled = false;
    let mut worker = None;
    let mut hold = Hold { guard: None };
    loop {
        let request = if enabled && worker.is_some() {
            match rx.recv_timeout(interval) {
                Ok(request) => Some(request),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            // No periodic wakeups or power/process queries on inactive installs.
            match rx.recv() {
                Ok(request) => Some(request),
                Err(_) => break,
            }
        };
        if let Some(request) = &request {
            match request.change {
                Change::Enabled(value) => enabled = value,
                Change::Worker(value) => worker = value,
                Change::Power => {}
            }
        }
        let power = if enabled && worker.is_some() {
            if worker.is_some_and(&mut process_running) {
                power_source()
            } else {
                // Wait for the next worker lifecycle event after an exit.
                worker = None;
                PowerSource::Unknown
            }
        } else {
            PowerSource::Unknown
        };
        hold.reconcile(enabled, worker, power, &mut acquire);
        if let Some(done) = request.and_then(|request| request.done) {
            let _ = done.send(());
        }
    }
}

fn sender() -> &'static mpsc::Sender<Request> {
    static SENDER: OnceLock<mpsc::Sender<Request>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Request>();
        let power_tx = tx.clone();
        std::thread::Builder::new()
            .name("arc-sharing-awake".into())
            .spawn(move || {
                run_controller(
                    rx,
                    POWER_CHECK_INTERVAL,
                    || SystemConditions.power_source(),
                    process_running,
                    KeepAwake::begin_sharing_for_process,
                );
            })
            .expect("sharing power controller thread");
        power_notifications(power_tx);
        tx
    })
}

// A settings/lifecycle command returns only after its hold has been released.
fn change(change: Change) {
    #[cfg(not(test))]
    {
        let (tx, rx) = mpsc::channel();
        if sender()
            .send(Request {
                change,
                done: Some(tx),
            })
            .is_ok()
        {
            let _ = rx.recv();
        }
    }
    #[cfg(test)]
    let _ = change; // Unit tests must never hold the developer's computer awake.
}

pub(crate) fn set_enabled(enabled: bool) {
    change(Change::Enabled(enabled));
}
pub(crate) fn set_worker(pid: Option<u32>) {
    change(Change::Worker(pid));
}

#[cfg(unix)]
fn process_running(pid: u32) -> bool {
    // SAFETY: signal 0 tests existence/permission without sending a signal.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
#[cfg(windows)]
fn process_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, WaitForSingleObject},
    };
    // SAFETY: handle is used only while owned here, then closed exactly once.
    unsafe {
        let handle = OpenProcess(0x0010_0000, 0, pid); // SYNCHRONIZE
        if handle.is_null() {
            return false;
        }
        let running = WaitForSingleObject(handle, 0) == 258; // WAIT_TIMEOUT
        CloseHandle(handle);
        running
    }
}
#[cfg(not(any(unix, windows)))]
fn process_running(_pid: u32) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn power_notifications(tx: mpsc::Sender<Request>) {
    use std::ffi::c_void;
    type Ref = *const c_void;
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSNotificationCreateRunLoopSource(
            callback: extern "C" fn(*mut c_void),
            context: *mut c_void,
        ) -> Ref;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRunLoopGetCurrent() -> Ref;
        fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
        fn CFRunLoopRun();
        fn CFRelease(value: Ref);
        static kCFRunLoopDefaultMode: Ref;
    }
    extern "C" fn changed(context: *mut c_void) {
        // SAFETY: the sender lives on the notification thread's stack until
        // its run loop exits and its source is released.
        let tx = unsafe { &*(context as *const mpsc::Sender<Request>) };
        let _ = tx.send(Request {
            change: Change::Power,
            done: None,
        });
    }
    let _ = std::thread::Builder::new()
        .name("arc-power-source".into())
        .spawn(move || {
            // SAFETY: source and loop are created/used on this thread; callback
            // context remains alive throughout CFRunLoopRun.
            unsafe {
                let source =
                    IOPSNotificationCreateRunLoopSource(changed, &tx as *const _ as *mut c_void);
                if source.is_null() {
                    return;
                }
                CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
                CFRunLoopRun();
                CFRelease(source);
            }
        });
}

#[cfg(not(target_os = "macos"))]
fn power_notifications(_tx: mpsc::Sender<Request>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Guard(Arc<AtomicUsize>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn sharing_hold_releases_on_battery_off_untick_stop_and_quit() {
        let active = Arc::new(AtomicUsize::new(0));
        let mut hold = Hold { guard: None };
        let acquire = |_| {
            active.fetch_add(1, Ordering::SeqCst);
            Guard(active.clone())
        };
        hold.reconcile(false, Some(1), PowerSource::Mains, acquire);
        assert_eq!(active.load(Ordering::SeqCst), 0, "default off");
        for (enabled, worker, power) in [
            (true, Some(1), PowerSource::Battery),
            (true, Some(1), PowerSource::Unknown),
            (false, Some(1), PowerSource::Mains),
            (true, None, PowerSource::Mains),
        ] {
            hold.reconcile(true, Some(1), PowerSource::Mains, acquire);
            hold.reconcile(true, Some(1), PowerSource::Mains, acquire);
            assert_eq!(active.load(Ordering::SeqCst), 1, "one hold only");
            hold.reconcile(enabled, worker, power, acquire);
            assert_eq!(active.load(Ordering::SeqCst), 0, "release immediately");
        }
        hold.reconcile(true, Some(1), PowerSource::Mains, acquire);
        drop(hold);
        assert_eq!(active.load(Ordering::SeqCst), 0, "quit drops the hold");
    }

    // Exercise the production receive loop with injected OS operations. No test
    // creates a real sleep assertion or requires a power-source change.
    struct Controller {
        tx: Option<mpsc::Sender<Request>>,
        thread: Option<std::thread::JoinHandle<()>>,
        power: Arc<std::sync::Mutex<PowerSource>>,
        queries: Arc<AtomicUsize>,
        process_checks: Arc<AtomicUsize>,
        alive: Arc<std::sync::atomic::AtomicBool>,
        active: Arc<AtomicUsize>,
        acquired: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl Controller {
        fn new(interval: Duration) -> Self {
            let (tx, rx) = mpsc::channel();
            let power = Arc::new(std::sync::Mutex::new(PowerSource::Mains));
            let queries = Arc::new(AtomicUsize::new(0));
            let process_checks = Arc::new(AtomicUsize::new(0));
            let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let active = Arc::new(AtomicUsize::new(0));
            let acquired = Arc::new(std::sync::Mutex::new(Vec::new()));
            let thread = {
                let power = power.clone();
                let queries = queries.clone();
                let process_checks = process_checks.clone();
                let alive = alive.clone();
                let active = active.clone();
                let acquired = acquired.clone();
                std::thread::spawn(move || {
                    run_controller(
                        rx,
                        interval,
                        || {
                            queries.fetch_add(1, Ordering::SeqCst);
                            *power.lock().unwrap()
                        },
                        |_| {
                            process_checks.fetch_add(1, Ordering::SeqCst);
                            alive.load(Ordering::SeqCst)
                        },
                        |pid| {
                            assert_eq!(
                                active.fetch_add(1, Ordering::SeqCst),
                                0,
                                "release old guard before acquiring its replacement"
                            );
                            acquired.lock().unwrap().push(pid);
                            Guard(active.clone())
                        },
                    )
                })
            };
            Self {
                tx: Some(tx),
                thread: Some(thread),
                power,
                queries,
                process_checks,
                alive,
                active,
                acquired,
            }
        }

        fn send(&self, change: Change) {
            let (tx, rx) = mpsc::channel();
            self.tx
                .as_ref()
                .unwrap()
                .send(Request {
                    change,
                    done: Some(tx),
                })
                .unwrap();
            rx.recv_timeout(Duration::from_secs(2))
                .expect("event acknowledged after reconcile");
        }

        fn assert_quiet(&self) {
            let queries = self.queries.load(Ordering::SeqCst);
            let checks = self.process_checks.load(Ordering::SeqCst);
            // Several periods of the injected 10 ms timer must pass without a
            // query, including when a power notification reaches an idle loop.
            self.send(Change::Power);
            std::thread::sleep(Duration::from_millis(80));
            assert_eq!(self.queries.load(Ordering::SeqCst), queries);
            assert_eq!(self.process_checks.load(Ordering::SeqCst), checks);
            assert_eq!(self.active.load(Ordering::SeqCst), 0);
        }
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            self.tx.take();
            self.thread.take().unwrap().join().unwrap();
            assert_eq!(self.active.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn no_periodic_power_queries_while_opted_out() {
        let c = Controller::new(Duration::from_millis(10));
        c.send(Change::Worker(Some(11)));
        c.assert_quiet(); // includes default off
        c.send(Change::Enabled(true));
        assert_eq!(c.active.load(Ordering::SeqCst), 1);
        c.send(Change::Enabled(false));
        c.assert_quiet(); // includes untick / sharing off / quit
    }

    #[test]
    fn no_periodic_power_queries_without_a_worker() {
        let c = Controller::new(Duration::from_millis(10));
        c.send(Change::Enabled(true));
        c.assert_quiet();
        c.send(Change::Worker(Some(11)));
        assert_eq!(c.active.load(Ordering::SeqCst), 1);
        c.send(Change::Worker(None));
        c.assert_quiet();
        c.send(Change::Worker(Some(12)));
        c.alive.store(false, Ordering::SeqCst);
        c.send(Change::Power); // also clears a stale PID after process death
        c.assert_quiet();
    }

    #[test]
    fn worker_pid_replacement_reacquires_and_same_pid_keeps_guard() {
        let c = Controller::new(Duration::from_secs(60));
        c.send(Change::Enabled(true));
        c.send(Change::Worker(Some(11)));
        c.send(Change::Worker(Some(11)));
        assert_eq!(*c.acquired.lock().unwrap(), [11]);
        c.send(Change::Worker(Some(22)));
        assert_eq!(*c.acquired.lock().unwrap(), [11, 22]);
        assert_eq!(c.active.load(Ordering::SeqCst), 1);
        c.send(Change::Worker(None));
        assert_eq!(c.active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn power_notifications_and_lifecycle_release_before_ack_without_waiting_for_timer() {
        let c = Controller::new(Duration::from_secs(60));
        c.send(Change::Enabled(true));
        c.send(Change::Worker(Some(11)));
        for power in [PowerSource::Battery, PowerSource::Unknown] {
            *c.power.lock().unwrap() = power;
            c.send(Change::Power);
            assert_eq!(c.active.load(Ordering::SeqCst), 0);
            *c.power.lock().unwrap() = PowerSource::Mains;
            c.send(Change::Power);
            assert_eq!(c.active.load(Ordering::SeqCst), 1);
        }
        c.send(Change::Enabled(false));
        assert_eq!(c.active.load(Ordering::SeqCst), 0);
        c.send(Change::Enabled(true));
        assert_eq!(c.active.load(Ordering::SeqCst), 1);
        c.send(Change::Worker(None));
        assert_eq!(c.active.load(Ordering::SeqCst), 0);
        c.send(Change::Worker(Some(22)));
        assert_eq!(c.active.load(Ordering::SeqCst), 1);
        // Disconnect/quit drops the final guard on its owning thread.
    }

    #[test]
    fn eligible_worker_safety_sample_releases_after_unnotified_power_change() {
        let c = Controller::new(Duration::from_millis(10));
        c.send(Change::Enabled(true));
        c.send(Change::Worker(Some(11)));
        *c.power.lock().unwrap() = PowerSource::Battery;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while c.active.load(Ordering::SeqCst) != 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(c.queries.load(Ordering::SeqCst) >= 2);
        assert_eq!(
            POWER_CHECK_INTERVAL,
            Duration::from_secs(if cfg!(target_os = "macos") { 3 } else { 1 })
        );
    }
}
