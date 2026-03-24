//! D4 augmentation + color permutation + Product of Experts voting.
//!
//! Strategy:
//! 1. Apply each of 8 rigid transforms (D4 dihedral group) to training pairs
//! 2. Optionally apply color permutations
//! 3. Solve the augmented task
//! 4. Inverse-transform the solution back
//! 5. Vote: pick the most common output across all augmentations

use crate::Grid;
use crate::primitives::grid;
use rayon::prelude::*;
use std::collections::HashMap;

// ============================================================
// D4 Transform table
// ============================================================

/// A rigid transform with its inverse.
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

/// The 8 elements of the D4 dihedral group.
pub fn d4_transforms() -> Vec<Transform> {
    vec![
        Transform { name: "identity",       forward: identity,       inverse: identity },
        Transform { name: "rot90",           forward: grid::rot90,    inverse: grid::rot270 },
        Transform { name: "rot180",          forward: grid::rot180,   inverse: grid::rot180 },
        Transform { name: "rot270",          forward: grid::rot270,   inverse: grid::rot90 },
        Transform { name: "flip_h",          forward: grid::hmirror,  inverse: grid::hmirror },
        Transform { name: "flip_v",          forward: grid::vmirror,  inverse: grid::vmirror },
        Transform { name: "transpose",       forward: grid::dmirror,  inverse: grid::dmirror },
        Transform { name: "anti_transpose",  forward: anti_transpose, inverse: anti_transpose },
    ]
}

// ============================================================
// Color permutation helpers
// ============================================================

/// Find background color (most common in grid).
fn find_bg(grid: &Grid) -> u8 {
    grid::mostcolor(grid)
}

/// Generate color permutations (non-background colors only).
///
/// Returns a list of mappings `(from, to)` for each permutation.
/// If there are 4 or fewer non-background colors, all permutations are
/// generated. Otherwise, up to 6 random-ish permutations are returned
/// (using deterministic shuffles based on rotation of the color list).
fn color_perms(grid: &Grid) -> Vec<Vec<(u8, u8)>> {
    let bg = find_bg(grid);

    // Collect non-background colors that appear in the grid.
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
        // Nothing to permute: return the identity mapping only.
        return vec![vec![]];
    }

    let n = colors.len();

    if n <= 4 {
        // Enumerate all permutations via Heap's algorithm.
        let mut perms: Vec<Vec<(u8, u8)>> = Vec::new();
        let mut arr = colors.clone();
        heap_permutations(&mut arr, n, &colors, &mut perms);
        perms
    } else {
        // Too many colors for exhaustive enumeration; produce up to 6
        // deterministic rotations of the color list.
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

/// Heap's algorithm for generating all permutations.
fn heap_permutations(
    arr: &mut Vec<u8>,
    k: usize,
    original: &[u8],
    out: &mut Vec<Vec<(u8, u8)>>,
) {
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
        if k % 2 == 0 {
            arr.swap(i, k - 1);
        } else {
            arr.swap(0, k - 1);
        }
    }
}

/// Apply a color mapping to a grid.
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

/// Inverse of a color mapping.
fn invert_color_map(mapping: &[(u8, u8)]) -> Vec<(u8, u8)> {
    mapping.iter().map(|&(a, b)| (b, a)).collect()
}

// ============================================================
// Product-of-Experts voting
// ============================================================

/// Pick the grid that appears most often in `candidates`.
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

// ============================================================
// Main entry point
// ============================================================

/// Solve a task with augmentation and PoE voting.
///
/// For each of the 8 D4 transforms the training pairs and test inputs
/// are transformed, the solver is invoked, and the results are
/// inverse-transformed back into the original coordinate frame.
/// The final answer for each test input is chosen by majority vote
/// across all augmentations.
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

    // Run solver under each D4 transform in parallel.
    let all_results: Vec<Vec<Option<Grid>>> = transforms
        .par_iter()
        .map(|xf| {
            // Transform training pairs.
            let aug_train: Vec<(Grid, Grid)> = train_pairs
                .iter()
                .map(|(inp, out)| ((xf.forward)(inp), (xf.forward)(out)))
                .collect();

            // Transform test inputs.
            let aug_tests: Vec<Grid> = test_inputs
                .iter()
                .map(|inp| (xf.forward)(inp))
                .collect();

            // Solve.
            let solutions = solver(&aug_train, &aug_tests);

            // Inverse-transform solutions back.
            solutions
                .into_iter()
                .map(|opt| opt.map(|g| (xf.inverse)(&g)))
                .collect()
        })
        .collect();

    // Vote for each test input.
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

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_grid() -> Grid {
        vec![
            vec![1, 2, 3],
            vec![4, 5, 6],
        ]
    }

    fn sample_square() -> Grid {
        vec![
            vec![1, 2, 3],
            vec![4, 5, 6],
            vec![7, 8, 9],
        ]
    }

    // ----------------------------------------------------------
    // D4 round-trip: forward(inverse(g)) == g for every transform
    // ----------------------------------------------------------

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

    // ----------------------------------------------------------
    // Color permutation round-trip
    // ----------------------------------------------------------

    #[test]
    fn color_map_round_trip() {
        let g = vec![
            vec![0, 1, 2],
            vec![3, 0, 1],
        ];
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
        // Grid with 3 non-bg colors (bg=0 appears most).
        let g = vec![
            vec![0, 0, 0, 0],
            vec![0, 1, 2, 3],
        ];
        let perms = color_perms(&g);
        // 3! = 6 permutations for 3 non-bg colors.
        assert_eq!(perms.len(), 6);
    }

    // ----------------------------------------------------------
    // Voting
    // ----------------------------------------------------------

    #[test]
    fn vote_picks_majority() {
        let a = vec![vec![1, 2], vec![3, 4]];
        let b = vec![vec![5, 6], vec![7, 8]];

        // 'a' appears 3 times, 'b' appears 2 times.
        let candidates = vec![a.clone(), b.clone(), a.clone(), b.clone(), a.clone()];
        let winner = vote(&candidates);
        assert_eq!(winner, Some(a));
    }

    #[test]
    fn vote_empty_returns_none() {
        let candidates: Vec<Grid> = vec![];
        assert_eq!(vote(&candidates), None);
    }

    // ----------------------------------------------------------
    // solve_with_augmentation integration test
    // ----------------------------------------------------------

    #[test]
    fn augmentation_with_identity_solver() {
        // A solver that just returns the first test input as-is.
        let solver = |_train: &[(Grid, Grid)], tests: &[Grid]| -> Vec<Option<Grid>> {
            tests.iter().map(|t| Some(t.clone())).collect()
        };

        let train = vec![
            (vec![vec![1, 2], vec![3, 4]], vec![vec![5, 6], vec![7, 8]]),
        ];
        let test_inputs = vec![vec![vec![9, 0], vec![1, 2]]];

        let results = solve_with_augmentation(&train, &test_inputs, solver);
        assert_eq!(results.len(), 1);
        // The identity solver returns the transformed test input, then the
        // inverse transform is applied. For every D4 element, inv(fwd(x)) == x,
        // so all 8 candidates are the original test input and the vote is unanimous.
        assert_eq!(results[0], Some(vec![vec![9, 0], vec![1, 2]]));
    }

    // ----------------------------------------------------------
    // find_bg
    // ----------------------------------------------------------

    #[test]
    fn find_bg_picks_most_common() {
        let g = vec![
            vec![0, 0, 0],
            vec![1, 2, 0],
        ];
        assert_eq!(find_bg(&g), 0);

        let g2 = vec![
            vec![3, 3, 3],
            vec![3, 1, 3],
        ];
        assert_eq!(find_bg(&g2), 3);
    }

    // ----------------------------------------------------------
    // apply_color_map
    // ----------------------------------------------------------

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
