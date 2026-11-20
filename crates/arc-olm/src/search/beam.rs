use super::enumerate::*;
use crate::Grid;
use rayon::prelude::*;
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct SearchResult {
    pub program: Vec<&'static str>,
    pub output: Grid,
    pub depth: usize,
    pub fitness: f64,
}

pub struct BeamConfig {
    pub beam_width: usize,
    pub max_depth: usize,
    pub timeout_ms: u64,
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

pub fn beam_search(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    config: &BeamConfig,
) -> Option<SearchResult> {
    beam_search_steered(train_pairs, test_inputs, config, None)
}

pub fn beam_search_steered(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    config: &BeamConfig,
    steering: Option<&super::steering::SteeringModel>,
) -> Option<SearchResult> {
    if train_pairs.is_empty() {
        return None;
    }

    let start = std::time::Instant::now();

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

    let target_output = &train_pairs[0].1;

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

        let new_candidates: Vec<PartialProgram> = beam
            .par_iter()
            .flat_map(|partial| {
                let mut expansions = Vec::new();

                let prim_order: Vec<usize> = if let Some(model) = steering {
                    let scores = super::steering::steer(
                        model,
                        &partial.current_value,
                        target_output,
                        &partial.current_type,
                    );
                    let mut ordered: Vec<usize> = Vec::new();
                    for (name, _score) in &scores {
                        for (idx, prim) in catalog.iter().enumerate() {
                            if ordered.contains(&idx) {
                                continue;
                            }
                            let matches =
                                prim.name == name.as_str() || matches_hodel_name(prim.name, name);
                            if matches {
                                ordered.push(idx);
                            }
                        }
                    }
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
                    if prim.input_types.len() == 1 && prim.input_types[0] == partial.current_type {
                        let args = vec![partial.current_value.clone()];
                        let apply_result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (prim.apply)(&args)
                            }));
                        if let Ok(Some(result)) = apply_result {
                            let fitness = if let DagValue::Grid(ref g) = result {
                                compute_fitness(g, target_output)
                            } else {
                                0.0
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

                    if prim.input_types.len() == 2
                        && prim.input_types[0] == partial.current_type
                        && prim.input_types[1] == DagType::Grid
                    {
                        let args = vec![
                            partial.current_value.clone(),
                            DagValue::Grid(train_pairs[0].0.clone()),
                        ];
                        let apply_result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (prim.apply)(&args)
                            }));
                        if let Ok(Some(result)) = apply_result {
                            let fitness = if let DagValue::Grid(ref g) = result {
                                compute_fitness(g, target_output)
                            } else {
                                0.0
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
                }

                expansions
            })
            .collect();

        let mut deduped: Vec<PartialProgram> = Vec::new();
        for candidate in new_candidates {
            if candidate.hash != 0 && !seen_hashes.contains(&candidate.hash) {
                seen_hashes.insert(candidate.hash);
                deduped.push(candidate);
            } else if candidate.hash == 0 {
                deduped.push(candidate);
            }
        }

        for candidate in &deduped {
            if candidate.fitness >= 0.9999
                && let DagValue::Grid(ref result_grid) = candidate.current_value
            {
                let all_match = verify_on_all_pairs(&candidate.steps, &catalog, train_pairs);
                if all_match {
                    let test_output = apply_program(&candidate.steps, &catalog, &test_inputs[0]);
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

        deduped.sort_by(|a, b| {
            b.fitness
                .partial_cmp(&a.fitness)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        deduped.truncate(config.beam_width);

        beam = deduped;

        if beam.is_empty() {
            break;
        }
    }

    None
}

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

fn apply_program(steps: &[&'static str], catalog: &[TypedPrimitive], input: &Grid) -> Option<Grid> {
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

fn matches_hodel_name(catalog_name: &str, hodel_name: &str) -> bool {
    match hodel_name {
        "objects" => catalog_name.starts_with("obj_"),
        "replace" => catalog_name.starts_with("replace_"),
        "switch" => catalog_name.starts_with("switch_"),
        "fill" => {
            catalog_name.starts_with("fill_idx_") || catalog_name.starts_with("underfill_idx_")
        }
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
        "asobject" => false,
        "occurrences" => false,
        "hupscale" | "vupscale" => catalog_name.starts_with("upscale_"),
        "compress" => catalog_name == "compress",
        _ => catalog_name == hodel_name || catalog_name.starts_with(hodel_name),
    }
}

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static EXHAUST_ITER_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn exhaustive_search(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    max_depth: usize,
    timeout_ms: u64,
) -> Option<SearchResult> {
    if train_pairs.is_empty() || test_inputs.is_empty() {
        return None;
    }

    let start = Instant::now();

    EXHAUST_ITER_COUNT.store(0, Ordering::Relaxed);

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

    let target = &train_pairs[0].1;
    let input = &train_pairs[0].0;
    let mut seen = HashSet::new();
    seen.insert(quick_hash(input));

    for depth in 1..=max_depth {
        if start.elapsed().as_millis() as u64 > timeout_ms {
            break;
        }

        let mut steps: Vec<&'static str> = Vec::new();
        let initial = DagValue::Grid(input.clone());

        if let Some(result) = exhaustive_dfs(
            &initial,
            &DagType::Grid,
            input,
            target,
            &catalog,
            &mut steps,
            depth,
            &mut seen,
            train_pairs,
            test_inputs,
            &start,
            timeout_ms,
        ) {
            return Some(result);
        }
    }

    None
}

#[allow(clippy::too_many_arguments)]
fn exhaustive_dfs(
    current: &DagValue,
    current_type: &DagType,
    input_grid: &Grid,
    target: &Grid,
    catalog: &[TypedPrimitive],
    steps: &mut Vec<&'static str>,
    remaining_depth: usize,
    seen: &mut HashSet<u64>,
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    start: &Instant,
    timeout_ms: u64,
) -> Option<SearchResult> {
    if remaining_depth == 0 {
        return None;
    }

    for prim in catalog {
        let count = EXHAUST_ITER_COUNT.fetch_add(1, Ordering::Relaxed);
        if count.is_multiple_of(5_000)
            && count > 0
            && start.elapsed().as_millis() as u64 > timeout_ms
        {
            return None;
        }

        let accepts = (prim.input_types.len() == 1 && prim.input_types[0] == *current_type)
            || (prim.input_types.len() == 2
                && prim.input_types[0] == *current_type
                && prim.input_types[1] == DagType::Grid);

        if !accepts {
            continue;
        }

        let args = if prim.input_types.len() == 1 {
            vec![current.clone()]
        } else {
            vec![current.clone(), DagValue::Grid(input_grid.clone())]
        };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (prim.apply)(&args)));

        let new_value = match result {
            Ok(Some(v)) => v,
            _ => continue,
        };

        steps.push(prim.name);

        if let DagValue::Grid(ref g) = new_value {
            let h = quick_hash(g);
            if !seen.contains(&h) {
                seen.insert(h);

                if g == target && verify_on_all_pairs(steps, catalog, train_pairs) {
                    let mut test_outputs = Vec::new();
                    let mut all_ok = true;
                    for test_in in test_inputs {
                        if let Some(out) = apply_program(steps, catalog, test_in) {
                            test_outputs.push(out);
                        } else {
                            all_ok = false;
                            break;
                        }
                    }
                    if all_ok && !test_outputs.is_empty() {
                        let result = SearchResult {
                            program: steps.clone(),
                            output: test_outputs[0].clone(),
                            depth: steps.len(),
                            fitness: 1.0,
                        };
                        steps.pop();
                        return Some(result);
                    }
                }
            }
        }

        if remaining_depth > 1 {
            let new_type = new_value.dag_type();
            if let Some(result) = exhaustive_dfs(
                &new_value,
                &new_type,
                input_grid,
                target,
                catalog,
                steps,
                remaining_depth - 1,
                seen,
                train_pairs,
                test_inputs,
                start,
                timeout_ms,
            ) {
                return Some(result);
            }
        }

        steps.pop();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::float_cmp)]
    #[test]
    fn test_beam_search_rot90() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];

        let result = beam_search(
            &[(input.clone(), output.clone())],
            &[input],
            &BeamConfig {
                beam_width: 50,
                max_depth: 3,
                timeout_ms: 5000,
            },
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
            &BeamConfig {
                beam_width: 50,
                max_depth: 3,
                timeout_ms: 5000,
            },
        );

        assert!(result.is_some());
        assert_eq!(result.unwrap().output, output);
    }

    #[allow(clippy::float_cmp)]
    #[test]
    fn test_exhaustive_search_rot90() {
        let input = vec![vec![1, 0], vec![0, 0]];
        let output = vec![vec![0, 1], vec![0, 0]];

        let result = exhaustive_search(&[(input.clone(), output.clone())], &[input], 3, 5000);

        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.output, output);
        assert_eq!(r.fitness, 1.0);
    }

    #[test]
    fn test_exhaustive_search_replace_color() {
        let input = vec![vec![1, 0, 2], vec![2, 1, 0]];
        let output = vec![vec![3, 0, 2], vec![2, 3, 0]];

        let result = exhaustive_search(&[(input.clone(), output.clone())], &[input], 3, 5000);

        assert!(result.is_some());
        assert_eq!(result.unwrap().output, output);
    }

    #[test]
    fn test_exhaustive_search_empty_input() {
        assert!(exhaustive_search(&[], &[], 3, 5000).is_none());
    }
}
