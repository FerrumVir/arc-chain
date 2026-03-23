//! ARC-AGI task loading and evaluation.

use crate::Grid;
use crate::search::beam::{beam_search, BeamConfig, SearchResult};
use std::path::Path;
use serde::Deserialize;

#[derive(Deserialize)]
struct TaskPair {
    input: Vec<Vec<u8>>,
    output: Vec<Vec<u8>>,
}

#[derive(Deserialize)]
struct TaskData {
    train: Vec<TaskPair>,
    test: Vec<TaskPair>,
}

pub struct EvalResult {
    pub task_id: String,
    pub solved: bool,
    pub program: Vec<String>,
    pub depth: usize,
    pub time_ms: u64,
}

/// Load and solve a single ARC-AGI task.
pub fn solve_task(path: &Path, config: &BeamConfig) -> EvalResult {
    let task_id = path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let start = std::time::Instant::now();

    let data: TaskData = match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(d) => d,
            Err(_) => return EvalResult {
                task_id, solved: false, program: vec![], depth: 0,
                time_ms: start.elapsed().as_millis() as u64,
            },
        },
        Err(_) => return EvalResult {
            task_id, solved: false, program: vec![], depth: 0,
            time_ms: start.elapsed().as_millis() as u64,
        },
    };

    let train_pairs: Vec<(Grid, Grid)> = data.train.iter()
        .map(|p| (p.input.clone(), p.output.clone()))
        .collect();

    let test_inputs: Vec<Grid> = data.test.iter()
        .map(|p| p.input.clone())
        .collect();

    let result = beam_search(&train_pairs, &test_inputs, config);

    let time_ms = start.elapsed().as_millis() as u64;

    match result {
        Some(sr) => {
            // Verify against test output
            let test_correct = if !data.test.is_empty() {
                sr.output == data.test[0].output
            } else {
                false
            };

            EvalResult {
                task_id,
                solved: test_correct,
                program: sr.program.iter().map(|s| s.to_string()).collect(),
                depth: sr.depth,
                time_ms,
            }
        }
        None => EvalResult {
            task_id, solved: false, program: vec![], depth: 0, time_ms,
        },
    }
}

/// Evaluate on a directory of ARC-AGI tasks.
pub fn evaluate_directory(dir: &Path, config: &BeamConfig, max_tasks: Option<usize>) -> Vec<EvalResult> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("Cannot read directory")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "json"))
        .collect();

    paths.sort();

    if let Some(max) = max_tasks {
        paths.truncate(max);
    }

    let total = paths.len();
    let mut results = Vec::new();
    let mut solved = 0;

    for (i, path) in paths.iter().enumerate() {
        let result = solve_task(path, config);
        if result.solved {
            solved += 1;
            println!("[{:3}/{}] {}: OK (depth={}, {}ms)",
                i + 1, total, result.task_id, result.depth, result.time_ms);
        } else {
            println!("[{:3}/{}] {}: --  ({}ms)",
                i + 1, total, result.task_id, result.time_ms);
        }
        results.push(result);
    }

    let accuracy = if total > 0 { solved as f64 / total as f64 * 100.0 } else { 0.0 };
    println!("\n===== RESULT: {}/{} ({:.1}%) =====", solved, total, accuracy);

    results
}
