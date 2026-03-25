//! Multi-engine solver — orchestrates synthesizer, diff-synth, beam search, augmentation, evolution, and LLM.
//!
//! Runs each engine in sequence with a time budget.  Returns as soon as any
//! engine finds a program that maps every training input to its expected output.
//!
//! Engine priority (with default time budgets):
//! 1. **Synthesizer** (~10%) — exhaustive DFS up to depth 5, fast for simple tasks
//! 1.5 **Diff-synth** (~20%) — diff-guided synthesis: analyze how output differs from input
//! 2. **Beam search** (~10%) — fitness-guided parallel search
//! 3. **Augmented beam search** (~30%) — D4 + PoE voting over beam search
//! 4. **Evolution** (~30%) — genetic refinement of partial solutions
//! 5. **LLM-guided** (remaining time) — Ollama LLM picks operations step by step

use crate::Grid;
use crate::search::beam::{beam_search, beam_search_steered, BeamConfig};
use crate::search::diff_synth;
use crate::search::synthesizer;
use crate::search::steering;
use crate::search::augmentation;
use crate::search::evolution::{self, EvolutionConfig};
use crate::search::llm_engine::{self, LlmConfig};
use std::time::Instant;

// ============================================================
// Configuration
// ============================================================

pub struct SolverConfig {
    pub timeout_ms: u64,         // total wall-clock timeout per task (default 30_000)
    pub use_augmentation: bool,  // enable D4 + color perm + PoE (default true)
    pub use_evolution: bool,     // enable evolutionary refinement (default true)
    pub use_llm: bool,           // enable LLM-guided search via Ollama (default false)
    pub beam_width: usize,       // beam search width (default 200)
    pub max_depth: usize,        // beam search max depth (default 6)
    pub verbose: bool,
    pub steering_model: Option<std::sync::Arc<steering::SteeringModel>>,
}

impl Default for SolverConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            use_augmentation: true,
            use_evolution: true,
            use_llm: false,
            beam_width: 200,
            max_depth: 6,
            verbose: false,
            steering_model: None,
        }
    }
}

// ============================================================
// Result
// ============================================================

pub struct SolveResult {
    pub test_outputs: Vec<Grid>,
    pub engine: &'static str,  // which engine solved it
    pub program: Vec<String>,
    pub time_ms: u64,
}

// ============================================================
// Main solver
// ============================================================

/// Solve a single ARC-AGI task using all engines in sequence.
///
/// Engine priority:
/// 1.   Synthesizer (5-level, ~10% time budget) -- fast, covers simple tasks
/// 1.5  Diff-synth (~20% time budget) -- diff-guided synthesis
/// 2.   Beam search (~10% time budget) -- type-driven parallel search
/// 3.   Augmented beam search (~30% time budget) -- D4 + PoE over beam search
/// 4.   Evolution (~30% time budget) -- refine partial solutions
pub fn solve(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    config: &SolverConfig,
) -> Option<SolveResult> {
    if train_pairs.is_empty() || test_inputs.is_empty() {
        return None;
    }

    let start = Instant::now();
    let elapsed = || start.elapsed().as_millis() as u64;
    let remaining = || config.timeout_ms.saturating_sub(elapsed());

    // ----------------------------------------------------------
    // Engine 1: Synthesizer (~10%)
    // ----------------------------------------------------------
    let synth_budget = config.timeout_ms / 10;
    if config.verbose {
        eprintln!("[solver] engine 1: synthesizer (budget={}ms)", synth_budget);
    }

    if let Some(sr) = synthesizer::synthesize(train_pairs, test_inputs, synth_budget) {
        // Cross-validate: if >=3 pairs, hold out each pair and re-solve with the rest.
        // This catches overfitting (programs that pass training but fail on new data).
        let cv_ok = if train_pairs.len() >= 3 {
            (0..train_pairs.len()).all(|hold_out| {
                let sub_pairs: Vec<_> = train_pairs.iter().enumerate()
                    .filter(|&(i, _)| i != hold_out)
                    .map(|(_, p)| p.clone())
                    .collect();
                let held = &train_pairs[hold_out];
                match synthesizer::synthesize(&sub_pairs, &[held.0.clone()], synth_budget / 2) {
                    Some(cv_result) => {
                        !cv_result.test_outputs.is_empty() && cv_result.test_outputs[0] == held.1
                    }
                    None => false,
                }
            })
        } else {
            true // not enough pairs for CV
        };

        if cv_ok {
            if config.verbose {
                eprintln!(
                    "[solver] synthesizer solved in {}ms (level {}): {}",
                    elapsed(), sr.level, sr.program_desc
                );
            }
            return Some(SolveResult {
                test_outputs: sr.test_outputs,
                engine: "synthesizer",
                program: vec![sr.program_desc],
                time_ms: elapsed(),
            });
        } else if config.verbose {
            eprintln!(
                "[solver] synthesizer found {} but failed cross-validation, skipping",
                sr.program_desc
            );
        }
    }

    if remaining() == 0 {
        return None;
    }

    // ----------------------------------------------------------
    // Engine 1.5: Diff-guided synthesis (~20%)
    // ----------------------------------------------------------
    let diff_budget = config.timeout_ms * 20 / 100;
    if config.verbose {
        eprintln!("[solver] engine 1.5: diff_synth (budget={}ms)", diff_budget);
    }

    if let Some(dr) = diff_synth::diff_synthesize(train_pairs, test_inputs, diff_budget) {
        // Cross-validate diff_synth results too
        let cv_ok = if train_pairs.len() >= 2 {
            (0..train_pairs.len()).all(|hold_out| {
                let sub_pairs: Vec<_> = train_pairs.iter().enumerate()
                    .filter(|&(i, _)| i != hold_out)
                    .map(|(_, p)| p.clone())
                    .collect();
                let held = &train_pairs[hold_out];
                match diff_synth::diff_synthesize(&sub_pairs, &[held.0.clone()], diff_budget / 2) {
                    Some(cv_result) => {
                        !cv_result.test_outputs.is_empty() && cv_result.test_outputs[0] == held.1
                    }
                    None => false,
                }
            })
        } else {
            true
        };

        if cv_ok {
            if config.verbose {
                eprintln!(
                    "[solver] diff_synth solved in {}ms: {}",
                    elapsed(), dr.program_desc
                );
            }
            return Some(SolveResult {
                test_outputs: dr.test_outputs,
                engine: "diff_synth",
                program: vec![dr.program_desc],
                time_ms: elapsed(),
            });
        } else if config.verbose {
            eprintln!(
                "[solver] diff_synth found {} but failed CV, skipping",
                dr.program_desc
            );
        }
    }

    if remaining() == 0 {
        return None;
    }

    // ----------------------------------------------------------
    // Engine 2: Beam search (~10%)
    // ----------------------------------------------------------
    let beam_budget = config.timeout_ms * 10 / 100;
    if config.verbose {
        eprintln!(
            "[solver] engine 2: beam search (width={}, depth={}, budget={}ms)",
            config.beam_width, config.max_depth, beam_budget
        );
    }

    let beam_config = BeamConfig {
        beam_width: config.beam_width,
        max_depth: config.max_depth,
        timeout_ms: beam_budget,
    };

    let steer_ref = config.steering_model.as_deref();
    if let Some(sr) = beam_search_steered(train_pairs, test_inputs, &beam_config, steer_ref) {
        let program: Vec<String> = sr.program.iter().map(|s| s.to_string()).collect();
        if config.verbose {
            eprintln!("[solver] beam search solved in {}ms: {:?}", elapsed(), program);
        }
        // beam_search returns output for the first test input only.
        // Apply the program to all test inputs.
        if let Some(test_outputs) = apply_to_all_tests(&program, train_pairs, test_inputs) {
            return Some(SolveResult {
                test_outputs,
                engine: "beam_search",
                program,
                time_ms: elapsed(),
            });
        }
    }

    if remaining() == 0 {
        return None;
    }

    // ----------------------------------------------------------
    // Engine 3: Augmented beam search (~30%)
    // ----------------------------------------------------------
    if config.use_augmentation {
        let aug_budget = config.timeout_ms * 30 / 100;
        if config.verbose {
            eprintln!("[solver] engine 3: augmented beam search (budget={}ms)", aug_budget);
        }

        let aug_beam_config = BeamConfig {
            beam_width: config.beam_width / 2, // smaller width per augmentation
            max_depth: config.max_depth.min(4),
            timeout_ms: aug_budget / 8, // each of 8 D4 transforms gets 1/8
        };

        let aug_results = augmentation::solve_with_augmentation(
            train_pairs,
            test_inputs,
            |aug_train, aug_tests| {
                // Wrap beam search as a multi-test solver
                let mut outputs: Vec<Option<Grid>> = Vec::new();
                for test_input in aug_tests {
                    match beam_search(aug_train, &[test_input.clone()], &aug_beam_config) {
                        Some(sr) => outputs.push(Some(sr.output)),
                        None => outputs.push(None),
                    }
                }
                outputs
            },
        );

        // Check if augmentation produced valid outputs for all test inputs
        if !aug_results.is_empty() && aug_results.iter().all(|r| r.is_some()) {
            let test_outputs: Vec<Grid> =
                aug_results.into_iter().map(|r| r.unwrap()).collect();
            if config.verbose {
                eprintln!("[solver] augmented beam search solved in {}ms", elapsed());
            }
            return Some(SolveResult {
                test_outputs,
                engine: "augmented_beam",
                program: vec!["(augmented_vote)".to_string()],
                time_ms: elapsed(),
            });
        }
    }

    if remaining() == 0 {
        return None;
    }

    // ----------------------------------------------------------
    // Engine 4: Evolution (~30%)
    // ----------------------------------------------------------
    if config.use_evolution {
        let evo_budget = remaining(); // give evolution all remaining time
        if config.verbose {
            eprintln!("[solver] engine 4: evolution (budget={}ms)", evo_budget);
        }

        let evo_config = EvolutionConfig {
            n_initial: 16,
            n_parents: 4,
            n_offspring: 4,
            n_generations: 4,
            timeout_ms: evo_budget,
        };

        if let Some(er) = evolution::evolve(train_pairs, test_inputs, &evo_config) {
            if config.verbose {
                eprintln!(
                    "[solver] evolution solved in {}ms (gen {}): {:?}",
                    elapsed(),
                    er.generation,
                    er.steps
                );
            }
            return Some(SolveResult {
                test_outputs: er.test_outputs,
                engine: "evolution",
                program: er.steps,
                time_ms: elapsed(),
            });
        }
    }

    if remaining() == 0 {
        return None;
    }

    // ----------------------------------------------------------
    // Engine 5: LLM-guided search (remaining time)
    // ----------------------------------------------------------
    if config.use_llm {
        let llm_budget = remaining();
        if config.verbose {
            eprintln!("[solver] engine 5: LLM-guided search (budget={}ms)", llm_budget);
        }

        let llm_config = LlmConfig {
            timeout_ms: llm_budget,
            ..LlmConfig::default()
        };

        if let Some(lr) = llm_engine::llm_guided_search(train_pairs, test_inputs, &llm_config, config.verbose) {
            if config.verbose {
                eprintln!(
                    "[solver] LLM solved in {}ms (attempt {}): {:?}",
                    elapsed(), lr.attempt, lr.program
                );
            }
            return Some(SolveResult {
                test_outputs: lr.test_outputs,
                engine: "llm",
                program: lr.program,
                time_ms: elapsed(),
            });
        }
    }

    None
}

/// Solve and evaluate against known test output.
///
/// Returns `(correct, result)` where `correct` is true iff the solver's
/// output matches the ground truth for every test input.
pub fn solve_and_score(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    test_outputs: &[Grid],  // ground truth
    config: &SolverConfig,
) -> (bool, Option<SolveResult>) {
    match solve(train_pairs, test_inputs, config) {
        Some(result) => {
            let correct = result.test_outputs.len() == test_outputs.len()
                && result
                    .test_outputs
                    .iter()
                    .zip(test_outputs.iter())
                    .all(|(predicted, expected)| predicted == expected);
            (correct, Some(result))
        }
        None => (false, None),
    }
}

// ============================================================
// Helpers
// ============================================================

/// Apply a program (by name) to all test inputs using the catalog built
/// from the training pairs' colors.
fn apply_to_all_tests(
    program: &[String],
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<Vec<Grid>> {
    use crate::search::enumerate::*;

    let mut colors: Vec<u8> = Vec::new();
    for (inp, out) in train_pairs {
        for row in inp {
            for &c in row {
                if !colors.contains(&c) {
                    colors.push(c);
                }
            }
        }
        for row in out {
            for &c in row {
                if !colors.contains(&c) {
                    colors.push(c);
                }
            }
        }
    }
    colors.sort();

    let catalog = build_primitive_catalog(&colors);
    let mut outputs = Vec::new();

    for test_input in test_inputs {
        match apply_program_local(program, &catalog, test_input) {
            Some(g) => outputs.push(g),
            None => return None,
        }
    }

    Some(outputs)
}

/// Apply a sequence of named primitives to an input grid.
fn apply_program_local(
    steps: &[String],
    catalog: &[crate::search::enumerate::TypedPrimitive],
    input: &Grid,
) -> Option<Grid> {
    use crate::search::enumerate::*;

    let mut current = DagValue::Grid(input.clone());

    for step_name in steps {
        let prim = catalog.iter().find(|p| p.name == step_name.as_str())?;

        let args = if prim.input_types.len() == 1 {
            vec![current.clone()]
        } else if prim.input_types.len() == 2 && prim.input_types[1] == DagType::Grid {
            vec![current.clone(), DagValue::Grid(input.clone())]
        } else {
            return None;
        };

        let result = std::panic::catch_unwind(
            std::panic::AssertUnwindSafe(|| (prim.apply)(&args))
        );
        current = match result {
            Ok(Some(v)) => v,
            _ => return None,
        };
    }

    if let DagValue::Grid(g) = current {
        Some(g)
    } else {
        None
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple rotation task: the synthesizer should pick it up immediately.
    #[test]
    fn test_solver_dispatches_to_synthesizer() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];
        let train_pairs = vec![(input.clone(), output.clone())];
        let test_inputs = vec![input];

        let config = SolverConfig {
            timeout_ms: 10_000,
            use_augmentation: false,
            use_evolution: false,
            verbose: false,
            ..Default::default()
        };

        let result = solve(&train_pairs, &test_inputs, &config);
        assert!(result.is_some());
        let sr = result.unwrap();
        // Should be solved by synthesizer or beam search (fast engines)
        assert!(
            sr.engine == "synthesizer" || sr.engine == "beam_search",
            "expected synthesizer or beam_search, got {}",
            sr.engine
        );
        assert_eq!(sr.test_outputs, vec![output]);
    }

    /// Color replacement task.
    #[test]
    fn test_solver_color_replace() {
        let input = vec![vec![1, 0, 2], vec![2, 1, 0]];
        let output = vec![vec![3, 0, 2], vec![2, 3, 0]];
        let train_pairs = vec![(input.clone(), output.clone())];
        let test_inputs = vec![input];

        let config = SolverConfig {
            timeout_ms: 10_000,
            verbose: false,
            ..Default::default()
        };

        let result = solve(&train_pairs, &test_inputs, &config);
        assert!(result.is_some());
        let sr = result.unwrap();
        assert_eq!(sr.test_outputs, vec![output]);
    }

    /// Multi-step task: vmirror.
    #[test]
    fn test_solver_multistep() {
        let input = vec![vec![1, 2], vec![3, 4]];
        let output = vec![vec![2, 1], vec![4, 3]]; // vmirror

        let train_pairs = vec![(input.clone(), output.clone())];
        let test_inputs = vec![input];

        let config = SolverConfig {
            timeout_ms: 10_000,
            verbose: false,
            ..Default::default()
        };

        let result = solve(&train_pairs, &test_inputs, &config);
        assert!(result.is_some());
        let sr = result.unwrap();
        assert_eq!(sr.test_outputs, vec![output]);
    }

    /// solve_and_score reports correct=true for a solvable task.
    #[test]
    fn test_solve_and_score_correct() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];
        let train_pairs = vec![(input.clone(), output.clone())];
        let test_inputs = vec![input];
        let test_outputs = vec![output];

        let config = SolverConfig {
            timeout_ms: 10_000,
            verbose: false,
            ..Default::default()
        };

        let (correct, result) = solve_and_score(
            &train_pairs,
            &test_inputs,
            &test_outputs,
            &config,
        );
        assert!(correct);
        assert!(result.is_some());
    }

    /// solve_and_score reports correct=false when ground truth differs.
    #[test]
    fn test_solve_and_score_incorrect() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];
        let wrong_gt = vec![vec![9, 9], vec![9, 9]]; // wrong ground truth
        let train_pairs = vec![(input.clone(), output)];
        let test_inputs = vec![input];
        let test_outputs = vec![wrong_gt];

        let config = SolverConfig {
            timeout_ms: 10_000,
            verbose: false,
            ..Default::default()
        };

        let (correct, _result) = solve_and_score(
            &train_pairs,
            &test_inputs,
            &test_outputs,
            &config,
        );
        assert!(!correct);
    }

    /// Disabling all optional engines still works (synthesizer + beam only).
    #[test]
    fn test_solver_no_optional_engines() {
        let input = vec![vec![1, 2], vec![3, 4]];
        let output = vec![vec![3, 1], vec![4, 2]]; // rot90

        let train_pairs = vec![(input.clone(), output.clone())];
        let test_inputs = vec![input];

        let config = SolverConfig {
            timeout_ms: 5_000,
            use_augmentation: false,
            use_evolution: false,
            verbose: false,
            ..Default::default()
        };

        let result = solve(&train_pairs, &test_inputs, &config);
        assert!(result.is_some());
        let sr = result.unwrap();
        assert_eq!(sr.test_outputs, vec![output]);
    }

    /// Empty inputs return None without panicking.
    #[test]
    fn test_solver_empty_input() {
        let config = SolverConfig::default();
        assert!(solve(&[], &[], &config).is_none());
        assert!(solve(
            &[(vec![vec![1]], vec![vec![2]])],
            &[],
            &config,
        ).is_none());
    }

    /// The engine field correctly identifies which engine solved it.
    #[test]
    fn test_engine_field() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];
        let train_pairs = vec![(input.clone(), output)];
        let test_inputs = vec![input];

        let config = SolverConfig {
            timeout_ms: 10_000,
            use_augmentation: false,
            use_evolution: false,
            verbose: false,
            ..Default::default()
        };

        let result = solve(&train_pairs, &test_inputs, &config);
        assert!(result.is_some());
        let engine = result.unwrap().engine;
        assert!(
            engine == "synthesizer" || engine == "beam_search",
            "expected synthesizer or beam_search, got {}",
            engine
        );
    }

    /// Multiple test inputs are all solved.
    #[test]
    fn test_solver_multiple_test_inputs() {
        let input1 = vec![vec![1, 0], vec![0, 0]];
        let output1 = vec![vec![0, 1], vec![0, 0]];
        let input2 = vec![vec![0, 0], vec![0, 1]];

        let train_pairs = vec![(input1.clone(), output1)];
        let test_inputs = vec![input1, input2];

        let config = SolverConfig {
            timeout_ms: 10_000,
            verbose: false,
            ..Default::default()
        };

        let result = solve(&train_pairs, &test_inputs, &config);
        assert!(result.is_some());
        let sr = result.unwrap();
        assert_eq!(sr.test_outputs.len(), 2);
    }
}
