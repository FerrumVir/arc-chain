//! Recognize the exact command lines released v0.7 supervisors start
//! `arc-node` with, and nothing else.
//!
//! * Desktop, every v0.6.0..=v0.7.11 app (`node_manager.rs` `start`):
//!   `--rpc 127.0.0.1:<p> --p2p-port <q> --data-dir <~/.arc> --validator-seed <phrase>
//!   --eth-rpc-port 0 [--seeds-file <f>] [--genesis <g>] [--community-mode] [--model <m>]`.
//!   It never passes `--stake`; v0.7's default stake was 5,000,000, so these
//!   nodes ran as accidental validators. The bridge always runs them at 0.
//! * Headless, `scripts/install-community-node.sh` (launchd, systemd, and the
//!   no-root and `--no-service` fallbacks):
//!   `--rpc 0.0.0.0:<p> --p2p-port <q> --seeds-file <f> --genesis <g>
//!   --validator-seed <seed> --stake 0 --min-stake 0 --eth-rpc-port 0
//!   --data-dir <ARC_DIR>/data [--model <m>] [--community-mode]`.
//!
//! An explicit non-zero `--stake` is an operator-configured validator and is
//! always refused. The `--validator-seed` value is consumed and dropped; it is
//! never stored, compared, or printed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyKind {
    Desktop,
    Headless,
}

impl LegacyKind {
    pub fn label(self) -> &'static str {
        match self {
            LegacyKind::Desktop => "desktop",
            LegacyKind::Headless => "headless",
        }
    }

    pub fn from_label(label: &str) -> Option<LegacyKind> {
        match label {
            "desktop" => Some(LegacyKind::Desktop),
            "headless" => Some(LegacyKind::Headless),
            _ => None,
        }
    }
}

/// A recognized v0.7 invocation. It deliberately has no seed field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyInvocation {
    pub kind: LegacyKind,
    pub rpc: SocketAddr,
    pub p2p_port: u16,
    pub data_dir: PathBuf,
    pub seeds_file: Option<PathBuf>,
    pub genesis: Option<PathBuf>,
    pub model: Option<PathBuf>,
    pub community_mode: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgvError {
    NotUtf8,
    UnknownArgument(String),
    MissingValue(&'static str),
    Duplicate(&'static str),
    InvalidValue(&'static str),
    Missing(&'static str),
    ExplicitValidatorStake,
    UnsupportedShape(&'static str),
}

impl fmt::Display for ArgvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgvError::NotUtf8 => write!(f, "an argument is not valid UTF-8"),
            ArgvError::UnknownArgument(name) => write!(f, "unrecognized argument {name}"),
            ArgvError::MissingValue(name) => write!(f, "{name} has no value"),
            ArgvError::Duplicate(name) => write!(f, "{name} is given more than once"),
            ArgvError::InvalidValue(name) => write!(f, "{name} has an invalid value"),
            ArgvError::Missing(name) => write!(f, "the v0.7 command line has no {name}"),
            ArgvError::ExplicitValidatorStake => write!(
                f,
                "this node was started with a non-zero --stake; validator nodes are never bridged"
            ),
            ArgvError::UnsupportedShape(detail) => {
                write!(f, "unrecognized v0.7 command line: {detail}")
            }
        }
    }
}

impl std::error::Error for ArgvError {}

const SEED_FLAG: &str = "--validator-seed";

const VALUE_FLAGS: &[&str] = &[
    "--rpc",
    "--p2p-port",
    "--data-dir",
    SEED_FLAG,
    "--eth-rpc-port",
    "--seeds-file",
    "--genesis",
    "--stake",
    "--min-stake",
    "--model",
];

const COMMUNITY_FLAG: &str = "--community-mode";

/// Parse the arguments after the program name.
pub fn parse(args: &[OsString]) -> Result<LegacyInvocation, ArgvError> {
    let mut values: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut community_mode = false;
    let mut index = 0;
    while index < args.len() {
        let raw = args[index].to_str().ok_or(ArgvError::NotUtf8)?;
        index += 1;
        let (flag, inline) = match raw.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (raw, None),
        };
        if flag == COMMUNITY_FLAG {
            if inline.is_some() {
                return Err(ArgvError::InvalidValue(COMMUNITY_FLAG));
            }
            if community_mode {
                return Err(ArgvError::Duplicate(COMMUNITY_FLAG));
            }
            community_mode = true;
            continue;
        }
        let Some(known) = VALUE_FLAGS.iter().copied().find(|name| *name == flag) else {
            return Err(ArgvError::UnknownArgument(describe_unknown(flag)));
        };
        let value = match inline {
            Some(value) => value.to_string(),
            None => {
                let next = args.get(index).ok_or(ArgvError::MissingValue(known))?;
                index += 1;
                next.to_str().ok_or(ArgvError::NotUtf8)?.to_string()
            }
        };
        // The legacy seed is consumed and dropped right here.
        let stored = if known == SEED_FLAG {
            String::new()
        } else {
            value
        };
        if values.insert(known, stored).is_some() {
            return Err(ArgvError::Duplicate(known));
        }
    }
    classify(&values, community_mode)
}

fn classify(
    values: &BTreeMap<&'static str, String>,
    community_mode: bool,
) -> Result<LegacyInvocation, ArgvError> {
    let get = |name: &str| values.get(name).map(String::as_str);
    if !values.contains_key(SEED_FLAG) {
        return Err(ArgvError::Missing(SEED_FLAG));
    }
    let data_dir = get("--data-dir")
        .filter(|value| !value.is_empty())
        .ok_or(ArgvError::Missing("--data-dir"))?;
    let rpc: SocketAddr = get("--rpc")
        .ok_or(ArgvError::Missing("--rpc"))?
        .parse()
        .map_err(|_| ArgvError::InvalidValue("--rpc"))?;
    let p2p_port: u16 = get("--p2p-port")
        .ok_or(ArgvError::Missing("--p2p-port"))?
        .parse()
        .map_err(|_| ArgvError::InvalidValue("--p2p-port"))?;
    if rpc.port() == 0 || p2p_port == 0 {
        return Err(ArgvError::UnsupportedShape("ports must be non-zero"));
    }
    if get("--eth-rpc-port").is_some_and(|eth| eth != "0") {
        return Err(ArgvError::UnsupportedShape("--eth-rpc-port must be 0"));
    }

    let kind = match (get("--stake"), get("--min-stake")) {
        (None, None) => LegacyKind::Desktop,
        (Some("0"), Some("0")) => LegacyKind::Headless,
        (Some(stake), _) if stake != "0" => return Err(ArgvError::ExplicitValidatorStake),
        _ => {
            return Err(ArgvError::UnsupportedShape(
                "--stake 0 must come with --min-stake 0",
            ));
        }
    };
    if kind == LegacyKind::Desktop {
        if !rpc.ip().is_loopback() {
            return Err(ArgvError::UnsupportedShape(
                "a desktop-managed node binds its RPC to 127.0.0.1",
            ));
        }
        if get("--eth-rpc-port").is_none() {
            return Err(ArgvError::Missing("--eth-rpc-port"));
        }
    } else {
        if get("--seeds-file").is_none() {
            return Err(ArgvError::Missing("--seeds-file"));
        }
        if get("--genesis").is_none() {
            return Err(ArgvError::Missing("--genesis"));
        }
    }

    Ok(LegacyInvocation {
        kind,
        rpc,
        p2p_port,
        data_dir: PathBuf::from(data_dir),
        seeds_file: get("--seeds-file").map(PathBuf::from),
        genesis: get("--genesis").map(PathBuf::from),
        // The v0.7 `--no-service` path passed `--model ""` without a model.
        model: get("--model")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        community_mode,
    })
}

/// Name an unknown argument without echoing anything that might be a secret.
fn describe_unknown(flag: &str) -> String {
    let plain_flag = flag.starts_with("--")
        && flag.len() <= 40
        && flag[2..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if plain_flag {
        flag.to_string()
    } else {
        "<a positional value>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str =
        "sunset quantum orbit rally window cabin echo velvet amber planet hazard tide";

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// Exact argv from v0.7.11 desktop `node_manager.rs` for a worker.
    fn desktop_worker() -> Vec<OsString> {
        os(&[
            "--rpc",
            "127.0.0.1:9944",
            "--p2p-port",
            "9945",
            "--data-dir",
            "/Users/ada/.arc",
            "--validator-seed",
            SEED,
            "--eth-rpc-port",
            "0",
            "--seeds-file",
            "/Applications/ARC Node.app/Contents/Resources/resources/testnet-seeds.txt",
            "--genesis",
            "/Applications/ARC Node.app/Contents/Resources/resources/genesis.toml",
            "--community-mode",
            "--model",
            "/Users/ada/.arc/models/llama2-7b.gguf",
        ])
    }

    /// Exact argv from the v0.7.11 community installer's launchd plist.
    fn headless_launchd() -> Vec<OsString> {
        os(&[
            "--rpc",
            "0.0.0.0:9944",
            "--p2p-port",
            "9945",
            "--seeds-file",
            "/home/ops/.arc/seeds.txt",
            "--genesis",
            "/home/ops/.arc/genesis.toml",
            "--validator-seed",
            "community-vps1-0a1b2c3d",
            "--stake",
            "0",
            "--min-stake",
            "0",
            "--eth-rpc-port",
            "0",
            "--data-dir",
            "/home/ops/.arc/data",
            "--community-mode",
        ])
    }

    #[test]
    fn desktop_worker_invocation_is_recognized_without_the_seed() {
        let invocation = parse(&desktop_worker()).unwrap();
        assert_eq!(invocation.kind, LegacyKind::Desktop);
        assert_eq!(invocation.rpc.port(), 9944);
        assert_eq!(invocation.p2p_port, 9945);
        assert_eq!(invocation.data_dir, PathBuf::from("/Users/ada/.arc"));
        assert!(invocation.community_mode);
        assert!(invocation.model.is_some());
        let debug = format!("{invocation:?}");
        assert!(!debug.contains("sunset"), "the seed must not be retained");
    }

    #[test]
    fn desktop_observer_without_bundled_resources_is_recognized() {
        let args = os(&[
            "--rpc",
            "127.0.0.1:9954",
            "--p2p-port",
            "9955",
            "--data-dir",
            "C:\\Users\\ada\\.arc",
            "--validator-seed",
            SEED,
            "--eth-rpc-port",
            "0",
        ]);
        let invocation = parse(&args).unwrap();
        assert_eq!(invocation.kind, LegacyKind::Desktop);
        assert!(!invocation.community_mode);
        assert_eq!(invocation.model, None);
    }

    #[test]
    fn headless_installer_invocations_are_recognized() {
        let invocation = parse(&headless_launchd()).unwrap();
        assert_eq!(invocation.kind, LegacyKind::Headless);
        assert!(invocation.community_mode);
        assert_eq!(invocation.data_dir, PathBuf::from("/home/ops/.arc/data"));

        // The v0.7.11 no-root Linux fallback omitted --community-mode.
        let mut no_root = headless_launchd();
        no_root.pop();
        assert!(!parse(&no_root).unwrap().community_mode);

        // The --no-service fallback passed `--model ""` without a model.
        let mut no_service = headless_launchd();
        no_service.pop();
        no_service.extend(os(&["--model", ""]));
        assert_eq!(parse(&no_service).unwrap().model, None);
    }

    #[test]
    fn equals_form_is_accepted() {
        let args = os(&[
            "--rpc=127.0.0.1:9944",
            "--p2p-port=9945",
            "--data-dir=/Users/ada/.arc",
            "--validator-seed=hidden words",
            "--eth-rpc-port=0",
        ]);
        assert_eq!(parse(&args).unwrap().kind, LegacyKind::Desktop);
    }

    #[test]
    fn explicit_validator_stake_is_refused() {
        let mut args = headless_launchd();
        let stake = args.iter().position(|arg| arg == "--stake").unwrap();
        args[stake + 1] = OsString::from("5000000");
        assert_eq!(parse(&args), Err(ArgvError::ExplicitValidatorStake));

        let mut desktop = desktop_worker();
        desktop.extend(os(&["--stake", "500000"]));
        assert_eq!(parse(&desktop), Err(ArgvError::ExplicitValidatorStake));
    }

    #[test]
    fn unknown_or_ambiguous_shapes_are_refused_without_echoing_values() {
        let mut args = headless_launchd();
        args.extend(os(&["--shard-range", "0:8"]));
        assert_eq!(
            parse(&args),
            Err(ArgvError::UnknownArgument("--shard-range".to_string()))
        );

        let mut stray = desktop_worker();
        stray.push(OsString::from("secret words that are not a flag"));
        let error = parse(&stray).unwrap_err().to_string();
        assert!(!error.contains("secret"), "{error}");

        let mut duplicate = desktop_worker();
        duplicate.extend(os(&["--data-dir", "/tmp/other"]));
        assert_eq!(parse(&duplicate), Err(ArgvError::Duplicate("--data-dir")));

        let mut half_stake = headless_launchd();
        let min = half_stake
            .iter()
            .position(|arg| arg == "--min-stake")
            .unwrap();
        half_stake.remove(min);
        half_stake.remove(min);
        assert!(matches!(
            parse(&half_stake),
            Err(ArgvError::UnsupportedShape(_))
        ));

        let mut public_desktop = desktop_worker();
        public_desktop[1] = OsString::from("0.0.0.0:9944");
        assert!(matches!(
            parse(&public_desktop),
            Err(ArgvError::UnsupportedShape(_))
        ));

        assert_eq!(parse(&[]), Err(ArgvError::Missing(SEED_FLAG)));
        assert_eq!(
            parse(&os(&["--validator-seed"])),
            Err(ArgvError::MissingValue(SEED_FLAG))
        );
    }
}
