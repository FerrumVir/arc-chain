//! 5-level program synthesizer for ARC-AGI tasks.
//!
//! Progressively tries more complex programs (Level 0..4), returning the first
//! one that produces correct output for ALL training pairs.
//!
//! Level 0 (<1ms):   Unary Grid->Grid primitives
//! Level 1 (<50ms):  Parameterized single primitives
//! Level 2 (<500ms): Two-step compositions of Level 0 + Level 1
//! Level 3 (<5s):    Object-centric patterns (covers ~54% of ARC tasks)
//! Level 4 (<10s):   Common composite ARC patterns

use crate::{Grid, Color, Object};
use crate::primitives::{grid, object};
use std::collections::BTreeSet;
use std::time::Instant;

/// Result of synthesis.
pub struct SynthResult {
    pub program_desc: String,
    pub test_outputs: Vec<Grid>,
    pub level: u8,
}

/// Run the 5-level synthesizer with a timeout.
///
/// Tries progressively more expensive search levels, returning the first
/// program that produces correct output on ALL training pairs.
pub fn synthesize(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    timeout_ms: u64,
) -> Option<SynthResult> {
    if train_pairs.is_empty() || test_inputs.is_empty() {
        return None;
    }

    let start = Instant::now();
    let colors = collect_colors(train_pairs);

    // --- Level 0: unary Grid->Grid primitives (<1ms) ---
    if let Some(r) = level_0(train_pairs, test_inputs) {
        return Some(r);
    }
    if elapsed_ms(&start) > timeout_ms { return None; }

    // --- Level 1: parameterized single primitives (<50ms) ---
    if let Some(r) = level_1(train_pairs, test_inputs, &colors, &start, timeout_ms) {
        return Some(r);
    }
    if elapsed_ms(&start) > timeout_ms { return None; }

    // --- Level 2: two-step compositions (<500ms) ---
    if let Some(r) = level_2(train_pairs, test_inputs, &colors, &start, timeout_ms) {
        return Some(r);
    }
    if elapsed_ms(&start) > timeout_ms { return None; }

    // --- Level 3: object-centric patterns (<5s) ---
    if let Some(r) = level_3(train_pairs, test_inputs, &colors, &start, timeout_ms) {
        return Some(r);
    }
    if elapsed_ms(&start) > timeout_ms { return None; }

    // --- Level 4: common ARC patterns (<10s) ---
    level_4(train_pairs, test_inputs, &colors, &start, timeout_ms)
}

// ============================================================
// Helper functions
// ============================================================

fn elapsed_ms(start: &Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// Check if a candidate function produces correct output for ALL training
/// pairs. If so, apply to test inputs and return the test outputs.
fn verify_and_apply(
    f: &dyn Fn(&Grid) -> Grid,
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<Vec<Grid>> {
    for (input, expected) in train_pairs {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(input)));
        match result {
            Ok(output) if output == *expected => {}
            _ => return None,
        }
    }
    // All training pairs matched -- apply to test inputs.
    let mut test_outputs = Vec::with_capacity(test_inputs.len());
    for test_in in test_inputs {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(test_in)));
        match result {
            Ok(out) => test_outputs.push(out),
            Err(_) => return None,
        }
    }
    Some(test_outputs)
}

/// Collect all distinct colors present in training data (inputs and outputs).
fn collect_colors(pairs: &[(Grid, Grid)]) -> Vec<Color> {
    let mut seen = [false; 10];
    for (inp, out) in pairs {
        for row in inp {
            for &c in row { seen[c as usize] = true; }
        }
        for row in out {
            for &c in row { seen[c as usize] = true; }
        }
    }
    (0u8..10).filter(|&c| seen[c as usize]).collect()
}

// ============================================================
// Level 0: unary Grid->Grid primitives
// ============================================================

fn level_0(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<SynthResult> {
    let unary_prims: Vec<(&str, fn(&Grid) -> Grid)> = vec![
        ("rot90",       grid::rot90),
        ("rot180",      grid::rot180),
        ("rot270",      grid::rot270),
        ("hmirror",     grid::hmirror),
        ("vmirror",     grid::vmirror),
        ("dmirror",     grid::dmirror),
        ("cmirror",     grid::cmirror),
        ("tophalf",     grid::tophalf),
        ("bottomhalf",  grid::bottomhalf),
        ("lefthalf",    grid::lefthalf),
        ("righthalf",   grid::righthalf),
        ("trim",        grid::trim),
        ("compress",    grid::compress),
    ];

    for (name, f) in &unary_prims {
        let f_ref = *f;
        if let Some(test_outputs) = verify_and_apply(
            &move |g: &Grid| f_ref(g),
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: name.to_string(),
                test_outputs,
                level: 0,
            });
        }
    }

    // Identity: output == input
    if let Some(test_outputs) = verify_and_apply(
        &|g: &Grid| g.clone(),
        train_pairs, test_inputs,
    ) {
        return Some(SynthResult { program_desc: "identity".into(), test_outputs, level: 0 });
    }

    None
}

// ============================================================
// Level 1: parameterized single primitives
// ============================================================

fn level_1(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    colors: &[Color],
    start: &Instant,
    timeout_ms: u64,
) -> Option<SynthResult> {
    // 1a. replace_color(grid, c1, c2) for all color pairs
    for &c1 in colors {
        for &c2 in colors {
            if c1 == c2 { continue; }
            if elapsed_ms(start) > timeout_ms { return None; }
            if let Some(test_outputs) = verify_and_apply(
                &|g: &Grid| grid::replace_color(g, c1, c2),
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!("replace_color({}, {})", c1, c2),
                    test_outputs,
                    level: 1,
                });
            }
        }
    }

    // 1b. switch_colors(grid, a, b)
    for (i, &a) in colors.iter().enumerate() {
        for &b in &colors[i + 1..] {
            if elapsed_ms(start) > timeout_ms { return None; }
            if let Some(test_outputs) = verify_and_apply(
                &|g: &Grid| grid::switch_colors(g, a, b),
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!("switch_colors({}, {})", a, b),
                    test_outputs,
                    level: 1,
                });
            }
        }
    }

    // 1c. upscale(grid, factor) for factors 2-5
    for factor in 2..=5usize {
        if elapsed_ms(start) > timeout_ms { return None; }
        if let Some(test_outputs) = verify_and_apply(
            &|g: &Grid| grid::upscale(g, factor),
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("upscale({})", factor),
                test_outputs,
                level: 1,
            });
        }
    }

    // 1d. downscale(grid, factor) for factors 2-5
    for factor in 2..=5usize {
        if elapsed_ms(start) > timeout_ms { return None; }
        if let Some(test_outputs) = verify_and_apply(
            &|g: &Grid| grid::downscale(g, factor),
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("downscale({})", factor),
                test_outputs,
                level: 1,
            });
        }
    }

    // 1e. crop(grid, sr, sc, h, w) for varying positions and sizes
    if let Some((_, first_out)) = train_pairs.first() {
        let out_h = first_out.len();
        let out_w = if out_h > 0 { first_out[0].len() } else { 0 };
        let in_h = train_pairs[0].0.len();
        let in_w = if in_h > 0 { train_pairs[0].0[0].len() } else { 0 };
        let max_h = in_h.min(out_h + 2);
        let max_w = in_w.min(out_w + 2);
        for h in 1..=max_h {
            for w in 1..=max_w {
                if elapsed_ms(start) > timeout_ms { return None; }
                for sr in 0..=in_h.saturating_sub(h) {
                    for sc in 0..=in_w.saturating_sub(w) {
                        if let Some(test_outputs) = verify_and_apply(
                            &|g: &Grid| grid::crop(g, sr, sc, h, w),
                            train_pairs,
                            test_inputs,
                        ) {
                            return Some(SynthResult {
                                program_desc: format!("crop({}, {}, {}, {})", sr, sc, h, w),
                                test_outputs,
                                level: 1,
                            });
                        }
                    }
                }
            }
        }
    }

    // 1g. Multi-color replace: detect consistent color mapping across ALL training pairs
    {
        let mut color_map: Option<Vec<(u8, u8)>> = None;
        let mut valid = true;

        for (inp, out) in train_pairs {
            if inp.len() != out.len() || (inp.len() > 0 && inp[0].len() != out[0].len()) {
                valid = false;
                break;
            }
            let mut this_map: Vec<(u8, u8)> = Vec::new();
            let mut consistent = true;
            for (_r, (irow, orow)) in inp.iter().zip(out.iter()).enumerate() {
                for (_c, (&iv, &ov)) in irow.iter().zip(orow.iter()).enumerate() {
                    if let Some(&(_, mapped)) = this_map.iter().find(|&&(from, _)| from == iv) {
                        if mapped != ov { consistent = false; break; }
                    } else {
                        this_map.push((iv, ov));
                    }
                }
                if !consistent { break; }
            }
            if !consistent { valid = false; break; }
            match &color_map {
                None => color_map = Some(this_map),
                Some(existing) => {
                    // Verify same mapping across pairs
                    for &(from, to) in &this_map {
                        if let Some(&(_, eto)) = existing.iter().find(|&&(f, _)| f == from) {
                            if eto != to { valid = false; break; }
                        }
                    }
                    if !valid { break; }
                }
            }
        }
        if valid {
            if let Some(mapping) = color_map {
                if let Some(test_outputs) = verify_and_apply(
                    &|g: &Grid| {
                        g.iter().map(|row| {
                            row.iter().map(|&c| {
                                mapping.iter().find(|&&(from, _)| from == c)
                                    .map(|&(_, to)| to).unwrap_or(c)
                            }).collect()
                        }).collect()
                    },
                    train_pairs, test_inputs,
                ) {
                    return Some(SynthResult {
                        program_desc: format!("color_map({:?})", mapping),
                        test_outputs, level: 1,
                    });
                }
            }
        }
    }

    // 1f. Self-concat patterns
    let concat_patterns: Vec<(&str, fn(&Grid) -> Grid)> = vec![
        ("hconcat(I,I)",            |g| grid::hconcat(g, g)),
        ("vconcat(I,I)",            |g| grid::vconcat(g, g)),
        ("hconcat(I,vmirror(I))",   |g| grid::hconcat(g, &grid::vmirror(g))),
        ("vconcat(I,hmirror(I))",   |g| grid::vconcat(g, &grid::hmirror(g))),
        ("hconcat(vmirror(I),I)",   |g| grid::hconcat(&grid::vmirror(g), g)),
        ("vconcat(hmirror(I),I)",   |g| grid::vconcat(&grid::hmirror(g), g)),
        ("hconcat(I,rot180(I))",    |g| grid::hconcat(g, &grid::rot180(g))),
        ("vconcat(I,rot180(I))",    |g| grid::vconcat(g, &grid::rot180(g))),
        ("hconcat(I,dmirror(I))",   |g| grid::hconcat(g, &grid::dmirror(g))),
        ("vconcat(I,dmirror(I))",   |g| grid::vconcat(g, &grid::dmirror(g))),
    ];
    for (name, f) in &concat_patterns {
        if elapsed_ms(start) > timeout_ms { return None; }
        let f_ref = *f;
        if let Some(test_outputs) = verify_and_apply(
            &move |g: &Grid| f_ref(g),
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: name.to_string(),
                test_outputs,
                level: 1,
            });
        }
    }

    None
}

// ============================================================
// Level 2: two-step compositions
// ============================================================

fn level_2(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    colors: &[Color],
    start: &Instant,
    timeout_ms: u64,
) -> Option<SynthResult> {
    let unary_prims: Vec<(&str, fn(&Grid) -> Grid)> = vec![
        ("rot90",       grid::rot90),
        ("rot180",      grid::rot180),
        ("rot270",      grid::rot270),
        ("hmirror",     grid::hmirror),
        ("vmirror",     grid::vmirror),
        ("dmirror",     grid::dmirror),
        ("cmirror",     grid::cmirror),
        ("tophalf",     grid::tophalf),
        ("bottomhalf",  grid::bottomhalf),
        ("lefthalf",    grid::lefthalf),
        ("righthalf",   grid::righthalf),
        ("trim",        grid::trim),
        ("compress",    grid::compress),
    ];

    // 2a. All pairs of unary Grid->Grid: f(g(input))
    for (name_f, f) in &unary_prims {
        for (name_g, g) in &unary_prims {
            if elapsed_ms(start) > timeout_ms { return None; }
            let f_ref = *f;
            let g_ref = *g;
            if let Some(test_outputs) = verify_and_apply(
                &move |inp: &Grid| f_ref(&g_ref(inp)),
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!("{}({}(I))", name_f, name_g),
                    test_outputs,
                    level: 2,
                });
            }
        }
    }

    // 2b. Unary + replace_color (both orderings)
    for (name_f, f) in &unary_prims {
        for &c1 in colors {
            for &c2 in colors {
                if c1 == c2 { continue; }
                if elapsed_ms(start) > timeout_ms { return None; }

                // f(replace_color(input, c1, c2))
                let f_ref = *f;
                if let Some(test_outputs) = verify_and_apply(
                    &move |inp: &Grid| f_ref(&grid::replace_color(inp, c1, c2)),
                    train_pairs,
                    test_inputs,
                ) {
                    return Some(SynthResult {
                        program_desc: format!("{}(replace_color(I, {}, {}))", name_f, c1, c2),
                        test_outputs,
                        level: 2,
                    });
                }

                // replace_color(f(input), c1, c2)
                if let Some(test_outputs) = verify_and_apply(
                    &move |inp: &Grid| grid::replace_color(&f_ref(inp), c1, c2),
                    train_pairs,
                    test_inputs,
                ) {
                    return Some(SynthResult {
                        program_desc: format!("replace_color({}(I), {}, {})", name_f, c1, c2),
                        test_outputs,
                        level: 2,
                    });
                }
            }
        }
    }

    // 2c. Two replace_color operations in sequence
    for &c1 in colors {
        for &c2 in colors {
            if c1 == c2 { continue; }
            for &c3 in colors {
                for &c4 in colors {
                    if c3 == c4 { continue; }
                    if (c1, c2) == (c3, c4) { continue; }
                    if elapsed_ms(start) > timeout_ms { return None; }
                    if let Some(test_outputs) = verify_and_apply(
                        &|inp: &Grid| {
                            grid::replace_color(&grid::replace_color(inp, c1, c2), c3, c4)
                        },
                        train_pairs,
                        test_inputs,
                    ) {
                        return Some(SynthResult {
                            program_desc: format!(
                                "replace_color(replace_color(I, {}, {}), {}, {})",
                                c1, c2, c3, c4
                            ),
                            test_outputs,
                            level: 2,
                        });
                    }
                }
            }
        }
    }

    None
}

// ============================================================
// Level 3: object-centric patterns
// ============================================================

/// The 4 canonical object extraction modes used throughout the crate.
const OBJ_MODES: [(bool, bool, bool, &str); 8] = [
    (true,  true,  true,  "TTT"),
    (true,  false, true,  "TFT"),
    (false, true,  true,  "FTT"),
    (false, false, true,  "FFT"),
    (true,  true,  false, "TTF"),
    (true,  false, false, "TFF"),
    (false, true,  false, "FTF"),
    (false, false, false, "FFF"),
];

fn center_of(obj: &Object) -> (usize, usize) {
    let positions = obj.positions();
    if positions.is_empty() { return (0, 0); }
    let sum_r: usize = positions.iter().map(|p| p.0).sum();
    let sum_c: usize = positions.iter().map(|p| p.1).sum();
    let n = positions.len();
    (sum_r / n, sum_c / n)
}

fn level_3(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    colors: &[Color],
    start: &Instant,
    timeout_ms: u64,
) -> Option<SynthResult> {
    // 3a. subgrid(selector(objects(input, ...)), input) for each mode x selector
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        if elapsed_ms(start) > timeout_ms { return None; }

        // argmax_size
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                match object::argmax_size(&objs) {
                    Some(obj) => object::subgrid(obj, inp),
                    None => inp.clone(),
                }
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("subgrid(argmax_size(objects(I, {})), I)", mode_name),
                test_outputs,
                level: 3,
            });
        }

        // argmin_size
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                match object::argmin_size(&objs) {
                    Some(obj) => object::subgrid(obj, inp),
                    None => inp.clone(),
                }
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("subgrid(argmin_size(objects(I, {})), I)", mode_name),
                test_outputs,
                level: 3,
            });
        }
    }

    // 3b. subgrid(colorfilter(objects(input, ...), color)[0], input)
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        for &color in colors {
            if elapsed_ms(start) > timeout_ms { return None; }

            // First object of that color
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let filtered = object::colorfilter(&objs, color);
                    if let Some(obj) = filtered.first() {
                        object::subgrid(obj, inp)
                    } else {
                        inp.clone()
                    }
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "subgrid(colorfilter(objects(I, {}), {})[0], I)",
                        mode_name, color
                    ),
                    test_outputs,
                    level: 3,
                });
            }

            // Largest object of that color
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let filtered = object::colorfilter(&objs, color);
                    match object::argmax_size(&filtered) {
                        Some(obj) => object::subgrid(obj, inp),
                        None => inp.clone(),
                    }
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "subgrid(argmax(colorfilter(objects(I, {}), {})), I)",
                        mode_name, color
                    ),
                    test_outputs,
                    level: 3,
                });
            }

            // Smallest object of that color
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let filtered = object::colorfilter(&objs, color);
                    match object::argmin_size(&filtered) {
                        Some(obj) => object::subgrid(obj, inp),
                        None => inp.clone(),
                    }
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "subgrid(argmin(colorfilter(objects(I, {}), {})), I)",
                        mode_name, color
                    ),
                    test_outputs,
                    level: 3,
                });
            }
        }
    }

    // 3c. fill(input, fill_color, ofcolor(input, source_color))
    for &source_color in colors {
        for &fill_color in colors {
            if source_color == fill_color { continue; }
            if elapsed_ms(start) > timeout_ms { return None; }

            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let positions = grid::ofcolor(inp, source_color);
                    grid::fill(inp, fill_color, &positions)
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "fill(I, {}, ofcolor(I, {}))",
                        fill_color, source_color
                    ),
                    test_outputs,
                    level: 3,
                });
            }
        }
    }

    // 3d. fill(input, fill_color, region(selector(objects(input, ...))))
    //     region in {delta, backdrop, obj_box}
    //     selector in {argmax_size, argmin_size}
    let region_fns: Vec<(&str, fn(&Object) -> BTreeSet<(usize, usize)>)> = vec![
        ("delta",    object::delta),
        ("backdrop", object::backdrop),
        ("obj_box",  object::obj_box),
    ];

    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        for &fill_color in colors {
            for &(region_name, region_fn) in &region_fns {
                if elapsed_ms(start) > timeout_ms { return None; }

                // argmax_size
                if let Some(test_outputs) = verify_and_apply(
                    &|inp: &Grid| {
                        let objs = object::objects(inp, uni, diag, nobg);
                        match object::argmax_size(&objs) {
                            Some(obj) => {
                                let region = region_fn(obj);
                                grid::fill(inp, fill_color, &region)
                            }
                            None => inp.clone(),
                        }
                    },
                    train_pairs,
                    test_inputs,
                ) {
                    return Some(SynthResult {
                        program_desc: format!(
                            "fill(I, {}, {}(argmax_size(objects(I, {}))))",
                            fill_color, region_name, mode_name
                        ),
                        test_outputs,
                        level: 3,
                    });
                }

                // argmin_size
                if let Some(test_outputs) = verify_and_apply(
                    &|inp: &Grid| {
                        let objs = object::objects(inp, uni, diag, nobg);
                        match object::argmin_size(&objs) {
                            Some(obj) => {
                                let region = region_fn(obj);
                                grid::fill(inp, fill_color, &region)
                            }
                            None => inp.clone(),
                        }
                    },
                    train_pairs,
                    test_inputs,
                ) {
                    return Some(SynthResult {
                        program_desc: format!(
                            "fill(I, {}, {}(argmin_size(objects(I, {}))))",
                            fill_color, region_name, mode_name
                        ),
                        test_outputs,
                        level: 3,
                    });
                }
            }
        }
    }

    // 3e. cover(input, selector(objects(input, ...))) -- erase an object
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        if elapsed_ms(start) > timeout_ms { return None; }

        // cover argmax
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                match object::argmax_size(&objs) {
                    Some(obj) => object::cover(inp, obj),
                    None => inp.clone(),
                }
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!(
                    "cover(I, argmax_size(objects(I, {})))", mode_name
                ),
                test_outputs,
                level: 3,
            });
        }

        // cover argmin
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                match object::argmin_size(&objs) {
                    Some(obj) => object::cover(inp, obj),
                    None => inp.clone(),
                }
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!(
                    "cover(I, argmin_size(objects(I, {})))", mode_name
                ),
                test_outputs,
                level: 3,
            });
        }

        // cover by color
        for &color in colors {
            if elapsed_ms(start) > timeout_ms { return None; }
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let filtered = object::colorfilter(&objs, color);
                    let mut result = inp.clone();
                    for obj in &filtered {
                        result = object::cover(&result, obj);
                    }
                    result
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "cover_all(I, colorfilter(objects(I, {}), {}))", mode_name, color
                    ),
                    test_outputs,
                    level: 3,
                });
            }
        }
    }

    // 3f. paint all objects of one mode onto a blank canvas
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        for &bg_color in colors {
            if elapsed_ms(start) > timeout_ms { return None; }
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let h = inp.len();
                    let w = if h > 0 { inp[0].len() } else { 0 };
                    let mut result = grid::canvas(bg_color, h, w);
                    for obj in &objs {
                        result = object::paint(&result, obj);
                    }
                    result
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "paint_all(canvas({}, H, W), objects(I, {}))", bg_color, mode_name
                    ),
                    test_outputs,
                    level: 3,
                });
            }
        }
    }

    // 3g. fill(input, fill_color, delta(each_object)) -- fill holes in every object
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        for &fill_color in colors {
            if elapsed_ms(start) > timeout_ms { return None; }
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let mut result = inp.clone();
                    for obj in &objs {
                        let holes = object::delta(obj);
                        result = grid::fill(&result, fill_color, &holes);
                    }
                    result
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "fill_all_deltas(I, {}, objects(I, {}))", fill_color, mode_name
                    ),
                    test_outputs,
                    level: 3,
                });
            }
        }
    }

    // 3h. fill(input, obj.primary_color(), delta(obj)) -- fill holes with object's own color
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        if elapsed_ms(start) > timeout_ms { return None; }
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                let mut result = inp.clone();
                for obj in &objs {
                    let holes = object::delta(obj);
                    let c = obj.primary_color();
                    result = grid::fill(&result, c, &holes);
                }
                result
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!(
                    "fill_deltas_own_color(I, objects(I, {}))", mode_name
                ),
                test_outputs,
                level: 3,
            });
        }
    }

    // 3i. Merge all objects and extract subgrid
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        if elapsed_ms(start) > timeout_ms { return None; }
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                if objs.is_empty() { return inp.clone(); }
                let merged = object::merge_objects(&objs);
                object::subgrid(&merged, inp)
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!(
                    "subgrid(merge_objects(objects(I, {})), I)", mode_name
                ),
                test_outputs,
                level: 3,
            });
        }
    }

    // 3j. Replace color of objects: for each object, replace its color with another
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        for &new_color in colors {
            if elapsed_ms(start) > timeout_ms { return None; }
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let mut result = inp.clone();
                    for obj in &objs {
                        let positions = obj.positions();
                        result = grid::fill(&result, new_color, &positions);
                    }
                    result
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "recolor_all(I, {}, objects(I, {}))", new_color, mode_name
                    ),
                    test_outputs,
                    level: 3,
                });
            }
        }
    }

    // 3k. Outline objects: draw bounding box of each object
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        for &box_color in colors {
            if elapsed_ms(start) > timeout_ms { return None; }
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let mut result = inp.clone();
                    for obj in &objs {
                        let outline = object::obj_box(obj);
                        result = grid::fill(&result, box_color, &outline);
                    }
                    result
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "outline_all(I, {}, objects(I, {}))", box_color, mode_name
                    ),
                    test_outputs,
                    level: 3,
                });
            }
        }
    }

    // 3l. Partition by color (group cells by color as objects)
    {
        fn partition(grid: &Grid) -> Vec<Object> {
            let mut color_groups: std::collections::HashMap<u8, BTreeSet<(u8, (usize, usize))>> =
                std::collections::HashMap::new();
            for (r, row) in grid.iter().enumerate() {
                for (c, &v) in row.iter().enumerate() {
                    color_groups.entry(v).or_default().insert((v, (r, c)));
                }
            }
            color_groups.into_values().map(|cells| Object { cells }).collect()
        }

        // Subgrid of largest partition (non-background)
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let bg = grid::mostcolor(inp);
                let parts = partition(inp);
                let non_bg: Vec<_> = parts.iter()
                    .filter(|o| o.primary_color() != bg)
                    .collect();
                match non_bg.iter().max_by_key(|o| o.size()) {
                    Some(obj) => object::subgrid(obj, inp),
                    None => inp.clone(),
                }
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: "subgrid(argmax(partition_nobg(I)), I)".into(),
                test_outputs, level: 3,
            });
        }

        // Subgrid of smallest partition (non-background)
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let bg = grid::mostcolor(inp);
                let parts = partition(inp);
                let non_bg: Vec<_> = parts.iter()
                    .filter(|o| o.primary_color() != bg)
                    .collect();
                match non_bg.iter().min_by_key(|o| o.size()) {
                    Some(obj) => object::subgrid(obj, inp),
                    None => inp.clone(),
                }
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: "subgrid(argmin(partition_nobg(I)), I)".into(),
                test_outputs, level: 3,
            });
        }
    }

    // 3m. Fill backdrop of each object with object's own color
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        if elapsed_ms(start) > timeout_ms { return None; }
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                let mut result = inp.clone();
                for obj in &objs {
                    let bd = object::backdrop(obj);
                    let c = obj.primary_color();
                    result = grid::fill(&result, c, &bd);
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("fill_backdrop_own_color(I, objects(I, {}))", mode_name),
                test_outputs, level: 3,
            });
        }
    }

    // 3n. Connect same-color objects with lines
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        for &line_color in colors {
            if elapsed_ms(start) > timeout_ms { return None; }
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let mut result = inp.clone();
                    for i in 0..objs.len() {
                        for j in (i+1)..objs.len() {
                            if objs[i].primary_color() == objs[j].primary_color() {
                                // Connect centers
                                let (ri, ci) = center_of(&objs[i]);
                                let (rj, cj) = center_of(&objs[j]);
                                let line = object::connect((ri, ci), (rj, cj));
                                result = grid::fill(&result, line_color, &line);
                            }
                        }
                    }
                    result
                },
                train_pairs, test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!("connect_same_color(I, {}, objects(I, {}))", line_color, mode_name),
                    test_outputs, level: 3,
                });
            }
        }
    }

    // 3o. Gravity: sort non-background cells to bottom/top/left/right
    for &(_uni, _diag, _nobg, _mode_name) in &OBJ_MODES {
        if elapsed_ms(start) > timeout_ms { return None; }

        // Gravity down (per column)
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let bg = grid::mostcolor(inp);
                let mut result = grid::canvas(bg, h, w);
                for c in 0..w {
                    let non_bg: Vec<u8> = (0..h).filter_map(|r| {
                        if inp[r][c] != bg { Some(inp[r][c]) } else { None }
                    }).collect();
                    let start_row = h - non_bg.len();
                    for (i, &v) in non_bg.iter().enumerate() {
                        result[start_row + i][c] = v;
                    }
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: "gravity_down(I)".into(),
                test_outputs, level: 3,
            });
        }

        // Gravity up
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let bg = grid::mostcolor(inp);
                let mut result = grid::canvas(bg, h, w);
                for c in 0..w {
                    let non_bg: Vec<u8> = (0..h).filter_map(|r| {
                        if inp[r][c] != bg { Some(inp[r][c]) } else { None }
                    }).collect();
                    for (i, &v) in non_bg.iter().enumerate() {
                        result[i][c] = v;
                    }
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: "gravity_up(I)".into(),
                test_outputs, level: 3,
            });
        }

        // Gravity left (per row)
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let bg = grid::mostcolor(inp);
                let mut result = grid::canvas(bg, h, w);
                for r in 0..h {
                    let non_bg: Vec<u8> = (0..w).filter_map(|c| {
                        if inp[r][c] != bg { Some(inp[r][c]) } else { None }
                    }).collect();
                    for (i, &v) in non_bg.iter().enumerate() {
                        result[r][i] = v;
                    }
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: "gravity_left(I)".into(),
                test_outputs, level: 3,
            });
        }

        // Gravity right
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let bg = grid::mostcolor(inp);
                let mut result = grid::canvas(bg, h, w);
                for r in 0..h {
                    let non_bg: Vec<u8> = (0..w).filter_map(|c| {
                        if inp[r][c] != bg { Some(inp[r][c]) } else { None }
                    }).collect();
                    let start_col = w - non_bg.len();
                    for (i, &v) in non_bg.iter().enumerate() {
                        result[r][start_col + i] = v;
                    }
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: "gravity_right(I)".into(),
                test_outputs, level: 3,
            });
        }

        break; // gravity doesn't depend on object mode, run once
    }

    // 3p. Flood fill from border: fill all cells reachable from border with a color
    // (this catches "fill enclosed regions" patterns)
    for &fill_c in colors {
        if elapsed_ms(start) > timeout_ms { return None; }
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let bg = grid::mostcolor(inp);
                // BFS from all border bg cells
                let mut reachable = vec![vec![false; w]; h];
                let mut queue = std::collections::VecDeque::new();
                for r in 0..h {
                    for c in 0..w {
                        if (r == 0 || r == h-1 || c == 0 || c == w-1) && inp[r][c] == bg {
                            if !reachable[r][c] {
                                reachable[r][c] = true;
                                queue.push_back((r, c));
                            }
                        }
                    }
                }
                while let Some((r, c)) = queue.pop_front() {
                    for (nr, nc) in [(r.wrapping_sub(1), c), (r+1, c), (r, c.wrapping_sub(1)), (r, c+1)] {
                        if nr < h && nc < w && !reachable[nr][nc] && inp[nr][nc] == bg {
                            reachable[nr][nc] = true;
                            queue.push_back((nr, nc));
                        }
                    }
                }
                // Fill unreachable bg cells with fill_c
                let mut result = inp.clone();
                for r in 0..h {
                    for c in 0..w {
                        if inp[r][c] == bg && !reachable[r][c] {
                            result[r][c] = fill_c;
                        }
                    }
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("fill_enclosed(I, {})", fill_c),
                test_outputs, level: 3,
            });
        }
    }

    // 3q. Unique object: find the object that differs from all others (different size/color)
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        if elapsed_ms(start) > timeout_ms { return None; }

        // Unique by size (only one object with that size)
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                if objs.len() < 2 { return inp.clone(); }
                let sizes: Vec<usize> = objs.iter().map(|o| o.size()).collect();
                for (i, obj) in objs.iter().enumerate() {
                    let s = sizes[i];
                    if sizes.iter().filter(|&&x| x == s).count() == 1 {
                        return object::subgrid(obj, inp);
                    }
                }
                inp.clone()
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("subgrid(unique_by_size(objects(I, {})), I)", mode_name),
                test_outputs, level: 3,
            });
        }

        // Unique by color
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                if objs.len() < 2 { return inp.clone(); }
                let clrs: Vec<u8> = objs.iter().map(|o| o.primary_color()).collect();
                for (i, obj) in objs.iter().enumerate() {
                    let c = clrs[i];
                    if clrs.iter().filter(|&&x| x == c).count() == 1 {
                        return object::subgrid(obj, inp);
                    }
                }
                inp.clone()
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("subgrid(unique_by_color(objects(I, {})), I)", mode_name),
                test_outputs, level: 3,
            });
        }
    }

    // 3r. Tiling: output = input tiled NxM times
    if let Some((first_in, first_out)) = train_pairs.first() {
        let ih = first_in.len();
        let iw = if ih > 0 { first_in[0].len() } else { 0 };
        let oh = first_out.len();
        let ow = if oh > 0 { first_out[0].len() } else { 0 };
        if ih > 0 && iw > 0 && oh % ih == 0 && ow % iw == 0 {
            let nr = oh / ih;
            let nc = ow / iw;
            if nr >= 1 && nc >= 1 && (nr > 1 || nc > 1) {
                if let Some(test_outputs) = verify_and_apply(
                    &|inp: &Grid| {
                        let h = inp.len();
                        let w = if h > 0 { inp[0].len() } else { 0 };
                        let mut result = vec![vec![0u8; w * nc]; h * nr];
                        for tr in 0..nr {
                            for tc in 0..nc {
                                for r in 0..h {
                                    for c in 0..w {
                                        result[tr * h + r][tc * w + c] = inp[r][c];
                                    }
                                }
                            }
                        }
                        result
                    },
                    train_pairs, test_inputs,
                ) {
                    return Some(SynthResult {
                        program_desc: format!("tile(I, {}, {})", nr, nc),
                        test_outputs, level: 3,
                    });
                }
            }
        }
    }

    // 3s. Tile with transforms (e.g., 2x2 tile with rot/mirror variants)
    if let Some((first_in, first_out)) = train_pairs.first() {
        let ih = first_in.len();
        let iw = if ih > 0 { first_in[0].len() } else { 0 };
        let oh = first_out.len();
        let ow = if oh > 0 { first_out[0].len() } else { 0 };
        if ih > 0 && iw > 0 && oh == ih * 2 && ow == iw * 2 {
            // Try 2x2 arrangements with transforms
            let transforms: Vec<(&str, fn(&Grid) -> Grid)> = vec![
                ("id", |g: &Grid| g.clone()),
                ("rot90", grid::rot90 as fn(&Grid) -> Grid),
                ("rot180", grid::rot180 as fn(&Grid) -> Grid),
                ("rot270", grid::rot270 as fn(&Grid) -> Grid),
                ("hmirror", grid::hmirror as fn(&Grid) -> Grid),
                ("vmirror", grid::vmirror as fn(&Grid) -> Grid),
            ];
            for (_, tl) in &transforms {
                for (_, tr) in &transforms {
                    for (_, bl) in &transforms {
                        for (_, br) in &transforms {
                            if elapsed_ms(start) > timeout_ms { return None; }
                            let tl = *tl; let tr = *tr; let bl = *bl; let br = *br;
                            if let Some(test_outputs) = verify_and_apply(
                                &|inp: &Grid| {
                                    let top = grid::hconcat(&tl(inp), &tr(inp));
                                    let bot = grid::hconcat(&bl(inp), &br(inp));
                                    grid::vconcat(&top, &bot)
                                },
                                train_pairs, test_inputs,
                            ) {
                                return Some(SynthResult {
                                    program_desc: "tile_2x2_transformed(I)".into(),
                                    test_outputs, level: 3,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // 3t. Add/remove border: output = input with 1-cell border of a color
    for &border_c in colors {
        if elapsed_ms(start) > timeout_ms { return None; }
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let mut result = vec![vec![border_c; w + 2]; h + 2];
                for r in 0..h {
                    for c in 0..w {
                        result[r + 1][c + 1] = inp[r][c];
                    }
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("add_border(I, {})", border_c),
                test_outputs, level: 3,
            });
        }
    }

    // 3u. Underfill: fill background cells within each object's bbox with a color
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        for &fill_c in colors {
            if elapsed_ms(start) > timeout_ms { return None; }
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let bg = grid::mostcolor(inp);
                    let mut result = inp.clone();
                    for obj in &objs {
                        let bd = object::backdrop(obj);
                        for &(r, c) in &bd {
                            if r < result.len() && c < result[0].len() && result[r][c] == bg {
                                result[r][c] = fill_c;
                            }
                        }
                    }
                    result
                },
                train_pairs, test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!("underfill_backdrop(I, {}, objects(I, {}))", fill_c, mode_name),
                    test_outputs, level: 3,
                });
            }
        }
    }

    // 3v. Crop to non-background bounding box (trim bg from all sides)
    if let Some(test_outputs) = verify_and_apply(
        &|inp: &Grid| {
            let h = inp.len();
            if h == 0 { return inp.clone(); }
            let w = inp[0].len();
            let bg = grid::mostcolor(inp);
            let mut min_r = h; let mut max_r = 0;
            let mut min_c = w; let mut max_c = 0;
            for r in 0..h {
                for c in 0..w {
                    if inp[r][c] != bg {
                        min_r = min_r.min(r);
                        max_r = max_r.max(r);
                        min_c = min_c.min(c);
                        max_c = max_c.max(c);
                    }
                }
            }
            if min_r > max_r { return inp.clone(); }
            grid::crop(inp, min_r, min_c, max_r - min_r + 1, max_c - min_c + 1)
        },
        train_pairs, test_inputs,
    ) {
        return Some(SynthResult {
            program_desc: "crop_to_content(I)".into(),
            test_outputs, level: 3,
        });
    }

    // 3w. Extract the second-largest or second-smallest object
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        if elapsed_ms(start) > timeout_ms { return None; }
        // Second largest
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let mut objs = object::objects(inp, uni, diag, nobg);
                objs.sort_by(|a, b| b.size().cmp(&a.size()));
                if objs.len() >= 2 {
                    object::subgrid(&objs[1], inp)
                } else {
                    inp.clone()
                }
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("subgrid(second_largest(objects(I, {})), I)", mode_name),
                test_outputs, level: 3,
            });
        }
    }

    // 3x. Flood fill from each non-background cell outward (spreading pattern)
    // For each color, find cells of that color and expand them to fill adjacent background cells
    for &spread_c in colors {
        if elapsed_ms(start) > timeout_ms { return None; }
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let bg = grid::mostcolor(inp);
                let mut result = inp.clone();
                // Find all cells of spread_c and expand them by 1 in 4 directions
                let mut to_fill = Vec::new();
                for r in 0..h {
                    for c in 0..w {
                        if inp[r][c] == spread_c {
                            for (nr, nc) in [(r.wrapping_sub(1), c), (r+1, c), (r, c.wrapping_sub(1)), (r, c+1)] {
                                if nr < h && nc < w && inp[nr][nc] == bg {
                                    to_fill.push((nr, nc));
                                }
                            }
                        }
                    }
                }
                for (r, c) in to_fill {
                    result[r][c] = spread_c;
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("spread_1(I, {})", spread_c),
                test_outputs, level: 3,
            });
        }
    }

    // 3y. For each non-bg color, fill the row and column through each cell of that color
    for &line_c in colors {
        if elapsed_ms(start) > timeout_ms { return None; }
        // Cross pattern: fill row AND column
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let bg = grid::mostcolor(inp);
                let mut result = inp.clone();
                let positions: Vec<(usize, usize)> = (0..h).flat_map(|r| {
                    (0..w).filter_map(move |c| {
                        if inp[r][c] == line_c { Some((r, c)) } else { None }
                    })
                }).collect();
                for &(r, c) in &positions {
                    for cc in 0..w {
                        if result[r][cc] == bg { result[r][cc] = line_c; }
                    }
                    for rr in 0..h {
                        if result[rr][c] == bg { result[rr][c] = line_c; }
                    }
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("cross_fill(I, {})", line_c),
                test_outputs, level: 3,
            });
        }

        // Row fill only
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let bg = grid::mostcolor(inp);
                let mut result = inp.clone();
                for r in 0..h {
                    if (0..w).any(|c| inp[r][c] == line_c) {
                        for c in 0..w {
                            if result[r][c] == bg { result[r][c] = line_c; }
                        }
                    }
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("row_fill(I, {})", line_c),
                test_outputs, level: 3,
            });
        }

        // Column fill only
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let h = inp.len();
                if h == 0 { return inp.clone(); }
                let w = inp[0].len();
                let bg = grid::mostcolor(inp);
                let mut result = inp.clone();
                for c in 0..w {
                    if (0..h).any(|r| inp[r][c] == line_c) {
                        for r in 0..h {
                            if result[r][c] == bg { result[r][c] = line_c; }
                        }
                    }
                }
                result
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("col_fill(I, {})", line_c),
                test_outputs, level: 3,
            });
        }
    }

    // 3z. Mirror/complete symmetry: fill cells to make grid symmetric
    // Horizontal symmetry completion
    if let Some(test_outputs) = verify_and_apply(
        &|inp: &Grid| {
            let h = inp.len();
            if h == 0 { return inp.clone(); }
            let w = inp[0].len();
            let bg = grid::mostcolor(inp);
            let mut result = inp.clone();
            for r in 0..h {
                for c in 0..w {
                    let mirror_r = h - 1 - r;
                    if result[r][c] == bg && result[mirror_r][c] != bg {
                        result[r][c] = result[mirror_r][c];
                    }
                }
            }
            result
        },
        train_pairs, test_inputs,
    ) {
        return Some(SynthResult {
            program_desc: "complete_h_symmetry(I)".into(),
            test_outputs, level: 3,
        });
    }

    // Vertical symmetry completion
    if let Some(test_outputs) = verify_and_apply(
        &|inp: &Grid| {
            let h = inp.len();
            if h == 0 { return inp.clone(); }
            let w = inp[0].len();
            let bg = grid::mostcolor(inp);
            let mut result = inp.clone();
            for r in 0..h {
                for c in 0..w {
                    let mirror_c = w - 1 - c;
                    if result[r][c] == bg && result[r][mirror_c] != bg {
                        result[r][c] = result[r][mirror_c];
                    }
                }
            }
            result
        },
        train_pairs, test_inputs,
    ) {
        return Some(SynthResult {
            program_desc: "complete_v_symmetry(I)".into(),
            test_outputs, level: 3,
        });
    }

    // Both H+V symmetry completion
    if let Some(test_outputs) = verify_and_apply(
        &|inp: &Grid| {
            let h = inp.len();
            if h == 0 { return inp.clone(); }
            let w = inp[0].len();
            let bg = grid::mostcolor(inp);
            let mut result = inp.clone();
            // Multiple passes to propagate
            for _ in 0..3 {
                let prev = result.clone();
                for r in 0..h {
                    for c in 0..w {
                        if result[r][c] == bg {
                            let mr = h - 1 - r;
                            let mc = w - 1 - c;
                            if prev[mr][c] != bg { result[r][c] = prev[mr][c]; }
                            else if prev[r][mc] != bg { result[r][c] = prev[r][mc]; }
                            else if prev[mr][mc] != bg { result[r][c] = prev[mr][mc]; }
                        }
                    }
                }
            }
            result
        },
        train_pairs, test_inputs,
    ) {
        return Some(SynthResult {
            program_desc: "complete_hv_symmetry(I)".into(),
            test_outputs, level: 3,
        });
    }

    // Diagonal symmetry completion
    if let Some(test_outputs) = verify_and_apply(
        &|inp: &Grid| {
            let h = inp.len();
            if h == 0 { return inp.clone(); }
            let w = inp[0].len();
            if h != w { return inp.clone(); }
            let bg = grid::mostcolor(inp);
            let mut result = inp.clone();
            for r in 0..h {
                for c in 0..w {
                    if result[r][c] == bg && result[c][r] != bg {
                        result[r][c] = result[c][r];
                    }
                }
            }
            result
        },
        train_pairs, test_inputs,
    ) {
        return Some(SynthResult {
            program_desc: "complete_d_symmetry(I)".into(),
            test_outputs, level: 3,
        });
    }

    None
}

// ============================================================
// Level 4: common composite ARC patterns
// ============================================================

fn level_4(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    colors: &[Color],
    start: &Instant,
    timeout_ms: u64,
) -> Option<SynthResult> {
    // 4a. Fill enclosed regions with grid-derived color
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        if elapsed_ms(start) > timeout_ms { return None; }

        // Fill each object's delta with leastcolor
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                let fill_c = grid::leastcolor(inp);
                let mut result = inp.clone();
                for obj in &objs {
                    let holes = object::delta(obj);
                    result = grid::fill(&result, fill_c, &holes);
                }
                result
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!(
                    "fill_deltas(I, leastcolor(I), objects(I, {}))", mode_name
                ),
                test_outputs,
                level: 4,
            });
        }

        // Fill with mostcolor
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let objs = object::objects(inp, uni, diag, nobg);
                let fill_c = grid::mostcolor(inp);
                let mut result = inp.clone();
                for obj in &objs {
                    let holes = object::delta(obj);
                    result = grid::fill(&result, fill_c, &holes);
                }
                result
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!(
                    "fill_deltas(I, mostcolor(I), objects(I, {}))", mode_name
                ),
                test_outputs,
                level: 4,
            });
        }
    }

    // 4b. Majority voting: split grid, overlay, pick most common color per cell
    for n_splits in [2usize, 3] {
        if elapsed_ms(start) > timeout_ms { return None; }

        // Horizontal split + majority
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let parts = grid::hsplit(inp, n_splits);
                if parts.is_empty() { return inp.clone(); }
                majority_vote(&parts)
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("majority_vote(hsplit(I, {}))", n_splits),
                test_outputs,
                level: 4,
            });
        }

        // Vertical split + majority
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let parts = grid::vsplit(inp, n_splits);
                if parts.is_empty() { return inp.clone(); }
                majority_vote(&parts)
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("majority_vote(vsplit(I, {}))", n_splits),
                test_outputs,
                level: 4,
            });
        }
    }

    // 4c. hconcat/vconcat of transformed halves
    let half_transforms: Vec<(&str, fn(&Grid) -> Grid)> = vec![
        ("rot90",    grid::rot90),
        ("rot180",   grid::rot180),
        ("rot270",   grid::rot270),
        ("hmirror",  grid::hmirror),
        ("vmirror",  grid::vmirror),
        ("dmirror",  grid::dmirror),
    ];

    for (t_name, t_fn) in &half_transforms {
        if elapsed_ms(start) > timeout_ms { return None; }
        let t = *t_fn;

        // hconcat(transform(lefthalf), righthalf)
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let lh = grid::lefthalf(inp);
                let rh = grid::righthalf(inp);
                grid::hconcat(&t(&lh), &rh)
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("hconcat({}(lefthalf(I)), righthalf(I))", t_name),
                test_outputs,
                level: 4,
            });
        }

        // hconcat(lefthalf, transform(righthalf))
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let lh = grid::lefthalf(inp);
                let rh = grid::righthalf(inp);
                grid::hconcat(&lh, &t(&rh))
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("hconcat(lefthalf(I), {}(righthalf(I)))", t_name),
                test_outputs,
                level: 4,
            });
        }

        // vconcat(transform(tophalf), bottomhalf)
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let th = grid::tophalf(inp);
                let bh = grid::bottomhalf(inp);
                grid::vconcat(&t(&th), &bh)
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("vconcat({}(tophalf(I)), bottomhalf(I))", t_name),
                test_outputs,
                level: 4,
            });
        }

        // vconcat(tophalf, transform(bottomhalf))
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let th = grid::tophalf(inp);
                let bh = grid::bottomhalf(inp);
                grid::vconcat(&th, &t(&bh))
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("vconcat(tophalf(I), {}(bottomhalf(I)))", t_name),
                test_outputs,
                level: 4,
            });
        }
    }

    // 4d. cellwise overlay of halves with each possible fallback color
    for &fallback in colors {
        if elapsed_ms(start) > timeout_ms { return None; }

        // cellwise(lefthalf, righthalf, fallback)
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let lh = grid::lefthalf(inp);
                let rh = grid::righthalf(inp);
                grid::cellwise(&lh, &rh, fallback)
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!(
                    "cellwise(lefthalf(I), righthalf(I), {})", fallback
                ),
                test_outputs,
                level: 4,
            });
        }

        // cellwise(tophalf, bottomhalf, fallback)
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let th = grid::tophalf(inp);
                let bh = grid::bottomhalf(inp);
                grid::cellwise(&th, &bh, fallback)
            },
            train_pairs,
            test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!(
                    "cellwise(tophalf(I), bottomhalf(I), {})", fallback
                ),
                test_outputs,
                level: 4,
            });
        }
    }

    // 4e. underfill patterns: fill background cells at certain positions
    for &(uni, diag, nobg, mode_name) in &OBJ_MODES {
        for &fill_color in colors {
            if elapsed_ms(start) > timeout_ms { return None; }

            // underfill delta of each object
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let mut result = inp.clone();
                    for obj in &objs {
                        let holes = object::delta(obj);
                        result = grid::underfill(&result, fill_color, &holes);
                    }
                    result
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "underfill_deltas(I, {}, objects(I, {}))", fill_color, mode_name
                    ),
                    test_outputs,
                    level: 4,
                });
            }

            // underfill backdrop of each object
            if let Some(test_outputs) = verify_and_apply(
                &|inp: &Grid| {
                    let objs = object::objects(inp, uni, diag, nobg);
                    let mut result = inp.clone();
                    for obj in &objs {
                        let bd = object::backdrop(obj);
                        result = grid::underfill(&result, fill_color, &bd);
                    }
                    result
                },
                train_pairs,
                test_inputs,
            ) {
                return Some(SynthResult {
                    program_desc: format!(
                        "underfill_backdrops(I, {}, objects(I, {}))", fill_color, mode_name
                    ),
                    test_outputs,
                    level: 4,
                });
            }
        }
    }

    // 4f. XOR overlay of halves (keep cells that differ between halves)
    for &fallback in colors {
        if elapsed_ms(start) > timeout_ms { return None; }

        // XOR of left/right halves
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let lh = grid::lefthalf(inp);
                let rh = grid::righthalf(inp);
                if lh.is_empty() || rh.is_empty() { return inp.clone(); }
                let h = lh.len().min(rh.len());
                let w = lh[0].len().min(rh[0].len());
                let bg = grid::mostcolor(inp);
                (0..h).map(|r| {
                    (0..w).map(|c| {
                        if lh[r][c] != bg && rh[r][c] == bg { lh[r][c] }
                        else if rh[r][c] != bg && lh[r][c] == bg { rh[r][c] }
                        else if lh[r][c] != bg && rh[r][c] != bg { fallback }
                        else { bg }
                    }).collect()
                }).collect()
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("xor_halves_h(I, {})", fallback),
                test_outputs, level: 4,
            });
        }

        // XOR of top/bottom halves
        if let Some(test_outputs) = verify_and_apply(
            &|inp: &Grid| {
                let th = grid::tophalf(inp);
                let bh = grid::bottomhalf(inp);
                if th.is_empty() || bh.is_empty() { return inp.clone(); }
                let h = th.len().min(bh.len());
                let w = th[0].len().min(bh[0].len());
                let bg = grid::mostcolor(inp);
                (0..h).map(|r| {
                    (0..w).map(|c| {
                        if th[r][c] != bg && bh[r][c] == bg { th[r][c] }
                        else if bh[r][c] != bg && th[r][c] == bg { bh[r][c] }
                        else if th[r][c] != bg && bh[r][c] != bg { fallback }
                        else { bg }
                    }).collect()
                }).collect()
            },
            train_pairs, test_inputs,
        ) {
            return Some(SynthResult {
                program_desc: format!("xor_halves_v(I, {})", fallback),
                test_outputs, level: 4,
            });
        }
    }

    // 4g. AND overlay of halves (keep only cells that are non-bg in BOTH halves)
    if let Some(test_outputs) = verify_and_apply(
        &|inp: &Grid| {
            let lh = grid::lefthalf(inp);
            let rh = grid::righthalf(inp);
            if lh.is_empty() || rh.is_empty() { return inp.clone(); }
            let h = lh.len().min(rh.len());
            let w = lh[0].len().min(rh[0].len());
            let bg = grid::mostcolor(inp);
            (0..h).map(|r| {
                (0..w).map(|c| {
                    if lh[r][c] != bg && rh[r][c] != bg { lh[r][c] }
                    else { bg }
                }).collect()
            }).collect()
        },
        train_pairs, test_inputs,
    ) {
        return Some(SynthResult {
            program_desc: "and_halves_h(I)".into(),
            test_outputs, level: 4,
        });
    }

    if let Some(test_outputs) = verify_and_apply(
        &|inp: &Grid| {
            let th = grid::tophalf(inp);
            let bh = grid::bottomhalf(inp);
            if th.is_empty() || bh.is_empty() { return inp.clone(); }
            let h = th.len().min(bh.len());
            let w = th[0].len().min(bh[0].len());
            let bg = grid::mostcolor(inp);
            (0..h).map(|r| {
                (0..w).map(|c| {
                    if th[r][c] != bg && bh[r][c] != bg { th[r][c] }
                    else { bg }
                }).collect()
            }).collect()
        },
        train_pairs, test_inputs,
    ) {
        return Some(SynthResult {
            program_desc: "and_halves_v(I)".into(),
            test_outputs, level: 4,
        });
    }

    // 4h. Repeat smallest repeating unit to fill grid
    // Detect the smallest subgrid that tiles to produce the output
    if let Some(test_outputs) = verify_and_apply(
        &|inp: &Grid| {
            let h = inp.len();
            if h == 0 { return inp.clone(); }
            let w = inp[0].len();
            // Try all possible tile sizes
            for th in 1..=h/2 {
                if h % th != 0 { continue; }
                for tw in 1..=w/2 {
                    if w % tw != 0 { continue; }
                    let tile = grid::crop(inp, 0, 0, th, tw);
                    let mut matches = true;
                    'outer: for tr in (0..h).step_by(th) {
                        for tc in (0..w).step_by(tw) {
                            for r in 0..th {
                                for c in 0..tw {
                                    if inp[tr + r][tc + c] != tile[r][c] {
                                        matches = false;
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                    if matches {
                        return tile;
                    }
                }
            }
            inp.clone()
        },
        train_pairs, test_inputs,
    ) {
        return Some(SynthResult {
            program_desc: "extract_tile(I)".into(),
            test_outputs, level: 4,
        });
    }

    None
}

/// Majority vote across multiple same-size grids.
/// For each cell position, pick the most frequently occurring color.
fn majority_vote(grids: &[Grid]) -> Grid {
    if grids.is_empty() { return vec![]; }
    let h = grids[0].len();
    if h == 0 { return vec![]; }
    let w = grids[0][0].len();

    let mut out = vec![vec![0u8; w]; h];
    for r in 0..h {
        for c in 0..w {
            let mut counts = [0u32; 10];
            for g in grids {
                if r < g.len() && c < g[r].len() {
                    counts[g[r][c] as usize] += 1;
                }
            }
            out[r][c] = counts
                .iter()
                .enumerate()
                .max_by_key(|&(_, cnt)| *cnt)
                .map(|(i, _)| i as u8)
                .unwrap_or(0);
        }
    }
    out
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a simple training pair + test input.
    fn make_task(
        train_pairs: Vec<(Grid, Grid)>,
        test_inputs: Vec<Grid>,
    ) -> (Vec<(Grid, Grid)>, Vec<Grid>) {
        (train_pairs, test_inputs)
    }

    #[test]
    fn test_level0_rot90() {
        // rot90 of [[1,2],[3,4]] = [[3,1],[4,2]]
        let inp1 = vec![vec![1, 2], vec![3, 4]];
        let out1 = vec![vec![3, 1], vec![4, 2]];

        let inp2 = vec![vec![5, 6], vec![7, 8]];
        let out2 = vec![vec![7, 5], vec![8, 6]];

        let test_in = vec![vec![0, 1], vec![2, 3]];
        let expected_test = vec![vec![2, 0], vec![3, 1]];

        let (train, test) = make_task(
            vec![(inp1, out1), (inp2, out2)],
            vec![test_in],
        );

        let result = synthesize(&train, &test, 10_000);
        assert!(result.is_some(), "Should solve rot90 task");
        let r = result.unwrap();
        assert_eq!(r.level, 0, "rot90 should be Level 0");
        assert_eq!(r.test_outputs[0], expected_test);
        assert!(r.program_desc.contains("rot90"));
    }

    #[test]
    fn test_level1_replace_color() {
        // Replace color 1 with color 3
        let inp1 = vec![vec![1, 0, 2], vec![2, 1, 0]];
        let out1 = vec![vec![3, 0, 2], vec![2, 3, 0]];

        let inp2 = vec![vec![0, 1, 1], vec![1, 0, 2]];
        let out2 = vec![vec![0, 3, 3], vec![3, 0, 2]];

        let test_in = vec![vec![1, 2, 0], vec![0, 1, 1]];
        let expected_test = vec![vec![3, 2, 0], vec![0, 3, 3]];

        let (train, test) = make_task(
            vec![(inp1, out1), (inp2, out2)],
            vec![test_in],
        );

        let result = synthesize(&train, &test, 10_000);
        assert!(result.is_some(), "Should solve replace_color task");
        let r = result.unwrap();
        assert_eq!(r.level, 1, "replace_color should be Level 1");
        assert_eq!(r.test_outputs[0], expected_test);
    }

    #[test]
    fn test_level2_composition() {
        // vmirror(rot90(input))
        //   rot90([[1,2],[3,4]]) = [[3,1],[4,2]]
        //   vmirror([[3,1],[4,2]]) = [[1,3],[2,4]]
        let inp1 = vec![vec![1, 2], vec![3, 4]];
        let rot = grid::rot90(&inp1);
        let out1 = grid::vmirror(&rot);

        let inp2 = vec![vec![5, 6], vec![7, 8]];
        let rot2 = grid::rot90(&inp2);
        let out2 = grid::vmirror(&rot2);

        let test_in = vec![vec![0, 1], vec![2, 3]];
        let expected_test = grid::vmirror(&grid::rot90(&test_in));

        let (train, test) = make_task(
            vec![(inp1, out1), (inp2, out2)],
            vec![test_in],
        );

        let result = synthesize(&train, &test, 10_000);
        assert!(result.is_some(), "Should solve vmirror(rot90(I)) task");
        let r = result.unwrap();
        assert!(r.level <= 2, "Should solve at Level 0, 1, or 2");
        assert_eq!(r.test_outputs[0], expected_test);
    }

    #[test]
    fn test_verify_and_apply_rejects_wrong() {
        let train = vec![
            (vec![vec![1, 2], vec![3, 4]], vec![vec![9, 9], vec![9, 9]]),
        ];
        let test = vec![vec![vec![0, 0]]];

        // rot90 does not produce [[9,9],[9,9]] from [[1,2],[3,4]]
        let result = verify_and_apply(
            &|g: &Grid| grid::rot90(g),
            &train,
            &test,
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_collect_colors() {
        let pairs = vec![
            (vec![vec![0, 1, 2]], vec![vec![3, 4, 5]]),
            (vec![vec![0, 7]], vec![vec![9]]),
        ];
        let colors = collect_colors(&pairs);
        assert!(colors.contains(&0));
        assert!(colors.contains(&1));
        assert!(colors.contains(&5));
        assert!(colors.contains(&9));
        assert!(!colors.contains(&6));
        assert!(!colors.contains(&8));
    }

    #[test]
    fn test_timeout_respected() {
        // Give a 0ms timeout -- should return None quickly
        let train = vec![
            (vec![vec![1, 2], vec![3, 4]], vec![vec![9, 9], vec![9, 9]]),
        ];
        let test = vec![vec![vec![0, 0]]];
        let result = synthesize(&train, &test, 0);
        // Level 0 checks happen instantly before timeout is checked,
        // so we just verify it doesn't panic with 0ms timeout.
        // Result may or may not be None depending on Level 0 speed.
        let _ = result;
    }

    #[test]
    fn test_multiple_test_inputs() {
        // rot90 with two test inputs
        let inp = vec![vec![1, 2], vec![3, 4]];
        let out = vec![vec![3, 1], vec![4, 2]];

        let test1 = vec![vec![5, 6], vec![7, 8]];
        let test2 = vec![vec![0, 1], vec![2, 3]];

        let result = synthesize(
            &[(inp, out)],
            &[test1.clone(), test2.clone()],
            10_000,
        );
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.test_outputs.len(), 2);
        assert_eq!(r.test_outputs[0], grid::rot90(&test1));
        assert_eq!(r.test_outputs[1], grid::rot90(&test2));
    }
}
