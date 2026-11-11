//! Desktop-owned opt-in hold. All OS guards are created and dropped on one
//! native thread (required by Windows), independently of per-job holds.
use crate::background_promotion::{DownloadConditions, PowerSource, SystemConditions};
use crate::keep_awake::KeepAwake;
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

#[derive(Default)]
struct Hold<G> {
    guard: Option<G>,
}

impl<G> Hold<G> {
    fn reconcile(
        &mut self,
        opted_in: bool,
        worker_running: bool,
        power: PowerSource,
        acquire: impl FnOnce() -> G,
    ) {
        if opted_in && worker_running && power == PowerSource::Mains {
            if self.guard.is_none() {
                self.guard = Some(acquire());
            }
        } else {
            self.guard = None;
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

fn sender() -> &'static mpsc::Sender<Request> {
    static SENDER: OnceLock<mpsc::Sender<Request>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Request>();
        let power_tx = tx.clone();
        std::thread::Builder::new()
            .name("arc-sharing-awake".into())
            .spawn(move || {
                let mut enabled = false;
                let mut worker = None;
                let mut hold = Hold {
                    guard: None::<KeepAwake>,
                };
                loop {
                    // Fallback for platforms without a source-change notification,
                    // and recovery if an OS notification is lost.
                    let request = match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(request) => Some(request),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    if let Some(request) = &request {
                        match request.change {
                            Change::Enabled(value) => enabled = value,
                            Change::Worker(value) => worker = value,
                            Change::Power => {}
                        }
                    }
                    hold.reconcile(
                        enabled,
                        worker.is_some_and(process_running),
                        SystemConditions.power_source(),
                        || KeepAwake::begin_for_process(true, worker.unwrap()),
                    );
                    if let Some(done) = request.and_then(|request| request.done) {
                        let _ = done.send(());
                    }
                }
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
        let acquire = || {
            active.fetch_add(1, Ordering::SeqCst);
            Guard(active.clone())
        };
        hold.reconcile(false, true, PowerSource::Mains, acquire);
        assert_eq!(active.load(Ordering::SeqCst), 0, "default off");
        for (enabled, worker, power) in [
            (true, true, PowerSource::Battery),
            (true, true, PowerSource::Unknown),
            (false, true, PowerSource::Mains),
            (true, false, PowerSource::Mains),
        ] {
            hold.reconcile(true, true, PowerSource::Mains, acquire);
            hold.reconcile(true, true, PowerSource::Mains, acquire);
            assert_eq!(active.load(Ordering::SeqCst), 1, "one hold only");
            hold.reconcile(enabled, worker, power, acquire);
            assert_eq!(active.load(Ordering::SeqCst), 0, "release immediately");
        }
        hold.reconcile(true, true, PowerSource::Mains, acquire);
        drop(hold);
        assert_eq!(active.load(Ordering::SeqCst), 0, "quit drops the hold");
    }
}
