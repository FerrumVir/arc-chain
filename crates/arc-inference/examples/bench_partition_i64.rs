//! Bounded local feasibility gate for useful integer model parallelism.
//!
//! This intentionally uses ARC's existing reference `integer_engine::matmul_i64`
//! kernel with a deterministic synthetic projection-shaped matrix. Workers own
//! disjoint output rows; the merge is therefore concatenation, not duplicated
//! verification work. Local threads demonstrate arithmetic and partition
//! accounting only. They make no WAN, complete-model, or production-readiness
//! claim.

use arc_inference::integer_engine::matmul_i64;
use serde::Serialize;
use std::env;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const DEFAULT_ROWS: usize = 4096;
const DEFAULT_COLS: usize = 4096;
const DEFAULT_REPEATS: usize = 1;
const DEFAULT_SEED: u64 = 0x4152_435f_5632;
const MAX_DIMENSION: usize = 16_384;
const MAX_WORK_ELEMENTS: usize = 32_000_000;
const MAX_REPEATS: usize = 20;
const MAX_WORKER_LIST: usize = 8;

#[derive(Debug, Clone)]
struct Config {
    rows: usize,
    cols: usize,
    repeats: usize,
    seed: u64,
    workers: Vec<usize>,
}

#[derive(Debug, Serialize)]
struct WorkerReport {
    worker_id: usize,
    row_start: usize,
    row_end: usize,
    rows: usize,
    output_elements: usize,
    work_elements: usize,
    output_checksum: i64,
}

#[derive(Debug, Serialize)]
struct Measurement {
    workers: usize,
    reference_ms: f64,
    parallel_total_ms: f64,
    worker_phase_ms: f64,
    merge_ms: f64,
    communication_boundary_ms: Option<f64>,
    speedup_vs_reference: f64,
    output_equal_reference: bool,
    work_elements: usize,
    covered_rows: usize,
    overlap_free: bool,
    output_nonzero: bool,
    workers_detail: Vec<WorkerReport>,
}

#[derive(Debug, Serialize)]
struct Report {
    benchmark: &'static str,
    execution_scope: &'static str,
    network_claim: bool,
    complete_model_claim: bool,
    synthetic_gate: bool,
    profile: &'static str,
    model_shape: Shape,
    seed: u64,
    repeats: usize,
    setup_ms: f64,
    total_latency_ms: f64,
    measurements: Vec<Measurement>,
}

#[derive(Debug, Serialize)]
struct Shape {
    projection: &'static str,
    rows: usize,
    cols: usize,
    representative_transformer_projection: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: bench_partition_i64 [--rows N] [--cols N] [--workers 1,2,4,8] [--repeats N] [--seed N]"
    );
    std::process::exit(2);
}

fn parse_usize(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or_else(|| usage())
}

fn validate(cfg: &Config) -> Result<(), &'static str> {
    if cfg.rows > MAX_DIMENSION || cfg.cols > MAX_DIMENSION {
        return Err("rows and cols must be <= 16384");
    }
    if cfg.rows.checked_mul(cfg.cols).unwrap_or(usize::MAX) > MAX_WORK_ELEMENTS {
        return Err("rows * cols must be <= 32000000");
    }
    if cfg.repeats > MAX_REPEATS {
        return Err("repeats must be <= 20");
    }
    if cfg.workers.is_empty() || cfg.workers.len() > MAX_WORKER_LIST {
        return Err("workers must contain 1 to 8 entries");
    }
    if cfg
        .workers
        .iter()
        .any(|&workers| !(1..=8).contains(&workers))
    {
        return Err("each worker count must be between 1 and 8");
    }
    Ok(())
}

fn parse() -> Config {
    let mut cfg = Config {
        rows: DEFAULT_ROWS,
        cols: DEFAULT_COLS,
        repeats: DEFAULT_REPEATS,
        seed: DEFAULT_SEED,
        workers: vec![1, 2, 4, 8],
    };
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let value = |index: &mut usize| {
            *index += 1;
            args.get(*index)
                .map(String::as_str)
                .unwrap_or_else(|| usage())
        };
        match args[i].as_str() {
            "--rows" => cfg.rows = parse_usize(Some(value(&mut i))),
            "--cols" => cfg.cols = parse_usize(Some(value(&mut i))),
            "--repeats" => cfg.repeats = parse_usize(Some(value(&mut i))),
            "--seed" => cfg.seed = value(&mut i).parse().unwrap_or_else(|_| usage()),
            "--workers" => {
                cfg.workers = value(&mut i)
                    .split(',')
                    .map(|v| v.parse().ok().filter(|&n| n > 0).unwrap_or_else(|| usage()))
                    .collect();
            }
            "--help" | "-h" => usage(),
            _ => usage(),
        }
        i += 1;
    }
    if validate(&cfg).is_err() {
        usage();
    }
    cfg
}

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

fn build_inputs(cfg: &Config) -> (Vec<i64>, Vec<i64>) {
    let mut state = cfg.seed;
    let weights = (0..cfg.rows * cfg.cols)
        .map(|_| ((next_u64(&mut state) >> 56) as i64) - 128)
        .collect();
    let input = (0..cfg.cols)
        .map(|_| ((next_u64(&mut state) >> 56) as i64) - 128)
        .collect();
    (weights, input)
}

fn checksum(values: &[i64]) -> i64 {
    values
        .iter()
        .fold(0i64, |acc, &value| acc.wrapping_add(value))
}

fn ranges(rows: usize, workers: usize) -> Vec<(usize, usize)> {
    let count = workers.min(rows).max(1);
    (0..count)
        .map(|worker| {
            let start = rows * worker / count;
            let end = rows * (worker + 1) / count;
            (start, end)
        })
        .collect()
}

fn run_parallel(
    weights: Arc<Vec<i64>>,
    input: Arc<Vec<i64>>,
    rows: usize,
    cols: usize,
    workers: usize,
) -> (Vec<i64>, Vec<WorkerReport>, f64, f64) {
    let ranges = ranges(rows, workers);
    let worker_start = Instant::now();
    let mut handles = Vec::with_capacity(ranges.len());
    for (worker_id, &(row_start, row_end)) in ranges.iter().enumerate() {
        let weights = Arc::clone(&weights);
        let input = Arc::clone(&input);
        handles.push(thread::spawn(move || {
            let output = matmul_i64(
                &weights[row_start * cols..row_end * cols],
                &[],
                &input,
                cols,
                row_end - row_start,
            );
            (worker_id, row_start, row_end, output)
        }));
    }
    let mut parts = Vec::with_capacity(handles.len());
    for handle in handles {
        parts.push(handle.join().expect("partition worker panicked"));
    }
    let worker_phase_ms = worker_start.elapsed().as_secs_f64() * 1000.0;
    parts.sort_by_key(|part| part.1);
    let merge_start = Instant::now();
    let mut merged = Vec::with_capacity(rows);
    let mut reports = Vec::with_capacity(parts.len());
    for (worker_id, row_start, row_end, output) in parts {
        reports.push(WorkerReport {
            worker_id,
            row_start,
            row_end,
            rows: row_end - row_start,
            output_elements: output.len(),
            work_elements: (row_end - row_start) * cols,
            output_checksum: checksum(&output),
        });
        merged.extend(output);
    }
    let merge_ms = merge_start.elapsed().as_secs_f64() * 1000.0;
    (merged, reports, worker_phase_ms, merge_ms)
}

fn main() {
    let cfg = parse();
    let setup_start = Instant::now();
    let (weights, input) = build_inputs(&cfg);
    let weights = Arc::new(weights);
    let input = Arc::new(input);
    let setup_ms = setup_start.elapsed().as_secs_f64() * 1000.0;
    let total_start = Instant::now();
    let mut measurements = Vec::new();

    for &workers in &cfg.workers {
        let mut reference_total_ms = 0.0;
        let mut parallel_total_ms_sum = 0.0;
        let mut worker_phase_ms_sum = 0.0;
        let mut merge_ms_sum = 0.0;
        let mut output_equal_reference = true;
        let mut detail = Vec::new();
        for _ in 0..cfg.repeats {
            let reference_start = Instant::now();
            let reference = matmul_i64(&weights, &[], &input, cfg.cols, cfg.rows);
            reference_total_ms += reference_start.elapsed().as_secs_f64() * 1000.0;
            let (parallel, run_detail, worker_phase_ms, merge_ms) = run_parallel(
                Arc::clone(&weights),
                Arc::clone(&input),
                cfg.rows,
                cfg.cols,
                workers,
            );
            output_equal_reference &= parallel == reference;
            parallel_total_ms_sum += worker_phase_ms + merge_ms;
            worker_phase_ms_sum += worker_phase_ms;
            merge_ms_sum += merge_ms;
            detail = run_detail;
        }
        let reference_ms = reference_total_ms / cfg.repeats as f64;
        let parallel_total_ms = parallel_total_ms_sum / cfg.repeats as f64;
        let worker_phase_ms = worker_phase_ms_sum / cfg.repeats as f64;
        let merge_ms = merge_ms_sum / cfg.repeats as f64;
        let covered_rows: usize = detail.iter().map(|w| w.rows).sum();
        let overlap_free = detail
            .windows(2)
            .all(|pair| pair[0].row_end == pair[1].row_start)
            && detail.first().map(|w| w.row_start == 0).unwrap_or(false)
            && detail
                .last()
                .map(|w| w.row_end == cfg.rows)
                .unwrap_or(false);
        let output_nonzero = detail.iter().any(|worker| worker.output_checksum != 0);
        measurements.push(Measurement {
            workers,
            reference_ms,
            parallel_total_ms,
            worker_phase_ms,
            merge_ms,
            communication_boundary_ms: None,
            speedup_vs_reference: reference_ms / parallel_total_ms.max(f64::MIN_POSITIVE),
            output_equal_reference,
            work_elements: detail.iter().map(|w| w.work_elements).sum(),
            covered_rows,
            overlap_free,
            output_nonzero,
            workers_detail: detail,
        });
    }
    let valid = measurements.iter().all(|measurement| {
        measurement.output_equal_reference
            && measurement.work_elements == cfg.rows * cfg.cols
            && measurement.covered_rows == cfg.rows
            && measurement.overlap_free
            && measurement.output_nonzero
    });
    let report = Report {
        benchmark: "arc-inference-partition-i64-v1",
        execution_scope: "local OS threads; no network simulation",
        network_claim: false,
        complete_model_claim: false,
        synthetic_gate: true,
        profile: "arc-inference::integer_engine::matmul_i64 (Q16 i64 integer kernel)",
        model_shape: Shape {
            projection: "synthetic row-partitioned transformer projection",
            rows: cfg.rows,
            cols: cfg.cols,
            representative_transformer_projection: cfg.rows == 4096 && cfg.cols == 4096,
        },
        seed: cfg.seed,
        repeats: cfg.repeats,
        setup_ms,
        total_latency_ms: total_start.elapsed().as_secs_f64() * 1000.0,
        measurements,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("serialize benchmark report")
    );
    if !valid {
        eprintln!("partition benchmark invariant failed");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_divisible_rows_cover_each_row_once() {
        let ranges = ranges(17, 8);
        assert_eq!(ranges.first(), Some(&(0, 2)));
        assert_eq!(ranges.last(), Some(&(14, 17)));
        assert!(ranges.windows(2).all(|pair| pair[0].1 == pair[1].0));
        assert_eq!(
            ranges.iter().map(|(start, end)| end - start).sum::<usize>(),
            17
        );
    }

    #[test]
    fn workers_above_rows_are_clamped_without_empty_partitions() {
        let ranges = ranges(3, 8);
        assert_eq!(ranges, vec![(0, 1), (1, 2), (2, 3)]);
    }

    #[test]
    fn bounded_config_rejects_overflow_and_unbounded_runs() {
        let mut cfg = Config {
            rows: 4096,
            cols: 4096,
            repeats: 1,
            seed: 7,
            workers: vec![1, 2, 4, 8],
        };
        assert!(validate(&cfg).is_ok());
        cfg.rows = 16_384;
        cfg.cols = 16_384;
        assert!(validate(&cfg).is_err());
        cfg.rows = 17;
        cfg.cols = 13;
        cfg.repeats = 21;
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn partitioned_integer_projection_matches_reference() {
        let rows = 17;
        let cols = 13;
        let cfg = Config {
            rows,
            cols,
            repeats: 1,
            seed: 7,
            workers: vec![1, 2, 4, 8],
        };
        let (weights, input) = build_inputs(&cfg);
        let reference = matmul_i64(&weights, &[], &input, cols, rows);
        for workers in cfg.workers {
            let (parallel, detail, _, _) = run_parallel(
                Arc::new(weights.clone()),
                Arc::new(input.clone()),
                rows,
                cols,
                workers,
            );
            assert_eq!(parallel, reference);
            assert!(parallel.iter().any(|&value| value != 0));
            assert_eq!(
                detail.iter().map(|w| w.work_elements).sum::<usize>(),
                rows * cols
            );
            assert!(
                detail
                    .windows(2)
                    .all(|pair| pair[0].row_end == pair[1].row_start)
            );
        }
    }
}
