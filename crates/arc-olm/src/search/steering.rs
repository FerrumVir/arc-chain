use crate::Grid;
use crate::primitives::{grid, object};
use crate::search::enumerate::{DagType, DagValue};
use std::path::Path;

fn grid_features(g: &Grid) -> Vec<f32> {
    let h_cells = g.len();
    let w_cells = if g.is_empty() { 0 } else { g[0].len() };
    let h = h_cells as f32;
    let w = w_cells as f32;
    let total = h * w;

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

    let bg = hist
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(i, _)| i)
        .unwrap_or(0);

    let n_colors = hist.iter().filter(|&&c| c > 0.0).count() as f32;

    let objs = object::objects(g, true, false, true);
    let n_objs = objs.len() as f32;
    let max_obj_size = objs.iter().map(|o| o.size()).max().unwrap_or(0) as f32;
    let min_obj_size = objs.iter().map(|o| o.size()).min().unwrap_or(0) as f32;

    let sym_h = if *g == grid::hmirror(g) { 1.0 } else { 0.0 };
    let sym_v = if *g == grid::vmirror(g) { 1.0 } else { 0.0 };
    let sym_d = if h_cells == w_cells && *g == grid::dmirror(g) {
        1.0
    } else {
        0.0
    };
    let sym_r = if *g == grid::rot180(g) { 1.0 } else { 0.0 };

    let is_square = if h_cells == w_cells { 1.0 } else { 0.0 };
    let bg_frac = hist[bg];

    let mut feats = Vec::with_capacity(32);
    feats.push(h / 30.0);
    feats.push(w / 30.0);
    feats.extend_from_slice(&hist);
    feats.push(n_colors / 10.0);
    feats.push(bg as f32 / 9.0);
    feats.push(n_objs / 20.0);
    feats.push(max_obj_size / total.max(1.0));
    feats.push(min_obj_size / total.max(1.0));
    feats.push(sym_h);
    feats.push(sym_v);
    feats.push(sym_d);
    feats.push(sym_r);
    feats.push(is_square);
    feats.push(bg_frac);
    while feats.len() < 32 {
        feats.push(0.0);
    }
    feats.truncate(32);
    feats
}

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

pub struct WeightMatrix {
    pub weights: Vec<f32>,
    pub bias: Vec<f32>,
    pub n_rows: usize,
    pub n_cols: usize,
}

pub struct TypeHead {
    pub weights: WeightMatrix,
    pub prim_names: Vec<String>,
}

pub struct SteeringModel {
    pub layer1: WeightMatrix,
    pub layer2: WeightMatrix,
    pub heads: Vec<TypeHead>,
}

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

fn matmul(matrix: &WeightMatrix, input: &[f32]) -> Vec<f32> {
    let mut output = vec![0.0f32; matrix.n_rows];
    for (i, out) in output.iter_mut().enumerate() {
        let mut sum = matrix.bias[i];
        let row_start = i * matrix.n_cols;
        for (j, &x) in input.iter().enumerate().take(matrix.n_cols) {
            sum += matrix.weights[row_start + j] * x;
        }
        *out = sum;
    }
    output
}

impl SteeringModel {
    pub fn load(path: &Path) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        if data.len() < 24 {
            return None;
        }
        if &data[0..4] != b"STMD" {
            return None;
        }
        let mut cursor = 4;

        let _version = read_u32(&data, &mut cursor);
        let input_dim = read_u32(&data, &mut cursor) as usize;
        let h1 = read_u32(&data, &mut cursor) as usize;
        let h2 = read_u32(&data, &mut cursor) as usize;
        let num_heads = read_u32(&data, &mut cursor) as usize;

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

        let layer1 = read_weight_matrix(&data, &mut cursor, h1, input_dim)?;
        let layer2 = read_weight_matrix(&data, &mut cursor, h2, h1)?;

        let mut heads = Vec::new();
        for (_, head_dim) in &head_configs {
            let w = read_weight_matrix(&data, &mut cursor, *head_dim, h2)?;
            heads.push(TypeHead {
                weights: w,
                prim_names: Vec::new(),
            });
        }

        for head in &mut heads {
            if cursor + 4 > data.len() {
                break;
            }
            let vocab_len = read_u32(&data, &mut cursor) as usize;
            if cursor + vocab_len > data.len() {
                break;
            }
            let vocab_str = String::from_utf8(data[cursor..cursor + vocab_len].to_vec()).ok()?;
            cursor += vocab_len;
            head.prim_names = vocab_str
                .split('\n')
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

impl SteeringModel {
    pub fn score_operations(&self, features: &[f32], current_type: &DagType) -> Vec<(String, f32)> {
        let h1 = matmul(&self.layer1, features);
        let h1: Vec<f32> = h1.into_iter().map(|x| x.max(0.0)).collect();

        let h2 = matmul(&self.layer2, &h1);
        let h2: Vec<f32> = h2.into_iter().map(|x| x.max(0.0)).collect();

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

pub fn steer(
    model: &SteeringModel,
    current: &DagValue,
    target: &Grid,
    current_type: &DagType,
) -> Vec<(String, f32)> {
    let features = extract_features(current, target, current_type);
    model.score_operations(&features, current_type)
}
