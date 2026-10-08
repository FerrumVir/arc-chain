//! native-updater-check: run the REAL `tauri-plugin-updater` `check()` against a chosen endpoint.
//!
//! THROWAWAY LAB FILE (wave0-v0712-lab, never merged). The released v0.7.11 ARC desktop app ships
//! tauri 2.11.2 and tauri-plugin-updater 2.10.1 (versions read from the strings of the released Linux
//! binary; the v0.7.11 tag has no desktop Cargo.lock). The app's "Install update" button runs the
//! plugin's `check()` against the single configured endpoint
//! `https://github.com/FerrumVir/arc-chain/releases/latest/download/latest.json`. This binary drives
//! that same plugin code, configured through the same `plugins.updater` config path an app uses
//! (`pubkey`, `endpoints`, `windows`), inside tauri's mock runtime (no webview), with the package
//! version set to the app's version. It NEVER installs anything: no install step of the plugin is ever called.
//!
//! CLI (fixed contract, parsed by wave0-lab/desktop_updater_check.py):
//!
//!   native-updater-check --endpoint URL --current-version 0.7.11 --pubkey BASE64
//!                        [--insecure-transport] [--control-download]
//!
//!   --insecure-transport  sets `dangerousInsecureTransportProtocol: true` in the updater config. It is the
//!                         ONLY deviation from the shipped config and exists so a test can use an
//!                         `http://127.0.0.1:PORT/...` fake server instead of github.com over TLS.
//!   --control-download    POSITIVE CONTROL ONLY: when the check finds an update, also call the update's
//!                         `download()` (bytes in memory, signature verification will fail on a fake
//!                         bundle) so the fake server sees the bundle request. Never installs.
//!
//! stdout: exactly ONE JSON line
//!   {"schema":"arc.legacy-bridge.wave0-lab.native-updater-check.v1",
//!    "plugin":"tauri-plugin-updater 2.10.1","tauri":"2.11.2","current_version":...,"endpoints":[...],
//!    "outcome":"no_update"|"update_available"|"error","error_kind":str|null,"error":str|null,
//!    "update":{"version":str,"download_url":str}|null,"download_attempted":bool}
//!   plus, only with --control-download and an update found, "download_result":"ok: N bytes"|"error: ...".
//! stderr: diagnostics. Exit 0 whenever the check ran (the caller judges `outcome`); exit 2 only for
//! usage or configuration errors, with a one-line message on stderr.

use std::process::ExitCode;

use serde_json::{json, Value};
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};
use tauri_plugin_updater::UpdaterExt;

const SCHEMA: &str = "arc.legacy-bridge.wave0-lab.native-updater-check.v1";
const PLUGIN: &str = "tauri-plugin-updater 2.10.1";
const TAURI: &str = "2.11.2";
const USAGE: &str = "usage: native-updater-check --endpoint URL --current-version X.Y.Z --pubkey BASE64 [--insecure-transport] [--control-download]";

struct Args {
    endpoint: String,
    current_version: String,
    pubkey: String,
    insecure_transport: bool,
    control_download: bool,
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Args, String> {
    let mut endpoint = None;
    let mut current_version = None;
    let mut pubkey = None;
    let mut insecure_transport = false;
    let mut control_download = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--endpoint" => endpoint = Some(args.next().ok_or("--endpoint needs a value")?),
            "--current-version" => {
                current_version = Some(args.next().ok_or("--current-version needs a value")?)
            }
            "--pubkey" => pubkey = Some(args.next().ok_or("--pubkey needs a value")?),
            "--insecure-transport" => insecure_transport = true,
            "--control-download" => control_download = true,
            other => return Err(format!("unknown argument {other:?}; {USAGE}")),
        }
    }
    Ok(Args {
        endpoint: endpoint.ok_or_else(|| format!("--endpoint is required; {USAGE}"))?,
        current_version: current_version
            .ok_or_else(|| format!("--current-version is required; {USAGE}"))?,
        pubkey: pubkey.ok_or_else(|| format!("--pubkey is required; {USAGE}"))?,
        insecure_transport,
        control_download,
    })
}

/// The variant name of an updater error, taken from its derived `Debug` (the enum is non-exhaustive).
fn error_kind(error: &tauri_plugin_updater::Error) -> String {
    format!("{error:?}")
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn run() -> Result<Value, String> {
    let args = parse_args(std::env::args().skip(1))?;

    // The same shape as `plugins.updater` in the shipped tauri.conf.json.
    let mut updater_config = json!({
        "pubkey": args.pubkey,
        "endpoints": [args.endpoint],
        "windows": { "installMode": "passive" },
    });
    if args.insecure_transport {
        updater_config["dangerousInsecureTransportProtocol"] = json!(true);
    }

    let version = semver::Version::parse(&args.current_version)
        .map_err(|error| format!("--current-version is not a semantic version: {error}"))?;
    let mut context: tauri::Context<MockRuntime> = mock_context(noop_assets());
    context
        .config_mut()
        .plugins
        .0
        .insert("updater".to_string(), updater_config);
    context.package_info_mut().version = version;

    let app = mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(context)
        .map_err(|error| format!("cannot start the mock app with the updater plugin: {error}"))?;
    // Exactly as an app does it: the updater comes from the plugin's configuration.
    let updater = app
        .updater()
        .map_err(|error| format!("cannot build the updater from its configuration: {error}"))?;

    let mut report = json!({
        "schema": SCHEMA,
        "plugin": PLUGIN,
        "tauri": TAURI,
        "current_version": args.current_version,
        "endpoints": [args.endpoint],
        "outcome": "error",
        "error_kind": Value::Null,
        "error": Value::Null,
        "update": Value::Null,
        "download_attempted": false,
    });

    match tauri::async_runtime::block_on(async { updater.check().await }) {
        Ok(None) => {
            report["outcome"] = json!("no_update");
        }
        Ok(Some(update)) => {
            report["outcome"] = json!("update_available");
            report["update"] = json!({
                "version": update.version.clone(),
                "download_url": update.download_url.to_string(),
            });
            if args.control_download {
                report["download_attempted"] = json!(true);
                let result =
                    tauri::async_runtime::block_on(async { update.download(|_, _| {}, || {}).await });
                report["download_result"] = json!(match result {
                    Ok(bytes) => format!("ok: {} bytes", bytes.len()),
                    Err(error) => format!("error: {error}"),
                });
            }
        }
        Err(error) => {
            report["outcome"] = json!("error");
            report["error_kind"] = json!(error_kind(&error));
            report["error"] = json!(error.to_string());
        }
    }
    Ok(report)
}

fn main() -> ExitCode {
    match run() {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("native-updater-check: {message}");
            ExitCode::from(2)
        }
    }
}
