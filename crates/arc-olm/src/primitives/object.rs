//! Object extraction and manipulation primitives.

use crate::{Grid, Color, Pos, PosSet, Object};
use crate::primitives::grid::mostcolor;
use std::collections::{BTreeSet, VecDeque};

/// Extract connected-component objects from a grid.
///
/// Parameters:
/// - `univalued`: if true, each object has one color. If false, multi-color.
/// - `diagonal`: if true, 8-connected. If false, 4-connected.
/// - `without_bg`: if true, ignore background (most common) color.
pub fn objects(grid: &Grid, univalued: bool, diagonal: bool, without_bg: bool) -> Vec<Object> {
    let h = grid.len();
    if h == 0 { return vec![]; }
    let w = grid[0].len();

    let bg = if without_bg { Some(mostcolor(grid)) } else { None };
    let mut visited = vec![vec![false; w]; h];
    let mut result = Vec::new();

    for r in 0..h {
        for c in 0..w {
            if visited[r][c] { continue; }
            let color = grid[r][c];
            if bg == Some(color) {
                visited[r][c] = true;
                continue;
            }

            // BFS to find connected component
            let mut cells = BTreeSet::new();
            let mut queue = VecDeque::new();
            queue.push_back((r, c));
            visited[r][c] = true;

            while let Some((cr, cc)) = queue.pop_front() {
                cells.insert((grid[cr][cc], (cr, cc)));

                // Neighbors
                let neighbors: Vec<(usize, usize)> = if diagonal {
                    vec![
                        (cr.wrapping_sub(1), cc), (cr + 1, cc),
                        (cr, cc.wrapping_sub(1)), (cr, cc + 1),
                        (cr.wrapping_sub(1), cc.wrapping_sub(1)),
                        (cr.wrapping_sub(1), cc + 1),
                        (cr + 1, cc.wrapping_sub(1)),
                        (cr + 1, cc + 1),
                    ]
                } else {
                    vec![
                        (cr.wrapping_sub(1), cc), (cr + 1, cc),
                        (cr, cc.wrapping_sub(1)), (cr, cc + 1),
                    ]
                };

                for (nr, nc) in neighbors {
                    if nr < h && nc < w && !visited[nr][nc] {
                        let nc_color = grid[nr][nc];
                        if bg == Some(nc_color) { continue; }
                        if univalued && nc_color != color { continue; }
                        visited[nr][nc] = true;
                        queue.push_back((nr, nc));
                    }
                }
            }

            if !cells.is_empty() {
                result.push(Object { cells });
            }
        }
    }

    result
}

/// Filter objects by color.
pub fn colorfilter(objs: &[Object], color: Color) -> Vec<Object> {
    objs.iter().filter(|o| o.primary_color() == color).cloned().collect()
}

/// Filter objects by size.
pub fn sizefilter(objs: &[Object], size: usize) -> Vec<Object> {
    objs.iter().filter(|o| o.size() == size).cloned().collect()
}

/// Select largest object.
pub fn argmax_size(objs: &[Object]) -> Option<&Object> {
    objs.iter().max_by_key(|o| o.size())
}

/// Select smallest object.
pub fn argmin_size(objs: &[Object]) -> Option<&Object> {
    objs.iter().min_by_key(|o| o.size())
}

/// Bounding box of an object's positions.
pub fn bbox(obj: &Object) -> (usize, usize, usize, usize) {
    let positions = obj.positions();
    if positions.is_empty() { return (0, 0, 0, 0); }
    let min_r = positions.iter().map(|p| p.0).min().unwrap();
    let max_r = positions.iter().map(|p| p.0).max().unwrap();
    let min_c = positions.iter().map(|p| p.1).min().unwrap();
    let max_c = positions.iter().map(|p| p.1).max().unwrap();
    (min_r, min_c, max_r, max_c)
}

/// Extract the smallest subgrid containing an object.
pub fn subgrid(obj: &Object, grid: &Grid) -> Grid {
    let (min_r, min_c, max_r, max_c) = bbox(obj);
    crate::primitives::grid::crop(grid, min_r, min_c, max_r - min_r + 1, max_c - min_c + 1)
}

/// Delta: bounding box positions MINUS object positions (holes).
pub fn delta(obj: &Object) -> PosSet {
    let (min_r, min_c, max_r, max_c) = bbox(obj);
    let positions = obj.positions();
    let mut result = PosSet::new();
    for r in min_r..=max_r {
        for c in min_c..=max_c {
            if !positions.contains(&(r, c)) {
                result.insert((r, c));
            }
        }
    }
    result
}

/// Backdrop: all positions in the bounding box.
pub fn backdrop(obj: &Object) -> PosSet {
    let (min_r, min_c, max_r, max_c) = bbox(obj);
    let mut result = PosSet::new();
    for r in min_r..=max_r {
        for c in min_c..=max_c {
            result.insert((r, c));
        }
    }
    result
}

/// Cover: erase an object from the grid (fill with background).
pub fn cover(grid: &Grid, obj: &Object) -> Grid {
    let bg = mostcolor(grid);
    let positions = obj.positions();
    crate::primitives::grid::fill(grid, bg, &positions)
}

/// Paint an object onto a grid.
pub fn paint(grid: &Grid, obj: &Object) -> Grid {
    let mut out = grid.clone();
    let h = out.len();
    let w = if h > 0 { out[0].len() } else { 0 };
    for &(color, (r, c)) in &obj.cells {
        if r < h && c < w {
            out[r][c] = color;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_objects_extraction() {
        let grid = vec![
            vec![0, 1, 0],
            vec![0, 1, 0],
            vec![0, 0, 2],
        ];
        let objs = objects(&grid, true, false, true);
        assert_eq!(objs.len(), 2); // object of color 1 and object of color 2
    }

    #[test]
    fn test_delta() {
        // 2x2 object with one hole
        let mut cells = BTreeSet::new();
        cells.insert((1, (0, 0)));
        cells.insert((1, (0, 1)));
        cells.insert((1, (1, 0)));
        // (1,1) is the hole
        let obj = Object { cells };
        let d = delta(&obj);
        assert!(d.contains(&(1, 1)));
        assert_eq!(d.len(), 1);
    }
}
