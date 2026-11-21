use crate::Grid;
use crate::search::augmentation;
use crate::search::beam::{BeamConfig, beam_search, beam_search_steered, exhaustive_search};
use crate::search::diff_synth;
use crate::search::evolution::{self, EvolutionConfig};
use crate::search::llm_engine::{self, LlmConfig};
use crate::search::steering;
use crate::search::synthesizer;
use std::time::Instant;

pub struct SolverConfig {
    pub timeout_ms: u64,
    pub use_augmentation: bool,
    pub use_evolution: bool,
    pub use_llm: bool,
    pub beam_width: usize,
    pub max_depth: usize,
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

pub struct SolveResult {
    pub test_outputs: Vec<Grid>,
    pub engine: &'static str,
    pub program: Vec<String>,
    pub time_ms: u64,
}

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

    let synth_budget = config.timeout_ms / 10;
    if config.verbose {
        eprintln!("[solver] engine 1: synthesizer (budget={}ms)", synth_budget);
    }

    if let Some(sr) = synthesizer::synthesize(train_pairs, test_inputs, synth_budget) {
        let cv_ok = if train_pairs.len() >= 3 {
            (0..train_pairs.len()).all(|hold_out| {
                let sub_pairs: Vec<_> = train_pairs
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| i != hold_out)
                    .map(|(_, p)| p.clone())
                    .collect();
                let held = &train_pairs[hold_out];
                match synthesizer::synthesize(
                    &sub_pairs,
                    std::slice::from_ref(&held.0),
                    synth_budget / 2,
                ) {
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
                    "[solver] synthesizer solved in {}ms (level {}): {}",
                    elapsed(),
                    sr.level,
                    sr.program_desc
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

    let diff_budget = config.timeout_ms * 20 / 100;
    if config.verbose {
        eprintln!("[solver] engine 1.5: diff_synth (budget={}ms)", diff_budget);
    }

    if let Some(dr) = diff_synth::diff_synthesize(train_pairs, test_inputs, diff_budget) {
        let cv_ok = if train_pairs.len() >= 2 {
            (0..train_pairs.len()).all(|hold_out| {
                let sub_pairs: Vec<_> = train_pairs
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| i != hold_out)
                    .map(|(_, p)| p.clone())
                    .collect();
                let held = &train_pairs[hold_out];
                match diff_synth::diff_synthesize(
                    &sub_pairs,
                    std::slice::from_ref(&held.0),
                    diff_budget / 2,
                ) {
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
                    elapsed(),
                    dr.program_desc
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

    let exhaust_budget = config.timeout_ms * 5 / 100;
    if config.verbose {
        eprintln!(
            "[solver] engine 1.8: exhaustive search (budget={}ms, depth=3)",
            exhaust_budget
        );
    }

    if let Some(sr) = exhaustive_search(train_pairs, test_inputs, 3, exhaust_budget) {
        let program: Vec<String> = sr.program.iter().map(|s| s.to_string()).collect();
        if config.verbose {
            eprintln!(
                "[solver] exhaustive search solved in {}ms: {:?}",
                elapsed(),
                program
            );
        }
        if let Some(test_outputs) = apply_to_all_tests(&program, train_pairs, test_inputs) {
            return Some(SolveResult {
                test_outputs,
                engine: "exhaustive",
                program,
                time_ms: elapsed(),
            });
        }
    }

    if remaining() == 0 {
        return None;
    }

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
            eprintln!(
                "[solver] beam search solved in {}ms: {:?}",
                elapsed(),
                program
            );
        }
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

    if config.use_augmentation {
        let aug_budget = config.timeout_ms * 30 / 100;
        if config.verbose {
            eprintln!(
                "[solver] engine 3: augmented beam search (budget={}ms)",
                aug_budget
            );
        }

        let aug_beam_config = BeamConfig {
            beam_width: config.beam_width / 2,
            max_depth: config.max_depth.min(4),
            timeout_ms: aug_budget / 8,
        };

        let aug_results = augmentation::solve_with_augmentation(
            train_pairs,
            test_inputs,
            |aug_train, aug_tests| {
                let mut outputs: Vec<Option<Grid>> = Vec::new();
                for test_input in aug_tests {
                    match beam_search(
                        aug_train,
                        std::slice::from_ref(test_input),
                        &aug_beam_config,
                    ) {
                        Some(sr) => outputs.push(Some(sr.output)),
                        None => outputs.push(None),
                    }
                }
                outputs
            },
        );

        if !aug_results.is_empty() && aug_results.iter().all(|r| r.is_some()) {
            let test_outputs: Vec<Grid> = aug_results.into_iter().map(|r| r.unwrap()).collect();
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

    if config.use_evolution {
        let evo_budget = remaining();
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

    if config.use_llm {
        let llm_budget = remaining();
        if config.verbose {
            eprintln!(
                "[solver] engine 5: LLM-guided search (budget={}ms)",
                llm_budget
            );
        }

        let llm_config = LlmConfig {
            timeout_ms: llm_budget,
            ..LlmConfig::default()
        };

        if let Some(lr) =
            llm_engine::llm_guided_search(train_pairs, test_inputs, &llm_config, config.verbose)
        {
            if config.verbose {
                eprintln!(
                    "[solver] LLM solved in {}ms (attempt {}): {:?}",
                    elapsed(),
                    lr.attempt,
                    lr.program
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

pub fn solve_and_score(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    test_outputs: &[Grid],
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

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (prim.apply)(&args)));
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            sr.engine == "synthesizer" || sr.engine == "beam_search",
            "expected synthesizer or beam_search, got {}",
            sr.engine
        );
        assert_eq!(sr.test_outputs, vec![output]);
    }

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

    #[test]
    fn test_solver_multistep() {
        let input = vec![vec![1, 2], vec![3, 4]];
        let output = vec![vec![2, 1], vec![4, 3]];

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

        let (correct, result) = solve_and_score(&train_pairs, &test_inputs, &test_outputs, &config);
        assert!(correct);
        assert!(result.is_some());
    }

    #[test]
    fn test_solve_and_score_incorrect() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];
        let wrong_gt = vec![vec![9, 9], vec![9, 9]];
        let train_pairs = vec![(input.clone(), output)];
        let test_inputs = vec![input];
        let test_outputs = vec![wrong_gt];

        let config = SolverConfig {
            timeout_ms: 10_000,
            verbose: false,
            ..Default::default()
        };

        let (correct, _result) =
            solve_and_score(&train_pairs, &test_inputs, &test_outputs, &config);
        assert!(!correct);
    }

    #[test]
    fn test_solver_no_optional_engines() {
        let input = vec![vec![1, 2], vec![3, 4]];
        let output = vec![vec![3, 1], vec![4, 2]];

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

    #[test]
    fn test_solver_empty_input() {
        let config = SolverConfig::default();
        assert!(solve(&[], &[], &config).is_none());
        assert!(solve(&[(vec![vec![1]], vec![vec![2]])], &[], &config,).is_none());
    }

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
