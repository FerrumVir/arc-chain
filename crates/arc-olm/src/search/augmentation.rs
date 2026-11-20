use crate::Grid;
use crate::primitives::grid;
use rayon::prelude::*;
use std::collections::HashMap;

pub struct Transform {
    pub name: &'static str,
    pub forward: fn(&Grid) -> Grid,
    pub inverse: fn(&Grid) -> Grid,
}

fn identity(g: &Grid) -> Grid {
    g.clone()
}

fn anti_transpose(g: &Grid) -> Grid {
    grid::cmirror(g)
}

pub fn d4_transforms() -> Vec<Transform> {
    vec![
        Transform {
            name: "identity",
            forward: identity,
            inverse: identity,
        },
        Transform {
            name: "rot90",
            forward: grid::rot90,
            inverse: grid::rot270,
        },
        Transform {
            name: "rot180",
            forward: grid::rot180,
            inverse: grid::rot180,
        },
        Transform {
            name: "rot270",
            forward: grid::rot270,
            inverse: grid::rot90,
        },
        Transform {
            name: "flip_h",
            forward: grid::hmirror,
            inverse: grid::hmirror,
        },
        Transform {
            name: "flip_v",
            forward: grid::vmirror,
            inverse: grid::vmirror,
        },
        Transform {
            name: "transpose",
            forward: grid::dmirror,
            inverse: grid::dmirror,
        },
        Transform {
            name: "anti_transpose",
            forward: anti_transpose,
            inverse: anti_transpose,
        },
    ]
}

fn find_bg(grid: &Grid) -> u8 {
    grid::mostcolor(grid)
}

fn color_perms(grid: &Grid) -> Vec<Vec<(u8, u8)>> {
    let bg = find_bg(grid);

    let mut present = [false; 10];
    for row in grid {
        for &c in row {
            if c != bg {
                present[c as usize] = true;
            }
        }
    }
    let colors: Vec<u8> = (0u8..10).filter(|&c| present[c as usize]).collect();

    if colors.is_empty() {
        return vec![vec![]];
    }

    let n = colors.len();

    if n <= 4 {
        let mut perms: Vec<Vec<(u8, u8)>> = Vec::new();
        let mut arr = colors.clone();
        heap_permutations(&mut arr, n, &colors, &mut perms);
        perms
    } else {
        let cap = 6.min(n);
        let mut perms: Vec<Vec<(u8, u8)>> = Vec::with_capacity(cap);
        for offset in 0..cap {
            let mapping: Vec<(u8, u8)> = colors
                .iter()
                .enumerate()
                .map(|(i, &from)| (from, colors[(i + offset) % n]))
                .collect();
            perms.push(mapping);
        }
        perms
    }
}

fn heap_permutations(arr: &mut Vec<u8>, k: usize, original: &[u8], out: &mut Vec<Vec<(u8, u8)>>) {
    if k == 1 {
        let mapping: Vec<(u8, u8)> = original
            .iter()
            .zip(arr.iter())
            .map(|(&from, &to)| (from, to))
            .collect();
        out.push(mapping);
        return;
    }
    for i in 0..k {
        heap_permutations(arr, k - 1, original, out);
        if k.is_multiple_of(2) {
            arr.swap(i, k - 1);
        } else {
            arr.swap(0, k - 1);
        }
    }
}

fn apply_color_map(grid: &Grid, mapping: &[(u8, u8)]) -> Grid {
    if mapping.is_empty() {
        return grid.clone();
    }
    let mut lut = [0u8; 10];
    for i in 0..10u8 {
        lut[i as usize] = i;
    }
    for &(from, to) in mapping {
        lut[from as usize] = to;
    }
    grid.iter()
        .map(|row| row.iter().map(|&c| lut[c as usize]).collect())
        .collect()
}

fn invert_color_map(mapping: &[(u8, u8)]) -> Vec<(u8, u8)> {
    mapping.iter().map(|&(a, b)| (b, a)).collect()
}

fn vote(candidates: &[Grid]) -> Option<Grid> {
    if candidates.is_empty() {
        return None;
    }

    let mut counts: HashMap<[u8; 32], (usize, &Grid)> = HashMap::new();
    for g in candidates {
        let h = grid::grid_hash(g);
        counts
            .entry(h)
            .and_modify(|(cnt, _)| *cnt += 1)
            .or_insert((1, g));
    }

    counts
        .into_values()
        .max_by_key(|(cnt, _)| *cnt)
        .map(|(_, g)| g.clone())
}

pub fn solve_with_augmentation<F>(
    train_pairs: &[(Grid, Grid)],
    test_inputs: &[Grid],
    solver: F,
) -> Vec<Option<Grid>>
where
    F: Fn(&[(Grid, Grid)], &[Grid]) -> Vec<Option<Grid>> + Send + Sync,
{
    let transforms = d4_transforms();
    let n_tests = test_inputs.len();

    let all_results: Vec<Vec<Option<Grid>>> = transforms
        .par_iter()
        .map(|xf| {
            let aug_train: Vec<(Grid, Grid)> = train_pairs
                .iter()
                .map(|(inp, out)| ((xf.forward)(inp), (xf.forward)(out)))
                .collect();

            let aug_tests: Vec<Grid> = test_inputs.iter().map(|inp| (xf.forward)(inp)).collect();

            let solutions = solver(&aug_train, &aug_tests);

            solutions
                .into_iter()
                .map(|opt| opt.map(|g| (xf.inverse)(&g)))
                .collect()
        })
        .collect();

    (0..n_tests)
        .map(|t| {
            let candidates: Vec<Grid> = all_results
                .iter()
                .filter_map(|run| run.get(t).and_then(|opt| opt.clone()))
                .collect();
            vote(&candidates)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_grid() -> Grid {
        vec![vec![1, 2, 3], vec![4, 5, 6]]
    }

    fn sample_square() -> Grid {
        vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]]
    }

    #[test]
    fn d4_round_trip_rectangular() {
        let g = sample_grid();
        for xf in d4_transforms() {
            let fwd = (xf.forward)(&g);
            let back = (xf.inverse)(&fwd);
            assert_eq!(back, g, "round-trip failed for {}", xf.name);
        }
    }

    #[test]
    fn d4_round_trip_square() {
        let g = sample_square();
        for xf in d4_transforms() {
            let fwd = (xf.forward)(&g);
            let back = (xf.inverse)(&fwd);
            assert_eq!(back, g, "round-trip failed for {} on square grid", xf.name);
        }
    }

    #[test]
    fn color_map_round_trip() {
        let g = vec![vec![0, 1, 2], vec![3, 0, 1]];
        let perms = color_perms(&g);
        for mapping in &perms {
            let mapped = apply_color_map(&g, mapping);
            let inv = invert_color_map(mapping);
            let restored = apply_color_map(&mapped, &inv);
            assert_eq!(restored, g, "color round-trip failed for {:?}", mapping);
        }
    }

    #[test]
    fn color_perms_count_small() {
        let g = vec![vec![0, 0, 0, 0], vec![0, 1, 2, 3]];
        let perms = color_perms(&g);
        assert_eq!(perms.len(), 6);
    }

    #[test]
    fn vote_picks_majority() {
        let a = vec![vec![1, 2], vec![3, 4]];
        let b = vec![vec![5, 6], vec![7, 8]];

        let candidates = vec![a.clone(), b.clone(), a.clone(), b.clone(), a.clone()];
        let winner = vote(&candidates);
        assert_eq!(winner, Some(a));
    }

    #[test]
    fn vote_empty_returns_none() {
        let candidates: Vec<Grid> = vec![];
        assert_eq!(vote(&candidates), None);
    }

    #[test]
    fn augmentation_with_identity_solver() {
        let solver = |_train: &[(Grid, Grid)], tests: &[Grid]| -> Vec<Option<Grid>> {
            tests.iter().map(|t| Some(t.clone())).collect()
        };

        let train = vec![(vec![vec![1, 2], vec![3, 4]], vec![vec![5, 6], vec![7, 8]])];
        let test_inputs = vec![vec![vec![9, 0], vec![1, 2]]];

        let results = solve_with_augmentation(&train, &test_inputs, solver);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], Some(vec![vec![9, 0], vec![1, 2]]));
    }

    #[test]
    fn find_bg_picks_most_common() {
        let g = vec![vec![0, 0, 0], vec![1, 2, 0]];
        assert_eq!(find_bg(&g), 0);

        let g2 = vec![vec![3, 3, 3], vec![3, 1, 3]];
        assert_eq!(find_bg(&g2), 3);
    }

    #[test]
    fn apply_empty_mapping_is_identity() {
        let g = vec![vec![0, 1, 2]];
        assert_eq!(apply_color_map(&g, &[]), g);
    }

    #[test]
    fn apply_swap_colors() {
        let g = vec![vec![1, 2, 3]];
        let mapping = vec![(1, 3), (3, 1)];
        assert_eq!(apply_color_map(&g, &mapping), vec![vec![3, 2, 1]]);
    }
}
