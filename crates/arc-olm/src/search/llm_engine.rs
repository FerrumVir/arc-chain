//! LLM program-generation engine — asks Ollama to generate a COMPLETE program
//! as a sequence of operation names, then verifies and executes.
//!
//! Instead of step-by-step DAG navigation (which overfits to training examples),
//! the LLM sees all training examples + the operation vocabulary and outputs the
//! full program in one shot.  We try multiple attempts with varying temperature
//! and also try nearby operation variants.
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
            n_attempts: 10,
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
// Compact grid formatting
// ============================================================

fn format_grid_compact(grid: &Grid) -> String {
    let h = grid.len();
    let w = if h > 0 { grid[0].len() } else { 0 };
    // For small grids, show inline rows
    if h <= 5 && w <= 5 {
        format!(
            "({}x{}): {}",
            h,
            w,
            grid.iter()
                .map(|r| format!("{:?}", r))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        // For large grids, show dimensions and color summary
        let mut counts = [0u32; 10];
        for row in grid {
            for &c in row {
                if (c as usize) < 10 {
                    counts[c as usize] += 1;
                }
            }
        }
        let colors: Vec<String> = counts
            .iter()
            .enumerate()
            .filter(|&(_, &c)| c > 0)
            .map(|(i, c)| format!("{}:{}", i, c))
            .collect();
        format!("({}x{}) colors=[{}]", h, w, colors.join(","))
    }
}

// ============================================================
// Ollama HTTP call
// ============================================================

fn call_ollama(prompt: &str, config: &LlmConfig, temperature: f32) -> Option<String> {
    let url = format!("{}/api/chat", config.base_url);
    let body = serde_json::json!({
        "model": config.model,
        "messages": [
            {
                "role": "system",
                "content": "You solve ARC-AGI puzzles by writing operation sequences. Reply with ONLY operation names separated by spaces. No explanation, no code, no commentary."
            },
            {"role": "user", "content": prompt}
        ],
        "stream": false,
        "options": {
            "temperature": temperature,
            "num_predict": 100
        }
    });

    let response = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(60))
        .set("Content-Type", "application/json")
        .send_bytes(&serde_json::to_vec(&body).ok()?)
        .ok()?;

    let text = response.into_string().ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json["message"]["content"]
        .as_str()
        .or_else(|| json["response"].as_str())
        .map(|s| s.trim().to_string())
}

fn call_ollama_python(prompt: &str, config: &LlmConfig, temperature: f32) -> Option<String> {
    let url = format!("{}/api/chat", config.base_url);
    let body = serde_json::json!({
        "model": config.model,
        "messages": [
            {
                "role": "system",
                "content": "You write short Python functions to transform grids. Reply with ONLY the Python function. No explanation."
            },
            {"role": "user", "content": prompt}
        ],
        "stream": false,
        "options": {
            "temperature": temperature,
            "num_predict": 300
        }
    });

    let response = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(60))
        .set("Content-Type", "application/json")
        .send_bytes(&serde_json::to_vec(&body).ok()?)
        .ok()?;

    let text = response.into_string().ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json["message"]["content"]
        .as_str()
        .or_else(|| json["response"].as_str())
        .map(|s| s.trim().to_string())
}

// ============================================================
// Build operation vocabulary string
// ============================================================

fn build_op_vocabulary(colors: &[u8]) -> String {
    let mut vocab = String::new();
    vocab.push_str("Grid->Grid: rot90, rot180, rot270, hmirror, vmirror, dmirror, cmirror, tophalf, bottomhalf, lefthalf, righthalf, compress, trim\n");
    vocab.push_str("Grid->Grid (concat): hconcat_self, vconcat_self, hconcat_vm, vconcat_hm, hconcat_vm_r, vconcat_hm_r\n");

    // Parameterized replace
    let color_list: Vec<String> = colors.iter().map(|c| c.to_string()).collect();
    vocab.push_str(&format!(
        "Grid->Grid (parameterized): replace_X_Y (replace color X with Y, where X,Y in {{{}}}), upscale_N, downscale_N\n",
        color_list.join(",")
    ));

    vocab.push_str("Grid->Objects: obj_TTT, obj_TFT, obj_FTT, obj_FFT, obj_TTF, obj_TFF, obj_FTF, obj_FFF (extract connected components)\n");
    vocab.push_str("Objects->Object: argmax_size, argmin_size (select by size)\n");
    vocab.push_str("Object+Grid->Grid: subgrid (extract bounding box region)\n");
    vocab.push_str("Indices+Grid->Grid: fill_idx_X (fill positions with color X)\n");
    vocab.push_str("Grid->Indices: ofcolor_X (get positions of color X)\n");
    vocab.push_str("Indices->Indices: idx_backdrop, idx_delta, idx_neighbors\n");

    vocab
}

// ============================================================
// Build the program-generation prompt
// ============================================================

fn build_program_prompt(train_pairs: &[(Grid, Grid)], colors: &[u8]) -> String {
    let mut prompt = String::from(
        "You are solving an ARC-AGI puzzle. Study the examples:\n\n",
    );

    for (i, (inp, out)) in train_pairs.iter().enumerate() {
        prompt.push_str(&format!(
            "Example {}:\nInput {}\nOutput {}\n\n",
            i + 1,
            format_grid_compact(inp),
            format_grid_compact(out)
        ));
    }

    prompt.push_str("The transformation rule maps input to output. Express it as a SEQUENCE of these operations:\n\n");
    prompt.push_str(&build_op_vocabulary(colors));

    prompt.push_str("\nReply with ONLY the operation sequence, separated by spaces.\n");
    prompt.push_str("Example: \"obj_TTT argmax_size subgrid\"\n");
    prompt.push_str("Example: \"vmirror\"\n");
    prompt.push_str("Example: \"replace_0_5 rot90\"\n");

    prompt
}

// ============================================================
// Build the Python code-gen prompt
// ============================================================

fn build_python_prompt(train_pairs: &[(Grid, Grid)]) -> String {
    let mut prompt = String::from(
        "Write a Python function `transform(grid)` that maps input grids to output grids.\n\n",
    );

    for (i, (inp, out)) in train_pairs.iter().enumerate() {
        prompt.push_str(&format!(
            "Example {}:\nInput {}\nOutput {}\n\n",
            i + 1,
            format_grid_compact(inp),
            format_grid_compact(out)
        ));
    }

    prompt.push_str("Use only these operations: numpy rot90/rot180/rot270, flipud (hmirror), fliplr (vmirror), transpose (dmirror), color replacement, cropping.\n");
    prompt.push_str("Write the function, keep it very short.\n");

    prompt
}

// ============================================================
// Parse LLM response into operation names
// ============================================================

fn parse_program(response: &str, catalog: &[TypedPrimitive]) -> Vec<String> {
    let catalog_names: std::collections::HashSet<&str> =
        catalog.iter().map(|p| p.name).collect();

    // Clean up the response: remove quotes, backticks, newlines, commas
    let cleaned = response
        .replace('`', "")
        .replace('"', "")
        .replace('\'', "")
        .replace(',', " ")
        .replace('\n', " ")
        .replace("->", " ")
        .replace("→", " ");

    // Split by whitespace and filter to valid operation names
    let mut ops: Vec<String> = Vec::new();
    for token in cleaned.split_whitespace() {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if token.is_empty() {
            continue;
        }
        if catalog_names.contains(token) {
            ops.push(token.to_string());
        }
    }

    ops
}

// ============================================================
// Parse Python code to extract operation names
// ============================================================

fn parse_python_to_ops(code: &str) -> Vec<String> {
    let mut ops = Vec::new();
    let lower = code.to_lowercase();

    // Map Python operations to our catalog names
    let mappings: &[(&[&str], &str)] = &[
        (&["rot90", "rotate.*90", "np.rot90"], "rot90"),
        (&["rot180", "rotate.*180"], "rot180"),
        (&["rot270", "rotate.*270", "np.rot90.*k=3", "np.rot90.*3"], "rot270"),
        (&["flipud", "flip.*up.*down", "[::-1]", "vertical.*flip"], "hmirror"),
        (&["fliplr", "flip.*left.*right", "horizontal.*flip", "[:, ::-1]"], "vmirror"),
        (&["transpose", ".T", "np.transpose", "diagonal"], "dmirror"),
        (&["trim", "crop", "remove.*border", "strip.*background"], "trim"),
        (&["compress", "remove.*duplicate"], "compress"),
        (&["top.*half", "[:h//2]"], "tophalf"),
        (&["bottom.*half", "[h//2:]"], "bottomhalf"),
        (&["left.*half", "[:, :w//2]"], "lefthalf"),
        (&["right.*half", "[:, w//2:]"], "righthalf"),
    ];

    for (patterns, op_name) in mappings {
        for pat in *patterns {
            if lower.contains(pat) {
                if !ops.contains(&op_name.to_string()) {
                    ops.push(op_name.to_string());
                }
                break;
            }
        }
    }

    // Look for color replacement patterns in Python code
    // Scan for patterns like "== X] = Y" or "replace(X, Y)" or "replace X with Y"
    let chars: Vec<char> = code.chars().collect();
    let len = chars.len();
    for i in 0..len.saturating_sub(5) {
        // Pattern: "== D] = D" or "== D ... = D"
        if i + 4 < len && chars[i] == '=' && chars[i + 1] == '=' {
            let after_eq = &code[i + 2..];
            let c1 = after_eq.trim_start().chars().next();
            if let Some(d1) = c1.filter(|c| c.is_ascii_digit()) {
                // Find the next "= D" pattern
                if let Some(assign_pos) = after_eq.find("] =") {
                    let rhs = after_eq[assign_pos + 3..].trim_start();
                    if let Some(d2) = rhs.chars().next().filter(|c| c.is_ascii_digit()) {
                        let op = format!("replace_{}_{}", d1, d2);
                        if !ops.contains(&op) {
                            ops.push(op);
                        }
                    }
                }
            }
        }
    }

    ops
}

// ============================================================
// Generate operation variants (nearby mutations)
// ============================================================

fn generate_variants(program: &[String]) -> Vec<Vec<String>> {
    let mut variants = Vec::new();

    // Single-op substitutions for geometric transforms
    let geo_variants: &[&[&str]] = &[
        &["rot90", "rot180", "rot270"],
        &["hmirror", "vmirror", "dmirror", "cmirror"],
        &["tophalf", "bottomhalf", "lefthalf", "righthalf"],
    ];

    for (i, op) in program.iter().enumerate() {
        for group in geo_variants {
            if group.contains(&op.as_str()) {
                for &alt in *group {
                    if alt != op.as_str() {
                        let mut variant = program.to_vec();
                        variant[i] = alt.to_string();
                        variants.push(variant);
                    }
                }
            }
        }
    }

    // Try removing each operation (if program has > 1 op)
    if program.len() > 1 {
        for i in 0..program.len() {
            let mut variant = program.to_vec();
            variant.remove(i);
            variants.push(variant);
        }
    }

    // Try reversing the program
    if program.len() > 1 {
        let mut reversed = program.to_vec();
        reversed.reverse();
        variants.push(reversed);
    }

    variants
}

// ============================================================
// Apply a program (sequence of operations)
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

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (prim.apply)(&args)));
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
// Verify a program against all training pairs
// ============================================================

fn verify_program(
    program: &[String],
    catalog: &[TypedPrimitive],
    train_pairs: &[(Grid, Grid)],
) -> bool {
    if program.is_empty() {
        return false;
    }
    train_pairs.iter().all(|(inp, out)| {
        match apply_program(program, catalog, inp) {
            Some(ref result) => result == out,
            None => false,
        }
    })
}

// ============================================================
// Single attempt: generate program, verify, try variants
// ============================================================

fn single_attempt(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    catalog: &[TypedPrimitive],
    config: &LlmConfig,
    colors: &[u8],
    temperature: f32,
    start: &Instant,
    verbose: bool,
) -> Option<(Vec<String>, Vec<Grid>)> {
    // Check wall-clock timeout
    if start.elapsed().as_millis() as u64 >= config.timeout_ms {
        return None;
    }

    // Build prompt and call LLM
    let prompt = build_program_prompt(train_pairs, colors);
    let response = call_ollama(&prompt, config, temperature)?;

    if verbose {
        eprintln!("[llm] response (temp={:.1}): {:?}", temperature, response);
    }

    // Parse response into operation names
    let program = parse_program(&response, catalog);

    if verbose {
        eprintln!("[llm] parsed program: {:?}", program);
    }

    if program.is_empty() {
        return None;
    }

    // Verify the program
    if verify_program(&program, catalog, train_pairs) {
        if verbose {
            eprintln!("[llm] SOLVED with program: {:?}", program);
        }
        let test_outputs: Option<Vec<Grid>> = test_inputs
            .iter()
            .map(|ti| apply_program(&program, catalog, ti))
            .collect();
        return test_outputs.map(|to| (program, to));
    }

    // Try variants of the program
    let variants = generate_variants(&program);
    for variant in &variants {
        if start.elapsed().as_millis() as u64 >= config.timeout_ms {
            break;
        }
        if verify_program(variant, catalog, train_pairs) {
            if verbose {
                eprintln!("[llm] SOLVED with variant: {:?}", variant);
            }
            let test_outputs: Option<Vec<Grid>> = test_inputs
                .iter()
                .map(|ti| apply_program(variant, catalog, ti))
                .collect();
            return test_outputs.map(|to| (variant.clone(), to));
        }
    }

    None
}

// ============================================================
// Python code-gen attempt
// ============================================================

fn python_attempt(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    catalog: &[TypedPrimitive],
    config: &LlmConfig,
    temperature: f32,
    start: &Instant,
    verbose: bool,
) -> Option<(Vec<String>, Vec<Grid>)> {
    if start.elapsed().as_millis() as u64 >= config.timeout_ms {
        return None;
    }

    let prompt = build_python_prompt(train_pairs);
    let response = call_ollama_python(&prompt, config, temperature)?;

    if verbose {
        eprintln!("[llm] python response: {:?}", &response[..response.len().min(200)]);
    }

    let ops = parse_python_to_ops(&response);
    if verbose {
        eprintln!("[llm] python->ops: {:?}", ops);
    }

    if ops.is_empty() {
        return None;
    }

    // Filter to only operations that exist in the catalog
    let catalog_names: std::collections::HashSet<&str> =
        catalog.iter().map(|p| p.name).collect();
    let valid_ops: Vec<String> = ops
        .into_iter()
        .filter(|o| catalog_names.contains(o.as_str()))
        .collect();

    if valid_ops.is_empty() {
        return None;
    }

    // Try the parsed ops
    if verify_program(&valid_ops, catalog, train_pairs) {
        if verbose {
            eprintln!("[llm] SOLVED via python codegen: {:?}", valid_ops);
        }
        let test_outputs: Option<Vec<Grid>> = test_inputs
            .iter()
            .map(|ti| apply_program(&valid_ops, catalog, ti))
            .collect();
        return test_outputs.map(|to| (valid_ops, to));
    }

    // Try each op individually (the Python might describe a single transform)
    for op in &valid_ops {
        let single = vec![op.clone()];
        if verify_program(&single, catalog, train_pairs) {
            if verbose {
                eprintln!("[llm] SOLVED via python single-op: {:?}", single);
            }
            let test_outputs: Option<Vec<Grid>> = test_inputs
                .iter()
                .map(|ti| apply_program(&single, catalog, ti))
                .collect();
            return test_outputs.map(|to| (single, to));
        }
    }

    // Try variants
    let variants = generate_variants(&valid_ops);
    for variant in &variants {
        if start.elapsed().as_millis() as u64 >= config.timeout_ms {
            break;
        }
        if verify_program(variant, catalog, train_pairs) {
            if verbose {
                eprintln!("[llm] SOLVED via python variant: {:?}", variant);
            }
            let test_outputs: Option<Vec<Grid>> = test_inputs
                .iter()
                .map(|ti| apply_program(variant, catalog, ti))
                .collect();
            return test_outputs.map(|to| (variant.clone(), to));
        }
    }

    None
}

// ============================================================
// Main entry point
// ============================================================

/// Run LLM program-generation search: ask the LLM to generate complete programs,
/// then verify against all training pairs.
///
/// Strategy:
/// 1. Try 8 direct program-generation attempts with varying temperature (0.1-1.0)
/// 2. Try 2 Python code-gen attempts (extract ops from Python code)
/// 3. For each attempt, also try operation variants (nearby mutations)
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

    // Collect all colors from training pairs
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

    // Temperature schedule: 8 direct program-gen + 2 python code-gen = 10 total
    let n_direct = config.n_attempts.max(2) - 2;
    let n_python = 2;

    let temperatures: Vec<f32> = (0..n_direct)
        .map(|i| {
            if n_direct <= 1 {
                0.3
            } else {
                0.1 + (i as f32) * 0.9 / (n_direct - 1) as f32
            }
        })
        .collect();

    if verbose {
        eprintln!(
            "[llm] program-gen search: model={}, attempts={} direct + {} python, temps={:?}",
            config.model, n_direct, n_python, temperatures
        );
    }

    // Phase 1: Direct program generation
    for (attempt, &temp) in temperatures.iter().enumerate() {
        if start.elapsed().as_millis() as u64 >= config.timeout_ms {
            if verbose {
                eprintln!("[llm] timeout after {} attempts", attempt);
            }
            break;
        }

        if verbose {
            eprintln!(
                "[llm] attempt {}/{} (temp={:.2})",
                attempt + 1,
                n_direct + n_python,
                temp
            );
        }

        if let Some((program, test_outputs)) = single_attempt(
            train_pairs,
            test_inputs,
            &catalog,
            config,
            &colors,
            temp,
            &start,
            verbose,
        ) {
            return Some(LlmResult {
                program,
                test_outputs,
                attempt: attempt + 1,
            });
        }
    }

    // Phase 2: Python code-gen attempts
    for py_attempt in 0..n_python {
        if start.elapsed().as_millis() as u64 >= config.timeout_ms {
            break;
        }

        let temp = 0.2 + py_attempt as f32 * 0.4;
        let attempt_num = n_direct + py_attempt;

        if verbose {
            eprintln!(
                "[llm] python attempt {}/{} (temp={:.2})",
                attempt_num + 1,
                n_direct + n_python,
                temp
            );
        }

        if let Some((program, test_outputs)) = python_attempt(
            train_pairs,
            test_inputs,
            &catalog,
            config,
            temp,
            &start,
            verbose,
        ) {
            return Some(LlmResult {
                program,
                test_outputs,
                attempt: attempt_num + 1,
            });
        }
    }

    if verbose {
        eprintln!(
            "[llm] no solution found after {} attempts ({}ms)",
            n_direct + n_python,
            start.elapsed().as_millis()
        );
    }

    None
}
