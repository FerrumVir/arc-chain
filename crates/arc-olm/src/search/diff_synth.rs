//! Diff-guided synthesis: analyze HOW the output differs from the input
//! across all training pairs, detect a consistent rule, and apply it.
//!
//! This catches the massive "identity + localized changes" category
//! (146/400 near-misses from identity alone).

use crate::{Grid, Color, Object};
use crate::primitives::{grid, object};
use std::collections::{HashMap, BTreeSet};
use std::time::Instant;

pub struct DiffResult {
    pub program_desc: String,
    pub test_outputs: Vec<Grid>,
}

/// Analyze diffs and try to construct a transformation.
pub fn diff_synthesize(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    timeout_ms: u64,
) -> Option<DiffResult> {
    if train_pairs.is_empty() || test_inputs.is_empty() {
        return None;
    }
    let start = Instant::now();
    let elapsed = || start.elapsed().as_millis() as u64;

    // Only operate on same-size pairs for most strategies
    let same_size = train_pairs.iter().all(|(inp, out)| {
        inp.len() == out.len()
            && !inp.is_empty()
            && !out.is_empty()
            && inp[0].len() == out[0].len()
    });

    // Strategy 1: Consistent color mapping
    if same_size {
        if let Some(r) = strategy_color_mapping(train_pairs, test_inputs) {
            return Some(r);
        }
    }
    if elapsed() > timeout_ms { return None; }

    // Strategy 2: Positional diff — cells that change share a spatial pattern
    if same_size {
        if let Some(r) = strategy_positional_diff(train_pairs, test_inputs) {
            return Some(r);
        }
    }
    if elapsed() > timeout_ms { return None; }

    // Strategy 3: Object diff — detect what happened to objects
    if let Some(r) = strategy_object_diff(train_pairs, test_inputs) {
        return Some(r);
    }
    if elapsed() > timeout_ms { return None; }

    // Strategy 4: Conditional color replace
    if same_size {
        if let Some(r) = strategy_conditional_replace(train_pairs, test_inputs) {
            return Some(r);
        }
    }
    if elapsed() > timeout_ms { return None; }

    // Strategy 5: Pattern stamp / copy
    if let Some(r) = strategy_pattern_stamp(train_pairs, test_inputs) {
        return Some(r);
    }
    if elapsed() > timeout_ms { return None; }

    // Strategy 6: Remove or keep specific objects
    if same_size {
        if let Some(r) = strategy_remove_objects(train_pairs, test_inputs) {
            return Some(r);
        }
    }
    if elapsed() > timeout_ms { return None; }

    // Strategy 7: Fill between objects
    if same_size {
        if let Some(r) = strategy_fill_between(train_pairs, test_inputs) {
            return Some(r);
        }
    }

    None
}

// ============================================================
// Helpers
// ============================================================

fn grid_dims(g: &Grid) -> (usize, usize) {
    let h = g.len();
    let w = if h > 0 { g[0].len() } else { 0 };
    (h, w)
}

fn colors_in_grid(g: &Grid) -> BTreeSet<u8> {
    let mut s = BTreeSet::new();
    for row in g {
        for &c in row {
            s.insert(c);
        }
    }
    s
}

/// Positions adjacent (4-connected) to any cell of a given color.
fn adjacent_to_color(g: &Grid, color: Color) -> BTreeSet<(usize, usize)> {
    let (h, w) = grid_dims(g);
    let mut result = BTreeSet::new();
    for r in 0..h {
        for c in 0..w {
            if g[r][c] != color {
                continue;
            }
            let neighbors: [(isize, isize); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
            for (dr, dc) in &neighbors {
                let nr = r as isize + dr;
                let nc = c as isize + dc;
                if nr >= 0 && nr < h as isize && nc >= 0 && nc < w as isize {
                    let (nr, nc) = (nr as usize, nc as usize);
                    if g[nr][nc] != color {
                        result.insert((nr, nc));
                    }
                }
            }
        }
    }
    result
}

/// Check if a cell has zero same-color 4-neighbors (isolated).
fn is_isolated(g: &Grid, r: usize, c: usize) -> bool {
    let (h, w) = grid_dims(g);
    let color = g[r][c];
    let neighbors: [(isize, isize); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
    for (dr, dc) in &neighbors {
        let nr = r as isize + dr;
        let nc = c as isize + dc;
        if nr >= 0 && nr < h as isize && nc >= 0 && nc < w as isize {
            if g[nr as usize][nc as usize] == color {
                return false;
            }
        }
    }
    true
}

/// Cells in the same row or column as any cell of a given color (excluding that color).
fn line_from_color(g: &Grid, color: Color) -> BTreeSet<(usize, usize)> {
    let (h, w) = grid_dims(g);
    let mut rows_with = BTreeSet::new();
    let mut cols_with = BTreeSet::new();
    for r in 0..h {
        for c in 0..w {
            if g[r][c] == color {
                rows_with.insert(r);
                cols_with.insert(c);
            }
        }
    }
    let mut result = BTreeSet::new();
    for r in 0..h {
        for c in 0..w {
            if g[r][c] != color && (rows_with.contains(&r) || cols_with.contains(&c)) {
                result.insert((r, c));
            }
        }
    }
    result
}

/// Flood-fill to find enclosed regions of background inside a color boundary.
fn enclosed_by_color(g: &Grid, boundary_color: Color) -> BTreeSet<(usize, usize)> {
    let (h, w) = grid_dims(g);
    if h == 0 || w == 0 {
        return BTreeSet::new();
    }
    // BFS from all border cells that are not boundary_color to find exterior
    let mut exterior = vec![vec![false; w]; h];
    let mut queue = std::collections::VecDeque::new();
    for r in 0..h {
        for c in 0..w {
            if (r == 0 || r == h - 1 || c == 0 || c == w - 1)
                && g[r][c] != boundary_color
            {
                exterior[r][c] = true;
                queue.push_back((r, c));
            }
        }
    }
    while let Some((r, c)) = queue.pop_front() {
        let neighbors: [(isize, isize); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
        for (dr, dc) in &neighbors {
            let nr = r as isize + dr;
            let nc = c as isize + dc;
            if nr >= 0 && nr < h as isize && nc >= 0 && nc < w as isize {
                let (nr, nc) = (nr as usize, nc as usize);
                if !exterior[nr][nc] && g[nr][nc] != boundary_color {
                    exterior[nr][nc] = true;
                    queue.push_back((nr, nc));
                }
            }
        }
    }
    // Interior = non-boundary cells that are not exterior
    let mut result = BTreeSet::new();
    for r in 0..h {
        for c in 0..w {
            if g[r][c] != boundary_color && !exterior[r][c] {
                result.insert((r, c));
            }
        }
    }
    result
}

/// Find cells between two same-color cells on the same row or column (gap fill).
fn gap_cells(g: &Grid, color: Color) -> BTreeSet<(usize, usize)> {
    let (h, w) = grid_dims(g);
    let bg = grid::mostcolor(g);
    let mut result = BTreeSet::new();

    // Horizontal gaps
    for r in 0..h {
        let cols: Vec<usize> = (0..w).filter(|&c| g[r][c] == color).collect();
        for i in 0..cols.len() {
            for j in (i + 1)..cols.len() {
                let c1 = cols[i];
                let c2 = cols[j];
                // Only fill if all cells between are background
                let all_bg = (c1 + 1..c2).all(|c| g[r][c] == bg);
                if all_bg && c2 > c1 + 1 {
                    for c in (c1 + 1)..c2 {
                        result.insert((r, c));
                    }
                }
            }
        }
    }

    // Vertical gaps
    for c in 0..w {
        let rows: Vec<usize> = (0..h).filter(|&r| g[r][c] == color).collect();
        for i in 0..rows.len() {
            for j in (i + 1)..rows.len() {
                let r1 = rows[i];
                let r2 = rows[j];
                let all_bg = (r1 + 1..r2).all(|r| g[r][c] == bg);
                if all_bg && r2 > r1 + 1 {
                    for r in (r1 + 1)..r2 {
                        result.insert((r, c));
                    }
                }
            }
        }
    }

    result
}

/// Compute the diff between two same-size grids: positions that changed.
fn compute_diff(inp: &Grid, out: &Grid) -> Vec<(usize, usize, u8, u8)> {
    let (h, w) = grid_dims(inp);
    let mut diffs = Vec::new();
    for r in 0..h {
        for c in 0..w {
            if inp[r][c] != out[r][c] {
                diffs.push((r, c, inp[r][c], out[r][c]));
            }
        }
    }
    diffs
}

// ============================================================
// Strategy 1: Consistent color mapping (global remap)
// ============================================================

fn strategy_color_mapping(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Build mapping from each training pair
    let mut global_map: HashMap<u8, u8> = HashMap::new();

    for (inp, out) in train_pairs {
        let (h, w) = grid_dims(inp);
        for r in 0..h {
            for c in 0..w {
                let from = inp[r][c];
                let to = out[r][c];
                if let Some(&existing) = global_map.get(&from) {
                    if existing != to {
                        return None; // Inconsistent mapping
                    }
                } else {
                    global_map.insert(from, to);
                }
            }
        }
    }

    // Must actually change something
    let changes = global_map.iter().any(|(k, v)| k != v);
    if !changes {
        return None;
    }

    // Verify on all training pairs
    for (inp, out) in train_pairs {
        let applied = apply_color_map(inp, &global_map);
        if applied != *out {
            return None;
        }
    }

    // Apply to test inputs
    let test_outputs: Vec<Grid> = test_inputs
        .iter()
        .map(|inp| apply_color_map(inp, &global_map))
        .collect();

    let desc = format!(
        "color_map({})",
        global_map
            .iter()
            .filter(|(k, v)| k != v)
            .map(|(k, v)| format!("{}->{}", k, v))
            .collect::<Vec<_>>()
            .join(",")
    );

    Some(DiffResult {
        program_desc: desc,
        test_outputs,
    })
}

fn apply_color_map(g: &Grid, map: &HashMap<u8, u8>) -> Grid {
    g.iter()
        .map(|row| {
            row.iter()
                .map(|&c| *map.get(&c).unwrap_or(&c))
                .collect()
        })
        .collect()
}

// ============================================================
// Strategy 2: Positional diff
// ============================================================

fn strategy_positional_diff(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Try each sub-strategy
    if let Some(r) = positional_adjacent_fill(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_enclosed_fill(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_line_fill(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_noise_removal(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_gap_fill(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_adjacent_fill_any(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_replace_from_color(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_intersection_fill(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_enclosed_flexible(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_enclosed_recolor(train_pairs, test_inputs) {
        return Some(r);
    }
    if let Some(r) = positional_diagonal_fill(train_pairs, test_inputs) {
        return Some(r);
    }
    None
}

/// 2f: Relaxed adjacency fill — changed cells of ANY source color adjacent to trigger
fn positional_adjacent_fill_any(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    for &trigger_color in &all_colors {
        for &from_color in &all_colors {
            for &to_color in &all_colors {
                if from_color == to_color { continue; }
                if from_color == trigger_color { continue; }

                let consistent = train_pairs.iter().all(|(inp, out)| {
                    let h = inp.len();
                    if h == 0 || inp[0].len() != out.get(0).map_or(0, |r| r.len()) { return false; }
                    let w = inp[0].len();
                    if inp.len() != out.len() { return false; }

                    // Check: every cell where inp==from_color AND adjacent to trigger_color
                    //   must become to_color in output
                    // And no other cells change
                    let mut ok = true;
                    for r in 0..h {
                        for c in 0..w {
                            let is_adj = [(r.wrapping_sub(1),c),(r+1,c),(r,c.wrapping_sub(1)),(r,c+1)]
                                .iter()
                                .any(|&(nr,nc)| nr < h && nc < w && inp[nr][nc] == trigger_color);

                            if inp[r][c] == from_color && is_adj {
                                if out[r][c] != to_color { ok = false; break; }
                            } else {
                                if inp[r][c] != out[r][c] { ok = false; break; }
                            }
                        }
                        if !ok { break; }
                    }
                    ok
                });

                if consistent {
                    let test_outputs: Vec<Grid> = test_inputs.iter().map(|inp| {
                        let h = inp.len();
                        if h == 0 { return inp.clone(); }
                        let w = inp[0].len();
                        let mut out = inp.clone();
                        for r in 0..h {
                            for c in 0..w {
                                if inp[r][c] == from_color {
                                    let is_adj = [(r.wrapping_sub(1),c),(r+1,c),(r,c.wrapping_sub(1)),(r,c+1)]
                                        .iter()
                                        .any(|&(nr,nc)| nr < h && nc < w && inp[nr][nc] == trigger_color);
                                    if is_adj { out[r][c] = to_color; }
                                }
                            }
                        }
                        out
                    }).collect();

                    return Some(DiffResult {
                        program_desc: format!(
                            "adj_fill({}->{},near={})", from_color, to_color, trigger_color
                        ),
                        test_outputs,
                    });
                }
            }
        }
    }
    None
}

/// 2g: Replace all cells of one specific color with another, but only if they
/// satisfy a spatial criterion (e.g., NOT on the border, or isolated)
fn positional_replace_from_color(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Check for "replace color X with Y, but only NON-border cells"
    let all_colors = collect_all_colors(train_pairs);

    for &from_c in &all_colors {
        for &to_c in &all_colors {
            if from_c == to_c { continue; }

            // Non-border variant: cells of from_c that are NOT on the grid border
            let consistent = train_pairs.iter().all(|(inp, out)| {
                let h = inp.len();
                if h == 0 { return false; }
                let w = inp[0].len();
                if inp.len() != out.len() { return false; }

                let mut ok = true;
                for r in 0..h {
                    for c in 0..w {
                        let is_border = r == 0 || r == h-1 || c == 0 || c == w-1;
                        if inp[r][c] == from_c && !is_border {
                            if out[r][c] != to_c { ok = false; break; }
                        } else {
                            if inp[r][c] != out[r][c] { ok = false; break; }
                        }
                    }
                    if !ok { break; }
                }
                ok
            });

            if consistent {
                let test_outputs: Vec<Grid> = test_inputs.iter().map(|inp| {
                    let h = inp.len();
                    if h == 0 { return inp.clone(); }
                    let w = inp[0].len();
                    let mut out = inp.clone();
                    for r in 0..h {
                        for c in 0..w {
                            let is_border = r == 0 || r == h-1 || c == 0 || c == w-1;
                            if inp[r][c] == from_c && !is_border {
                                out[r][c] = to_c;
                            }
                        }
                    }
                    out
                }).collect();
                return Some(DiffResult {
                    program_desc: format!("replace_nonborder({}->{}", from_c, to_c),
                    test_outputs,
                });
            }

            // Isolated variant: cells of from_c with NO same-color 4-neighbors
            let consistent = train_pairs.iter().all(|(inp, out)| {
                let h = inp.len();
                if h == 0 { return false; }
                let w = inp[0].len();
                if inp.len() != out.len() { return false; }

                let mut ok = true;
                for r in 0..h {
                    for c in 0..w {
                        let has_same_neighbor = [(r.wrapping_sub(1),c),(r+1,c),(r,c.wrapping_sub(1)),(r,c+1)]
                            .iter()
                            .any(|&(nr,nc)| nr < h && nc < w && inp[nr][nc] == from_c);

                        if inp[r][c] == from_c && !has_same_neighbor {
                            // Isolated cell of from_c → should become to_c
                            if out[r][c] != to_c { ok = false; break; }
                        } else {
                            if inp[r][c] != out[r][c] { ok = false; break; }
                        }
                    }
                    if !ok { break; }
                }
                ok
            });

            if consistent {
                let test_outputs: Vec<Grid> = test_inputs.iter().map(|inp| {
                    let h = inp.len();
                    if h == 0 { return inp.clone(); }
                    let w = inp[0].len();
                    let mut out = inp.clone();
                    for r in 0..h {
                        for c in 0..w {
                            let has_same_neighbor = [(r.wrapping_sub(1),c),(r+1,c),(r,c.wrapping_sub(1)),(r,c+1)]
                                .iter()
                                .any(|&(nr,nc)| nr < h && nc < w && inp[nr][nc] == from_c);
                            if inp[r][c] == from_c && !has_same_neighbor {
                                out[r][c] = to_c;
                            }
                        }
                    }
                    out
                }).collect();
                return Some(DiffResult {
                    program_desc: format!("replace_isolated({}->{}", from_c, to_c),
                    test_outputs,
                });
            }
        }
    }
    None
}

/// 2h: Row/column intersection fill.
/// Find rows containing color A, columns containing color B;
/// at each intersection cell that is background, fill with F.
fn positional_intersection_fill(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    for &row_marker in &all_colors {
        for &col_marker in &all_colors {
            for &fill_color in &all_colors {
                if fill_color == row_marker && row_marker == col_marker {
                    continue;
                }

                let consistent = train_pairs.iter().all(|(inp, out)| {
                    let (h, w) = grid_dims(inp);
                    if h == 0 || inp.len() != out.len() { return false; }
                    let bg = grid::mostcolor(inp);
                    if fill_color == bg { return false; }

                    let mut rows_with = BTreeSet::new();
                    let mut cols_with = BTreeSet::new();
                    for r in 0..h {
                        for c in 0..w {
                            if inp[r][c] == row_marker { rows_with.insert(r); }
                            if inp[r][c] == col_marker { cols_with.insert(c); }
                        }
                    }
                    if rows_with.is_empty() || cols_with.is_empty() { return false; }

                    let mut ok = true;
                    for r in 0..h {
                        for c in 0..w {
                            let is_intersect = rows_with.contains(&r) && cols_with.contains(&c) && inp[r][c] == bg;
                            if is_intersect {
                                if out[r][c] != fill_color { ok = false; break; }
                            } else {
                                if inp[r][c] != out[r][c] { ok = false; break; }
                            }
                        }
                        if !ok { break; }
                    }
                    ok
                });

                if consistent {
                    let test_outputs: Vec<Grid> = test_inputs.iter().map(|inp| {
                        let (h, w) = grid_dims(inp);
                        let bg = grid::mostcolor(inp);
                        let mut rows_with = BTreeSet::new();
                        let mut cols_with = BTreeSet::new();
                        for r in 0..h {
                            for c in 0..w {
                                if inp[r][c] == row_marker { rows_with.insert(r); }
                                if inp[r][c] == col_marker { cols_with.insert(c); }
                            }
                        }
                        let mut out = inp.clone();
                        for r in 0..h {
                            for c in 0..w {
                                if rows_with.contains(&r) && cols_with.contains(&c) && out[r][c] == bg {
                                    out[r][c] = fill_color;
                                }
                            }
                        }
                        out
                    }).collect();

                    return Some(DiffResult {
                        program_desc: format!(
                            "intersection_fill(row={},col={},fill={})",
                            row_marker, col_marker, fill_color
                        ),
                        test_outputs,
                    });
                }
            }
        }
    }
    None
}

/// 2i: Enclosed region fill with flexible barrier.
/// BFS from border through background only; bg cells unreachable from border are enclosed.
fn positional_enclosed_flexible(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    for &fill_color in &all_colors {
        let consistent = train_pairs.iter().all(|(inp, out)| {
            let (h, w) = grid_dims(inp);
            if h == 0 || inp.len() != out.len() { return false; }
            let bg = grid::mostcolor(inp);
            if fill_color == bg { return false; }

            let enclosed = find_enclosed_bg(inp, bg);
            if enclosed.is_empty() { return false; }

            let mut ok = true;
            for r in 0..h {
                for c in 0..w {
                    if enclosed.contains(&(r, c)) {
                        if out[r][c] != fill_color { ok = false; break; }
                    } else {
                        if inp[r][c] != out[r][c] { ok = false; break; }
                    }
                }
                if !ok { break; }
            }
            ok
        });

        if consistent {
            let test_outputs: Vec<Grid> = test_inputs.iter().map(|inp| {
                let bg = grid::mostcolor(inp);
                let enclosed = find_enclosed_bg(inp, bg);
                let mut out = inp.clone();
                for (r, c) in enclosed {
                    out[r][c] = fill_color;
                }
                out
            }).collect();

            return Some(DiffResult {
                program_desc: format!("enclosed_flexible_fill(fill={})", fill_color),
                test_outputs,
            });
        }
    }
    None
}

/// BFS from all border background cells; return bg cells NOT reachable from border.
fn find_enclosed_bg(grid: &Grid, bg: Color) -> BTreeSet<(usize, usize)> {
    let (h, w) = grid_dims(grid);
    if h == 0 || w == 0 {
        return BTreeSet::new();
    }
    let mut reachable = vec![vec![false; w]; h];
    let mut queue = std::collections::VecDeque::new();

    // Seed BFS from all border cells that ARE background
    for r in 0..h {
        for c in 0..w {
            if (r == 0 || r == h - 1 || c == 0 || c == w - 1) && grid[r][c] == bg {
                reachable[r][c] = true;
                queue.push_back((r, c));
            }
        }
    }
    while let Some((r, c)) = queue.pop_front() {
        let neighbors: [(isize, isize); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
        for (dr, dc) in &neighbors {
            let nr = r as isize + dr;
            let nc = c as isize + dc;
            if nr >= 0 && nr < h as isize && nc >= 0 && nc < w as isize {
                let (nr, nc) = (nr as usize, nc as usize);
                if !reachable[nr][nc] && grid[nr][nc] == bg {
                    reachable[nr][nc] = true;
                    queue.push_back((nr, nc));
                }
            }
        }
    }

    let mut enclosed = BTreeSet::new();
    for r in 0..h {
        for c in 0..w {
            if grid[r][c] == bg && !reachable[r][c] {
                enclosed.insert((r, c));
            }
        }
    }
    enclosed
}

/// 2j: Enclosed recolor — replace cells of color X with Y only when inside enclosed regions.
fn positional_enclosed_recolor(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    for &from_c in &all_colors {
        for &to_c in &all_colors {
            if from_c == to_c { continue; }

            let consistent = train_pairs.iter().all(|(inp, out)| {
                let (h, w) = grid_dims(inp);
                if h == 0 || inp.len() != out.len() { return false; }
                let bg = grid::mostcolor(inp);

                // Enclosed bg cells define the interior region.
                // We also include non-bg cells that are fully surrounded by
                // the enclosed region (i.e., all 4-neighbors are enclosed-bg or non-bg-interior).
                // Simpler: use BFS from border through anything that is NOT a barrier.
                // A barrier is any non-bg, non-from_c cell.
                // Actually, simplest correct approach: a cell of from_c is "enclosed"
                // if BFS from border through bg cannot reach it.
                let enclosed_bg = find_enclosed_bg(inp, bg);
                if enclosed_bg.is_empty() { return false; }

                // A cell of from_c is enclosed if ALL its 4-neighbors are either
                // enclosed-bg, non-bg (barrier), or another enclosed from_c cell.
                // Simpler: do BFS from border through bg AND from_c, see which from_c cells are reachable.
                let mut reachable = vec![vec![false; w]; h];
                let mut queue = std::collections::VecDeque::new();
                for r in 0..h {
                    for c in 0..w {
                        if (r == 0 || r == h - 1 || c == 0 || c == w - 1)
                            && (inp[r][c] == bg || inp[r][c] == from_c)
                        {
                            reachable[r][c] = true;
                            queue.push_back((r, c));
                        }
                    }
                }
                while let Some((r, c)) = queue.pop_front() {
                    let neighbors: [(isize, isize); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
                    for (dr, dc) in &neighbors {
                        let nr = r as isize + dr;
                        let nc = c as isize + dc;
                        if nr >= 0 && nr < h as isize && nc >= 0 && nc < w as isize {
                            let (nr, nc) = (nr as usize, nc as usize);
                            if !reachable[nr][nc] && (inp[nr][nc] == bg || inp[nr][nc] == from_c) {
                                reachable[nr][nc] = true;
                                queue.push_back((nr, nc));
                            }
                        }
                    }
                }

                let mut ok = true;
                let mut any_enclosed = false;
                for r in 0..h {
                    for c in 0..w {
                        if inp[r][c] == from_c && !reachable[r][c] {
                            any_enclosed = true;
                            if out[r][c] != to_c { ok = false; break; }
                        } else {
                            if inp[r][c] != out[r][c] { ok = false; break; }
                        }
                    }
                    if !ok { break; }
                }
                ok && any_enclosed
            });

            if consistent {
                let test_outputs: Vec<Grid> = test_inputs.iter().map(|inp| {
                    let (h, w) = grid_dims(inp);
                    let bg = grid::mostcolor(inp);
                    let mut reachable = vec![vec![false; w]; h];
                    let mut queue = std::collections::VecDeque::new();
                    for r in 0..h {
                        for c in 0..w {
                            if (r == 0 || r == h - 1 || c == 0 || c == w - 1)
                                && (inp[r][c] == bg || inp[r][c] == from_c)
                            {
                                reachable[r][c] = true;
                                queue.push_back((r, c));
                            }
                        }
                    }
                    while let Some((r, c)) = queue.pop_front() {
                        let neighbors: [(isize, isize); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
                        for (dr, dc) in &neighbors {
                            let nr = r as isize + dr;
                            let nc = c as isize + dc;
                            if nr >= 0 && nr < h as isize && nc >= 0 && nc < w as isize {
                                let (nr, nc) = (nr as usize, nc as usize);
                                if !reachable[nr][nc] && (inp[nr][nc] == bg || inp[nr][nc] == from_c) {
                                    reachable[nr][nc] = true;
                                    queue.push_back((nr, nc));
                                }
                            }
                        }
                    }
                    let mut out = inp.clone();
                    for r in 0..h {
                        for c in 0..w {
                            if inp[r][c] == from_c && !reachable[r][c] {
                                out[r][c] = to_c;
                            }
                        }
                    }
                    out
                }).collect();

                return Some(DiffResult {
                    program_desc: format!("enclosed_recolor({}->{}", from_c, to_c),
                    test_outputs,
                });
            }
        }
    }
    None
}

/// 2k: Diagonal neighbors fill — like adjacency fill but 8-connected.
fn positional_diagonal_fill(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    for &trigger_color in &all_colors {
        for &fill_color in &all_colors {
            if trigger_color == fill_color { continue; }

            let consistent = train_pairs.iter().all(|(inp, out)| {
                let (h, w) = grid_dims(inp);
                if h == 0 || inp.len() != out.len() { return false; }
                let bg = grid::mostcolor(inp);
                if fill_color == bg { return false; }

                let adj8 = adjacent_8_to_color(inp, trigger_color);

                let mut ok = true;
                for r in 0..h {
                    for c in 0..w {
                        if adj8.contains(&(r, c)) && inp[r][c] == bg {
                            if out[r][c] != fill_color { ok = false; break; }
                        } else {
                            if inp[r][c] != out[r][c] { ok = false; break; }
                        }
                    }
                    if !ok { break; }
                }
                ok
            });

            if consistent {
                let test_outputs: Vec<Grid> = test_inputs.iter().map(|inp| {
                    let bg = grid::mostcolor(inp);
                    let adj8 = adjacent_8_to_color(inp, trigger_color);
                    let mut out = inp.clone();
                    for (r, c) in adj8 {
                        if out[r][c] == bg {
                            out[r][c] = fill_color;
                        }
                    }
                    out
                }).collect();

                return Some(DiffResult {
                    program_desc: format!(
                        "diagonal_fill(trigger={},fill={})",
                        trigger_color, fill_color
                    ),
                    test_outputs,
                });
            }
        }
    }
    None
}

/// Positions 8-connected (including diagonals) to any cell of a given color.
fn adjacent_8_to_color(g: &Grid, color: Color) -> BTreeSet<(usize, usize)> {
    let (h, w) = grid_dims(g);
    let mut result = BTreeSet::new();
    for r in 0..h {
        for c in 0..w {
            if g[r][c] != color {
                continue;
            }
            let neighbors: [(isize, isize); 8] = [
                (-1, -1), (-1, 0), (-1, 1),
                (0, -1),           (0, 1),
                (1, -1),  (1, 0),  (1, 1),
            ];
            for (dr, dc) in &neighbors {
                let nr = r as isize + dr;
                let nc = c as isize + dc;
                if nr >= 0 && nr < h as isize && nc >= 0 && nc < w as isize {
                    let (nr, nc) = (nr as usize, nc as usize);
                    if g[nr][nc] != color {
                        result.insert((nr, nc));
                    }
                }
            }
        }
    }
    result
}

/// 2a: Changed cells are ALL adjacent to a specific color — fill with a target color.
fn positional_adjacent_fill(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    for &trigger_color in &all_colors {
        for &fill_color in &all_colors {
            if trigger_color == fill_color {
                continue;
            }

            let consistent = train_pairs.iter().all(|(inp, out)| {
                let diffs = compute_diff(inp, out);
                if diffs.is_empty() {
                    return false;
                }
                let adj = adjacent_to_color(inp, trigger_color);
                // All changed cells must be adjacent to trigger_color,
                // changed to fill_color, and from background
                let bg = grid::mostcolor(inp);
                diffs.iter().all(|&(r, c, from, to)| {
                    adj.contains(&(r, c)) && to == fill_color && from == bg
                }) && {
                    // And ALL adjacent-to-trigger bg cells must have changed
                    let expected: BTreeSet<_> = adj
                        .iter()
                        .filter(|&&(r, c)| inp[r][c] == bg)
                        .cloned()
                        .collect();
                    let changed: BTreeSet<_> =
                        diffs.iter().map(|&(r, c, _, _)| (r, c)).collect();
                    expected == changed
                }
            });

            if consistent {
                let test_outputs: Vec<Grid> = test_inputs
                    .iter()
                    .map(|inp| {
                        let bg = grid::mostcolor(inp);
                        let adj = adjacent_to_color(inp, trigger_color);
                        let mut out = inp.clone();
                        for (r, c) in adj {
                            if out[r][c] == bg {
                                out[r][c] = fill_color;
                            }
                        }
                        out
                    })
                    .collect();

                return Some(DiffResult {
                    program_desc: format!(
                        "adjacent_fill(trigger={},fill={})",
                        trigger_color, fill_color
                    ),
                    test_outputs,
                });
            }
        }
    }
    None
}

/// 2b: Changed cells are ALL enclosed by a specific color.
fn positional_enclosed_fill(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    for &boundary_color in &all_colors {
        for &fill_color in &all_colors {
            if boundary_color == fill_color {
                continue;
            }

            let consistent = train_pairs.iter().all(|(inp, out)| {
                let diffs = compute_diff(inp, out);
                if diffs.is_empty() {
                    return false;
                }
                let enclosed = enclosed_by_color(inp, boundary_color);
                if enclosed.is_empty() {
                    return false;
                }
                let changed: BTreeSet<_> =
                    diffs.iter().map(|&(r, c, _, _)| (r, c)).collect();
                // All changed cells enclosed, all changed to fill_color
                diffs.iter().all(|&(_, _, _, to)| to == fill_color)
                    && changed.is_subset(&enclosed)
                    && {
                        // All enclosed cells that weren't boundary must have changed
                        let expected: BTreeSet<_> = enclosed
                            .iter()
                            .filter(|&&(r, c)| inp[r][c] != boundary_color && inp[r][c] != fill_color)
                            .cloned()
                            .collect();
                        expected == changed
                    }
            });

            if consistent {
                let test_outputs: Vec<Grid> = test_inputs
                    .iter()
                    .map(|inp| {
                        let enclosed = enclosed_by_color(inp, boundary_color);
                        let mut out = inp.clone();
                        for (r, c) in enclosed {
                            if out[r][c] != boundary_color {
                                out[r][c] = fill_color;
                            }
                        }
                        out
                    })
                    .collect();

                return Some(DiffResult {
                    program_desc: format!(
                        "enclosed_fill(boundary={},fill={})",
                        boundary_color, fill_color
                    ),
                    test_outputs,
                });
            }
        }
    }
    None
}

/// 2c: Changed cells are in same row/col as a specific color.
fn positional_line_fill(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    for &trigger_color in &all_colors {
        for &fill_color in &all_colors {
            if trigger_color == fill_color {
                continue;
            }

            let consistent = train_pairs.iter().all(|(inp, out)| {
                let diffs = compute_diff(inp, out);
                if diffs.is_empty() {
                    return false;
                }
                let line_cells = line_from_color(inp, trigger_color);
                let bg = grid::mostcolor(inp);
                let changed: BTreeSet<_> =
                    diffs.iter().map(|&(r, c, _, _)| (r, c)).collect();
                diffs.iter().all(|&(_, _, from, to)| from == bg && to == fill_color)
                    && {
                        let expected: BTreeSet<_> = line_cells
                            .iter()
                            .filter(|&&(r, c)| inp[r][c] == bg)
                            .cloned()
                            .collect();
                        expected == changed
                    }
            });

            if consistent {
                let test_outputs: Vec<Grid> = test_inputs
                    .iter()
                    .map(|inp| {
                        let bg = grid::mostcolor(inp);
                        let line_cells = line_from_color(inp, trigger_color);
                        let mut out = inp.clone();
                        for (r, c) in line_cells {
                            if out[r][c] == bg {
                                out[r][c] = fill_color;
                            }
                        }
                        out
                    })
                    .collect();

                return Some(DiffResult {
                    program_desc: format!(
                        "line_fill(trigger={},fill={})",
                        trigger_color, fill_color
                    ),
                    test_outputs,
                });
            }
        }
    }
    None
}

/// 2d: Noise removal — isolated cells replaced by background.
fn positional_noise_removal(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Check: every diff is an isolated cell being replaced by background
    let consistent = train_pairs.iter().all(|(inp, out)| {
        let diffs = compute_diff(inp, out);
        if diffs.is_empty() {
            return false;
        }
        let bg = grid::mostcolor(inp);
        diffs.iter().all(|&(r, c, _from, to)| {
            to == bg && is_isolated(inp, r, c)
        }) && {
            // Verify all isolated non-bg cells were removed
            let (h, w) = grid_dims(inp);
            let changed: BTreeSet<_> =
                diffs.iter().map(|&(r, c, _, _)| (r, c)).collect();
            let expected: BTreeSet<_> = (0..h)
                .flat_map(|r| (0..w).map(move |c| (r, c)))
                .filter(|&(r, c)| inp[r][c] != bg && is_isolated(inp, r, c))
                .collect();
            expected == changed
        }
    });

    if !consistent {
        return None;
    }

    let test_outputs: Vec<Grid> = test_inputs
        .iter()
        .map(|inp| {
            let bg = grid::mostcolor(inp);
            let (h, w) = grid_dims(inp);
            let mut out = inp.clone();
            for r in 0..h {
                for c in 0..w {
                    if out[r][c] != bg && is_isolated(inp, r, c) {
                        out[r][c] = bg;
                    }
                }
            }
            out
        })
        .collect();

    Some(DiffResult {
        program_desc: "noise_removal(isolated)".to_string(),
        test_outputs,
    })
}

/// 2e: Gap fill — fill between two same-color cells.
fn positional_gap_fill(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    for &gap_color in &all_colors {
        let consistent = train_pairs.iter().all(|(inp, out)| {
            let diffs = compute_diff(inp, out);
            if diffs.is_empty() {
                return false;
            }
            let gaps = gap_cells(inp, gap_color);
            if gaps.is_empty() {
                return false;
            }
            let changed: BTreeSet<_> =
                diffs.iter().map(|&(r, c, _, _)| (r, c)).collect();
            diffs.iter().all(|&(_, _, _, to)| to == gap_color) && changed == gaps
        });

        if consistent {
            let test_outputs: Vec<Grid> = test_inputs
                .iter()
                .map(|inp| {
                    let gaps = gap_cells(inp, gap_color);
                    let mut out = inp.clone();
                    for (r, c) in gaps {
                        out[r][c] = gap_color;
                    }
                    out
                })
                .collect();

            return Some(DiffResult {
                program_desc: format!("gap_fill(color={})", gap_color),
                test_outputs,
            });
        }
    }
    None
}

// ============================================================
// Strategy 3: Object diff
// ============================================================

fn strategy_object_diff(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Try object recoloring
    if let Some(r) = object_recolor(train_pairs, test_inputs) {
        return Some(r);
    }
    // Try object movement
    if let Some(r) = object_move(train_pairs, test_inputs) {
        return Some(r);
    }
    None
}

/// Detect consistent object recoloring across training pairs.
fn object_recolor(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // For each pair, extract objects and see if some are recolored
    let mut recolor_map: Option<HashMap<u8, u8>> = None;

    for (inp, out) in train_pairs {
        let inp_objs = object::objects(inp, true, false, true);
        let out_objs = object::objects(out, true, false, true);

        let mut pair_map: HashMap<u8, u8> = HashMap::new();

        for inp_obj in &inp_objs {
            let inp_pos = inp_obj.positions();
            let inp_color = inp_obj.primary_color();

            // Find matching object in output by position overlap
            let mut best_match: Option<&Object> = None;
            let mut best_overlap = 0;
            for out_obj in &out_objs {
                let out_pos = out_obj.positions();
                let overlap = inp_pos.intersection(&out_pos).count();
                if overlap > best_overlap {
                    best_overlap = overlap;
                    best_match = Some(out_obj);
                }
            }

            if let Some(matched) = best_match {
                let out_color = matched.primary_color();
                if inp_color != out_color {
                    if let Some(&existing) = pair_map.get(&inp_color) {
                        if existing != out_color {
                            return None; // Inconsistent within pair
                        }
                    } else {
                        pair_map.insert(inp_color, out_color);
                    }
                }
            }
        }

        if pair_map.is_empty() {
            continue;
        }

        match &recolor_map {
            None => recolor_map = Some(pair_map),
            Some(existing) => {
                // Must be consistent across pairs
                for (k, v) in &pair_map {
                    if let Some(&ev) = existing.get(k) {
                        if ev != *v {
                            return None;
                        }
                    }
                }
                let mut merged = existing.clone();
                merged.extend(pair_map);
                recolor_map = Some(merged);
            }
        }
    }

    let recolor_map = recolor_map?;
    if recolor_map.is_empty() {
        return None;
    }

    // Verify on training pairs
    for (inp, out) in train_pairs {
        let applied = apply_color_map(inp, &recolor_map);
        if applied != *out {
            return None;
        }
    }

    let test_outputs: Vec<Grid> = test_inputs
        .iter()
        .map(|inp| apply_color_map(inp, &recolor_map))
        .collect();

    Some(DiffResult {
        program_desc: format!("object_recolor({:?})", recolor_map),
        test_outputs,
    })
}

/// Detect consistent object movement across training pairs.
fn object_move(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Check if the same-size grids have objects that moved consistently
    let same_size = train_pairs.iter().all(|(inp, out)| {
        inp.len() == out.len()
            && !inp.is_empty()
            && !out.is_empty()
            && inp[0].len() == out[0].len()
    });
    if !same_size {
        return None;
    }

    // Extract objects and detect consistent delta
    let mut consistent_dr: Option<isize> = None;
    let mut consistent_dc: Option<isize> = None;
    let mut move_color: Option<u8> = None;

    for (inp, out) in train_pairs {
        let inp_objs = object::objects(inp, true, false, true);
        let out_objs = object::objects(out, true, false, true);

        for inp_obj in &inp_objs {
            let inp_pos = inp_obj.positions();
            let inp_color = inp_obj.primary_color();

            // Find matching object by same shape (same relative positions)
            let inp_bbox = object::bbox(inp_obj);
            let inp_rel: BTreeSet<(usize, usize)> = inp_pos
                .iter()
                .map(|&(r, c)| (r - inp_bbox.0, c - inp_bbox.1))
                .collect();

            for out_obj in &out_objs {
                let out_color = out_obj.primary_color();
                if out_color != inp_color {
                    continue;
                }
                let out_pos = out_obj.positions();
                let out_bbox = object::bbox(out_obj);
                let out_rel: BTreeSet<(usize, usize)> = out_pos
                    .iter()
                    .map(|&(r, c)| (r - out_bbox.0, c - out_bbox.1))
                    .collect();

                if inp_rel == out_rel && inp_bbox != out_bbox {
                    let dr = out_bbox.0 as isize - inp_bbox.0 as isize;
                    let dc = out_bbox.1 as isize - inp_bbox.1 as isize;

                    match (consistent_dr, consistent_dc) {
                        (None, None) => {
                            consistent_dr = Some(dr);
                            consistent_dc = Some(dc);
                            move_color = Some(inp_color);
                        }
                        (Some(edr), Some(edc)) => {
                            if edr != dr || edc != dc {
                                return None;
                            }
                        }
                        _ => return None,
                    }
                }
            }
        }
    }

    let dr = consistent_dr?;
    let dc = consistent_dc?;
    let mv_color = move_color?;

    // Verify on training pairs
    for (inp, out) in train_pairs {
        let applied = apply_object_move(inp, dr, dc, mv_color);
        if applied != *out {
            return None;
        }
    }

    let test_outputs: Vec<Grid> = test_inputs
        .iter()
        .map(|inp| apply_object_move(inp, dr, dc, mv_color))
        .collect();

    Some(DiffResult {
        program_desc: format!("object_move(color={},dr={},dc={})", mv_color, dr, dc),
        test_outputs,
    })
}

fn apply_object_move(g: &Grid, dr: isize, dc: isize, color: u8) -> Grid {
    let objs = object::objects(g, true, false, true);
    let mut result = g.clone();
    for obj in &objs {
        if obj.primary_color() == color {
            result = object::move_obj(&result, obj, dr, dc);
        }
    }
    result
}

// ============================================================
// Strategy 4: Conditional color replace
// ============================================================

fn strategy_conditional_replace(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Group changes by (from_color, to_color) and find what condition characterizes them
    let all_colors = collect_all_colors(train_pairs);

    // For each (from, to), check if the condition is "adjacent to color X"
    for &from_color in &all_colors {
        for &to_color in &all_colors {
            if from_color == to_color {
                continue;
            }
            for &adj_color in &all_colors {
                if adj_color == from_color {
                    continue;
                }

                let consistent = train_pairs.iter().all(|(inp, out)| {
                    let (h, w) = grid_dims(inp);
                    // Find all cells that are from_color AND adjacent to adj_color
                    let mut expected_changes = BTreeSet::new();
                    for r in 0..h {
                        for c in 0..w {
                            if inp[r][c] == from_color && has_neighbor(inp, r, c, adj_color) {
                                expected_changes.insert((r, c));
                            }
                        }
                    }
                    if expected_changes.is_empty() {
                        return false;
                    }
                    // Check that exactly these cells changed from from_color to to_color
                    let diffs = compute_diff(inp, out);
                    let actual_relevant: BTreeSet<_> = diffs
                        .iter()
                        .filter(|&&(_, _, f, t)| f == from_color && t == to_color)
                        .map(|&(r, c, _, _)| (r, c))
                        .collect();
                    // All other diffs should be (from_color, to_color) only
                    let other_diffs: Vec<_> = diffs
                        .iter()
                        .filter(|&&(_, _, f, t)| !(f == from_color && t == to_color))
                        .collect();
                    expected_changes == actual_relevant && other_diffs.is_empty()
                });

                if consistent {
                    let test_outputs: Vec<Grid> = test_inputs
                        .iter()
                        .map(|inp| {
                            let (h, w) = grid_dims(inp);
                            let mut out = inp.clone();
                            for r in 0..h {
                                for c in 0..w {
                                    if out[r][c] == from_color
                                        && has_neighbor(inp, r, c, adj_color)
                                    {
                                        out[r][c] = to_color;
                                    }
                                }
                            }
                            out
                        })
                        .collect();

                    return Some(DiffResult {
                        program_desc: format!(
                            "conditional_replace(from={},to={},adj={})",
                            from_color, to_color, adj_color
                        ),
                        test_outputs,
                    });
                }
            }
        }
    }

    // Check "on border of grid" condition
    for &from_color in &all_colors {
        for &to_color in &all_colors {
            if from_color == to_color {
                continue;
            }

            let consistent = train_pairs.iter().all(|(inp, out)| {
                let (h, w) = grid_dims(inp);
                let mut expected = BTreeSet::new();
                for r in 0..h {
                    for c in 0..w {
                        if inp[r][c] == from_color
                            && (r == 0 || r == h - 1 || c == 0 || c == w - 1)
                        {
                            expected.insert((r, c));
                        }
                    }
                }
                if expected.is_empty() {
                    return false;
                }
                let diffs = compute_diff(inp, out);
                let actual: BTreeSet<_> = diffs.iter().map(|&(r, c, _, _)| (r, c)).collect();
                diffs.iter().all(|&(_, _, f, t)| f == from_color && t == to_color)
                    && expected == actual
            });

            if consistent {
                let test_outputs: Vec<Grid> = test_inputs
                    .iter()
                    .map(|inp| {
                        let (h, w) = grid_dims(inp);
                        let mut out = inp.clone();
                        for r in 0..h {
                            for c in 0..w {
                                if out[r][c] == from_color
                                    && (r == 0 || r == h - 1 || c == 0 || c == w - 1)
                                {
                                    out[r][c] = to_color;
                                }
                            }
                        }
                        out
                    })
                    .collect();

                return Some(DiffResult {
                    program_desc: format!(
                        "conditional_replace_border(from={},to={})",
                        from_color, to_color
                    ),
                    test_outputs,
                });
            }
        }
    }

    None
}

fn has_neighbor(g: &Grid, r: usize, c: usize, color: Color) -> bool {
    let (h, w) = grid_dims(g);
    let neighbors: [(isize, isize); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
    for (dr, dc) in &neighbors {
        let nr = r as isize + dr;
        let nc = c as isize + dc;
        if nr >= 0 && nr < h as isize && nc >= 0 && nc < w as isize {
            if g[nr as usize][nc as usize] == color {
                return true;
            }
        }
    }
    false
}

// ============================================================
// Strategy 5: Pattern stamp / copy
// ============================================================

fn strategy_pattern_stamp(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Sub-case A: output is a subgrid of the input (extract a pattern)
    if let Some(r) = pattern_extract(train_pairs, test_inputs) {
        return Some(r);
    }
    // Sub-case B: output is the input tiled
    if let Some(r) = pattern_tile(train_pairs, test_inputs) {
        return Some(r);
    }
    None
}

/// Extract: the output is the bounding box of the non-background region.
fn pattern_extract(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let consistent = train_pairs.iter().all(|(inp, out)| {
        let bg = grid::mostcolor(inp);
        let (h, w) = grid_dims(inp);
        // Find bounding box of non-bg cells
        let mut min_r = h;
        let mut max_r = 0;
        let mut min_c = w;
        let mut max_c = 0;
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
        if min_r > max_r {
            return false;
        }
        let extracted = grid::crop(inp, min_r, min_c, max_r - min_r + 1, max_c - min_c + 1);
        extracted == *out
    });

    if !consistent {
        return None;
    }

    let test_outputs: Vec<Grid> = test_inputs
        .iter()
        .map(|inp| {
            let bg = grid::mostcolor(inp);
            let (h, w) = grid_dims(inp);
            let mut min_r = h;
            let mut max_r = 0;
            let mut min_c = w;
            let mut max_c = 0;
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
            if min_r > max_r {
                return inp.clone();
            }
            grid::crop(inp, min_r, min_c, max_r - min_r + 1, max_c - min_c + 1)
        })
        .collect();

    Some(DiffResult {
        program_desc: "pattern_extract(bbox_nonbg)".to_string(),
        test_outputs,
    })
}

/// Tile: the output is the input repeated/tiled.
fn pattern_tile(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Check if output dimensions are exact multiples of input dimensions
    let mut tile_r: Option<usize> = None;
    let mut tile_c: Option<usize> = None;

    for (inp, out) in train_pairs {
        let (ih, iw) = grid_dims(inp);
        let (oh, ow) = grid_dims(out);
        if ih == 0 || iw == 0 || oh % ih != 0 || ow % iw != 0 {
            return None;
        }
        let tr = oh / ih;
        let tc = ow / iw;
        if tr <= 1 && tc <= 1 {
            return None;
        }

        match (tile_r, tile_c) {
            (None, None) => {
                tile_r = Some(tr);
                tile_c = Some(tc);
            }
            (Some(er), Some(ec)) => {
                if er != tr || ec != tc {
                    return None;
                }
            }
            _ => return None,
        }
    }

    let tr = tile_r?;
    let tc = tile_c?;

    // Verify tiling produces the correct output
    for (inp, out) in train_pairs {
        let tiled = tile_grid(inp, tr, tc);
        if tiled != *out {
            return None;
        }
    }

    let test_outputs: Vec<Grid> = test_inputs
        .iter()
        .map(|inp| tile_grid(inp, tr, tc))
        .collect();

    Some(DiffResult {
        program_desc: format!("pattern_tile({}x{})", tr, tc),
        test_outputs,
    })
}

fn tile_grid(g: &Grid, rows: usize, cols: usize) -> Grid {
    let (h, w) = grid_dims(g);
    let mut out = vec![vec![0u8; w * cols]; h * rows];
    for tr in 0..rows {
        for tc in 0..cols {
            for r in 0..h {
                for c in 0..w {
                    out[tr * h + r][tc * w + c] = g[r][c];
                }
            }
        }
    }
    out
}

// ============================================================
// Strategy 6: Remove or keep specific objects
// ============================================================

fn strategy_remove_objects(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    // Check: remove smallest objects
    if let Some(r) = remove_by_criterion(train_pairs, test_inputs, "smallest", |objs| {
        if objs.is_empty() {
            return BTreeSet::new();
        }
        let min_size = objs.iter().map(|o| o.size()).min().unwrap();
        objs.iter()
            .enumerate()
            .filter(|(_, o)| o.size() == min_size)
            .map(|(i, _)| i)
            .collect()
    }) {
        return Some(r);
    }

    // Check: remove objects of a specific color
    let all_colors = collect_all_colors(train_pairs);
    for &remove_color in &all_colors {
        if let Some(r) = remove_by_criterion(
            train_pairs,
            test_inputs,
            &format!("color_{}", remove_color),
            |objs| {
                objs.iter()
                    .enumerate()
                    .filter(|(_, o)| o.primary_color() == remove_color)
                    .map(|(i, _)| i)
                    .collect()
            },
        ) {
            return Some(r);
        }
    }

    None
}

fn remove_by_criterion(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    desc: &str,
    criterion: impl Fn(&[Object]) -> BTreeSet<usize>,
) -> Option<DiffResult> {
    let consistent = train_pairs.iter().all(|(inp, out)| {
        let bg = grid::mostcolor(inp);
        let objs = object::objects(inp, true, false, true);
        let to_remove = criterion(&objs);
        if to_remove.is_empty() {
            return false;
        }
        let mut result = inp.clone();
        for &idx in &to_remove {
            if idx < objs.len() {
                let positions = objs[idx].positions();
                result = grid::fill(&result, bg, &positions);
            }
        }
        result == *out
    });

    if !consistent {
        return None;
    }

    let test_outputs: Vec<Grid> = test_inputs
        .iter()
        .map(|inp| {
            let bg = grid::mostcolor(inp);
            let objs = object::objects(inp, true, false, true);
            let to_remove = criterion(&objs);
            let mut result = inp.clone();
            for &idx in &to_remove {
                if idx < objs.len() {
                    let positions = objs[idx].positions();
                    result = grid::fill(&result, bg, &positions);
                }
            }
            result
        })
        .collect();

    Some(DiffResult {
        program_desc: format!("remove_objects({})", desc),
        test_outputs,
    })
}

// ============================================================
// Strategy 7: Fill between objects
// ============================================================

fn strategy_fill_between(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
) -> Option<DiffResult> {
    let all_colors = collect_all_colors(train_pairs);

    // For each color, try filling between same-color objects
    for &fill_color in &all_colors {
        let consistent = train_pairs.iter().all(|(inp, out)| {
            let objs = object::objects(inp, true, false, true);
            let color_objs: Vec<_> = objs
                .iter()
                .filter(|o| o.primary_color() == fill_color)
                .collect();
            if color_objs.len() < 2 {
                return false;
            }

            let mut result = inp.clone();
            // Fill lines between bounding-box centers of same-color objects
            for i in 0..color_objs.len() {
                for j in (i + 1)..color_objs.len() {
                    let bb_i = object::bbox(color_objs[i]);
                    let bb_j = object::bbox(color_objs[j]);
                    let ci_r = (bb_i.0 + bb_i.2) / 2;
                    let ci_c = (bb_i.1 + bb_i.3) / 2;
                    let cj_r = (bb_j.0 + bb_j.2) / 2;
                    let cj_c = (bb_j.1 + bb_j.3) / 2;

                    // Horizontal line
                    if ci_r == cj_r {
                        let min_c = ci_c.min(cj_c);
                        let max_c = ci_c.max(cj_c);
                        for c in min_c..=max_c {
                            if c < result[0].len() {
                                result[ci_r][c] = fill_color;
                            }
                        }
                    }
                    // Vertical line
                    if ci_c == cj_c {
                        let min_r = ci_r.min(cj_r);
                        let max_r = ci_r.max(cj_r);
                        for r in min_r..=max_r {
                            if r < result.len() {
                                result[r][ci_c] = fill_color;
                            }
                        }
                    }
                }
            }
            result == *out
        });

        if consistent {
            let test_outputs: Vec<Grid> = test_inputs
                .iter()
                .map(|inp| {
                    let objs = object::objects(inp, true, false, true);
                    let color_objs: Vec<_> = objs
                        .iter()
                        .filter(|o| o.primary_color() == fill_color)
                        .collect();
                    let mut result = inp.clone();
                    for i in 0..color_objs.len() {
                        for j in (i + 1)..color_objs.len() {
                            let bb_i = object::bbox(color_objs[i]);
                            let bb_j = object::bbox(color_objs[j]);
                            let ci_r = (bb_i.0 + bb_i.2) / 2;
                            let ci_c = (bb_i.1 + bb_i.3) / 2;
                            let cj_r = (bb_j.0 + bb_j.2) / 2;
                            let cj_c = (bb_j.1 + bb_j.3) / 2;

                            if ci_r == cj_r {
                                let min_c = ci_c.min(cj_c);
                                let max_c = ci_c.max(cj_c);
                                for c in min_c..=max_c {
                                    if c < result[0].len() {
                                        result[ci_r][c] = fill_color;
                                    }
                                }
                            }
                            if ci_c == cj_c {
                                let min_r = ci_r.min(cj_r);
                                let max_r = ci_r.max(cj_r);
                                for r in min_r..=max_r {
                                    if r < result.len() {
                                        result[r][ci_c] = fill_color;
                                    }
                                }
                            }
                        }
                    }
                    result
                })
                .collect();

            return Some(DiffResult {
                program_desc: format!("fill_between(color={})", fill_color),
                test_outputs,
            });
        }
    }

    None
}

// ============================================================
// Utilities
// ============================================================

fn collect_all_colors(train_pairs: &[(Grid, Grid)]) -> Vec<u8> {
    let mut colors = BTreeSet::new();
    for (inp, out) in train_pairs {
        colors.extend(colors_in_grid(inp));
        colors.extend(colors_in_grid(out));
    }
    colors.into_iter().collect()
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_color_mapping() {
        // Replace color 1 -> 3
        let input = vec![vec![1, 0, 2], vec![2, 1, 0]];
        let output = vec![vec![3, 0, 2], vec![2, 3, 0]];
        let train = vec![(input.clone(), output.clone())];
        let test_in = vec![input];

        let result = diff_synthesize(&train, &test_in, 5000);
        assert!(result.is_some());
        let dr = result.unwrap();
        assert_eq!(dr.test_outputs, vec![output]);
        assert!(dr.program_desc.contains("color_map"));
    }

    #[test]
    fn test_noise_removal() {
        // Input has isolated cell at (0,2), should be removed
        let input = vec![
            vec![0, 0, 1, 0],
            vec![0, 2, 2, 0],
            vec![0, 2, 2, 0],
            vec![0, 0, 0, 0],
        ];
        let output = vec![
            vec![0, 0, 0, 0],
            vec![0, 2, 2, 0],
            vec![0, 2, 2, 0],
            vec![0, 0, 0, 0],
        ];
        let train = vec![(input.clone(), output.clone())];
        let test_in = vec![input];

        let result = diff_synthesize(&train, &test_in, 5000);
        assert!(result.is_some());
        let dr = result.unwrap();
        assert_eq!(dr.test_outputs, vec![output]);
    }

    #[test]
    fn test_gap_fill() {
        // Two cells of color 3 with a gap between them
        let input = vec![
            vec![0, 0, 0, 0, 0],
            vec![0, 3, 0, 0, 3],
            vec![0, 0, 0, 0, 0],
        ];
        let output = vec![
            vec![0, 0, 0, 0, 0],
            vec![0, 3, 3, 3, 3],
            vec![0, 0, 0, 0, 0],
        ];
        let train = vec![(input.clone(), output.clone())];
        let test_in = vec![input];

        let result = diff_synthesize(&train, &test_in, 5000);
        assert!(result.is_some());
        let dr = result.unwrap();
        assert_eq!(dr.test_outputs, vec![output]);
    }

    #[test]
    fn test_pattern_tile() {
        let input = vec![vec![1, 2], vec![3, 4]];
        let output = vec![
            vec![1, 2, 1, 2],
            vec![3, 4, 3, 4],
            vec![1, 2, 1, 2],
            vec![3, 4, 3, 4],
        ];
        let train = vec![(input.clone(), output.clone())];
        let test_in = vec![input];

        let result = diff_synthesize(&train, &test_in, 5000);
        assert!(result.is_some());
        let dr = result.unwrap();
        assert_eq!(dr.test_outputs, vec![output]);
    }

    #[test]
    fn test_pattern_extract() {
        let input = vec![
            vec![0, 0, 0, 0],
            vec![0, 5, 6, 0],
            vec![0, 7, 8, 0],
            vec![0, 0, 0, 0],
        ];
        let output = vec![vec![5, 6], vec![7, 8]];
        let train = vec![(input.clone(), output.clone())];
        let test_in = vec![input];

        let result = diff_synthesize(&train, &test_in, 5000);
        assert!(result.is_some());
        let dr = result.unwrap();
        assert_eq!(dr.test_outputs, vec![output]);
    }

    #[test]
    fn test_empty_input() {
        assert!(diff_synthesize(&[], &[], 5000).is_none());
    }
}
