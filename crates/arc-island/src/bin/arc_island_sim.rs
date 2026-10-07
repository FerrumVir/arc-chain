//! `arc-island-sim`: the offline Kimi regional swarm capacity simulator.
//!
//! ```text
//! arc-island-sim [--seed N] [--nodes 100,130,1000,10000] [--out DIR] [--legacy]
//! ```
//!
//! Writes `kimi-island-capacity.md` and `kimi-island-capacity.json` to DIR
//! (default: the current directory) and prints the markdown. Offline: no
//! network access, no node, no keys.

use arc_island::model::ModelSpec;
use arc_island::sim::{markdown, regional_scenarios, run, standard_scenarios};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut seed = 7u64;
    let mut nodes = vec![100usize, 130, 1_000, 10_000];
    let mut out = PathBuf::from(".");
    let mut legacy = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--legacy" {
            legacy = true;
            continue;
        }
        let value = args.next();
        let parsed = match (arg.as_str(), value) {
            ("--seed", Some(v)) => v.parse().map(|s| seed = s).is_ok(),
            ("--nodes", Some(v)) => v
                .split(',')
                .map(str::parse)
                .collect::<Result<Vec<usize>, _>>()
                .map(|n| nodes = n)
                .is_ok(),
            ("--out", Some(v)) => {
                out = PathBuf::from(v);
                true
            }
            _ => false,
        };
        if !parsed {
            eprintln!(
                "usage: arc-island-sim [--seed N] [--nodes 100,130,1000,10000] [--out DIR] [--legacy]"
            );
            return ExitCode::from(2);
        }
    }

    let model = ModelSpec::kimi_k26_int4();
    let scenarios = if legacy {
        standard_scenarios(seed, &nodes)
    } else {
        regional_scenarios(seed, &nodes)
    };
    let reports: Vec<_> = scenarios.iter().map(|s| run(s, &model)).collect();
    let md = markdown(&reports, seed);
    let json = match serde_json::to_string_pretty(&reports) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("serialising the report: {e}");
            return ExitCode::FAILURE;
        }
    };
    for (name, body) in [
        ("kimi-island-capacity.md", &md),
        ("kimi-island-capacity.json", &json),
    ] {
        if let Err(e) = std::fs::write(out.join(name), body) {
            eprintln!("writing {name}: {e}");
            return ExitCode::FAILURE;
        }
    }
    print!("{md}");
    ExitCode::SUCCESS
}
