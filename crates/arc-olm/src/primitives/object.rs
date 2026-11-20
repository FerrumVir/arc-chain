use crate::primitives::grid::mostcolor;
use crate::{Color, Grid, Object, Pos, PosSet};
use std::collections::{BTreeSet, VecDeque};

pub fn objects(grid: &Grid, univalued: bool, diagonal: bool, without_bg: bool) -> Vec<Object> {
    let h = grid.len();
    if h == 0 {
        return vec![];
    }
    let w = grid[0].len();

    let bg = if without_bg {
        Some(mostcolor(grid))
    } else {
        None
    };
    let mut visited = vec![vec![false; w]; h];
    let mut result = Vec::new();

    for r in 0..h {
        for c in 0..w {
            if visited[r][c] {
                continue;
            }
            let color = grid[r][c];
            if bg == Some(color) {
                visited[r][c] = true;
                continue;
            }

            let mut cells = BTreeSet::new();
            let mut queue = VecDeque::new();
            queue.push_back((r, c));
            visited[r][c] = true;

            while let Some((cr, cc)) = queue.pop_front() {
                cells.insert((grid[cr][cc], (cr, cc)));

                let neighbors: Vec<(usize, usize)> = if diagonal {
                    vec![
                        (cr.wrapping_sub(1), cc),
                        (cr + 1, cc),
                        (cr, cc.wrapping_sub(1)),
                        (cr, cc + 1),
                        (cr.wrapping_sub(1), cc.wrapping_sub(1)),
                        (cr.wrapping_sub(1), cc + 1),
                        (cr + 1, cc.wrapping_sub(1)),
                        (cr + 1, cc + 1),
                    ]
                } else {
                    vec![
                        (cr.wrapping_sub(1), cc),
                        (cr + 1, cc),
                        (cr, cc.wrapping_sub(1)),
                        (cr, cc + 1),
                    ]
                };

                for (nr, nc) in neighbors {
                    if nr < h && nc < w && !visited[nr][nc] {
                        let nc_color = grid[nr][nc];
                        if bg == Some(nc_color) {
                            continue;
                        }
                        if univalued && nc_color != color {
                            continue;
                        }
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

pub fn colorfilter(objs: &[Object], color: Color) -> Vec<Object> {
    objs.iter()
        .filter(|o| o.primary_color() == color)
        .cloned()
        .collect()
}

pub fn sizefilter(objs: &[Object], size: usize) -> Vec<Object> {
    objs.iter().filter(|o| o.size() == size).cloned().collect()
}

pub fn argmax_size(objs: &[Object]) -> Option<&Object> {
    objs.iter().max_by_key(|o| o.size())
}

pub fn argmin_size(objs: &[Object]) -> Option<&Object> {
    objs.iter().min_by_key(|o| o.size())
}

pub fn bbox(obj: &Object) -> (usize, usize, usize, usize) {
    let positions = obj.positions();
    if positions.is_empty() {
        return (0, 0, 0, 0);
    }
    let min_r = positions.iter().map(|p| p.0).min().unwrap();
    let max_r = positions.iter().map(|p| p.0).max().unwrap();
    let min_c = positions.iter().map(|p| p.1).min().unwrap();
    let max_c = positions.iter().map(|p| p.1).max().unwrap();
    (min_r, min_c, max_r, max_c)
}

pub fn subgrid(obj: &Object, grid: &Grid) -> Grid {
    let (min_r, min_c, max_r, max_c) = bbox(obj);
    crate::primitives::grid::crop(grid, min_r, min_c, max_r - min_r + 1, max_c - min_c + 1)
}

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

pub fn cover(grid: &Grid, obj: &Object) -> Grid {
    let bg = mostcolor(grid);
    let positions = obj.positions();
    crate::primitives::grid::fill(grid, bg, &positions)
}

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

pub fn obj_box(obj: &Object) -> PosSet {
    let (min_r, min_c, max_r, max_c) = bbox(obj);
    let mut result = PosSet::new();
    for r in min_r..=max_r {
        result.insert((r, min_c));
        result.insert((r, max_c));
    }
    for c in min_c..=max_c {
        result.insert((min_r, c));
        result.insert((max_r, c));
    }
    result
}

pub fn corners(obj: &Object) -> PosSet {
    let (min_r, min_c, max_r, max_c) = bbox(obj);
    let mut result = PosSet::new();
    result.insert((min_r, min_c));
    result.insert((min_r, max_c));
    result.insert((max_r, min_c));
    result.insert((max_r, max_c));
    result
}

pub fn inbox(obj: &Object) -> PosSet {
    let (min_r, min_c, max_r, max_c) = bbox(obj);
    if max_r <= min_r + 1 || max_c <= min_c + 1 {
        return PosSet::new();
    }
    let ir = min_r + 1;
    let ic = min_c + 1;
    let er = max_r - 1;
    let ec = max_c - 1;
    let mut result = PosSet::new();
    for r in ir..=er {
        result.insert((r, ic));
        result.insert((r, ec));
    }
    for c in ic..=ec {
        result.insert((ir, c));
        result.insert((er, c));
    }
    result
}

pub fn outbox(obj: &Object) -> PosSet {
    let (min_r, min_c, max_r, max_c) = bbox(obj);
    let or = if min_r > 0 { min_r - 1 } else { 0 };
    let oc = if min_c > 0 { min_c - 1 } else { 0 };
    let er = max_r + 1;
    let ec = max_c + 1;
    let mut result = PosSet::new();
    for r in or..=er {
        result.insert((r, oc));
        result.insert((r, ec));
    }
    for c in oc..=ec {
        result.insert((or, c));
        result.insert((er, c));
    }
    result
}

pub fn shift(obj: &Object, dr: isize, dc: isize) -> Object {
    let cells = obj
        .cells
        .iter()
        .filter_map(|&(color, (r, c))| {
            let nr = r as isize + dr;
            let nc = c as isize + dc;
            if nr >= 0 && nc >= 0 {
                Some((color, (nr as usize, nc as usize)))
            } else {
                None
            }
        })
        .collect();
    Object { cells }
}

pub fn move_obj(grid: &Grid, obj: &Object, dr: isize, dc: isize) -> Grid {
    let covered = cover(grid, obj);
    let shifted = shift(obj, dr, dc);
    paint(&covered, &shifted)
}

pub fn merge_objects(objs: &[Object]) -> Object {
    let mut cells = BTreeSet::new();
    for obj in objs {
        cells.extend(&obj.cells);
    }
    Object { cells }
}

pub fn obj_height(obj: &Object) -> usize {
    let (min_r, _, max_r, _) = bbox(obj);
    max_r - min_r + 1
}

pub fn obj_width(obj: &Object) -> usize {
    let (_, min_c, _, max_c) = bbox(obj);
    max_c - min_c + 1
}

pub fn connect(a: (usize, usize), b: (usize, usize)) -> PosSet {
    let mut result = PosSet::new();
    if a.0 == b.0 {
        let (start, end) = if a.1 <= b.1 { (a.1, b.1) } else { (b.1, a.1) };
        for c in start..=end {
            result.insert((a.0, c));
        }
    } else if a.1 == b.1 {
        let (start, end) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
        for r in start..=end {
            result.insert((r, a.1));
        }
    } else {
        let dr: isize = if b.0 > a.0 { 1 } else { -1 };
        let dc: isize = if b.1 > a.1 { 1 } else { -1 };
        let mut r = a.0 as isize;
        let mut c = a.1 as isize;
        let steps = (b.0 as isize - a.0 as isize)
            .unsigned_abs()
            .max((b.1 as isize - a.1 as isize).unsigned_abs());
        for _ in 0..=steps {
            if r >= 0 && c >= 0 {
                result.insert((r as usize, c as usize));
            }
            r += dr;
            c += dc;
        }
    }
    result
}

pub fn shoot(start: (usize, usize), dr: isize, dc: isize) -> PosSet {
    let mut result = PosSet::new();
    let mut r = start.0 as isize;
    let mut c = start.1 as isize;
    for _ in 0..30 {
        if r >= 0 && c >= 0 {
            result.insert((r as usize, c as usize));
        }
        r += dr;
        c += dc;
    }
    result
}

pub fn dneighbors(pos: (usize, usize)) -> PosSet {
    let mut result = PosSet::new();
    let (r, c) = pos;
    if r > 0 {
        result.insert((r - 1, c));
    }
    result.insert((r + 1, c));
    if c > 0 {
        result.insert((r, c - 1));
    }
    result.insert((r, c + 1));
    result
}

pub fn neighbors(pos: (usize, usize)) -> PosSet {
    let mut result = dneighbors(pos);
    let (r, c) = pos;
    if r > 0 && c > 0 {
        result.insert((r - 1, c - 1));
    }
    if r > 0 {
        result.insert((r - 1, c + 1));
    }
    if c > 0 {
        result.insert((r + 1, c - 1));
    }
    result.insert((r + 1, c + 1));
    result
}

pub fn mapply_neighbors(positions: &PosSet) -> PosSet {
    let mut result = PosSet::new();
    for &pos in positions {
        result.extend(neighbors(pos));
    }
    result
}

pub fn mapply_dneighbors(positions: &PosSet) -> PosSet {
    let mut result = PosSet::new();
    for &pos in positions {
        result.extend(dneighbors(pos));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_objects_extraction() {
        let grid = vec![vec![0, 1, 0], vec![0, 1, 0], vec![0, 0, 2]];
        let objs = objects(&grid, true, false, true);
        assert_eq!(objs.len(), 2);
    }

    #[test]
    fn test_delta() {
        let mut cells = BTreeSet::new();
        cells.insert((1, (0, 0)));
        cells.insert((1, (0, 1)));
        cells.insert((1, (1, 0)));
        let obj = Object { cells };
        let d = delta(&obj);
        assert!(d.contains(&(1, 1)));
        assert_eq!(d.len(), 1);
    }
}
