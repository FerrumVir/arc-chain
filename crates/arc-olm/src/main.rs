//! arc-olm CLI — run the beam search on ARC-AGI tasks.
//!
//! Usage:
//!   cargo run -p arc-olm --release -- eval /path/to/arc-agi-1/evaluation --max-tasks 50
//!   cargo run -p arc-olm --release -- solve /path/to/task.json

use arc_olm::search::beam::BeamConfig;
use arc_olm::search::eval::{evaluate_directory, solve_task};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        eprintln!("Usage:");
        eprintln!("  arc-olm eval <task_dir> [--max-tasks N] [--beam-width N] [--max-depth N] [--timeout-ms N]");
        eprintln!("  arc-olm solve <task.json>");
        std::process::exit(1);
    }

    let command = &args[1];
    let path = &args[2];

    let mut config = BeamConfig {
        beam_width: 200,
        max_depth: 6,
        timeout_ms: 30_000,
    };
    let mut max_tasks: Option<usize> = None;

    // Parse optional args
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--max-tasks" => { max_tasks = args.get(i + 1).and_then(|s| s.parse().ok()); i += 2; }
            "--beam-width" => { config.beam_width = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(200); i += 2; }
            "--max-depth" => { config.max_depth = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(6); i += 2; }
            "--timeout-ms" => { config.timeout_ms = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(30_000); i += 2; }
            _ => { i += 1; }
        }
    }

    match command.as_str() {
        "eval" => {
            println!("OLM Beam Search Evaluation");
            println!("  Path: {}", path);
            println!("  Beam width: {}", config.beam_width);
            println!("  Max depth: {}", config.max_depth);
            println!("  Timeout: {}ms per task", config.timeout_ms);
            if let Some(max) = max_tasks {
                println!("  Max tasks: {}", max);
            }
            println!();

            evaluate_directory(Path::new(path), &config, max_tasks);
        }
        "solve" => {
            let result = solve_task(Path::new(path), &config);
            if result.solved {
                println!("SOLVED: {} (depth={}, {}ms)", result.task_id, result.depth, result.time_ms);
                println!("Program: {:?}", result.program);
            } else {
                println!("UNSOLVED: {} ({}ms)", result.task_id, result.time_ms);
            }
        }
        _ => {
            eprintln!("Unknown command: {}", command);
            std::process::exit(1);
        }
    }
}
