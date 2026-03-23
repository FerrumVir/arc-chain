//! # arc-olm — Ontological Language Model
//!
//! Deterministic reasoning through typed DAG search.
//! The model doesn't predict tokens — it navigates a computation graph.
//! Every step is typed, verified, and deterministic.
//!
//! ## Architecture
//! - `primitives`: 160+ typed operations (grid, object, color, spatial)
//! - `search`: fitness beam search with Merkle dedup (parallel via Rayon)
//! - `ontology`: grid parser, diff engine, search guidance

pub mod primitives;
pub mod search;
pub mod ontology;

/// ARC-AGI grid: 2D array of colors (0-9), max 30x30.
pub type Grid = Vec<Vec<u8>>;

/// Color value (0-9 in ARC-AGI).
pub type Color = u8;

/// Position in a grid.
pub type Pos = (usize, usize);

/// Set of positions (for objects, indices, regions).
pub type PosSet = std::collections::BTreeSet<Pos>;

/// An object: a set of (color, position) cells.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Object {
    pub cells: std::collections::BTreeSet<(Color, Pos)>,
}

impl Object {
    pub fn positions(&self) -> PosSet {
        self.cells.iter().map(|(_, p)| *p).collect()
    }

    pub fn primary_color(&self) -> Color {
        let mut counts = [0u32; 10];
        for (c, _) in &self.cells {
            counts[*c as usize] += 1;
        }
        counts.iter().enumerate().max_by_key(|&(_, c)| *c).map(|(i, _)| i as u8).unwrap_or(0)
    }

    pub fn size(&self) -> usize {
        self.cells.len()
    }
}
