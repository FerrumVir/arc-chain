use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::archive;
use crate::argv::{self, ArgvError, LegacyKind};
use crate::consent::{self, WorkerModePreference};
use crate::exit::{self, EX_OK};
use crate::fetch::Fetcher;
use crate::launch;
use crate::layout::{self, BRIDGE_DIR_NAME, Layout};
use crate::logging::{Log, unix_now};
use crate::pins::{Pins, Platform, parse_version};
use crate::release::{self, quiet_command};
use crate::shard_policy;
use crate::state::{self, BridgeState, STATE_SCHEMA};

pub const PRESERVED_DIR_NAME: &str = "preserved";
pub const PRESERVED_BINARY_NAME: &str = "arc-node.before-bridge";
pub const ROLLED_BACK_MARKER: &str = "ROLLED-BACK";
const VERSION_LINE_PREFIX: &str = "arc-node ";
const BRIDGE_VERSION_MARKER: &str = " (ARC legacy bridge; starts arc-node ";

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

pub fn version_line() -> String {
    let node = Pins::embedded()
        .map(|pins| pins.node_release.version)
        .unwrap_or_else(|_| "unknown".to_string());
    format!(
        "{VERSION_LINE_PREFIX}{}{BRIDGE_VERSION_MARKER}{node})",
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

Consenting headless nodes default to --worker-mode auto: when the pinned
release provides shard pins and this machine passes fresh memory and
--check-only preflight, the node runs as a partial-shard worker (with
rewards disabled); otherwise auto falls back to the full integer worker.
Explicit shard mode fails closed if qualification fails; explicit full
mode skips partial-shard preflight.

Operator commands:
  arc-node --legacy-bridge-status [--context NAME]
  arc-node --legacy-bridge-compute on [--model PATH | --download-model] [--worker-mode auto|shard|full] [--context NAME]
  arc-node --legacy-bridge-compute off [--context NAME]
  arc-node --legacy-bridge-worker-mode auto|shard|full [--context NAME]
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
    let role =
        shard_policy::resolve_worker_role(&layout, &pins, &fetched.node, &compute, &mut log)?;
    let plan = launch::plan_for_role(&layout, &invocation, &pins, &fetched, &role);
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
        compute: role.describe(),
        supervisor: state::detect_supervisor(invocation.kind == LegacyKind::Desktop),
        updated_unix: unix_now(),
    };
    record.write(&layout.state_file())?;
    let next = consent::next_step(&layout, &invocation, &pins, &compute);
    announce(&mut log, &record, &pins, &next);
    drop(lock);
    launch::exec(&plan)
}

fn community_status_summary(record: &BridgeState) -> &'static str {
    if !record.community_registration {
        return "read-only";
    }
    match shard_policy::classify_stored_compute(&record.compute) {
        shard_policy::StoredComputeRole::CommunityShardWorker => {
            "configured for the pinned shard coordinator (rewards disabled)"
        }
        shard_policy::StoredComputeRole::Observer
        | shard_policy::StoredComputeRole::FullIntegerWorker
        | shard_policy::StoredComputeRole::Unknown => "configured for the six validators",
    }
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
        "Community config: {}",
        if record.community_registration {
            community_status_summary(record)
        } else {
            "off (read-only until a privacy-safe node build is pinned)"
        }
    ));
    log.info(&format!("Compute: {}", record.compute));
    log.info(&format!("Next: {next}"));
    log.info("------------------------------------------------------------");
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedNodeVersion {
    version: String,
    parsed: (u64, u64, u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedBridgeVersion {
    version: String,
    target_node: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CandidateBinaryVersion {
    ArcNode(ParsedNodeVersion),
    LegacyBridge(ParsedBridgeVersion),
}

impl CandidateBinaryVersion {
    fn pre_bridge_version(&self, own: (u64, u64, u64)) -> Option<&str> {
        match self {
            Self::ArcNode(node) if node.parsed < own => Some(&node.version),
            Self::ArcNode(_) | Self::LegacyBridge(_) => None,
        }
    }

    fn describe_for_log(&self) -> String {
        match self {
            Self::ArcNode(node) => format!("arc-node {}", node.version),
            Self::LegacyBridge(bridge) => {
                let version = &bridge.version;
                let target = &bridge.target_node;
                format!("ARC legacy bridge {version} (starts arc-node {target})")
            }
        }
    }
}

fn own_bridge_version() -> (u64, u64, u64) {
    parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0))
}

fn parse_candidate_version(stdout: &str) -> Option<CandidateBinaryVersion> {
    let line = stdout
        .strip_suffix("\r\n")
        .or_else(|| stdout.strip_suffix('\n'))
        .unwrap_or(stdout);
    let rest = line.strip_prefix(VERSION_LINE_PREFIX)?;
    if let Some(parsed) = parse_version(rest) {
        return Some(CandidateBinaryVersion::ArcNode(ParsedNodeVersion {
            version: rest.to_string(),
            parsed,
        }));
    }
    let (version, target_with_close) = rest.split_once(BRIDGE_VERSION_MARKER)?;
    let target_node = target_with_close.strip_suffix(')')?;
    parse_version(version)?;
    if target_node != "unknown" && parse_version(target_node).is_none() {
        return None;
    }
    Some(CandidateBinaryVersion::LegacyBridge(ParsedBridgeVersion {
        version: version.to_string(),
        target_node: target_node.to_string(),
    }))
}

fn probe_candidate_version(candidate: &Path) -> Option<CandidateBinaryVersion> {
    if !candidate.is_file() {
        return None;
    }
    let output = quiet_command(candidate).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    parse_candidate_version(stdout)
}

fn preserve_previous_binary(layout: &Layout, log: &mut Log) {
    let previous = layout.root.join("bin").join("arc-node.prev");
    let preserved_dir = layout.bridge_dir.join(PRESERVED_DIR_NAME);
    let preserved = preserved_dir.join(PRESERVED_BINARY_NAME);
    if fs::symlink_metadata(&preserved).is_ok() || !previous.is_file() {
        return;
    }
    let own = own_bridge_version();
    let Some(candidate) = probe_candidate_version(&previous) else {
        log.info(&format!(
            "not keeping {} for rollback because it did not report an earlier pre-bridge arc-node version",
            previous.display()
        ));
        return;
    };
    let Some(version) = candidate.pre_bridge_version(own) else {
        let described = candidate.describe_for_log();
        log.info(&format!(
            "not keeping {} for rollback because it is {described}, not a pre-bridge arc-node earlier than {}",
            previous.display(),
            env!("CARGO_PKG_VERSION")
        ));
        return;
    };
    let result = layout::ensure_private_dir(&preserved_dir).and_then(|()| {
        fs::copy(&previous, &preserved)
            .map(|_| ())
            .map_err(anyhow::Error::from)
    });
    match result {
        Ok(()) => log.info(&format!(
            "kept a copy of pre-bridge arc-node {version} for rollback: {}",
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
    worker_mode: Option<String>,
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
                "--context" | "--model" | "--worker-mode" => {
                    let value = args
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or_else(|| exit::usage(format!("{arg} needs a value")))?;
                    index += 1;
                    match arg {
                        "--context" => options.context = Some(value.to_string()),
                        "--model" => options.model = Some(PathBuf::from(value)),
                        _ => options.worker_mode = Some(value.to_string()),
                    }
                }
                "--download-model" => options.download_model = true,
                "--hash" => options.hash = true,
                other => {
                    if let Some(value) = other.strip_prefix("--worker-mode=") {
                        if value.is_empty() {
                            return Err(exit::usage("--worker-mode needs a value"));
                        }
                        options.worker_mode = Some(value.to_string());
                    } else if other.starts_with("--") {
                        return Err(exit::usage(format!("unknown option {other}; see --help")));
                    } else {
                        options.positional.push(other.to_string());
                    }
                }
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
        "--legacy-bridge-worker-mode" => worker_mode_command(&launcher, &root, &options),
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
            community_status_summary(&node.state)
        );
        println!("  compute:             {}", node.state.compute);
        if node.layout.kind == LegacyKind::Headless {
            let worker_mode = match consent::read_worker_mode(&node.layout) {
                Ok(mode) => mode.label().to_string(),
                Err(error) => format!("invalid ({error:#})"),
            };
            println!("  worker mode:         {worker_mode}");
        }
        println!(
            "  restart:             {}",
            state::restart_hint(&node.state.supervisor)
        );
    }
    Ok(EX_OK)
}

fn compute_command(launcher: &Path, root: &Path, options: &OperatorOptions) -> Result<i32> {
    let pins = Pins::embedded()?;
    compute_command_with_pins(launcher, root, options, &pins)
}

fn compute_command_with_pins(
    launcher: &Path,
    root: &Path,
    options: &OperatorOptions,
    pins: &Pins,
) -> Result<i32> {
    let enable = match options.positional.as_slice() {
        [value] if value == "on" => true,
        [value] if value == "off" => false,
        _ => {
            return Err(exit::usage(
                "use --legacy-bridge-compute on or --legacy-bridge-compute off",
            ));
        }
    };
    if !enable && options.worker_mode.is_some() {
        return Err(exit::usage(
            "--worker-mode cannot be combined with --legacy-bridge-compute off",
        ));
    }
    let requested_worker_mode = if enable {
        options
            .worker_mode
            .as_deref()
            .map(WorkerModePreference::parse)
            .transpose()?
    } else {
        None
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
    let model = match (
        &options.model,
        options.download_model,
        &node.state.legacy_model,
    ) {
        (Some(model), _, _) => model.clone(),
        (None, true, _) => download_model(&node.layout, pins)?,
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
    consent::verify_and_record_model(&node.layout, pins, &model, |done| {
        if done >= next_report {
            eprintln!("  verified {} MiB", done / (1024 * 1024));
            next_report += step;
        }
    })?;
    if let Some(mode) = requested_worker_mode {
        consent::write_worker_mode(&node.layout, mode)?;
    }
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

fn worker_mode_command(launcher: &Path, root: &Path, options: &OperatorOptions) -> Result<i32> {
    if options.model.is_some()
        || options.download_model
        || options.hash
        || options.worker_mode.is_some()
    {
        return Err(exit::usage(
            "use --legacy-bridge-worker-mode auto|shard|full [--context NAME]",
        ));
    }
    let mode = match options.positional.as_slice() {
        [value] => WorkerModePreference::parse(value)?,
        _ => {
            return Err(exit::usage(
                "use --legacy-bridge-worker-mode auto|shard|full [--context NAME]",
            ));
        }
    };
    let node = select(bridged_nodes(launcher, root)?, options.context.as_deref())?;
    if node.layout.kind == LegacyKind::Desktop {
        return Err(exit::refused(
            "this node belongs to the ARC desktop app; worker mode overrides apply only to headless bridged nodes",
        ));
    }
    consent::write_worker_mode(&node.layout, mode)?;
    let restart = state::restart_hint(&node.state.supervisor);
    println!(
        "Worker mode is set to {}. Saved compute consent is unchanged. Restart the node to apply it: {restart}",
        mode.label()
    );
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
    let own = own_bridge_version();
    let mut chosen: Option<(PathBuf, String)> = None;
    for candidate in candidates {
        let Some(probed) = probe_candidate_version(&candidate) else {
            continue;
        };
        if let Some(version) = probed.pre_bridge_version(own) {
            chosen = Some((candidate, version.to_string()));
            break;
        }
    }
    let Some((previous, version)) = chosen else {
        return Err(exit::refused(
            "no earlier pre-bridge arc-node binary is available to roll back to; see docs/LEGACY-BRIDGE.md to reinstall one",
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
            OsString::from("--worker-mode"),
            OsString::from("shard"),
            OsString::from("--context"),
            OsString::from("headless-0123"),
        ])
        .unwrap();
        assert_eq!(options.positional, vec!["on".to_string()]);
        assert_eq!(options.model, Some(PathBuf::from("/m.gguf")));
        assert_eq!(options.worker_mode.as_deref(), Some("shard"));
        assert_eq!(options.context.as_deref(), Some("headless-0123"));

        let equals = OperatorOptions::parse(&[OsString::from("--worker-mode=full")]).unwrap();
        assert_eq!(equals.worker_mode.as_deref(), Some("full"));

        assert!(OperatorOptions::parse(&[OsString::from("--worker-mode=")]).is_err());
        assert!(OperatorOptions::parse(&[OsString::from("--worker-mode")]).is_err());
        assert!(OperatorOptions::parse(&[OsString::from("--bogus")]).is_err());
    }

    #[test]
    fn help_names_every_operator_command() {
        let help = help_text();
        for command in [
            "--legacy-bridge-status",
            "--legacy-bridge-compute on",
            "--legacy-bridge-compute off",
            "--legacy-bridge-worker-mode",
            "--worker-mode auto|shard|full",
            "rewards disabled",
            "--legacy-bridge-verify-archive",
            "--legacy-bridge-rollback",
        ] {
            assert!(help.contains(command), "{command}");
        }
    }

    #[test]
    fn worker_mode_and_compute_commands_preserve_consent_and_validate_before_persist() {
        use crate::layout::test_support::{TempDir, fake_arc_dir};

        let temp = TempDir::new("run-worker-mode");
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);

        let headless_node = root
            .join(BRIDGE_DIR_NAME)
            .join("nodes")
            .join("headless-0123456789ab");
        fs::create_dir_all(&headless_node).unwrap();
        let headless_layout =
            layout::from_existing_node(&launcher, &headless_node, &root.join("data")).unwrap();
        layout::prepare(&headless_layout).unwrap();
        let headless_state = BridgeState {
            schema: STATE_SCHEMA.to_string(),
            bridge_version: env!("CARGO_PKG_VERSION").to_string(),
            legacy_kind: "headless".to_string(),
            legacy_data_dir: root.join("data").to_string_lossy().into_owned(),
            legacy_rpc_port: 9944,
            legacy_p2p_port: 9945,
            legacy_model: None,
            archive_record: "v0.7-data-archive-0001.json".to_string(),
            archive_generation: 1,
            node_release_tag: "v0.8.27".to_string(),
            node_binary: "/bin/arc-node".to_string(),
            node_data_dir: headless_layout.data_dir().to_string_lossy().into_owned(),
            node_keyfile: headless_layout.keyfile().to_string_lossy().into_owned(),
            node_address: "ab".repeat(32),
            node_rpc: "127.0.0.1:9944".to_string(),
            stake: 0,
            community_registration: true,
            compute: "off: no compute consent has been recorded".to_string(),
            supervisor: "systemd".to_string(),
            updated_unix: 1,
        };
        headless_state.write(&headless_layout.state_file()).unwrap();

        let desktop_node = root
            .join(BRIDGE_DIR_NAME)
            .join("nodes")
            .join("desktop-0123456789ab");
        fs::create_dir_all(&desktop_node).unwrap();
        let desktop_layout = layout::from_existing_node(&launcher, &desktop_node, &root).unwrap();
        layout::prepare(&desktop_layout).unwrap();
        let desktop_state = BridgeState {
            legacy_kind: "desktop".to_string(),
            legacy_data_dir: root.to_string_lossy().into_owned(),
            node_data_dir: desktop_layout.data_dir().to_string_lossy().into_owned(),
            node_keyfile: desktop_layout.keyfile().to_string_lossy().into_owned(),
            supervisor: "desktop-app".to_string(),
            ..headless_state.clone()
        };
        desktop_state.write(&desktop_layout.state_file()).unwrap();

        let off_with_mode = OperatorOptions::parse(&[
            OsString::from("off"),
            OsString::from("--worker-mode"),
            OsString::from("shard"),
            OsString::from("--context"),
            OsString::from("headless-0123456789ab"),
        ])
        .unwrap();
        let err = compute_command(&launcher, &root, &off_with_mode).unwrap_err();
        assert_eq!(exit::code_for(&err), exit::EX_USAGE);
        assert!(!headless_layout.consent_file().exists());
        assert!(!headless_layout.worker_mode_file().exists());

        let bad_model = temp.path().join("wrong.gguf");
        fs::write(&bad_model, b"wrong size").unwrap();
        let on_bad_mode = OperatorOptions::parse(&[
            OsString::from("on"),
            OsString::from("--model"),
            bad_model.as_os_str().to_os_string(),
            OsString::from("--worker-mode"),
            OsString::from("bogus"),
            OsString::from("--context"),
            OsString::from("headless-0123456789ab"),
        ])
        .unwrap();
        let err = compute_command(&launcher, &root, &on_bad_mode).unwrap_err();
        assert_eq!(exit::code_for(&err), exit::EX_USAGE);
        assert!(!headless_layout.consent_file().exists());
        assert!(!headless_layout.worker_mode_file().exists());

        let on_bad_model = OperatorOptions::parse(&[
            OsString::from("on"),
            OsString::from("--model"),
            bad_model.as_os_str().to_os_string(),
            OsString::from("--worker-mode"),
            OsString::from("shard"),
            OsString::from("--context"),
            OsString::from("headless-0123456789ab"),
        ])
        .unwrap();
        let err = compute_command(&launcher, &root, &on_bad_model).unwrap_err();
        assert_eq!(exit::code_for(&err), exit::EX_CONFIG);
        assert!(!headless_layout.consent_file().exists());
        assert!(!headless_layout.model_record().exists());
        assert!(!headless_layout.worker_mode_file().exists());

        let set_shard = OperatorOptions::parse(&[
            OsString::from("shard"),
            OsString::from("--context"),
            OsString::from("headless-0123456789ab"),
        ])
        .unwrap();
        assert_eq!(
            worker_mode_command(&launcher, &root, &set_shard).unwrap(),
            EX_OK
        );
        assert_eq!(
            consent::read_worker_mode(&headless_layout).unwrap(),
            WorkerModePreference::Shard
        );
        assert!(!headless_layout.consent_file().exists());
        assert!(!headless_layout.model_record().exists());

        consent::write_consent(&headless_layout, false).unwrap();
        let set_full = OperatorOptions::parse(&[
            OsString::from("full"),
            OsString::from("--context"),
            OsString::from("headless-0123456789ab"),
        ])
        .unwrap();
        assert_eq!(
            worker_mode_command(&launcher, &root, &set_full).unwrap(),
            EX_OK
        );
        assert_eq!(
            consent::read_worker_mode(&headless_layout).unwrap(),
            WorkerModePreference::Full
        );
        let saved_consent = consent::read_consent(&headless_layout).unwrap();
        assert_eq!(saved_consent, Some(false));

        let extra_flag = OperatorOptions::parse(&[
            OsString::from("auto"),
            OsString::from("--download-model"),
            OsString::from("--context"),
            OsString::from("headless-0123456789ab"),
        ])
        .unwrap();
        let err = worker_mode_command(&launcher, &root, &extra_flag).unwrap_err();
        assert_eq!(exit::code_for(&err), exit::EX_USAGE);
        assert_eq!(
            consent::read_worker_mode(&headless_layout).unwrap(),
            WorkerModePreference::Full
        );

        let desktop_override = OperatorOptions::parse(&[
            OsString::from("shard"),
            OsString::from("--context"),
            OsString::from("desktop-0123456789ab"),
        ])
        .unwrap();
        let err = worker_mode_command(&launcher, &root, &desktop_override).unwrap_err();
        assert_eq!(exit::code_for(&err), exit::EX_CONFIG);
        assert!(!desktop_layout.worker_mode_file().exists());

        let valid_model = temp.path().join("verified.gguf");
        let model_bytes = b"exact pinned model bytes";
        fs::write(&valid_model, model_bytes).unwrap();
        let mut test_pins = Pins::embedded().unwrap();
        test_pins.model.sha256 = crate::hashing::sha256_bytes(model_bytes);
        test_pins.model.size = model_bytes.len() as u64;
        let on_verified = OperatorOptions::parse(&[
            OsString::from("on"),
            OsString::from("--model"),
            valid_model.as_os_str().to_os_string(),
            OsString::from("--worker-mode"),
            OsString::from("shard"),
            OsString::from("--context"),
            OsString::from("headless-0123456789ab"),
        ])
        .unwrap();
        assert_eq!(
            compute_command_with_pins(&launcher, &root, &on_verified, &test_pins).unwrap(),
            EX_OK
        );
        let saved_consent = consent::read_consent(&headless_layout).unwrap();
        assert_eq!(saved_consent, Some(true));
        assert_eq!(
            consent::read_worker_mode(&headless_layout).unwrap(),
            WorkerModePreference::Shard
        );
        assert!(
            consent::verified_model(&headless_layout, &test_pins)
                .unwrap()
                .is_some()
        );

        let on_without_override = OperatorOptions::parse(&[
            OsString::from("on"),
            OsString::from("--model"),
            valid_model.as_os_str().to_os_string(),
            OsString::from("--context"),
            OsString::from("headless-0123456789ab"),
        ])
        .unwrap();
        assert_eq!(
            compute_command_with_pins(&launcher, &root, &on_without_override, &test_pins).unwrap(),
            EX_OK
        );
        assert_eq!(
            consent::read_worker_mode(&headless_layout).unwrap(),
            WorkerModePreference::Shard
        );
    }

    #[test]
    fn bridge_state_from_shard_role_stays_compatible_with_v0714_schema() {
        #[allow(dead_code)]
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct V0714BridgeState {
            schema: String,
            bridge_version: String,
            legacy_kind: String,
            legacy_data_dir: String,
            legacy_rpc_port: u16,
            legacy_p2p_port: u16,
            legacy_model: Option<String>,
            archive_record: String,
            archive_generation: u32,
            node_release_tag: String,
            node_binary: String,
            node_data_dir: String,
            node_keyfile: String,
            node_address: String,
            node_rpc: String,
            stake: u64,
            community_registration: bool,
            compute: String,
            supervisor: String,
            updated_unix: u64,
        }

        let pin = crate::pins::ShardWorkerPin {
            coordinator_url: "https://149.28.153.31".to_string(),
            coordinator_id: "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b"
                .to_string(),
            transaction_domain: "3b9147e44d878646caf2d0729d06bd2e082b265e0525760595bd10605f37a10f"
                .to_string(),
            expected_model_id: "934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67"
                .to_string(),
            required_admitted_layers: 6,
        };
        let role = shard_policy::WorkerLaunchRole::CommunityShardWorker {
            model: PathBuf::from("/models/llama.gguf"),
            pin: Box::new(pin),
            max_working_set_bytes: 8 * shard_policy::GIB,
            admitted_max_resident_layers: 6,
        };
        let state = BridgeState {
            schema: STATE_SCHEMA.to_string(),
            bridge_version: env!("CARGO_PKG_VERSION").to_string(),
            legacy_kind: "headless".to_string(),
            legacy_data_dir: "/home/ops/.arc/data".to_string(),
            legacy_rpc_port: 9944,
            legacy_p2p_port: 9945,
            legacy_model: Some("/models/llama.gguf".to_string()),
            archive_record: "v0.7-data-archive-0001.json".to_string(),
            archive_generation: 1,
            node_release_tag: "v0.8.27".to_string(),
            node_binary: "/home/ops/.arc/legacy-bridge/releases/v0.8.27/arc-node-linux-x86_64"
                .to_string(),
            node_data_dir: "/home/ops/.arc/legacy-bridge/nodes/headless-0123456789ab/data"
                .to_string(),
            node_keyfile: "/home/ops/.arc/legacy-bridge/nodes/headless-0123456789ab/identity/validator-key.json"
                .to_string(),
            node_address: "ab".repeat(32),
            node_rpc: "127.0.0.1:9944".to_string(),
            stake: 0,
            community_registration: true,
            compute: role.describe(),
            supervisor: "systemd".to_string(),
            updated_unix: 1,
        };
        let bytes = serde_json::to_vec_pretty(&state).unwrap();
        let decoded: V0714BridgeState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.schema, STATE_SCHEMA);
        assert_eq!(decoded.stake, 0);
        assert!(decoded.community_registration);
        assert!(decoded.compute.contains("partial shard"));
        assert!(decoded.compute.contains("rewards disabled"));

        let shard_summary = community_status_summary(&state);
        assert_eq!(
            shard_summary,
            "configured for the pinned shard coordinator (rewards disabled)"
        );
        assert!(!shard_summary.contains("registered"));

        let mut full_tricky_path = state.clone();
        full_tricky_path.compute = shard_policy::WorkerLaunchRole::FullIntegerWorker {
            model: PathBuf::from("/srv/partial shard/llama.gguf"),
            fallback_reason: None,
        }
        .describe();
        let full_summary = community_status_summary(&full_tricky_path);
        assert_eq!(full_summary, "configured for the six validators");
        assert!(!full_summary.contains("registered"));

        let mut legacy_whole_tricky = state.clone();
        legacy_whole_tricky.compute =
            consent::Compute::On(PathBuf::from("/opt/partial shard/llama2-7b.gguf")).describe();
        assert_eq!(
            community_status_summary(&legacy_whole_tricky),
            "configured for the six validators"
        );

        let observer_role =
            shard_policy::WorkerLaunchRole::Observer("partial shard path not used".to_string());
        let mut observer = state.clone();
        observer.compute = observer_role.describe();
        assert_eq!(
            community_status_summary(&observer),
            "configured for the six validators"
        );

        let mut read_only = state;
        read_only.community_registration = false;
        assert_eq!(community_status_summary(&read_only), "read-only");
    }

    #[test]
    fn candidate_version_parsing_classifies_nodes_bridges_and_rejects_malformed() {
        let v077 = parse_candidate_version("arc-node 0.7.7\n").unwrap();
        assert_eq!(
            v077,
            CandidateBinaryVersion::ArcNode(ParsedNodeVersion {
                version: "0.7.7".to_string(),
                parsed: (0, 7, 7),
            })
        );
        assert_eq!(v077.pre_bridge_version((0, 7, 14)), Some("0.7.7"));
        assert_eq!(v077.pre_bridge_version((0, 7, 15)), Some("0.7.7"));

        let v0711 = parse_candidate_version("arc-node 0.7.11\r\n").unwrap();
        assert_eq!(v0711.pre_bridge_version((0, 7, 15)), Some("0.7.11"));

        let same_node = parse_candidate_version("arc-node 0.7.15\n").unwrap();
        assert_eq!(same_node.pre_bridge_version((0, 7, 15)), None);

        let newer_node = parse_candidate_version("arc-node 0.8.27\n").unwrap();
        assert_eq!(newer_node.pre_bridge_version((0, 7, 15)), None);

        let older_bridge = parse_candidate_version(
            "arc-node 0.7.14 (ARC legacy bridge; starts arc-node 0.8.13)\n",
        )
        .unwrap();
        assert_eq!(
            older_bridge,
            CandidateBinaryVersion::LegacyBridge(ParsedBridgeVersion {
                version: "0.7.14".to_string(),
                target_node: "0.8.13".to_string(),
            })
        );
        assert_eq!(older_bridge.pre_bridge_version((0, 7, 15)), None);

        let unknown_bridge = parse_candidate_version(
            "arc-node 0.7.13 (ARC legacy bridge; starts arc-node unknown)\r\n",
        )
        .unwrap();
        assert_eq!(
            unknown_bridge,
            CandidateBinaryVersion::LegacyBridge(ParsedBridgeVersion {
                version: "0.7.13".to_string(),
                target_node: "unknown".to_string(),
            })
        );
        assert_eq!(unknown_bridge.pre_bridge_version((0, 7, 14)), None);

        let current_line = version_line();
        let current_bridge = parse_candidate_version(&current_line).unwrap();
        let pinned_node = Pins::embedded().unwrap().node_release.version;
        assert_eq!(
            current_bridge,
            CandidateBinaryVersion::LegacyBridge(ParsedBridgeVersion {
                version: env!("CARGO_PKG_VERSION").to_string(),
                target_node: pinned_node,
            })
        );
        assert_eq!(current_bridge.pre_bridge_version((0, 7, 99)), None);

        for malformed in [
            "",
            "\n",
            "arc-node 0.7.7 extra\n",
            "arc-node 0.7.7 \n",
            " arc-node 0.7.7\n",
            "arc-node  0.7.7\n",
            "arc-node v0.7.7\n",
            "arc-node 0.07.7\n",
            "arc-cli 0.7.7\n",
            "arc-node 0.7.7\nsecond-line\n",
            "arc-node 0.7.7\n\n",
            "arc-node 0.7.7 (custom wrapper)\n",
            "arc-node 0.7.14 (ARC legacy bridge; starts arc-node 0.8.13) trailing\n",
            "arc-node 0.7.14 (ARC legacy bridge; starts arc-node 0.8.13\n",
            "arc-node 0.7.14 (ARC legacy bridge; starts arc-node bad)\n",
            "arc-node bad (ARC legacy bridge; starts arc-node 0.8.13)\n",
            "arc-node 0.7.14 (ARC legacy bridge; starts arc-node 0.8.13)\narc-node 0.7.7\n",
            "arc-node 0.7.7\narc-node 0.7.14 (ARC legacy bridge; starts arc-node 0.8.13)\n",
        ] {
            let parsed = parse_candidate_version(malformed);
            assert_eq!(parsed, None, "{malformed:?}");
        }
    }

    #[cfg(unix)]
    mod rollback_preservation_tests {
        use super::*;
        use crate::layout::test_support::{TempDir, fake_arc_dir};
        use std::os::unix::fs::PermissionsExt;

        fn headless_layout(temp: &TempDir) -> Layout {
            let root = temp.path().join(".arc");
            let launcher = fake_arc_dir(&root);
            fs::create_dir_all(root.join("data")).unwrap();
            let invocation = argv::LegacyInvocation {
                kind: LegacyKind::Headless,
                rpc: "127.0.0.1:9944".parse().unwrap(),
                p2p_port: 9945,
                data_dir: root.join("data"),
                seeds_file: None,
                genesis: None,
                model: None,
                community_mode: true,
            };
            let cwd = temp.path();
            let layout = layout::resolve(&launcher, &invocation, cwd).unwrap();
            layout::prepare(&layout).unwrap();
            layout
        }

        fn older_bridge_version_string() -> String {
            let (major, minor, patch) = own_bridge_version();
            assert!(patch > 0);
            format!("{major}.{minor}.{}", patch - 1)
        }

        fn older_bridge_version_line() -> String {
            let older = older_bridge_version_string();
            format!("{VERSION_LINE_PREFIX}{older}{BRIDGE_VERSION_MARKER}0.8.13)")
        }

        fn write_fake_binary(path: &Path, version_output: &str, exit_code: i32) -> Vec<u8> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let escaped = version_output.replace('\'', "'\"'\"'");
            let script = format!("#!/bin/sh\nprintf '%s\\n' '{escaped}'\nexit {exit_code}\n");
            let _ = fs::remove_file(path);
            fs::write(path, script.as_bytes()).unwrap();
            let perms = fs::Permissions::from_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
            script.into_bytes()
        }

        fn preserved_path(layout: &Layout) -> PathBuf {
            layout
                .bridge_dir
                .join(PRESERVED_DIR_NAME)
                .join(PRESERVED_BINARY_NAME)
        }

        fn previous_path(layout: &Layout) -> PathBuf {
            layout.root.join("bin").join("arc-node.prev")
        }

        fn kept_launcher_path(layout: &Layout) -> PathBuf {
            let name = format!("arc-node-bridge-{}", env!("CARGO_PKG_VERSION"));
            layout.bridge_dir.join(name)
        }

        fn marker_path(layout: &Layout) -> PathBuf {
            layout.bridge_dir.join(ROLLED_BACK_MARKER)
        }

        fn staging_path(layout: &Layout) -> PathBuf {
            layout.root.join("bin").join(".arc-node.rollback")
        }

        #[test]
        fn preserve_previous_binary_keeps_genuine_v07_and_never_replaces_it() {
            let temp = TempDir::new("run-preserve-genuine");
            let layout = headless_layout(&temp);
            let mut log = Log::stderr_only();
            let previous = previous_path(&layout);
            let preserved = preserved_path(&layout);

            let original_v077 = write_fake_binary(&previous, "arc-node 0.7.7", 0);
            preserve_previous_binary(&layout, &mut log);
            assert_eq!(fs::read(&preserved).unwrap(), original_v077);

            write_fake_binary(&previous, &older_bridge_version_line(), 0);
            preserve_previous_binary(&layout, &mut log);
            assert_eq!(fs::read(&preserved).unwrap(), original_v077);

            write_fake_binary(&previous, "arc-node 0.7.11", 0);
            preserve_previous_binary(&layout, &mut log);
            assert_eq!(fs::read(&preserved).unwrap(), original_v077);
        }

        #[test]
        fn preserve_previous_binary_rejects_older_bridge_and_malformed_previous() {
            let temp = TempDir::new("run-preserve-reject");
            let layout = headless_layout(&temp);
            let log_path = layout.log_file();
            let mut log = Log::with_file(&log_path);
            let previous = previous_path(&layout);
            let preserved_dir = layout.bridge_dir.join(PRESERVED_DIR_NAME);
            let preserved = preserved_path(&layout);

            let older_bridge = older_bridge_version_line();
            let rejected = [
                (older_bridge.as_str(), 0),
                ("arc-node 0.8.27", 0),
                ("arc-node 0.7.7 extra", 0),
                ("arc-node 0.7.7", 1),
            ];
            for (output, status) in rejected {
                write_fake_binary(&previous, output, status);
                preserve_previous_binary(&layout, &mut log);
                assert!(!preserved.exists(), "{output} (status {status})");
                assert!(!preserved_dir.exists(), "{output} (status {status})");
            }
            let log_text = fs::read_to_string(&log_path).unwrap();
            assert!(log_text.contains("ARC legacy bridge"), "{log_text}");
        }

        #[test]
        fn rollback_refuses_older_bridge_without_replacing_launcher_or_writing_marker() {
            let temp = TempDir::new("run-rollback-refuse-bridge");
            let layout = headless_layout(&temp);
            let launcher_bytes = write_fake_binary(&layout.launcher, &version_line(), 0);
            let older_bridge = older_bridge_version_line();
            let previous = previous_path(&layout);
            write_fake_binary(&previous, &older_bridge, 0);

            let expected = CandidateBinaryVersion::LegacyBridge(ParsedBridgeVersion {
                version: older_bridge_version_string(),
                target_node: "0.8.13".to_string(),
            });
            assert_eq!(probe_candidate_version(&previous), Some(expected));

            let error = rollback(&layout.launcher, &layout.root).unwrap_err();
            assert_eq!(exit::code_for(&error), exit::EX_CONFIG);
            assert_eq!(fs::read(&layout.launcher).unwrap(), launcher_bytes);
            assert!(!marker_path(&layout).exists());
            assert!(!kept_launcher_path(&layout).exists());
            assert!(!staging_path(&layout).exists());

            write_fake_binary(&preserved_path(&layout), &older_bridge, 0);
            let error = rollback(&layout.launcher, &layout.root).unwrap_err();
            assert_eq!(exit::code_for(&error), exit::EX_CONFIG);
            assert_eq!(fs::read(&layout.launcher).unwrap(), launcher_bytes);
            assert!(!marker_path(&layout).exists());
            assert!(!kept_launcher_path(&layout).exists());
            assert!(!staging_path(&layout).exists());
        }

        #[test]
        fn rollback_restores_genuine_previous_fallback_when_preserved_is_missing() {
            let temp = TempDir::new("run-rollback-fallback-missing");
            let layout = headless_layout(&temp);
            let launcher_bytes = write_fake_binary(&layout.launcher, &version_line(), 0);
            let previous = previous_path(&layout);
            let v077_bytes = write_fake_binary(&previous, "arc-node 0.7.7", 0);

            let code = rollback(&layout.launcher, &layout.root).unwrap();
            assert_eq!(code, EX_OK);
            assert_eq!(fs::read(&layout.launcher).unwrap(), v077_bytes);
            let kept = kept_launcher_path(&layout);
            assert_eq!(fs::read(&kept).unwrap(), launcher_bytes);
            assert!(!staging_path(&layout).exists());
            let marker = marker_path(&layout);
            let contents = fs::read_to_string(&marker).unwrap();
            assert!(contents.contains("to arc-node 0.7.7 at "), "{contents}");
        }

        #[test]
        fn rollback_skips_bridge_in_preserved_and_restores_genuine_previous() {
            let temp = TempDir::new("run-rollback-fallback-bridge-preserved");
            let layout = headless_layout(&temp);
            let launcher_bytes = write_fake_binary(&layout.launcher, &version_line(), 0);
            let preserved = preserved_path(&layout);
            let previous = previous_path(&layout);
            write_fake_binary(&preserved, &older_bridge_version_line(), 0);
            let v0711_bytes = write_fake_binary(&previous, "arc-node 0.7.11", 0);

            let code = rollback(&layout.launcher, &layout.root).unwrap();
            assert_eq!(code, EX_OK);
            assert_eq!(fs::read(&layout.launcher).unwrap(), v0711_bytes);
            let kept = kept_launcher_path(&layout);
            assert_eq!(fs::read(&kept).unwrap(), launcher_bytes);
            assert!(!staging_path(&layout).exists());
            let marker = marker_path(&layout);
            let contents = fs::read_to_string(&marker).unwrap();
            assert!(contents.contains("to arc-node 0.7.11 at "), "{contents}");
        }

        #[test]
        fn rollback_prefers_preserved_original_over_bridge_previous() {
            let temp = TempDir::new("run-rollback-prefer-preserved");
            let layout = headless_layout(&temp);
            let mut log = Log::stderr_only();
            let launcher_bytes = write_fake_binary(&layout.launcher, &version_line(), 0);
            let previous = previous_path(&layout);
            let preserved = preserved_path(&layout);

            let v077_bytes = write_fake_binary(&previous, "arc-node 0.7.7", 0);
            preserve_previous_binary(&layout, &mut log);
            assert_eq!(fs::read(&preserved).unwrap(), v077_bytes);

            write_fake_binary(&previous, &older_bridge_version_line(), 0);
            preserve_previous_binary(&layout, &mut log);
            assert_eq!(fs::read(&preserved).unwrap(), v077_bytes);

            let code = rollback(&layout.launcher, &layout.root).unwrap();
            assert_eq!(code, EX_OK);
            assert_eq!(fs::read(&layout.launcher).unwrap(), v077_bytes);
            assert_eq!(fs::read(&preserved).unwrap(), v077_bytes);
            let kept = kept_launcher_path(&layout);
            assert_eq!(fs::read(&kept).unwrap(), launcher_bytes);
            assert!(!staging_path(&layout).exists());
            let marker = marker_path(&layout);
            let contents = fs::read_to_string(&marker).unwrap();
            assert!(contents.contains("to arc-node 0.7.7 at "), "{contents}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn rollback_is_refused_on_windows_without_touching_files() {
        use crate::layout::test_support::{TempDir, fake_arc_dir};

        let temp = TempDir::new("run-rollback-windows");
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);
        let before = fs::read(&launcher).unwrap();

        let error = rollback(&launcher, &root).unwrap_err();
        assert_eq!(exit::code_for(&error), exit::EX_CONFIG);
        assert_eq!(fs::read(&launcher).unwrap(), before);
        let marker = root.join(BRIDGE_DIR_NAME).join(ROLLED_BACK_MARKER);
        assert!(!marker.exists());
    }
}
