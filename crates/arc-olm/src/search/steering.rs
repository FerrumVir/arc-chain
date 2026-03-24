//! Neural steering model for guided beam search.
//!
//! A 3-layer MLP with per-type output heads that scores operations
//! based on grid features. Trained on the 400 known Hodel solutions.
//! All inference is integer arithmetic (f32 weights, integer features).

use crate::Grid;
use crate::primitives::{grid, object};
use crate::search::enumerate::{DagType, DagValue};
use std::path::Path;

// ---------------------------------------------------------------------------
// Feature extraction (64 dims)
// ---------------------------------------------------------------------------

/// Extract 32-dimensional features from a single grid.
fn grid_features(g: &Grid) -> Vec<f32> {
    let h = g.len() as f32;
    let w = if g.is_empty() { 0.0 } else { g[0].len() as f32 };
    let total = h * w;

    // Color histogram (10 values, normalized to 0-1)
    let mut hist = [0f32; 10];
    for row in g {
        for &c in row {
            if (c as usize) < 10 {
                hist[c as usize] += 1.0;
            }
        }
    }
    if total > 0.0 {
        for h in &mut hist {
            *h /= total;
        }
    }

    // Background color (most common)
    let bg = hist
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(i, _)| i)
        .unwrap_or(0);

    // Unique colors
    let n_colors = hist.iter().filter(|&&c| c > 0.0).count() as f32;

    // Object count (simple 4-connected, skip bg)
    let objs = object::objects(g, true, false, true);
    let n_objs = objs.len() as f32;
    let max_obj_size = objs.iter().map(|o| o.size()).max().unwrap_or(0) as f32;
    let min_obj_size = objs.iter().map(|o| o.size()).min().unwrap_or(0) as f32;

    // Symmetry checks (fast)
    let sym_h = if *g == grid::hmirror(g) { 1.0 } else { 0.0 };
    let sym_v = if *g == grid::vmirror(g) { 1.0 } else { 0.0 };
    let sym_d = if h == w && *g == grid::dmirror(g) {
        1.0
    } else {
        0.0
    };
    let sym_r = if *g == grid::rot180(g) { 1.0 } else { 0.0 };

    let is_square = if h == w { 1.0 } else { 0.0 };
    let bg_frac = hist[bg];

    // Assemble 32-dim vector
    let mut feats = Vec::with_capacity(32);
    feats.push(h / 30.0); // normalized height
    feats.push(w / 30.0); // normalized width
    feats.extend_from_slice(&hist); // 10 color histogram values
    feats.push(n_colors / 10.0);
    feats.push(bg as f32 / 9.0);
    feats.push(n_objs / 20.0); // normalized object count
    feats.push(max_obj_size / total.max(1.0));
    feats.push(min_obj_size / total.max(1.0));
    feats.push(sym_h);
    feats.push(sym_v);
    feats.push(sym_d);
    feats.push(sym_r);
    feats.push(is_square);
    feats.push(bg_frac);
    // Pad to 32
    while feats.len() < 32 {
        feats.push(0.0);
    }
    feats.truncate(32);
    feats
}

/// Extract a 64-dimensional feature vector from the current DAG state
/// and the target grid.
///
/// Layout: `[grid_features(current) | grid_features(target)]`
///
/// If `current` is not a Grid variant, the first 32 dims are zero.
pub fn extract_features(current: &DagValue, target: &Grid, _current_type: &DagType) -> Vec<f32> {
    let current_feats = match current {
        DagValue::Grid(g) => grid_features(g),
        _ => vec![0.0; 32],
    };
    let target_feats = grid_features(target);

    let mut features = Vec::with_capacity(64);
    features.extend_from_slice(&current_feats);
    features.extend_from_slice(&target_feats);
    features
}

// ---------------------------------------------------------------------------
// Model structure
// ---------------------------------------------------------------------------

/// Weight matrix stored as f32 (simple, fast enough for 138K params).
pub struct WeightMatrix {
    pub weights: Vec<f32>, // row-major [n_rows x n_cols]
    pub bias: Vec<f32>,    // [n_rows]
    pub n_rows: usize,
    pub n_cols: usize,
}

/// Per-type output head.
pub struct TypeHead {
    pub weights: WeightMatrix,
    pub prim_names: Vec<String>,
}

/// The steering model: a 3-layer MLP with per-type output heads.
pub struct SteeringModel {
    pub layer1: WeightMatrix, // hidden1 x input
    pub layer2: WeightMatrix, // hidden2 x hidden1
    pub heads: Vec<TypeHead>, // one per DagType (Grid=0, Objects=1, ...)
}

// ---------------------------------------------------------------------------
// Binary helpers
// ---------------------------------------------------------------------------

fn read_u32(data: &[u8], cursor: &mut usize) -> u32 {
    let val = u32::from_le_bytes([
        data[*cursor],
        data[*cursor + 1],
        data[*cursor + 2],
        data[*cursor + 3],
    ]);
    *cursor += 4;
    val
}

fn read_f32(data: &[u8], cursor: &mut usize) -> f32 {
    let val = f32::from_le_bytes([
        data[*cursor],
        data[*cursor + 1],
        data[*cursor + 2],
        data[*cursor + 3],
    ]);
    *cursor += 4;
    val
}

fn read_weight_matrix(
    data: &[u8],
    cursor: &mut usize,
    n_rows: usize,
    n_cols: usize,
) -> Option<WeightMatrix> {
    let r = read_u32(data, cursor) as usize;
    let c = read_u32(data, cursor) as usize;
    if r != n_rows || c != n_cols {
        return None;
    }

    let mut weights = Vec::with_capacity(r * c);
    for _ in 0..(r * c) {
        weights.push(read_f32(data, cursor));
    }
    let mut bias = Vec::with_capacity(r);
    for _ in 0..r {
        bias.push(read_f32(data, cursor));
    }

    Some(WeightMatrix {
        weights,
        bias,
        n_rows: r,
        n_cols: c,
    })
}

// ---------------------------------------------------------------------------
// Matrix operations
// ---------------------------------------------------------------------------

fn matmul(matrix: &WeightMatrix, input: &[f32]) -> Vec<f32> {
    let mut output = vec![0.0f32; matrix.n_rows];
    for i in 0..matrix.n_rows {
        let mut sum = matrix.bias[i];
        let row_start = i * matrix.n_cols;
        for j in 0..matrix.n_cols.min(input.len()) {
            sum += matrix.weights[row_start + j] * input[j];
        }
        output[i] = sum;
    }
    output
}

// ---------------------------------------------------------------------------
// Model loading
// ---------------------------------------------------------------------------

impl SteeringModel {
    /// Load from binary file produced by `train_steering.py`.
    ///
    /// Binary format:
    /// - 4 bytes magic: `STMD`
    /// - u32: version, input_dim, h1, h2, num_heads
    /// - per head: u32 dim, u32 name_len, name bytes
    /// - weight matrices: layer1, layer2, then per-head weights + vocab
    pub fn load(path: &Path) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        // Read header: magic + dimensions
        if data.len() < 24 {
            return None;
        }
        if &data[0..4] != b"STMD" {
            return None;
        }
        let mut cursor = 4;

        // Read u32 values: version, input_dim, h1, h2, num_heads
        let _version = read_u32(&data, &mut cursor);
        let input_dim = read_u32(&data, &mut cursor) as usize;
        let h1 = read_u32(&data, &mut cursor) as usize;
        let h2 = read_u32(&data, &mut cursor) as usize;
        let num_heads = read_u32(&data, &mut cursor) as usize;

        // Read head configs
        let mut head_configs = Vec::new();
        for _ in 0..num_heads {
            let dim = read_u32(&data, &mut cursor) as usize;
            let name_len = read_u32(&data, &mut cursor) as usize;
            if cursor + name_len > data.len() {
                return None;
            }
            let name = String::from_utf8(data[cursor..cursor + name_len].to_vec()).ok()?;
            cursor += name_len;
            head_configs.push((name, dim));
        }

        // Read weight matrices
        let layer1 = read_weight_matrix(&data, &mut cursor, h1, input_dim)?;
        let layer2 = read_weight_matrix(&data, &mut cursor, h2, h1)?;

        let mut heads = Vec::new();
        for (_, head_dim) in &head_configs {
            let w = read_weight_matrix(&data, &mut cursor, *head_dim, h2)?;
            heads.push(TypeHead { weights: w, prim_names: Vec::new() });
        }

        // Read vocab section (newline-separated strings per head)
        for head in &mut heads {
            if cursor + 4 > data.len() { break; }
            let vocab_len = read_u32(&data, &mut cursor) as usize;
            if cursor + vocab_len > data.len() { break; }
            let vocab_str = String::from_utf8(data[cursor..cursor + vocab_len].to_vec()).ok()?;
            cursor += vocab_len;
            head.prim_names = vocab_str.split('\n')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
        }

        Some(SteeringModel {
            layer1,
            layer2,
            heads,
        })
    }
}

// ---------------------------------------------------------------------------
// Forward pass
// ---------------------------------------------------------------------------

impl SteeringModel {
    /// Score operations for the given state and type.
    /// Returns `(primitive_name, score)` pairs sorted by descending score.
    pub fn score_operations(
        &self,
        features: &[f32],
        current_type: &DagType,
    ) -> Vec<(String, f32)> {
        // Layer 1: matmul + ReLU
        let h1 = matmul(&self.layer1, features);
        let h1: Vec<f32> = h1.into_iter().map(|x| x.max(0.0)).collect();

        // Layer 2: matmul + ReLU
        let h2 = matmul(&self.layer2, &h1);
        let h2: Vec<f32> = h2.into_iter().map(|x| x.max(0.0)).collect();

        // Select type head
        let head_idx = match current_type {
            DagType::Grid => 0,
            DagType::Objects => 1,
            DagType::Object => 2,
            DagType::Indices => 3,
            DagType::Color => 4,
            DagType::Int => 5,
        };

        if head_idx >= self.heads.len() {
            return Vec::new();
        }

        let head = &self.heads[head_idx];
        let logits = matmul(&head.weights, &h2);

        // Pair with names, sort descending
        let mut scored: Vec<(String, f32)> = head
            .prim_names
            .iter()
            .zip(logits.iter())
            .map(|(name, &score)| (name.clone(), score))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Convenience function: extract features and score operations in one call.
pub fn steer(
    model: &SteeringModel,
    current: &DagValue,
    target: &Grid,
    current_type: &DagType,
) -> Vec<(String, f32)> {
    let features = extract_features(current, target, current_type);
    model.score_operations(&features, current_type)
}
