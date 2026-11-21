use crate::search::beam::{BeamConfig, beam_search};
use crate::search::enumerate::*;
use crate::{Color, Grid};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub steps: Vec<String>,
    pub fitness: f64,
    pub cell_accuracy: f64,
    pub generation: u8,
}

pub struct EvolutionConfig {
    pub n_initial: usize,
    pub n_parents: usize,
    pub n_offspring: usize,
    pub n_generations: usize,
    pub timeout_ms: u64,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            n_initial: 16,
            n_parents: 4,
            n_offspring: 4,
            n_generations: 4,
            timeout_ms: 60_000,
        }
    }
}

pub struct EvolutionResult {
    pub steps: Vec<String>,
    pub test_outputs: Vec<Grid>,
    pub fitness: f64,
    pub generation: u8,
}

fn apply_program_by_name(
    steps: &[String],
    catalog: &[TypedPrimitive],
    input: &Grid,
) -> Option<Grid> {
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

pub fn score_candidate(
    steps: &[String],
    train_pairs: &[(Grid, Grid)],
    catalog: &[TypedPrimitive],
) -> (f64, f64) {
    if train_pairs.is_empty() {
        return (0.0, 0.0);
    }

    let mut exact_matches = 0usize;
    let mut total_cells = 0usize;
    let mut correct_cells = 0usize;

    for (input, expected) in train_pairs {
        match apply_program_by_name(steps, catalog, input) {
            Some(ref result) => {
                if result == expected {
                    exact_matches += 1;
                }

                if result.len() == expected.len()
                    && !result.is_empty()
                    && result[0].len() == expected[0].len()
                {
                    let cells = result.len() * result[0].len();
                    total_cells += cells;
                    for (rr, er) in result.iter().zip(expected.iter()) {
                        for (&rv, &ev) in rr.iter().zip(er.iter()) {
                            if rv == ev {
                                correct_cells += 1;
                            }
                        }
                    }
                } else {
                    let cells = expected.len() * expected.first().map_or(0, |r| r.len());
                    total_cells += cells;
                }
            }
            None => {
                let cells = expected.len() * expected.first().map_or(0, |r| r.len());
                total_cells += cells;
            }
        }
    }

    let fitness = exact_matches as f64 / train_pairs.len() as f64;
    let cell_accuracy = if total_cells > 0 {
        correct_cells as f64 / total_cells as f64
    } else {
        0.0
    };

    (fitness, cell_accuracy)
}

fn collect_colors(train_pairs: &[(Grid, Grid)]) -> Vec<Color> {
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
    colors
}

fn type_valid_ops<'a>(
    catalog: &'a [TypedPrimitive],
    input_type: &DagType,
) -> Vec<&'a TypedPrimitive> {
    catalog
        .iter()
        .filter(|p| {
            (p.input_types.len() == 1 && p.input_types[0] == *input_type)
                || (p.input_types.len() == 2
                    && p.input_types[0] == *input_type
                    && p.input_types[1] == DagType::Grid)
        })
        .collect()
}

fn output_type_of(steps: &[String], catalog: &[TypedPrimitive]) -> DagType {
    if steps.is_empty() {
        return DagType::Grid;
    }
    if let Some(last) = steps.last()
        && let Some(prim) = catalog.iter().find(|p| p.name == last.as_str())
    {
        return prim.output_type.clone();
    }
    DagType::Grid
}

fn mutate_swap(parent: &Candidate, catalog: &[TypedPrimitive]) -> Vec<Candidate> {
    let mut offspring = Vec::new();

    for pos in 0..parent.steps.len() {
        let input_type = if pos == 0 {
            DagType::Grid
        } else {
            output_type_of(&parent.steps[..pos], catalog)
        };

        let valid_ops = type_valid_ops(catalog, &input_type);

        for op in &valid_ops {
            if op.name == parent.steps[pos].as_str() {
                continue;
            }

            if pos + 1 < parent.steps.len() {
                let next_name = &parent.steps[pos + 1];
                if let Some(next_prim) = catalog.iter().find(|p| p.name == next_name.as_str())
                    && next_prim.input_types[0] != op.output_type
                {
                    continue;
                }
            }

            let mut new_steps = parent.steps.clone();
            new_steps[pos] = op.name.to_string();

            offspring.push(Candidate {
                steps: new_steps,
                fitness: 0.0,
                cell_accuracy: 0.0,
                generation: parent.generation + 1,
            });
        }
    }

    offspring
}

fn mutate_extend(parent: &Candidate, catalog: &[TypedPrimitive]) -> Vec<Candidate> {
    let tail_type = output_type_of(&parent.steps, catalog);
    let valid_ops = type_valid_ops(catalog, &tail_type);

    valid_ops
        .iter()
        .map(|op| {
            let mut new_steps = parent.steps.clone();
            new_steps.push(op.name.to_string());
            Candidate {
                steps: new_steps,
                fitness: 0.0,
                cell_accuracy: 0.0,
                generation: parent.generation + 1,
            }
        })
        .collect()
}

fn mutate_truncate(parent: &Candidate) -> Option<Candidate> {
    if parent.steps.len() <= 1 {
        return None;
    }
    let mut new_steps = parent.steps.clone();
    new_steps.pop();
    Some(Candidate {
        steps: new_steps,
        fitness: 0.0,
        cell_accuracy: 0.0,
        generation: parent.generation + 1,
    })
}

fn crossover(a: &Candidate, b: &Candidate, catalog: &[TypedPrimitive]) -> Vec<Candidate> {
    let mut offspring = Vec::new();

    for split_a in 1..a.steps.len() {
        let type_at_a = output_type_of(&a.steps[..split_a], catalog);

        for split_b in 1..b.steps.len() {
            let suffix_input = if split_b < b.steps.len() {
                if let Some(prim) = catalog.iter().find(|p| p.name == b.steps[split_b].as_str()) {
                    prim.input_types[0].clone()
                } else {
                    continue;
                }
            } else {
                continue;
            };

            if type_at_a == suffix_input {
                let mut new_steps: Vec<String> = a.steps[..split_a].to_vec();
                new_steps.extend_from_slice(&b.steps[split_b..]);

                offspring.push(Candidate {
                    steps: new_steps,
                    fitness: 0.0,
                    cell_accuracy: 0.0,
                    generation: a.generation.max(b.generation) + 1,
                });
            }
        }
    }

    offspring
}

pub fn evolve(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    config: &EvolutionConfig,
) -> Option<EvolutionResult> {
    if train_pairs.is_empty() || test_inputs.is_empty() {
        return None;
    }

    let start = Instant::now();

    let colors = collect_colors(train_pairs);
    let catalog = build_primitive_catalog(&colors);

    let mut population: Vec<Candidate> = Vec::new();

    let beam_configs = [
        BeamConfig {
            beam_width: 50,
            max_depth: 3,
            timeout_ms: config.timeout_ms / 8,
        },
        BeamConfig {
            beam_width: 30,
            max_depth: 4,
            timeout_ms: config.timeout_ms / 8,
        },
        BeamConfig {
            beam_width: 80,
            max_depth: 2,
            timeout_ms: config.timeout_ms / 8,
        },
    ];

    for bc in &beam_configs {
        if start.elapsed().as_millis() as u64 > config.timeout_ms {
            break;
        }

        if let Some(sr) = beam_search(train_pairs, test_inputs, bc) {
            let steps: Vec<String> = sr.program.iter().map(|s| s.to_string()).collect();
            let (fitness, cell_accuracy) = score_candidate(&steps, train_pairs, &catalog);

            let candidate = Candidate {
                steps,
                fitness,
                cell_accuracy,
                generation: 1,
            };

            if fitness >= 1.0 - f64::EPSILON
                && let Some(result) =
                    try_produce_result(&candidate, train_pairs, test_inputs, &catalog)
            {
                return Some(result);
            }

            population.push(candidate);
        }
    }

    if population.len() < config.n_initial {
        for prim in &catalog {
            if prim.input_types.len() == 1 && prim.input_types[0] == DagType::Grid {
                let steps = vec![prim.name.to_string()];
                let (fitness, cell_accuracy) = score_candidate(&steps, train_pairs, &catalog);
                population.push(Candidate {
                    steps,
                    fitness,
                    cell_accuracy,
                    generation: 1,
                });
            }
            if population.len() >= config.n_initial * 2 {
                break;
            }
        }
    }

    {
        let mut singles = population.clone();
        singles.sort_by(|a, b| {
            b.cell_accuracy
                .partial_cmp(&a.cell_accuracy)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let top_singles: Vec<_> = singles.into_iter().take(config.n_parents).collect();
        for parent in &top_singles {
            let extensions = mutate_extend(parent, &catalog);
            for ext in extensions.into_iter().take(config.n_offspring) {
                let (fitness, cell_accuracy) = score_candidate(&ext.steps, train_pairs, &catalog);
                population.push(Candidate {
                    steps: ext.steps,
                    fitness,
                    cell_accuracy,
                    generation: 1,
                });
            }
        }
    }

    sort_population(&mut population);

    if let Some(best) = population.first()
        && best.fitness >= 1.0 - f64::EPSILON
        && let Some(result) = try_produce_result(best, train_pairs, test_inputs, &catalog)
    {
        return Some(result);
    }

    for generation in 2..=(config.n_generations as u8) {
        if start.elapsed().as_millis() as u64 > config.timeout_ms {
            break;
        }

        let parents: Vec<Candidate> = population.iter().take(config.n_parents).cloned().collect();

        let mut offspring: Vec<Candidate> = Vec::new();

        for parent in &parents {
            let swaps = mutate_swap(parent, &catalog);
            for child in swaps.into_iter().take(config.n_offspring) {
                offspring.push(child);
            }

            let extensions = mutate_extend(parent, &catalog);
            for child in extensions.into_iter().take(config.n_offspring) {
                offspring.push(child);
            }

            if let Some(child) = mutate_truncate(parent) {
                offspring.push(child);
            }
        }

        for i in 0..parents.len() {
            for j in (i + 1)..parents.len() {
                let crosses = crossover(&parents[i], &parents[j], &catalog);
                for child in crosses.into_iter().take(2) {
                    offspring.push(child);
                }
            }
        }

        for child in &mut offspring {
            let (fitness, cell_accuracy) = score_candidate(&child.steps, train_pairs, &catalog);
            child.fitness = fitness;
            child.cell_accuracy = cell_accuracy;
            child.generation = generation;
        }

        population.extend(offspring);
        sort_population(&mut population);
        population.truncate(config.n_initial * 2);

        if let Some(best) = population.first()
            && best.fitness >= 1.0 - f64::EPSILON
            && let Some(result) = try_produce_result(best, train_pairs, test_inputs, &catalog)
        {
            return Some(result);
        }
    }

    None
}

fn sort_population(pop: &mut [Candidate]) {
    pop.sort_by(|a, b| {
        b.fitness
            .partial_cmp(&a.fitness)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.cell_accuracy
                    .partial_cmp(&a.cell_accuracy)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.steps.len().cmp(&b.steps.len()))
    });
}

fn try_produce_result(
    candidate: &Candidate,
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    catalog: &[TypedPrimitive],
) -> Option<EvolutionResult> {
    for (input, expected) in train_pairs {
        match apply_program_by_name(&candidate.steps, catalog, input) {
            Some(ref result) if result == expected => {}
            _ => return None,
        }
    }

    let mut test_outputs = Vec::new();
    for test_input in test_inputs {
        match apply_program_by_name(&candidate.steps, catalog, test_input) {
            Some(output) => test_outputs.push(output),
            None => return None,
        }
    }

    Some(EvolutionResult {
        steps: candidate.steps.clone(),
        test_outputs,
        fitness: candidate.fitness,
        generation: candidate.generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_score_candidate_exact_match() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];
        let pairs = vec![(input, output)];
        let colors = collect_colors(&pairs);
        let catalog = build_primitive_catalog(&colors);

        let steps = vec!["rot90".to_string()];
        let (fitness, cell_accuracy) = score_candidate(&steps, &pairs, &catalog);
        assert!((fitness - 1.0).abs() < 1e-9);
        assert!((cell_accuracy - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_score_candidate_no_match() {
        let input = vec![vec![1, 2], vec![3, 4]];
        let output = vec![vec![9, 9], vec![9, 9]];
        let pairs = vec![(input, output)];
        let colors = collect_colors(&pairs);
        let catalog = build_primitive_catalog(&colors);

        let steps = vec!["rot90".to_string()];
        let (fitness, _cell_accuracy) = score_candidate(&steps, &pairs, &catalog);
        assert!(fitness < 1.0);
    }

    #[test]
    fn test_score_candidate_empty_program() {
        let input = vec![vec![1, 2], vec![3, 4]];
        let output = vec![vec![1, 2], vec![3, 4]];
        let pairs = vec![(input, output)];
        let colors = collect_colors(&pairs);
        let catalog = build_primitive_catalog(&colors);

        let steps: Vec<String> = vec![];
        let (fitness, cell_accuracy) = score_candidate(&steps, &pairs, &catalog);
        assert!((fitness - 1.0).abs() < 1e-9);
        assert!((cell_accuracy - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_mutate_extend_produces_candidates() {
        let colors = vec![0u8, 1, 2];
        let catalog = build_primitive_catalog(&colors);
        let parent = Candidate {
            steps: vec!["rot90".to_string()],
            fitness: 0.5,
            cell_accuracy: 0.7,
            generation: 1,
        };
        let extensions = mutate_extend(&parent, &catalog);
        assert!(!extensions.is_empty());
        for ext in &extensions {
            assert_eq!(ext.steps.len(), 2);
            assert_eq!(ext.steps[0], "rot90");
        }
    }

    #[test]
    fn test_mutate_truncate() {
        let parent = Candidate {
            steps: vec!["rot90".to_string(), "hmirror".to_string()],
            fitness: 0.5,
            cell_accuracy: 0.7,
            generation: 1,
        };
        let truncated = mutate_truncate(&parent);
        assert!(truncated.is_some());
        let t = truncated.unwrap();
        assert_eq!(t.steps, vec!["rot90".to_string()]);
    }

    #[test]
    fn test_mutate_truncate_single_step() {
        let parent = Candidate {
            steps: vec!["rot90".to_string()],
            fitness: 0.5,
            cell_accuracy: 0.7,
            generation: 1,
        };
        assert!(mutate_truncate(&parent).is_none());
    }

    #[test]
    fn test_evolve_simple_rot90() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];
        let train_pairs = vec![(input.clone(), output)];
        let test_inputs = vec![input];

        let config = EvolutionConfig {
            n_initial: 8,
            n_parents: 4,
            n_offspring: 4,
            n_generations: 2,
            timeout_ms: 5_000,
        };

        let result = evolve(&train_pairs, &test_inputs, &config);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!((r.fitness - 1.0).abs() < 1e-9);
        assert!(!r.test_outputs.is_empty());
    }

    #[test]
    fn test_crossover_type_safety() {
        let colors = vec![0u8, 1, 2];
        let catalog = build_primitive_catalog(&colors);

        let a = Candidate {
            steps: vec!["rot90".to_string(), "hmirror".to_string()],
            fitness: 0.5,
            cell_accuracy: 0.5,
            generation: 1,
        };
        let b = Candidate {
            steps: vec!["vmirror".to_string(), "rot180".to_string()],
            fitness: 0.5,
            cell_accuracy: 0.5,
            generation: 1,
        };

        let crosses = crossover(&a, &b, &catalog);
        for child in &crosses {
            assert!(!child.steps.is_empty());
        }
    }

    #[test]
    fn test_evolve_color_replace() {
        let input = vec![vec![1, 0, 2], vec![2, 1, 0]];
        let output = vec![vec![3, 0, 2], vec![2, 3, 0]];
        let train_pairs = vec![(input.clone(), output)];
        let test_inputs = vec![input];

        let config = EvolutionConfig {
            n_initial: 16,
            n_parents: 4,
            n_offspring: 4,
            n_generations: 3,
            timeout_ms: 5_000,
        };

        let result = evolve(&train_pairs, &test_inputs, &config);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!((r.fitness - 1.0).abs() < 1e-9);
    }
}
