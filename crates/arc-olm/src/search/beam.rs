//! Fitness beam search — parallel, deterministic, with Merkle dedup.
//!
//! At each depth:
//! 1. Take the top-K partial programs (by fitness toward target)
//! 2. Expand each by applying all type-valid next primitives
//! 3. Score each expansion by fitness (pixel accuracy vs target)
//! 4. Dedup by hash (Merkle property: same result = same hash)
//! 5. Prune: keep top-K, kill the rest forever
//! 6. Check for exact match (fitness = 1.0)
//!
//! Parallel via Rayon: each beam element expanded independently.

use rayon::prelude::*;
use std::collections::HashSet;
use crate::Grid;
use super::enumerate::*;

/// Result of a successful search.
#[derive(Clone, Debug)]
pub struct SearchResult {
    pub program: Vec<&'static str>,
    pub output: Grid,
    pub depth: usize,
    pub fitness: f64,
}

/// Beam search parameters.
pub struct BeamConfig {
    pub beam_width: usize,      // how many partial programs to keep per depth
    pub max_depth: usize,       // maximum program depth
    pub timeout_ms: u64,        // wall-clock timeout
}

impl Default for BeamConfig {
    fn default() -> Self {
        Self {
            beam_width: 200,
            max_depth: 8,
            timeout_ms: 30_000,
        }
    }
}

/// Run fitness beam search.
///
/// For each training pair, search for a program that transforms input → output.
/// The program must work for ALL training pairs.
///
/// Returns the first program that achieves fitness = 1.0 on all pairs.
pub fn beam_search(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    config: &BeamConfig,
) -> Option<SearchResult> {
    if train_pairs.is_empty() { return None; }

    let start = std::time::Instant::now();

    // Collect colors from all training grids
    let mut colors: Vec<u8> = Vec::new();
    for (inp, out) in train_pairs {
        for row in inp { for &c in row { if !colors.contains(&c) { colors.push(c); } } }
        for row in out { for &c in row { if !colors.contains(&c) { colors.push(c); } } }
    }
    colors.sort();

    // Build primitive catalog
    let catalog = build_primitive_catalog(&colors);

    // Use the first training pair for fitness scoring during search
    // Verify on ALL pairs before accepting
    let target_output = &train_pairs[0].1;

    // Initialize beam with the input grid
    let initial = PartialProgram {
        steps: vec![],
        current_value: DagValue::Grid(train_pairs[0].0.clone()),
        current_type: DagType::Grid,
        fitness: compute_fitness(&train_pairs[0].0, target_output),
        hash: quick_hash(&train_pairs[0].0),
    };

    let mut beam = vec![initial];
    let mut seen_hashes: HashSet<u64> = HashSet::new();
    seen_hashes.insert(beam[0].hash);

    for depth in 0..config.max_depth {
        if start.elapsed().as_millis() as u64 > config.timeout_ms {
            break;
        }

        // Expand all beam elements in parallel
        let new_candidates: Vec<PartialProgram> = beam.par_iter()
            .flat_map(|partial| {
                let mut expansions = Vec::new();

                for prim in &catalog {
                    // Type check: does this primitive accept the current type?
                    if prim.input_types.len() == 1 && prim.input_types[0] == partial.current_type {
                        let args = vec![partial.current_value.clone()];
                        let apply_result = std::panic::catch_unwind(
                            std::panic::AssertUnwindSafe(|| (prim.apply)(&args))
                        );
                        if let Ok(Some(result)) = apply_result {
                            let fitness = if let DagValue::Grid(ref g) = result {
                                compute_fitness(g, target_output)
                            } else {
                                0.0 // non-Grid intermediate values get 0 fitness
                            };

                            let hash = match &result {
                                DagValue::Grid(g) => quick_hash(g),
                                _ => 0,
                            };

                            let mut new_steps = partial.steps.clone();
                            new_steps.push(prim.name);

                            expansions.push(PartialProgram {
                                steps: new_steps,
                                current_type: result.dag_type(),
                                current_value: result,
                                fitness,
                                hash,
                            });
                        }
                    }

                    // Binary operations: primitive needs (current_type, Grid)
                    // where the second arg is the original input
                    if prim.input_types.len() == 2
                        && prim.input_types[0] == partial.current_type
                        && prim.input_types[1] == DagType::Grid
                    {
                        let args = vec![
                            partial.current_value.clone(),
                            DagValue::Grid(train_pairs[0].0.clone()),
                        ];
                        let apply_result = std::panic::catch_unwind(
                            std::panic::AssertUnwindSafe(|| (prim.apply)(&args))
                        );
                        if let Ok(Some(result)) = apply_result {
                            let fitness = if let DagValue::Grid(ref g) = result {
                                compute_fitness(g, target_output)
                            } else { 0.0 };

                            let hash = match &result {
                                DagValue::Grid(g) => quick_hash(g),
                                _ => 0,
                            };

                            let mut new_steps = partial.steps.clone();
                            new_steps.push(prim.name);

                            expansions.push(PartialProgram {
                                steps: new_steps,
                                current_type: result.dag_type(),
                                current_value: result,
                                fitness,
                                hash,
                            });
                        }
                    }
                }

                expansions
            })
            .collect();

        // Dedup by hash
        let mut deduped: Vec<PartialProgram> = Vec::new();
        for candidate in new_candidates {
            if candidate.hash != 0 && !seen_hashes.contains(&candidate.hash) {
                seen_hashes.insert(candidate.hash);
                deduped.push(candidate);
            } else if candidate.hash == 0 {
                // Non-grid intermediate values: keep without dedup
                deduped.push(candidate);
            }
        }

        // Check for exact match (fitness = 1.0)
        for candidate in &deduped {
            if candidate.fitness >= 0.9999 {
                if let DagValue::Grid(ref result_grid) = candidate.current_value {
                    // Verify on ALL training pairs
                    let all_match = verify_on_all_pairs(
                        &candidate.steps, &catalog, train_pairs,
                    );
                    if all_match {
                        // Apply to test inputs
                        let test_output = apply_program(
                            &candidate.steps, &catalog,
                            &test_inputs[0],
                        );
                        if let Some(output) = test_output {
                            return Some(SearchResult {
                                program: candidate.steps.clone(),
                                output,
                                depth: depth + 1,
                                fitness: 1.0,
                            });
                        }
                    }
                }
            }
        }

        // Sort by fitness, keep top beam_width
        deduped.sort_by(|a, b| b.fitness.partial_cmp(&a.fitness).unwrap_or(std::cmp::Ordering::Equal));
        deduped.truncate(config.beam_width);

        beam = deduped;

        if beam.is_empty() { break; }
    }

    None
}

/// Verify a program works on ALL training pairs.
fn verify_on_all_pairs(
    steps: &[&'static str],
    catalog: &[TypedPrimitive],
    pairs: &[(Grid, Grid)],
) -> bool {
    for (input, expected) in pairs {
        match apply_program(steps, catalog, input) {
            Some(result) if result == *expected => continue,
            _ => return false,
        }
    }
    true
}

/// Apply a program (sequence of primitive names) to an input grid.
fn apply_program(
    steps: &[&'static str],
    catalog: &[TypedPrimitive],
    input: &Grid,
) -> Option<Grid> {
    let mut current = DagValue::Grid(input.clone());

    for &step_name in steps {
        let prim = catalog.iter().find(|p| p.name == step_name)?;

        let args = if prim.input_types.len() == 1 {
            vec![current.clone()]
        } else if prim.input_types.len() == 2 && prim.input_types[1] == DagType::Grid {
            vec![current.clone(), DagValue::Grid(input.clone())]
        } else {
            return None;
        };

        current = (prim.apply)(&args)?;
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
    fn test_beam_search_rot90() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];

        let result = beam_search(
            &[(input.clone(), output.clone())],
            &[input],
            &BeamConfig { beam_width: 50, max_depth: 3, timeout_ms: 5000 },
        );

        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.output, output);
        assert_eq!(r.fitness, 1.0);
    }

    #[test]
    fn test_beam_search_replace_color() {
        let input = vec![vec![1, 0, 2], vec![2, 1, 0]];
        let output = vec![vec![3, 0, 2], vec![2, 3, 0]];

        let result = beam_search(
            &[(input.clone(), output.clone())],
            &[input],
            &BeamConfig { beam_width: 50, max_depth: 3, timeout_ms: 5000 },
        );

        assert!(result.is_some());
        assert_eq!(result.unwrap().output, output);
    }
}
