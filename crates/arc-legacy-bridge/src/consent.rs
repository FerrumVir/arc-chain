//! Whether the bridged node contributes compute. Off unless the operator says
//! otherwise.
//!
//! * Desktop: always off here. The v0.7 app cannot ask, so the updated ARC
//!   desktop app asks on first launch and records the answer itself.
//! * Headless: on only when the operator wrote `yes` to `compute-consent`
//!   (normally through `arc-node --legacy-bridge-compute on`) and a model
//!   record proves the exact pinned model bytes are on disk. Released v0.7
//!   has no consent flag or file to inherit: every v0.7 community installer
//!   hard-coded `--community-mode`, and `--model` named a file, not a choice
//!   about the new network.
//! * Never on a pin whose node build would publish the computer's hostname.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::argv::{LegacyInvocation, LegacyKind};
use crate::exit;
use crate::hashing::sha256_file_with_progress;
use crate::layout::{Layout, write_atomic};
use crate::pins::Pins;

pub const MODEL_RECORD_SCHEMA: &str = "arc.legacy-bridge.compute-model.v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compute {
    Off(String),
    On(PathBuf),
}

impl Compute {
    pub fn is_on(&self) -> bool {
        matches!(self, Compute::On(_))
    }

    pub fn describe(&self) -> String {
        match self {
            Compute::On(model) => format!("on, with the verified model {}", model.display()),
            Compute::Off(reason) => format!("off: {reason}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRecord {
    pub schema: String,
    pub path: String,
    pub size: u64,
    pub modified_unix_nanos: Option<u64>,
    pub sha256: String,
}

pub fn modified_nanos(metadata: &fs::Metadata) -> Option<u64> {
    let since_epoch = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since_epoch.as_nanos()).ok()
}

/// `Some(true)` for "yes", `Some(false)` for "no", `None` if never answered.
pub fn read_consent(layout: &Layout) -> Result<Option<bool>> {
    let path = layout.consent_file();
    match fs::read_to_string(&path) {
        Ok(text) => match text.trim() {
            "yes" => Ok(Some(true)),
            "no" => Ok(Some(false)),
            _ => Err(exit::refused(format!(
                "{} must contain exactly yes or no",
                path.display()
            ))),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn write_consent(layout: &Layout, yes: bool) -> Result<()> {
    let answer: &[u8] = if yes { b"yes\n" } else { b"no\n" };
    write_atomic(&layout.consent_file(), answer)
}

/// The recorded model, if it still is exactly the bytes that were verified.
pub fn verified_model(layout: &Layout, pins: &Pins) -> Result<Option<PathBuf>> {
    let path = layout.model_record();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let Ok(record) = serde_json::from_str::<ModelRecord>(&text) else {
        return Ok(None);
    };
    if record.schema != MODEL_RECORD_SCHEMA
        || record.sha256 != pins.model.sha256
        || record.size != pins.model.size
    {
        return Ok(None);
    }
    let model = PathBuf::from(&record.path);
    let Ok(metadata) = fs::metadata(&model) else {
        return Ok(None);
    };
    if !metadata.is_file()
        || metadata.len() != record.size
        || modified_nanos(&metadata) != record.modified_unix_nanos
    {
        return Ok(None);
    }
    Ok(Some(model))
}

/// Hash `model` and, if it is exactly the pinned model, record it.
pub fn verify_and_record_model(
    layout: &Layout,
    pins: &Pins,
    model: &Path,
    progress: impl FnMut(u64),
) -> Result<()> {
    let model = crate::layout::canonical_or_lexical(model);
    let metadata =
        fs::metadata(&model).with_context(|| format!("cannot read {}", model.display()))?;
    if !metadata.is_file() || metadata.len() != pins.model.size {
        return Err(exit::refused(format!(
            "{} is not the {} ({} bytes) the network runs",
            model.display(),
            pins.model.file_name,
            pins.model.size
        )));
    }
    let digest = sha256_file_with_progress(&model, progress)
        .with_context(|| format!("cannot hash {}", model.display()))?;
    if digest != pins.model.sha256 {
        return Err(exit::refused(format!(
            "{} has SHA-256 {digest}, not the pinned {}",
            model.display(),
            pins.model.sha256
        )));
    }
    let record = ModelRecord {
        schema: MODEL_RECORD_SCHEMA.to_string(),
        path: model.to_string_lossy().into_owned(),
        size: metadata.len(),
        modified_unix_nanos: modified_nanos(&metadata),
        sha256: digest,
    };
    write_atomic(&layout.model_record(), &serde_json::to_vec_pretty(&record)?)
}

pub fn decide(layout: &Layout, invocation: &LegacyInvocation, pins: &Pins) -> Result<Compute> {
    if invocation.kind == LegacyKind::Desktop {
        return Ok(Compute::Off(
            "the updated ARC desktop app asks whether this computer keeps contributing compute"
                .to_string(),
        ));
    }
    if !pins.node_release.worker_names_privacy_safe {
        return Ok(Compute::Off(format!(
            "arc-node {} would publish this computer's hostname when it registers, so community registration and compute stay off until a bridge release pins a privacy-safe build",
            pins.node_release.version
        )));
    }
    match read_consent(layout)? {
        None => Ok(Compute::Off("no compute consent has been recorded".to_string())),
        Some(false) => Ok(Compute::Off(
            "the operator turned compute contribution off".to_string(),
        )),
        Some(true) => match verified_model(layout, pins)? {
            Some(model) => Ok(Compute::On(model)),
            None => Ok(Compute::Off(
                "consent is recorded but no verified model is; run --legacy-bridge-compute on again".to_string(),
            )),
        },
    }
}

/// The one next step to print after the node starts.
pub fn next_step(
    layout: &Layout,
    invocation: &LegacyInvocation,
    pins: &Pins,
    compute: &Compute,
) -> String {
    match (invocation.kind, compute) {
        (LegacyKind::Desktop, _) => {
            "Open ARC Node, then Settings > Check for updates > Install, and answer \"Keep contributing compute?\".".to_string()
        }
        (LegacyKind::Headless, Compute::On(_)) => format!(
            "Compute is on. To stop contributing: {} --legacy-bridge-compute off, then restart the node.",
            layout.launcher.display()
        ),
        (LegacyKind::Headless, Compute::Off(_)) if !pins.node_release.worker_names_privacy_safe => {
            "Nothing to do now: the next bridge release enables community registration, and then you can opt in to compute.".to_string()
        }
        (LegacyKind::Headless, Compute::Off(_)) => {
            // The v0.7 no-root fallback passed its model path with literal
            // quote characters.
            let model = match &invocation.model {
                Some(model) => format!(" --model {}", model.display().to_string().trim_matches('"')),
                None => " --download-model".to_string(),
            };
            // Operator commands are recognized only as the first argument.
            format!(
                "To contribute compute: {} --legacy-bridge-compute on{model}, then restart the node.",
                layout.launcher.display()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::test_support::{TempDir, fake_arc_dir};
    use crate::layout::{prepare, resolve};
    use std::net::SocketAddr;

    fn headless(temp: &TempDir, model: Option<PathBuf>) -> (Layout, LegacyInvocation) {
        let root = temp.path().join(".arc");
        let launcher = fake_arc_dir(&root);
        fs::create_dir_all(root.join("data")).unwrap();
        let invocation = LegacyInvocation {
            kind: if cfg!(windows) {
                LegacyKind::Desktop
            } else {
                LegacyKind::Headless
            },
            rpc: "0.0.0.0:9944".parse::<SocketAddr>().unwrap(),
            p2p_port: 9945,
            data_dir: if cfg!(windows) {
                root.clone()
            } else {
                root.join("data")
            },
            seeds_file: None,
            genesis: None,
            model,
            community_mode: true,
        };
        let layout = resolve(&launcher, &invocation, temp.path()).unwrap();
        prepare(&layout).unwrap();
        (layout, invocation)
    }

    fn privacy_safe_pins(model_body: &[u8]) -> Pins {
        let mut pins = Pins::embedded().unwrap();
        pins.node_release.worker_names_privacy_safe = true;
        pins.model.sha256 = crate::hashing::sha256_bytes(model_body);
        pins.model.size = model_body.len() as u64;
        pins
    }

    #[test]
    fn desktop_never_turns_compute_on() {
        let temp = TempDir::new("consent-desktop");
        let (layout, mut invocation) = headless(&temp, None);
        invocation.kind = LegacyKind::Desktop;
        let pins = privacy_safe_pins(b"model");
        write_consent(&layout, true).unwrap();
        assert!(!decide(&layout, &invocation, &pins).unwrap().is_on());
    }

    #[cfg(unix)]
    #[test]
    fn headless_compute_needs_consent_a_verified_model_and_a_privacy_safe_pin() {
        let temp = TempDir::new("consent-headless");
        let model_body = b"pretend these are the exact pinned model bytes".to_vec();
        let model = temp.path().join("llama.gguf");
        fs::write(&model, &model_body).unwrap();
        let (layout, invocation) = headless(&temp, Some(model.clone()));
        let pins = privacy_safe_pins(&model_body);

        assert!(
            !decide(&layout, &invocation, &pins).unwrap().is_on(),
            "no consent yet"
        );
        write_consent(&layout, true).unwrap();
        assert!(
            !decide(&layout, &invocation, &pins).unwrap().is_on(),
            "no verified model yet"
        );
        verify_and_record_model(&layout, &pins, &model, |_| {}).unwrap();
        assert!(decide(&layout, &invocation, &pins).unwrap().is_on());

        let mut unsafe_pins = pins.clone();
        unsafe_pins.node_release.worker_names_privacy_safe = false;
        assert!(!decide(&layout, &invocation, &unsafe_pins).unwrap().is_on());

        write_consent(&layout, false).unwrap();
        assert!(!decide(&layout, &invocation, &pins).unwrap().is_on());
        write_consent(&layout, true).unwrap();
        // Replacing the model file invalidates the record.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&model, b"other bytes with a different length").unwrap();
        assert!(!decide(&layout, &invocation, &pins).unwrap().is_on());
    }

    #[cfg(unix)]
    #[test]
    fn a_wrong_model_is_never_recorded() {
        let temp = TempDir::new("consent-wrong-model");
        let model = temp.path().join("other.gguf");
        fs::write(&model, b"same length, different bytes!!").unwrap();
        let (layout, _) = headless(&temp, None);
        let pins = privacy_safe_pins(b"same length, different bytes??");
        let error = verify_and_record_model(&layout, &pins, &model, |_| {}).unwrap_err();
        assert_eq!(exit::code_for(&error), exit::EX_CONFIG);
        assert!(!layout.model_record().exists());
    }

    /// Every command line a released v0.6.0..=v0.7.11 supervisor starts
    /// `arc-node` with (`tests/legacy-bridge/check_v07_fixtures.py` checks
    /// them against each tag).
    const V07_ARGV: &str = include_str!("../../../tests/legacy-bridge/fixtures/v07-argv.json");

    fn args_of(shape: &serde_json::Value) -> Vec<std::ffi::OsString> {
        shape["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| std::ffi::OsString::from(arg.as_str().unwrap()))
            .collect()
    }

    fn release_in(layout: &Layout) -> crate::release::VerifiedRelease {
        let dir = layout.releases_dir().join("pinned");
        crate::release::VerifiedRelease {
            node: dir.join("arc-node"),
            cli: dir.join("arc-cli"),
            genesis: dir.join("genesis.toml"),
            seeds: dir.join("testnet-seeds.txt"),
            dir,
        }
    }

    fn launch_args(
        layout: &Layout,
        invocation: &LegacyInvocation,
        pins: &Pins,
        compute: &Compute,
    ) -> Vec<String> {
        crate::launch::plan(layout, invocation, pins, &release_in(layout), compute)
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn every_released_v07_command_line_bridges_with_compute_off() {
        let fixture: serde_json::Value = serde_json::from_str(V07_ARGV).unwrap();
        let shapes = fixture["shapes"].as_array().unwrap();
        assert!(shapes.len() >= 19);
        let mut bridged = 0;
        for shape in shapes {
            let name = shape["name"].as_str().unwrap();
            let expect = &shape["expect"];
            let parsed = crate::argv::parse(&args_of(shape));
            if let Some(error) = expect["error"].as_str() {
                // Refused before anything starts: no node, so no compute.
                assert_eq!(format!("{:?}", parsed.unwrap_err()), error, "{name}");
                continue;
            }
            let parsed = parsed.unwrap_or_else(|error| panic!("{name}: {error}"));
            let kind = LegacyKind::from_label(expect["kind"].as_str().unwrap()).unwrap();
            assert_eq!(parsed.kind, kind, "{name}");
            assert_eq!(
                parsed.community_mode,
                expect["community_mode"].as_bool().unwrap(),
                "{name}"
            );
            assert_eq!(
                parsed.model.is_some(),
                expect["model"].as_bool().unwrap(),
                "{name}"
            );
            if expect["layout_refused"].as_bool() == Some(true) {
                let temp = TempDir::new("consent-v07-layout");
                let launcher = temp.path().join(shape["launcher"].as_str().unwrap());
                fs::create_dir_all(launcher.parent().unwrap()).unwrap();
                fs::write(&launcher, b"").unwrap();
                assert!(
                    crate::layout::resolve(&launcher, &parsed, temp.path()).is_err(),
                    "{name}"
                );
                continue;
            }
            if cfg!(windows) && kind == LegacyKind::Headless {
                // Released v0.7 had no Windows headless installer; the layout
                // refuses it before anything starts.
                continue;
            }

            // Run it through the bridge as installed under a temporary root.
            let temp = TempDir::new("consent-v07-argv");
            let root = temp.path().join(".arc");
            let launcher = fake_arc_dir(&root);
            let data_dir = match kind {
                LegacyKind::Desktop => root.clone(),
                LegacyKind::Headless => root.join("data"),
            };
            fs::create_dir_all(&data_dir).unwrap();
            let invocation = LegacyInvocation { data_dir, ..parsed };
            let layout = resolve(&launcher, &invocation, temp.path()).unwrap();
            prepare(&layout).unwrap();
            let model_body = b"exact pinned model bytes".to_vec();
            let model = temp.path().join("pinned.gguf");
            fs::write(&model, &model_body).unwrap();

            // Today's pin and a privacy-safe one: off without consent, even
            // when v0.7 passed --model and --community-mode.
            for pins in [Pins::embedded().unwrap(), privacy_safe_pins(&model_body)] {
                let compute = decide(&layout, &invocation, &pins).unwrap();
                assert!(!compute.is_on(), "{name}: {}", compute.describe());
                let args = launch_args(&layout, &invocation, &pins, &compute);
                for flag in ["--model", "--full-integer-worker"] {
                    assert!(!args.contains(&flag.to_string()), "{name}: {flag}");
                }
                let stake = args.iter().position(|arg| arg == "--stake").unwrap();
                assert_eq!(args[stake + 1], "0", "{name}");
                assert!(
                    !args.iter().any(|arg| arg.contains("validator-seed")),
                    "{name}"
                );
            }

            // Only the owner's own opt-in turns it on, and never under the
            // v0.7 desktop app, which cannot ask.
            let pins = privacy_safe_pins(&model_body);
            write_consent(&layout, true).unwrap();
            assert!(
                !decide(&layout, &invocation, &pins).unwrap().is_on(),
                "{name}"
            );
            verify_and_record_model(&layout, &pins, &model, |_| {}).unwrap();
            let compute = decide(&layout, &invocation, &pins).unwrap();
            assert_eq!(compute.is_on(), kind == LegacyKind::Headless, "{name}");
            if compute.is_on() {
                let args = launch_args(&layout, &invocation, &pins, &compute);
                assert!(
                    args.contains(&"--full-integer-worker".to_string()),
                    "{name}"
                );
            }
            write_consent(&layout, false).unwrap();
            assert!(
                !decide(&layout, &invocation, &pins).unwrap().is_on(),
                "{name}"
            );
            bridged += 1;
        }
        assert!(bridged >= if cfg!(windows) { 5 } else { 13 });
    }

    #[test]
    fn the_compute_hint_is_a_command_the_launcher_accepts() {
        let temp = TempDir::new("consent-hint");
        let quoted = PathBuf::from("\"/home/ops/.arc/model.gguf\"");
        let (layout, invocation) = headless(&temp, Some(quoted));
        if invocation.kind != LegacyKind::Headless {
            return;
        }
        let mut pins = Pins::embedded().unwrap();
        pins.node_release.worker_names_privacy_safe = true;
        let hint = next_step(&layout, &invocation, &pins, &Compute::Off("none".into()));
        let command = format!(
            "{} --legacy-bridge-compute on --model /home/ops/.arc/model.gguf,",
            layout.launcher.display()
        );
        assert!(hint.contains(&command), "{hint}");
    }

    #[test]
    fn consent_file_must_be_yes_or_no() {
        let temp = TempDir::new("consent-file");
        let (layout, _) = headless(&temp, None);
        assert_eq!(read_consent(&layout).unwrap(), None);
        fs::write(layout.consent_file(), b"maybe\n").unwrap();
        assert!(read_consent(&layout).is_err());
        fs::write(layout.consent_file(), b"yes").unwrap();
        assert_eq!(read_consent(&layout).unwrap(), Some(true));
    }
}
