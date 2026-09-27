//! One private Unix-socket daemon per resident bundle, shared by SSH relays.
//! Daemon: tensor_row_shared_worker serve --rows-dir PATH --artifact HASH
//!         --socket /private/owned-0700-directory/rows.sock [--max-clients 8]
//! Relay (the cohort's SSH command): tensor_row_shared_worker relay --socket PATH
//! This exposes no TCP listener and does not cache inference results.

#[cfg(unix)]
fn main() -> Result<(), String> {
    use arc_inference::row_service::{
        PrivateRowSocket, RowServiceLimits, SharedRowBundle, SharedRowService,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    static STOP: AtomicBool = AtomicBool::new(false);
    extern "C" fn stop(_: libc::c_int) {
        STOP.store(true, Ordering::Relaxed);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().ok_or("required mode: serve or relay")?;
    let mut values = std::collections::BTreeMap::new();
    let mut i = 1;
    while i < args.len() {
        let flag = args[i].as_str();
        if ![
            "--socket",
            "--rows-dir",
            "--artifact",
            "--max-clients",
            "--idle-timeout-ms",
            "--frame-timeout-ms",
            "--call-timeout-ms",
        ]
        .contains(&flag)
        {
            return Err(format!("unknown flag {flag}"));
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("missing {flag} value"))?;
        if values.insert(flag, value.as_str()).is_some() {
            return Err(format!("duplicate {flag}"));
        }
        i += 2;
    }
    let socket = std::path::Path::new(values.get("--socket").ok_or("required --socket")?);
    if mode == "relay" {
        if values.len() != 1 {
            return Err("relay accepts only --socket".into());
        }
        return arc_inference::row_service::relay(socket).map_err(|e| e.to_string());
    }
    if mode != "serve" {
        return Err("mode must be serve or relay".into());
    }
    let rows = std::path::Path::new(values.get("--rows-dir").ok_or("required --rows-dir")?);
    let artifact = arc_crypto::Hash256::from_hex(
        values
            .get("--artifact")
            .ok_or("required --artifact")?
            .trim_start_matches("0x"),
    )
    .map_err(|e| e.to_string())?;
    let mut limits = RowServiceLimits::default();
    if let Some(n) = values.get("--max-clients") {
        limits.max_clients = n.parse().map_err(|_| "invalid --max-clients")?;
    }
    for (flag, target) in [
        ("--idle-timeout-ms", &mut limits.idle_timeout),
        ("--frame-timeout-ms", &mut limits.frame_timeout),
        ("--call-timeout-ms", &mut limits.call_timeout),
    ] {
        if let Some(n) = values.get(flag) {
            *target = Duration::from_millis(n.parse().map_err(|_| format!("invalid {flag}"))?);
        }
    }
    limits.validate().map_err(|e| e.to_string())?;
    // No connection can trigger a model load: the socket is reserved before
    // loading, and connections are accepted only after that single load.
    let (_socket, listener) = PrivateRowSocket::bind(socket).map_err(|e| e.to_string())?;
    let bundle = SharedRowBundle::load(rows, artifact).map_err(|e| e.to_string())?;
    let service = SharedRowService::new(bundle, limits).map_err(|e| e.to_string())?;
    // SAFETY: handlers only store into a lock-free AtomicBool, perform no
    // allocation or I/O, and remain alive for the process lifetime.
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let watched = cancel.clone();
    let watch_service = service.clone();
    let watcher = std::thread::spawn(move || {
        while !watched.load(Ordering::Relaxed) {
            if STOP.load(Ordering::Relaxed) {
                watched.store(true, Ordering::Relaxed);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!(
            "{}",
            serde_json::json!({"event":"row_service_stopping","stats":watch_service.stats()})
        );
    });
    eprintln!(
        "{}",
        serde_json::json!({"event":"row_service_started","artifact":artifact.to_hex(),"stats":service.stats()})
    );
    let result = service.serve(listener, cancel.clone());
    cancel.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    eprintln!(
        "{}",
        serde_json::json!({"event":"row_service_stopped","stats":service.stats()})
    );
    result.map_err(|e| e.to_string())
}
#[cfg(not(unix))]
fn main() -> Result<(), String> {
    Err("shared private row service requires Unix-domain sockets".into())
}
