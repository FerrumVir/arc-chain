//! arc-olm CLI — multi-engine ARC-AGI solver.
//!
//! Usage:
//!   arc-olm eval /path/to/arc-agi/training [--max-tasks N] [--timeout-ms N]
//!   arc-olm solve /path/to/task.json

use arc_olm::search::solver::{solve, SolverConfig};
use std::path::Path;
use serde::Deserialize;

#[derive(Deserialize)]
struct TaskPair { input: Vec<Vec<u8>>, output: Vec<Vec<u8>> }
#[derive(Deserialize)]
struct TaskData { train: Vec<TaskPair>, test: Vec<TaskPair> }

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage:");
        eprintln!("  arc-olm eval <task_dir> [--max-tasks N] [--timeout-ms N] [--verbose]");
        eprintln!("  arc-olm solve <task.json>");
        std::process::exit(1);
    }

    let command = &args[1];
    let path = &args[2];
    let mut config = SolverConfig::default();
    let mut max_tasks: Option<usize> = None;

    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--max-tasks" => { max_tasks = args.get(i+1).and_then(|s| s.parse().ok()); i += 2; }
            "--timeout-ms" => { config.timeout_ms = args.get(i+1).and_then(|s| s.parse().ok()).unwrap_or(30_000); i += 2; }
            "--beam-width" => { config.beam_width = args.get(i+1).and_then(|s| s.parse().ok()).unwrap_or(200); i += 2; }
            "--max-depth" => { config.max_depth = args.get(i+1).and_then(|s| s.parse().ok()).unwrap_or(6); i += 2; }
            "--no-aug" => { config.use_augmentation = false; i += 1; }
            "--no-evo" => { config.use_evolution = false; i += 1; }
            "--verbose" | "-v" => { config.verbose = true; i += 1; }
            "--model" => {
                if let Some(path) = args.get(i+1) {
                    match arc_olm::search::steering::SteeringModel::load(std::path::Path::new(path)) {
                        Some(m) => {
                            config.steering_model = Some(std::sync::Arc::new(m));
                            eprintln!("Loaded steering model from {}", path);
                        }
                        None => eprintln!("WARNING: Failed to load steering model from {}", path),
                    }
                }
                i += 2;
            }
            _ => { i += 1; }
        }
    }

    match command.as_str() {
        "eval" => run_eval(path, &config, max_tasks),
        "solve" => run_solve(path, &config),
        _ => { eprintln!("Unknown command: {command}"); std::process::exit(1); }
    }
}

fn run_eval(dir: &str, config: &SolverConfig, max_tasks: Option<usize>) {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("Cannot read directory")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "json"))
        .collect();
    paths.sort();
    if let Some(max) = max_tasks { paths.truncate(max); }

    let total = paths.len();
    let mut solved = 0;
    let mut total_ms = 0u64;
    let mut engines: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();

    println!("ARC-OLM Multi-Engine Solver");
    println!("  Tasks: {}", total);
    println!("  Timeout: {}ms", config.timeout_ms);
    println!("  Engines: synth + beam + aug({}) + evo({})",
        if config.use_augmentation { "on" } else { "off" },
        if config.use_evolution { "on" } else { "off" });
    println!();

    for (i, path) in paths.iter().enumerate() {
        let task_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        let data: TaskData = match std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(d) => d,
            None => { println!("[{:3}/{}] {}: SKIP (parse error)", i+1, total, task_id); continue; }
        };

        let train_pairs: Vec<_> = data.train.iter().map(|p| (p.input.clone(), p.output.clone())).collect();
        let test_inputs: Vec<_> = data.test.iter().map(|p| p.input.clone()).collect();

        let start = std::time::Instant::now();
        let result = solve(&train_pairs, &test_inputs, config);
        let elapsed = start.elapsed().as_millis() as u64;
        total_ms += elapsed;

        match result {
            Some(r) => {
                let correct = !data.test.is_empty()
                    && !r.test_outputs.is_empty()
                    && r.test_outputs[0] == data.test[0].output;
                if correct {
                    solved += 1;
                    *engines.entry(r.engine).or_insert(0) += 1;
                    println!("[{:3}/{}] {}: OK  ({}, {}ms)", i+1, total, task_id, r.engine, elapsed);
                } else {
                    println!("[{:3}/{}] {}: WRONG ({}, {}ms)", i+1, total, task_id, r.engine, elapsed);
                }
            }
            None => {
                println!("[{:3}/{}] {}: --  ({}ms)", i+1, total, task_id, elapsed);
            }
        }
    }

    let accuracy = if total > 0 { solved as f64 / total as f64 * 100.0 } else { 0.0 };
    let avg_ms = if total > 0 { total_ms / total as u64 } else { 0 };

    println!();
    println!("═══════════════════════════════════════");
    println!("  RESULT: {}/{} ({:.1}%)", solved, total, accuracy);
    println!("  Avg time: {}ms/task", avg_ms);
    println!("  Engines:");
    for (engine, count) in &engines {
        println!("    {}: {}", engine, count);
    }
    println!("═══════════════════════════════════════");
}

fn run_solve(path: &str, config: &SolverConfig) {
    let data: TaskData = serde_json::from_str(
        &std::fs::read_to_string(path).expect("Cannot read file")
    ).expect("Cannot parse JSON");

    let train_pairs: Vec<_> = data.train.iter().map(|p| (p.input.clone(), p.output.clone())).collect();
    let test_inputs: Vec<_> = data.test.iter().map(|p| p.input.clone()).collect();

    match solve(&train_pairs, &test_inputs, config) {
        Some(r) => {
            println!("SOLVED ({}, {}ms)", r.engine, r.time_ms);
            println!("Program: {:?}", r.program);
            for (i, output) in r.test_outputs.iter().enumerate() {
                println!("Test output {}:", i);
                for row in output { println!("  {:?}", row); }
            }
        }
        None => println!("UNSOLVED"),
    }
}
