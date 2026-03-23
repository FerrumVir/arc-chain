//! Grid transformation primitives — the foundation of the OLM.
//!
//! Every function is pure: same input → same output.
//! Every function operates on integer grids (u8 values).
//! Zero floating point. Fully deterministic.

use crate::{Grid, Color, Pos, PosSet};

// ============================================================
// Spatial transforms (Grid → Grid)
// ============================================================

/// Rotate 90 degrees clockwise.
pub fn rot90(grid: &Grid) -> Grid {
    let h = grid.len();
    if h == 0 { return grid.clone(); }
    let w = grid[0].len();
    let mut out = vec![vec![0u8; h]; w];
    for r in 0..h {
        for c in 0..w {
            out[c][h - 1 - r] = grid[r][c];
        }
    }
    out
}

/// Rotate 180 degrees.
pub fn rot180(grid: &Grid) -> Grid {
    let h = grid.len();
    if h == 0 { return grid.clone(); }
    let w = grid[0].len();
    let mut out = vec![vec![0u8; w]; h];
    for r in 0..h {
        for c in 0..w {
            out[h - 1 - r][w - 1 - c] = grid[r][c];
        }
    }
    out
}

/// Rotate 270 degrees clockwise (= 90 degrees counter-clockwise).
pub fn rot270(grid: &Grid) -> Grid {
    let h = grid.len();
    if h == 0 { return grid.clone(); }
    let w = grid[0].len();
    let mut out = vec![vec![0u8; h]; w];
    for r in 0..h {
        for c in 0..w {
            out[w - 1 - c][r] = grid[r][c];
        }
    }
    out
}

/// Flip top-to-bottom (horizontal mirror).
pub fn hmirror(grid: &Grid) -> Grid {
    let mut out = grid.clone();
    out.reverse();
    out
}

/// Flip left-to-right (vertical mirror).
pub fn vmirror(grid: &Grid) -> Grid {
    grid.iter().map(|row| {
        let mut r = row.clone();
        r.reverse();
        r
    }).collect()
}

/// Transpose (mirror along main diagonal).
pub fn dmirror(grid: &Grid) -> Grid {
    let h = grid.len();
    if h == 0 { return grid.clone(); }
    let w = grid[0].len();
    let mut out = vec![vec![0u8; h]; w];
    for r in 0..h {
        for c in 0..w {
            out[c][r] = grid[r][c];
        }
    }
    out
}

/// Mirror along anti-diagonal.
pub fn cmirror(grid: &Grid) -> Grid {
    vmirror(&dmirror(&vmirror(grid)))
}

// ============================================================
// Grid splitting (Grid → Grid)
// ============================================================

/// Top half of grid.
pub fn tophalf(grid: &Grid) -> Grid {
    let h = grid.len() / 2;
    grid[..h].to_vec()
}

/// Bottom half of grid.
pub fn bottomhalf(grid: &Grid) -> Grid {
    let h = grid.len();
    let start = h / 2 + h % 2;
    grid[start..].to_vec()
}

/// Left half of grid.
pub fn lefthalf(grid: &Grid) -> Grid {
    rot270(&tophalf(&rot90(grid)))
}

/// Right half of grid.
pub fn righthalf(grid: &Grid) -> Grid {
    rot270(&bottomhalf(&rot90(grid)))
}

// ============================================================
// Color operations (Grid → Grid)
// ============================================================

/// Replace all occurrences of one color with another.
pub fn replace_color(grid: &Grid, old: Color, new: Color) -> Grid {
    grid.iter().map(|row| {
        row.iter().map(|&c| if c == old { new } else { c }).collect()
    }).collect()
}

/// Swap two colors.
pub fn switch_colors(grid: &Grid, a: Color, b: Color) -> Grid {
    grid.iter().map(|row| {
        row.iter().map(|&c| {
            if c == a { b } else if c == b { a } else { c }
        }).collect()
    }).collect()
}

/// Most common color in the grid.
pub fn mostcolor(grid: &Grid) -> Color {
    let mut counts = [0u32; 10];
    for row in grid {
        for &c in row {
            counts[c as usize] += 1;
        }
    }
    counts.iter().enumerate().max_by_key(|&(_, c)| *c).map(|(i, _)| i as u8).unwrap_or(0)
}

/// Least common color in the grid (among colors that appear).
pub fn leastcolor(grid: &Grid) -> Color {
    let mut counts = [0u32; 10];
    for row in grid {
        for &c in row {
            counts[c as usize] += 1;
        }
    }
    counts.iter().enumerate()
        .filter(|&(_, c)| *c > 0)
        .min_by_key(|&(_, c)| *c)
        .map(|(i, _)| i as u8)
        .unwrap_or(0)
}

// ============================================================
// Grid scaling
// ============================================================

/// Upscale grid by integer factor.
pub fn upscale(grid: &Grid, factor: usize) -> Grid {
    let mut out = Vec::new();
    for row in grid {
        let expanded_row: Vec<u8> = row.iter()
            .flat_map(|&c| std::iter::repeat(c).take(factor))
            .collect();
        for _ in 0..factor {
            out.push(expanded_row.clone());
        }
    }
    out
}

/// Downscale grid by integer factor (majority vote per block).
pub fn downscale(grid: &Grid, factor: usize) -> Grid {
    let h = grid.len();
    if h == 0 || factor == 0 { return grid.clone(); }
    let w = grid[0].len();
    let nh = h / factor;
    let nw = w / factor;
    let mut out = vec![vec![0u8; nw]; nh];
    for r in 0..nh {
        for c in 0..nw {
            // Take the value at the top-left of each block
            out[r][c] = grid[r * factor][c * factor];
        }
    }
    out
}

// ============================================================
// Grid concatenation
// ============================================================

/// Concatenate horizontally (side by side).
pub fn hconcat(a: &Grid, b: &Grid) -> Grid {
    let h = a.len().min(b.len());
    (0..h).map(|r| {
        let mut row = a[r].clone();
        row.extend_from_slice(&b[r]);
        row
    }).collect()
}

/// Concatenate vertically (top to bottom).
pub fn vconcat(a: &Grid, b: &Grid) -> Grid {
    let mut out = a.clone();
    out.extend_from_slice(b);
    out
}

// ============================================================
// Grid cropping
// ============================================================

/// Crop a subgrid starting at (sr, sc) with dimensions (h, w).
pub fn crop(grid: &Grid, sr: usize, sc: usize, h: usize, w: usize) -> Grid {
    let gh = grid.len();
    if gh == 0 { return vec![]; }
    let gw = grid[0].len();
    if sr >= gh || sc >= gw { return vec![]; }
    let eh = (sr + h).min(gh);
    let ew = (sc + w).min(gw);
    (sr..eh).map(|r| grid[r][sc..ew].to_vec()).collect()
}

/// Trim 1-cell border.
pub fn trim(grid: &Grid) -> Grid {
    let h = grid.len();
    if h <= 2 { return vec![]; }
    let w = grid[0].len();
    if w <= 2 { return vec![]; }
    (1..h-1).map(|r| grid[r][1..w-1].to_vec()).collect()
}

// ============================================================
// Index operations
// ============================================================

/// Get positions of all cells with a specific color.
pub fn ofcolor(grid: &Grid, color: Color) -> PosSet {
    let mut positions = PosSet::new();
    for (r, row) in grid.iter().enumerate() {
        for (c, &v) in row.iter().enumerate() {
            if v == color {
                positions.insert((r, c));
            }
        }
    }
    positions
}

/// Fill positions with a color.
pub fn fill(grid: &Grid, color: Color, positions: &PosSet) -> Grid {
    let mut out = grid.clone();
    for &(r, c) in positions {
        if r < out.len() && c < out[0].len() {
            out[r][c] = color;
        }
    }
    out
}

/// Fill positions with a color only where background.
pub fn underfill(grid: &Grid, color: Color, positions: &PosSet) -> Grid {
    let bg = mostcolor(grid);
    let mut out = grid.clone();
    for &(r, c) in positions {
        if r < out.len() && c < out[0].len() && out[r][c] == bg {
            out[r][c] = color;
        }
    }
    out
}

/// Compress: remove uniform rows and columns.
pub fn compress(grid: &Grid) -> Grid {
    let h = grid.len();
    if h == 0 { return grid.clone(); }
    let w = grid[0].len();

    // Find uniform rows
    let keep_rows: Vec<bool> = grid.iter()
        .map(|row| row.iter().collect::<std::collections::HashSet<_>>().len() > 1)
        .collect();

    // Find uniform columns
    let keep_cols: Vec<bool> = (0..w)
        .map(|c| {
            let col_vals: std::collections::HashSet<u8> = grid.iter().map(|row| row[c]).collect();
            col_vals.len() > 1
        })
        .collect();

    grid.iter().enumerate()
        .filter(|(r, _)| keep_rows[*r])
        .map(|(_, row)| {
            row.iter().enumerate()
                .filter(|(c, _)| keep_cols[*c])
                .map(|(_, &v)| v)
                .collect()
        })
        .collect()
}

/// Canvas: create a grid filled with one color.
pub fn canvas(color: Color, h: usize, w: usize) -> Grid {
    vec![vec![color; w]; h]
}

// ============================================================
// Grid comparison
// ============================================================

/// Cellwise comparison: keep matching values, use fallback for mismatches.
pub fn cellwise(a: &Grid, b: &Grid, fallback: Color) -> Grid {
    let h = a.len().min(b.len());
    if h == 0 { return vec![]; }
    let w = a[0].len().min(b[0].len());
    (0..h).map(|r| {
        (0..w).map(|c| {
            if a[r][c] == b[r][c] { a[r][c] } else { fallback }
        }).collect()
    }).collect()
}

/// Split grid horizontally into n parts.
pub fn hsplit(grid: &Grid, n: usize) -> Vec<Grid> {
    if n == 0 { return vec![]; }
    let h = grid.len();
    let w = grid[0].len();
    let pw = w / n;
    (0..n).map(|i| crop(grid, 0, pw * i, h, pw)).collect()
}

/// Split grid vertically into n parts.
pub fn vsplit(grid: &Grid, n: usize) -> Vec<Grid> {
    if n == 0 { return vec![]; }
    let h = grid.len();
    let w = grid[0].len();
    let ph = h / n;
    (0..n).map(|i| crop(grid, ph * i, 0, ph, w)).collect()
}

// ============================================================
// Deterministic grid hash (for Merkle)
// ============================================================

/// Blake3 hash of a grid. Deterministic across all platforms.
pub fn grid_hash(grid: &Grid) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for row in grid {
        hasher.update(row);
    }
    *hasher.finalize().as_bytes()
}

/// Check if two grids are identical.
pub fn grids_equal(a: &Grid, b: &Grid) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rot90() {
        let grid = vec![vec![1, 2], vec![3, 4]];
        let rotated = rot90(&grid);
        assert_eq!(rotated, vec![vec![3, 1], vec![4, 2]]);
    }

    #[test]
    fn test_vmirror() {
        let grid = vec![vec![1, 2], vec![3, 4]];
        let mirrored = vmirror(&grid);
        assert_eq!(mirrored, vec![vec![2, 1], vec![4, 3]]);
    }

    #[test]
    fn test_replace_color() {
        let grid = vec![vec![1, 0, 2], vec![2, 1, 0]];
        let result = replace_color(&grid, 1, 3);
        assert_eq!(result, vec![vec![3, 0, 2], vec![2, 3, 0]]);
    }

    #[test]
    fn test_upscale() {
        let grid = vec![vec![1, 2], vec![3, 4]];
        let scaled = upscale(&grid, 2);
        assert_eq!(scaled.len(), 4);
        assert_eq!(scaled[0].len(), 4);
        assert_eq!(scaled[0], vec![1, 1, 2, 2]);
    }

    #[test]
    fn test_hconcat() {
        let a = vec![vec![1, 2], vec![3, 4]];
        let b = vec![vec![5, 6], vec![7, 8]];
        let result = hconcat(&a, &b);
        assert_eq!(result, vec![vec![1, 2, 5, 6], vec![3, 4, 7, 8]]);
    }

    #[test]
    fn test_fill() {
        let grid = vec![vec![0, 0, 0], vec![0, 0, 0]];
        let mut positions = PosSet::new();
        positions.insert((0, 1));
        positions.insert((1, 2));
        let result = fill(&grid, 5, &positions);
        assert_eq!(result, vec![vec![0, 5, 0], vec![0, 0, 5]]);
    }

    #[test]
    fn test_grid_hash_determinism() {
        let grid = vec![vec![1, 2, 3], vec![4, 5, 6]];
        let h1 = grid_hash(&grid);
        let h2 = grid_hash(&grid);
        assert_eq!(h1, h2); // deterministic
    }
}
