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
    beam_search_steered(train_pairs, test_inputs, config, None)
}

/// Beam search with optional neural steering model.
pub fn beam_search_steered(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    config: &BeamConfig,
    steering: Option<&super::steering::SteeringModel>,
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

                // If steering model available, score and sort primitives
                let prim_order: Vec<usize> = if let Some(model) = steering {
                    let scores = super::steering::steer(
                        model,
                        &partial.current_value,
                        target_output,
                        &partial.current_type,
                    );
                    // Map scored names to catalog indices with fuzzy matching
                    // Hodel names don't match Rust catalog exactly:
                    //   "objects" → "obj_TTT", "obj_TFT", "obj_FTT", ...
                    //   "replace" → "replace_1_3", "replace_0_2", ...
                    //   "fill" → "fill_idx_0", "fill_idx_1", ...
                    let mut ordered: Vec<usize> = Vec::new();
                    for (name, _score) in &scores {
                        for (idx, prim) in catalog.iter().enumerate() {
                            if ordered.contains(&idx) { continue; }
                            let matches = prim.name == name.as_str()
                                || matches_hodel_name(prim.name, name);
                            if matches {
                                ordered.push(idx);
                            }
                        }
                    }
                    // Add any catalog entries not covered by the model
                    for idx in 0..catalog.len() {
                        if !ordered.contains(&idx) {
                            ordered.push(idx);
                        }
                    }
                    ordered
                } else {
                    (0..catalog.len()).collect()
                };

                for &prim_idx in &prim_order {
                    let prim = &catalog[prim_idx];
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

/// Match a Hodel operation name to a Rust catalog entry name.
/// Hodel uses names like "objects", "replace", "fill", "ofcolor".
/// Rust catalog uses parameterized names like "obj_TTT", "replace_1_3", "fill_idx_2".
fn matches_hodel_name(catalog_name: &str, hodel_name: &str) -> bool {
    match hodel_name {
        "objects" => catalog_name.starts_with("obj_"),
        "replace" => catalog_name.starts_with("replace_"),
        "switch" => catalog_name.starts_with("switch_"),
        "fill" => catalog_name.starts_with("fill_idx_") || catalog_name.starts_with("underfill_idx_"),
        "ofcolor" => catalog_name.starts_with("ofcolor_"),
        "paint" => catalog_name.starts_with("paint_all_") || catalog_name == "cover",
        "upscale" => catalog_name.starts_with("upscale_"),
        "crop" => catalog_name == "trim" || catalog_name.starts_with("crop"),
        "hconcat" => catalog_name.starts_with("hconcat"),
        "vconcat" => catalog_name.starts_with("vconcat"),
        "hsplit" => catalog_name.starts_with("hsplit"),
        "vsplit" => catalog_name.starts_with("vsplit"),
        "partition" | "fgpartition" => catalog_name.starts_with("obj_"),
        "frontiers" => catalog_name.starts_with("obj_"),
        "asindices" => catalog_name == "obj_positions" || catalog_name.starts_with("ofcolor_"),
        "asobject" => false, // not in catalog yet
        "occurrences" => false,
        "hupscale" | "vupscale" => catalog_name.starts_with("upscale_"),
        "compress" => catalog_name == "compress",
        _ => {
            // Direct name match or prefix match
            catalog_name == hodel_name || catalog_name.starts_with(hodel_name)
        }
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
