//! LLM-guided DAG search — uses Ollama to pick operations at each step.
//!
//! Instead of exhaustive enumeration, this engine asks a local LLM (via Ollama)
//! to choose which typed primitive to apply at each step.  The LLM sees the
//! training examples, the current state, the target, and a numbered list of
//! type-valid operations.  It replies with a number and we apply that operation.
//!
//! This is Engine 5 in the solver pipeline — runs after evolution if `--llm`
//! is passed and time remains.

use crate::Grid;
use crate::search::enumerate::*;
use std::time::Instant;

// ============================================================
// Configuration
// ============================================================

pub struct LlmConfig {
    pub model: String,
    pub base_url: String,
    pub max_depth: usize,
    pub n_attempts: usize,
    pub timeout_ms: u64,
    pub temperature: f32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "qwen2.5-coder:7b".to_string(),
            base_url: "http://localhost:11434".to_string(),
            max_depth: 6,
            n_attempts: 5,
            timeout_ms: 30_000,
            temperature: 0.3,
        }
    }
}

// ============================================================
// Result
// ============================================================

pub struct LlmResult {
    pub program: Vec<String>,
    pub test_outputs: Vec<Grid>,
    pub attempt: usize,
}

// ============================================================
// Grid formatting
// ============================================================

fn format_grid(grid: &Grid) -> String {
    let h = grid.len();
    let w = if h > 0 { grid[0].len() } else { 0 };
    let mut s = format!("({}x{}):\n", h, w);
    for row in grid {
        s.push_str(&format!("  {:?}\n", row));
    }
    s
}

// ============================================================
// Ollama HTTP call
// ============================================================

fn call_ollama(prompt: &str, config: &LlmConfig) -> Option<String> {
    let url = format!("{}/api/chat", config.base_url);
    let body = serde_json::json!({
        "model": config.model,
        "messages": [
            {"role": "system", "content": "You solve ARC-AGI puzzles by selecting grid operations. Reply with ONLY the operation name. No explanation."},
            {"role": "user", "content": prompt}
        ],
        "stream": false,
        "options": {
            "temperature": config.temperature,
            "num_predict": 30
        }
    });

    let response = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(30))
        .set("Content-Type", "application/json")
        .send_bytes(&serde_json::to_vec(&body).ok()?)
        .ok()?;

    let text = response.into_string().ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    // /api/chat returns { "message": { "content": "..." } }
    json["message"]["content"].as_str()
        .or_else(|| json["response"].as_str())
        .map(|s| s.trim().to_string())
}

// ============================================================
// Response parsing
// ============================================================

/// Parse the LLM response into a 0-based index.
/// Handles formats like: "3", "3.", "3. rot90", "rot90", "  3  ".
fn parse_choice(response: &str, options: &[(&str, &str)]) -> Option<usize> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Try to parse a leading number (1-based from prompt, convert to 0-based)
    let num_str: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !num_str.is_empty() {
        if let Ok(n) = num_str.parse::<usize>() {
            if n >= 1 && n <= options.len() {
                return Some(n - 1);
            }
        }
    }

    // Try to match by operation name (case-insensitive substring)
    let lower = trimmed.to_lowercase();
    for (i, (name, _desc)) in options.iter().enumerate() {
        if lower.contains(&name.to_lowercase()) {
            return Some(i);
        }
    }

    None
}

// ============================================================
// Build prompt
// ============================================================

fn build_prompt(
    train_pairs: &[(Grid, Grid)],
    current_value: &DagValue,
    current_type: &DagType,
    target: &Grid,
    options: &[(&str, &str)], // (name, description)
) -> String {
    let mut prompt = String::from(
        "You are solving an ARC-AGI task. Study the training examples:\n\n",
    );

    for (i, (inp, out)) in train_pairs.iter().enumerate() {
        prompt.push_str(&format!("Example {}:\nInput {}\nOutput {}\n",
            i + 1, format_grid(inp), format_grid(out)));
    }

    prompt.push_str(&format!("Current state type: {:?}\n", current_type));
    match current_value {
        DagValue::Grid(g) => {
            prompt.push_str(&format!("Current value {}\n", format_grid(g)));
        }
        DagValue::Objects(objs) => {
            prompt.push_str(&format!("Current value: {} objects\n", objs.len()));
        }
        DagValue::Object(obj) => {
            prompt.push_str(&format!("Current value: object with {} cells\n", obj.size()));
        }
        DagValue::Indices(idx) => {
            prompt.push_str(&format!("Current value: {} positions\n", idx.len()));
        }
        DagValue::Color(c) => {
            prompt.push_str(&format!("Current value: color {}\n", c));
        }
        DagValue::Int(n) => {
            prompt.push_str(&format!("Current value: {}\n", n));
        }
    }

    prompt.push_str(&format!("\nTarget output {}\n", format_grid(target)));

    prompt.push_str("Available operations (pick ONE by number):\n");
    for (i, (name, desc)) in options.iter().enumerate() {
        prompt.push_str(&format!("{}. {} — {}\n", i + 1, name, desc));
    }

    prompt.push_str("\nWhich operation should be applied next? Reply with ONLY the number.\n");
    prompt
}

// ============================================================
// Describe primitives
// ============================================================

fn describe_primitive(name: &str, input_types: &[DagType], output_type: &DagType) -> String {
    let type_sig = format!(
        "{} -> {:?}",
        input_types.iter().map(|t| format!("{:?}", t)).collect::<Vec<_>>().join(" + "),
        output_type,
    );

    let desc = match name {
        "rot90" => "Rotate 90 degrees clockwise",
        "rot180" => "Rotate 180 degrees",
        "rot270" => "Rotate 270 degrees clockwise",
        "hmirror" => "Flip top-to-bottom (horizontal mirror)",
        "vmirror" => "Flip left-to-right (vertical mirror)",
        "dmirror" => "Mirror along main diagonal",
        "cmirror" => "Mirror along counter-diagonal",
        "tophalf" => "Take top half of grid",
        "bottomhalf" => "Take bottom half of grid",
        "lefthalf" => "Take left half of grid",
        "righthalf" => "Take right half of grid",
        "compress" => "Remove duplicate rows/columns",
        "trim" => "Remove border of background color",
        "hconcat_self" => "Concatenate grid with itself horizontally",
        "vconcat_self" => "Concatenate grid with itself vertically",
        "hconcat_vm" => "Concat grid with its vertical mirror",
        "vconcat_hm" => "Concat grid with its horizontal mirror",
        "hconcat_vm_r" => "Concat vertical mirror with grid",
        "vconcat_hm_r" => "Concat horizontal mirror with grid",
        "argmax_size" => "Select largest object",
        "argmin_size" => "Select smallest object",
        "argmax_height" => "Select tallest object",
        "argmin_height" => "Select shortest object",
        "argmax_width" => "Select widest object",
        "first_obj" => "Select first object",
        "last_obj" => "Select last object",
        "subgrid" => "Extract subgrid of object",
        "cover" => "Erase object from grid",
        "move_down" => "Move object down 1 cell",
        "move_up" => "Move object up 1 cell",
        "move_right" => "Move object right 1 cell",
        "move_left" => "Move object left 1 cell",
        "idx_backdrop" => "Bounding box of positions",
        "idx_delta" => "Holes inside bounding box",
        "idx_neighbors" => "Neighboring positions",
        "obj_box" => "Object bounding box positions",
        "corners" => "Object corner positions",
        "inbox" => "Positions inside object",
        "outbox" => "Positions outside object bounding box",
        "obj_delta" => "Holes inside object bounding box",
        "obj_backdrop" => "Object bounding box fill",
        "obj_positions" => "All object cell positions",
        "grid_height" => "Grid height",
        "grid_width" => "Grid width",
        "obj_height" => "Object height",
        "obj_width" => "Object width",
        "obj_size" => "Object cell count",
        "obj_color" => "Object primary color",
        "mostcolor" => "Most common color in grid",
        "leastcolor" => "Least common color in grid",
        _ if name.starts_with("replace_") => "Replace one color with another",
        _ if name.starts_with("switch_") => "Swap two colors",
        _ if name.starts_with("ofcolor_") => "Get positions of a specific color",
        _ if name.starts_with("fill_idx_") => "Fill positions with a color",
        _ if name.starts_with("underfill_idx_") => "Fill positions (under existing)",
        _ if name.starts_with("paint_all_") => "Paint all objects with a color",
        _ if name.starts_with("cf") && name.ends_with("_argmax") => "Filter by color, pick largest",
        _ if name.starts_with("obj_T") || name.starts_with("obj_F") => "Extract objects",
        _ if name.starts_with("upscale_") => "Upscale grid by factor",
        _ if name.starts_with("downscale_") => "Downscale grid by factor",
        _ if name.starts_with("mapply_") => "Apply region function to all objects",
        _ => name,
    };

    format!("{} [{}]", desc, type_sig)
}

// ============================================================
// Get type-valid next operations
// ============================================================

fn get_valid_operations<'a>(
    current_type: &DagType,
    catalog: &'a [TypedPrimitive],
    _input_grid: &Grid,
) -> Vec<(usize, &'a str, String)> {
    // (catalog_index, name, description)
    let mut ops = Vec::new();
    for (idx, prim) in catalog.iter().enumerate() {
        if prim.input_types.is_empty() {
            continue;
        }
        // The first input type must match the current state type
        if &prim.input_types[0] != current_type {
            continue;
        }
        // If the primitive needs a second Grid arg, we supply the original input
        if prim.input_types.len() == 2 && prim.input_types[1] == DagType::Grid {
            // ok, we can supply the original input grid
        } else if prim.input_types.len() > 1 {
            // multi-arg primitives we can't feed — skip
            continue;
        }
        let desc = describe_primitive(prim.name, &prim.input_types, &prim.output_type);
        ops.push((idx, prim.name, desc));
    }
    // Limit to a reasonable number (avoid overwhelming the LLM)
    if ops.len() > 30 {
        ops.truncate(30);
    }
    ops
}

// ============================================================
// Single attempt
// ============================================================

fn single_attempt(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    catalog: &[TypedPrimitive],
    config: &LlmConfig,
    temperature: f32,
    start: &Instant,
    verbose: bool,
) -> Option<(Vec<String>, Vec<Grid>)> {
    // Work on the first training pair as the pilot
    let (pilot_input, pilot_target) = &train_pairs[0];
    let mut current = DagValue::Grid(pilot_input.clone());
    let mut current_type = DagType::Grid;
    let mut program: Vec<String> = Vec::new();

    let mut attempt_config = LlmConfig {
        temperature,
        ..LlmConfig {
            model: config.model.clone(),
            base_url: config.base_url.clone(),
            max_depth: config.max_depth,
            n_attempts: config.n_attempts,
            timeout_ms: config.timeout_ms,
            temperature,
        }
    };

    for depth in 0..config.max_depth {
        // Check wall-clock timeout
        if start.elapsed().as_millis() as u64 >= config.timeout_ms {
            if verbose {
                eprintln!("[llm] timeout at depth {}", depth);
            }
            return None;
        }

        // Get type-valid operations
        let ops = get_valid_operations(&current_type, catalog, pilot_input);
        if ops.is_empty() {
            if verbose {
                eprintln!("[llm] no valid operations at depth {} for type {:?}", depth, current_type);
            }
            return None;
        }

        // Build options list for prompt
        let options: Vec<(&str, &str)> = ops.iter()
            .map(|(_, name, desc)| (*name, desc.as_str()))
            .collect();

        // Build and send prompt
        let prompt = build_prompt(train_pairs, &current, &current_type, pilot_target, &options);

        attempt_config.temperature = temperature;
        let response = match call_ollama(&prompt, &attempt_config) {
            Some(r) => r,
            None => {
                if verbose {
                    eprintln!("[llm] ollama call failed at depth {}", depth);
                }
                return None;
            }
        };

        if verbose {
            eprintln!("[llm] depth {}: LLM responded: {:?}", depth, response);
        }

        // Parse choice
        let choice_idx = match parse_choice(&response, &options) {
            Some(i) => i,
            None => {
                if verbose {
                    eprintln!("[llm] could not parse response at depth {}: {:?}", depth, response);
                }
                // Fall back to first option
                0
            }
        };

        let (cat_idx, chosen_name, _) = &ops[choice_idx];
        if verbose {
            eprintln!("[llm] depth {}: applying {} (option {})", depth, chosen_name, choice_idx + 1);
        }
        program.push(chosen_name.to_string());

        // Apply the chosen operation
        let prim = &catalog[*cat_idx];
        let args = if prim.input_types.len() == 1 {
            vec![current.clone()]
        } else if prim.input_types.len() == 2 && prim.input_types[1] == DagType::Grid {
            vec![current.clone(), DagValue::Grid(pilot_input.clone())]
        } else {
            return None;
        };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (prim.apply)(&args)));
        current = match result {
            Ok(Some(v)) => v,
            _ => {
                if verbose {
                    eprintln!("[llm] operation {} failed at depth {}", chosen_name, depth);
                }
                return None;
            }
        };
        current_type = current.dag_type();

        // If we have a Grid, check if it matches the target for ALL training pairs
        if let DagValue::Grid(ref g) = current {
            if g == pilot_target {
                // Verify on all training pairs
                let all_match = train_pairs.iter().all(|(inp, out)| {
                    match apply_program(&program, catalog, inp) {
                        Some(result) => &result == out,
                        None => false,
                    }
                });

                if all_match {
                    if verbose {
                        eprintln!("[llm] SOLVED at depth {}! Program: {:?}", depth + 1, program);
                    }
                    // Apply to test inputs
                    let mut test_outputs = Vec::new();
                    for test_input in test_inputs {
                        match apply_program(&program, catalog, test_input) {
                            Some(g) => test_outputs.push(g),
                            None => return None,
                        }
                    }
                    return Some((program, test_outputs));
                } else if verbose {
                    eprintln!("[llm] depth {}: matches pilot but not all training pairs", depth + 1);
                }
            } else if verbose {
                let fitness = compute_fitness(g, pilot_target);
                eprintln!("[llm] depth {}: fitness = {:.2}", depth + 1, fitness);
            }
        }
    }

    None
}

// ============================================================
// Apply a program
// ============================================================

fn apply_program(steps: &[String], catalog: &[TypedPrimitive], input: &Grid) -> Option<Grid> {
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

// ============================================================
// Main entry point
// ============================================================

/// Run LLM-guided search: try multiple attempts with increasing temperature.
///
/// Each attempt is an independent search starting from the input grid.
/// The LLM is queried at each step to choose which operation to apply.
pub fn llm_guided_search(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    config: &LlmConfig,
    verbose: bool,
) -> Option<LlmResult> {
    if train_pairs.is_empty() || test_inputs.is_empty() {
        return None;
    }

    let start = Instant::now();

    // Collect all colors from training pairs for catalog
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

    // Temperature schedule: increasing across attempts
    let temperatures: Vec<f32> = (0..config.n_attempts)
        .map(|i| {
            if config.n_attempts <= 1 {
                config.temperature
            } else {
                0.2 + (i as f32) * 0.8 / (config.n_attempts - 1) as f32
            }
        })
        .collect();

    if verbose {
        eprintln!(
            "[llm] starting LLM-guided search: model={}, max_depth={}, attempts={}, temps={:?}",
            config.model, config.max_depth, config.n_attempts, temperatures
        );
    }

    for (attempt, &temp) in temperatures.iter().enumerate() {
        if start.elapsed().as_millis() as u64 >= config.timeout_ms {
            if verbose {
                eprintln!("[llm] global timeout after {} attempts", attempt);
            }
            break;
        }

        if verbose {
            eprintln!("[llm] attempt {}/{} (temp={:.2})", attempt + 1, config.n_attempts, temp);
        }

        if let Some((program, test_outputs)) =
            single_attempt(train_pairs, test_inputs, &catalog, config, temp, &start, verbose)
        {
            return Some(LlmResult {
                program,
                test_outputs,
                attempt: attempt + 1,
            });
        }
    }

    if verbose {
        eprintln!(
            "[llm] no solution found after {} attempts ({}ms)",
            config.n_attempts,
            start.elapsed().as_millis()
        );
    }

    None
}
