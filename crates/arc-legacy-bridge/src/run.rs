//! Entry point: a legacy v0.7 invocation is bridged and replaced by the
//! pinned v0.8 node; `--legacy-bridge-*` arguments are operator commands.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::archive;
use crate::argv::{self, ArgvError, LegacyKind};
use crate::consent;
use crate::exit::{self, EX_OK};
use crate::fetch::Fetcher;
use crate::launch;
use crate::layout::{self, BRIDGE_DIR_NAME, Layout};
use crate::logging::{Log, unix_now};
use crate::pins::{Pins, Platform, parse_version};
use crate::release::{self, quiet_command};
use crate::state::{self, BridgeState, STATE_SCHEMA};

pub const PRESERVED_DIR_NAME: &str = "preserved";
pub const PRESERVED_BINARY_NAME: &str = "arc-node.before-bridge";
pub const ROLLED_BACK_MARKER: &str = "ROLLED-BACK";

pub fn main_entry(args: Vec<OsString>) -> i32 {
    let rest: Vec<OsString> = args.into_iter().skip(1).collect();
    match dispatch(&rest) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("arc-legacy-bridge: error: {error:#}");
            exit::code_for(&error)
        }
    }
}

fn dispatch(args: &[OsString]) -> Result<i32> {
    let first = args.first().and_then(|arg| arg.to_str());
    match first {
        Some("--version" | "-V") if args.len() == 1 => {
            println!("{}", version_line());
            Ok(EX_OK)
        }
        Some("--help" | "-h") if args.len() == 1 => {
            print!("{}", help_text());
            Ok(EX_OK)
        }
        Some(command) if command.starts_with("--legacy-bridge-") => operator(command, &args[1..]),
        _ => bridge(args),
    }
}

/// The first whitespace token after the program name is the version, which
/// is what the v0.7 desktop and both v0.7 headless updaters read.
pub fn version_line() -> String {
    let node = Pins::embedded()
        .map(|pins| pins.node_release.version)
        .unwrap_or_else(|_| "unknown".to_string());
    format!(
        "arc-node {} (ARC legacy bridge; starts arc-node {node})",
        env!("CARGO_PKG_VERSION")
    )
}

pub fn help_text() -> String {
    format!(
        "arc-node {version}: ARC legacy bridge

Released v0.7 updaters install this program as <ARC dir>/bin/arc-node and
start it with their old command line. It leaves the v0.7 data directory
untouched, installs the pinned, signature-verified arc-node v0.8 under
<ARC dir>/{BRIDGE_DIR_NAME}/, and runs it at stake 0 with a fresh identity in a
fresh data directory. Compute stays off until you opt in.

Operator commands:
  arc-node --legacy-bridge-status [--context NAME]
  arc-node --legacy-bridge-compute on [--model PATH | --download-model] [--context NAME]
  arc-node --legacy-bridge-compute off [--context NAME]
  arc-node --legacy-bridge-verify-archive [--hash] [--context NAME]
  arc-node --legacy-bridge-rollback
  arc-node --version

Rollback, reinstall, and support: docs/LEGACY-BRIDGE.md in FerrumVir/arc-chain.
",
        version = env!("CARGO_PKG_VERSION")
    )
}

fn bridge(args: &[OsString]) -> Result<i32> {
    let invocation = argv::parse(args).map_err(|error| {
        if error == ArgvError::ExplicitValidatorStake {
            exit::refused(error.to_string())
        } else {
            exit::usage(format!(
                "{error}. The bridge only starts nodes for released v0.7 desktop and headless installs; see --help"
            ))
        }
    })?;
    let pins = Pins::embedded().context("the compiled-in bridge pins are invalid")?;
    let platform = Platform::current()
        .ok_or_else(|| exit::refused("this platform has no v0.8 build to bridge to"))?;
    let launcher = std::env::current_exe().context("cannot locate the bridge executable")?;
    let cwd = std::env::current_dir().context("cannot read the working directory")?;
    let layout = layout::resolve(&launcher, &invocation, &cwd)?;
    layout::prepare(&layout)?;
    let mut log = Log::with_file(&layout.log_file());
    let lock = layout::lock(&layout, Duration::from_secs(60))?;
    log.info(&format!(
        "bridge {} started by a released v0.7 {} install whose data directory is {}",
        env!("CARGO_PKG_VERSION"),
        invocation.kind.label(),
        layout.legacy_data_dir.display()
    ));

    if let Some(pid) = layout::live_legacy_node(&layout, &invocation) {
        return Err(exit::temporary(format!(
            "a v0.7 arc-node (pid {pid}) still runs on {}; v0.7 and v0.8 never run side by side, so the new node starts after that process stops",
            layout.legacy_data_dir.display()
        )));
    }

    if invocation.kind == LegacyKind::Headless {
        preserve_previous_binary(&layout, &mut log);
    }
    let archived = archive::ensure(&layout, &mut log)?;
    let fetched = release::ensure(&layout, &pins, platform, &mut log)?;
    release::probe_version(&fetched.node, &pins.node_release.version)?;
    release::probe_version(&fetched.cli, &pins.node_release.version)?;
    let address = release::ensure_identity(&layout, &fetched.cli, &mut log)?;
    let compute = consent::decide(&layout, &invocation, &pins)?;
    let plan = launch::plan(&layout, &invocation, &pins, &fetched, &compute);
    launch::check_autodiscovery(&plan)?;

    let record = BridgeState {
        schema: STATE_SCHEMA.to_string(),
        bridge_version: env!("CARGO_PKG_VERSION").to_string(),
        legacy_kind: invocation.kind.label().to_string(),
        legacy_data_dir: layout.legacy_data_dir.to_string_lossy().into_owned(),
        legacy_rpc_port: invocation.rpc.port(),
        legacy_p2p_port: invocation.p2p_port,
        legacy_model: invocation
            .model
            .as_ref()
            .map(|model| model.to_string_lossy().into_owned()),
        archive_record: archived.path.to_string_lossy().into_owned(),
        archive_generation: archived.generation,
        node_release_tag: pins.node_release.tag.clone(),
        node_binary: fetched.node.to_string_lossy().into_owned(),
        node_data_dir: layout.data_dir().to_string_lossy().into_owned(),
        node_keyfile: layout.keyfile().to_string_lossy().into_owned(),
        node_address: address,
        node_rpc: format!("127.0.0.1:{}", invocation.rpc.port()),
        stake: 0,
        community_registration: plan.community,
        compute: compute.describe(),
        supervisor: state::detect_supervisor(invocation.kind == LegacyKind::Desktop),
        updated_unix: unix_now(),
    };
    record.write(&layout.state_file())?;
    let next = consent::next_step(&layout, &invocation, &pins, &compute);
    announce(&mut log, &record, &pins, &next);
    drop(lock);
    launch::exec(&plan)
}

fn announce(log: &mut Log, record: &BridgeState, pins: &Pins, next: &str) {
    log.info("------------------------------------------------------------");
    log.info(&format!(
        "Your ARC node is upgrading to the new network. It now runs arc-node {} as a stake-0 community node.",
        pins.node_release.version
    ));
    log.info(&format!(
        "Your v0.7 data stays where it was, unchanged: {}",
        record.legacy_data_dir
    ));
    log.info(&format!("New data directory: {}", record.node_data_dir));
    log.info(&format!("Node address: {}", record.node_address));
    log.info(&format!(
        "Community registration: {}",
        if record.community_registration {
            "on"
        } else {
            "off (read-only until a privacy-safe node build is pinned)"
        }
    ));
    log.info(&format!("Compute: {}", record.compute));
    log.info(&format!("Next: {next}"));
    log.info("------------------------------------------------------------");
}

/// Keep the binary the v0.7 updater moved aside (`bin/arc-node.prev`) so a
/// later rollback can restore it even after another update. Best effort.
fn preserve_previous_binary(layout: &Layout, log: &mut Log) {
    let previous = layout.root.join("bin").join("arc-node.prev");
    let preserved_dir = layout.bridge_dir.join(PRESERVED_DIR_NAME);
    let preserved = preserved_dir.join(PRESERVED_BINARY_NAME);
    if fs::symlink_metadata(&preserved).is_ok() || !previous.is_file() {
        return;
    }
    let result = layout::ensure_private_dir(&preserved_dir).and_then(|()| {
        fs::copy(&previous, &preserved)
            .map(|_| ())
            .map_err(anyhow::Error::from)
    });
    match result {
        Ok(()) => log.info(&format!(
            "kept a copy of the pre-bridge binary for rollback: {}",
            preserved.display()
        )),
        Err(error) => log.warn(&format!("could not keep the pre-bridge binary: {error:#}")),
    }
}

#[derive(Debug, Default)]
struct OperatorOptions {
    positional: Vec<String>,
    context: Option<String>,
    model: Option<PathBuf>,
    download_model: bool,
    hash: bool,
}

impl OperatorOptions {
    fn parse(args: &[OsString]) -> Result<OperatorOptions> {
        let mut options = OperatorOptions::default();
        let mut index = 0;
        while index < args.len() {
            let arg = args[index]
                .to_str()
                .ok_or_else(|| exit::usage("arguments must be valid UTF-8"))?;
            index += 1;
            match arg {
                "--context" | "--model" => {
                    let value = args
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or_else(|| exit::usage(format!("{arg} needs a value")))?;
                    index += 1;
                    if arg == "--context" {
                        options.context = Some(value.to_string());
                    } else {
                        options.model = Some(PathBuf::from(value));
                    }
                }
                "--download-model" => options.download_model = true,
                "--hash" => options.hash = true,
                other if other.starts_with("--") => {
                    return Err(exit::usage(format!("unknown option {other}; see --help")));
                }
                other => options.positional.push(other.to_string()),
            }
        }
        Ok(options)
    }
}

fn operator(command: &str, args: &[OsString]) -> Result<i32> {
    let options = OperatorOptions::parse(args)?;
    let launcher = std::env::current_exe().context("cannot locate the bridge executable")?;
    let root = layout::root_for_launcher(&launcher)?;
    match command {
        "--legacy-bridge-status" => status(&launcher, &root, &options),
        "--legacy-bridge-compute" => compute_command(&launcher, &root, &options),
        "--legacy-bridge-verify-archive" => verify_archive(&launcher, &root, &options),
        "--legacy-bridge-rollback" => rollback(&launcher, &root),
        _ => Err(exit::usage(format!(
            "unknown command {command}; see --help"
        ))),
    }
}

struct BridgedNode {
    name: String,
    layout: Layout,
    state: BridgeState,
}

fn bridged_nodes(launcher: &Path, root: &Path) -> Result<Vec<BridgedNode>> {
    let nodes_dir = root.join(BRIDGE_DIR_NAME).join("nodes");
    let listing = match fs::read_dir(&nodes_dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot list {}", nodes_dir.display()));
        }
    };
    let mut nodes = Vec::new();
    for child in listing.flatten() {
        let node_dir = child.path();
        let Ok(state) = BridgeState::read(&node_dir.join("bridge-state.json")) else {
            continue;
        };
        let layout =
            layout::from_existing_node(launcher, &node_dir, Path::new(&state.legacy_data_dir))?;
        nodes.push(BridgedNode {
            name: child.file_name().to_string_lossy().into_owned(),
            layout,
            state,
        });
    }
    nodes.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(nodes)
}

fn select(nodes: Vec<BridgedNode>, context: Option<&str>) -> Result<BridgedNode> {
    let mut matching: Vec<BridgedNode> = nodes
        .into_iter()
        .filter(|node| context.is_none_or(|name| node.name == name))
        .collect();
    match matching.len() {
        0 => Err(exit::usage(
            "no bridged node matches; run --legacy-bridge-status",
        )),
        1 => Ok(matching.remove(0)),
        _ => Err(exit::usage(
            "several bridged nodes exist on this machine; pass --context NAME from --legacy-bridge-status",
        )),
    }
}

fn status(launcher: &Path, root: &Path, options: &OperatorOptions) -> Result<i32> {
    let nodes = bridged_nodes(launcher, root)?;
    if nodes.is_empty() {
        println!("No v0.7 install on this machine has been bridged yet.");
        return Ok(EX_OK);
    }
    for node in nodes {
        if options
            .context
            .as_deref()
            .is_some_and(|name| node.name != name)
        {
            continue;
        }
        println!("{}", node.name);
        println!(
            "  v0.7 install:        {} at {}",
            node.state.legacy_kind, node.state.legacy_data_dir
        );
        println!(
            "  v0.7 data record:    {} (generation {})",
            node.state.archive_record, node.state.archive_generation
        );
        println!(
            "  node:                arc-node {} on {}",
            node.state.node_release_tag, node.state.node_rpc
        );
        println!("  stake:               {}", node.state.stake);
        println!("  address:             {}", node.state.node_address);
        println!("  data directory:      {}", node.state.node_data_dir);
        println!(
            "  community:           {}",
            if node.state.community_registration {
                "registered with the six validators"
            } else {
                "read-only"
            }
        );
        println!("  compute:             {}", node.state.compute);
        println!(
            "  restart:             {}",
            state::restart_hint(&node.state.supervisor)
        );
    }
    Ok(EX_OK)
}

fn compute_command(launcher: &Path, root: &Path, options: &OperatorOptions) -> Result<i32> {
    let enable = match options.positional.as_slice() {
        [value] if value == "on" => true,
        [value] if value == "off" => false,
        _ => {
            return Err(exit::usage(
                "use --legacy-bridge-compute on or --legacy-bridge-compute off",
            ));
        }
    };
    let node = select(bridged_nodes(launcher, root)?, options.context.as_deref())?;
    if node.layout.kind == LegacyKind::Desktop {
        return Err(exit::refused(
            "this node belongs to the ARC desktop app; update the app and choose there whether to contribute compute",
        ));
    }
    let restart = state::restart_hint(&node.state.supervisor);
    if !enable {
        consent::write_consent(&node.layout, false)?;
        println!("Compute contribution is off. Restart the node to apply it: {restart}");
        return Ok(EX_OK);
    }
    let pins = Pins::embedded()?;
    let model = match (
        &options.model,
        options.download_model,
        &node.state.legacy_model,
    ) {
        (Some(model), _, _) => model.clone(),
        (None, true, _) => download_model(&node.layout, &pins)?,
        (None, false, Some(legacy)) => PathBuf::from(legacy),
        (None, false, None) => {
            return Err(exit::usage(format!(
                "no model given. Pass --model /path/to/{} ({} bytes, SHA-256 {}) or --download-model",
                pins.model.file_name, pins.model.size, pins.model.sha256
            )));
        }
    };
    println!(
        "Verifying {} against the pinned SHA-256 ({} bytes to read)...",
        model.display(),
        pins.model.size
    );
    let step = 512 * 1024 * 1024;
    let mut next_report = step;
    consent::verify_and_record_model(&node.layout, &pins, &model, |done| {
        if done >= next_report {
            eprintln!("  verified {} MiB", done / (1024 * 1024));
            next_report += step;
        }
    })?;
    consent::write_consent(&node.layout, true)?;
    if pins.node_release.worker_names_privacy_safe {
        println!("Compute contribution is on. Restart the node to apply it: {restart}");
    } else {
        println!(
            "Consent and the verified model are recorded. Compute starts once a bridge release pins a node build that does not publish hostnames (arc-node {} does).",
            pins.node_release.version
        );
    }
    Ok(EX_OK)
}

fn download_model(layout: &Layout, pins: &Pins) -> Result<PathBuf> {
    let dir = layout.models_dir();
    layout::ensure_private_dir(&dir)?;
    let (base, name) = pins
        .model
        .url
        .rsplit_once('/')
        .ok_or_else(|| anyhow!("the pinned model URL has no file name"))?;
    let dest = dir.join(&pins.model.file_name);
    println!(
        "Downloading {} ({} bytes) from {}; an interrupted download resumes.",
        pins.model.file_name, pins.model.size, pins.model.url
    );
    let mut log = Log::with_file(&layout.log_file());
    Fetcher::https_prefix(base).ensure_file(
        name,
        &pins.model.sha256,
        pins.model.size,
        &dest,
        &mut log,
    )?;
    Ok(dest)
}

fn verify_archive(launcher: &Path, root: &Path, options: &OperatorOptions) -> Result<i32> {
    let node = select(bridged_nodes(launcher, root)?, options.context.as_deref())?;
    let Some((path, record)) = archive::latest(&node.layout)? else {
        return Err(exit::usage("this node has no v0.7 archive record yet"));
    };
    let (current, truncated) = archive::snapshot(&node.layout)?;
    let (added, removed, changed) = archive::diff(&record, &current);
    println!(
        "v0.7 data directory: {}",
        node.layout.legacy_data_dir.display()
    );
    println!(
        "archive record:      {} (generation {}, {} entries)",
        path.display(),
        record.generation,
        record.entries.len()
    );
    let unchanged = added.is_empty()
        && removed.is_empty()
        && changed.is_empty()
        && truncated == record.truncated;
    if unchanged {
        println!("unchanged: every recorded entry has the same type, size, and modification time");
    }
    for path in &added {
        println!("added:   {path}");
    }
    for path in &removed {
        println!("removed: {path}");
    }
    for path in &changed {
        println!("changed: {path}");
    }
    if options.hash {
        for (path, digest) in archive::content_digests(&node.layout.legacy_data_dir, &current)? {
            println!("{digest}  {path}");
        }
    }
    Ok(if unchanged { EX_OK } else { 1 })
}

/// Put the pre-bridge binary back as `bin/arc-node` (headless, Unix only).
fn rollback(launcher: &Path, root: &Path) -> Result<i32> {
    if cfg!(windows) {
        return Err(exit::refused(
            "the desktop app manages arc-node on Windows; reinstall ARC Node to roll back",
        ));
    }
    let bridge_dir = root.join(BRIDGE_DIR_NAME);
    let candidates = [
        bridge_dir
            .join(PRESERVED_DIR_NAME)
            .join(PRESERVED_BINARY_NAME),
        root.join("bin").join("arc-node.prev"),
    ];
    let own = parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0));
    let mut chosen: Option<(PathBuf, String)> = None;
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let Ok(output) = quiet_command(&candidate).arg("--version").output() else {
            continue;
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let version = stdout
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string();
        if output.status.success() && parse_version(&version).is_some_and(|parsed| parsed < own) {
            chosen = Some((candidate, version));
            break;
        }
    }
    let Some((previous, version)) = chosen else {
        return Err(exit::refused(
            "no earlier arc-node binary is available to roll back to; see docs/LEGACY-BRIDGE.md to reinstall one",
        ));
    };
    layout::ensure_private_dir(&bridge_dir)?;
    let kept_launcher = bridge_dir.join(format!("arc-node-bridge-{}", env!("CARGO_PKG_VERSION")));
    fs::copy(launcher, &kept_launcher).with_context(|| {
        format!(
            "cannot keep the bridge launcher at {}",
            kept_launcher.display()
        )
    })?;
    let staging = root.join("bin").join(".arc-node.rollback");
    fs::copy(&previous, &staging)
        .with_context(|| format!("cannot stage {}", previous.display()))?;
    fs::rename(&staging, launcher)
        .with_context(|| format!("cannot restore {}", launcher.display()))?;
    layout::write_atomic(
        &bridge_dir.join(ROLLED_BACK_MARKER),
        format!(
            "rolled back from bridge {} to arc-node {version} at {}\n",
            env!("CARGO_PKG_VERSION"),
            crate::logging::utc_rfc3339(unix_now())
        )
        .as_bytes(),
    )?;
    let restart = bridged_nodes(launcher, root)
        .ok()
        .and_then(|nodes| {
            nodes
                .into_iter()
                .find(|node| node.layout.kind == LegacyKind::Headless)
        })
        .map_or_else(
            || "restart arc-node".to_string(),
            |node| state::restart_hint(&node.state.supervisor),
        );
    println!("Restored arc-node {version} as {}.", launcher.display());
    println!("Restart the node to run it on its unchanged v0.7 data: {restart}");
    // A running binary cannot be overwritten in place (ETXTBSY), so the
    // reinstall instruction stages a copy and renames it, as v0.7 did.
    println!(
        "To bridge again later: cp {kept} {target}.new && mv {target}.new {target}, then restart.",
        kept = kept_launcher.display(),
        target = launcher.display()
    );
    Ok(EX_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_leads_with_the_bridge_version_token() {
        let line = version_line();
        let tokens: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(tokens[0], "arc-node");
        assert_eq!(tokens[1], env!("CARGO_PKG_VERSION"));
        // scripts/auto-update.sh takes the first X.Y.Z it finds.
        let first_version = tokens
            .iter()
            .find(|token| {
                parse_version(token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.'))
                    .is_some()
            })
            .unwrap();
        assert_eq!(*first_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn unrecognized_command_lines_exit_with_usage_and_validators_are_refused() {
        let code = main_entry(vec![
            OsString::from("arc-node"),
            OsString::from("--benchmark"),
        ]);
        assert_eq!(code, exit::EX_USAGE);
        let validator = [
            "arc-node",
            "--rpc",
            "0.0.0.0:9944",
            "--p2p-port",
            "9945",
            "--data-dir",
            "/srv/arc/data",
            "--validator-seed",
            "seed",
            "--stake",
            "5000000",
            "--min-stake",
            "500000",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(main_entry(validator), exit::EX_CONFIG);
    }

    #[test]
    fn operator_options_parse() {
        let options = OperatorOptions::parse(&[
            OsString::from("on"),
            OsString::from("--model"),
            OsString::from("/m.gguf"),
            OsString::from("--context"),
            OsString::from("headless-0123"),
        ])
        .unwrap();
        assert_eq!(options.positional, vec!["on".to_string()]);
        assert_eq!(options.model, Some(PathBuf::from("/m.gguf")));
        assert_eq!(options.context.as_deref(), Some("headless-0123"));
        assert!(OperatorOptions::parse(&[OsString::from("--bogus")]).is_err());
    }

    #[test]
    fn help_names_every_operator_command() {
        let help = help_text();
        for command in [
            "--legacy-bridge-status",
            "--legacy-bridge-compute on",
            "--legacy-bridge-compute off",
            "--legacy-bridge-verify-archive",
            "--legacy-bridge-rollback",
        ] {
            assert!(help.contains(command), "{command}");
        }
    }
}
