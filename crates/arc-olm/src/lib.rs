//! # arc-olm - Ontological Language Model
//!
//! Deterministic reasoning through typed DAG search.
//! The model doesn't predict tokens - it navigates a computation graph.
//! Every step is typed, verified, and deterministic.
//!
//! ## Architecture
//! - `primitives`: 160+ typed operations (grid, object, color, spatial)
//! - `search`: fitness beam search with Merkle dedup (parallel via Rayon)
//! - `ontology`: grid parser, diff engine, search guidance
//!
//! This crate is research-track: it ships combinatorial helpers + imports
//! that are unused at any given time depending on which solver variant is
//! active. Allow dead_code, unused_imports, and the stylistic clippy lints
//! crate-wide so CI doesn't block experimentation.
//!
//! Note `clippy::never_loop` is deny-by-default (treated as a correctness
//! bug) - our ARC-AGI solver synthesizer has patterns that false-positive
//! on it, so we allow it crate-wide.
#![allow(dead_code, unused_imports, unused_variables, clippy::never_loop)]

pub mod ontology;
pub mod primitives;
pub mod search;

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
        counts
            .iter()
            .enumerate()
            .max_by_key(|&(_, c)| *c)
            .map(|(i, _)| i as u8)
            .unwrap_or(0)
    }

    pub fn size(&self) -> usize {
        self.cells.len()
    }
}
