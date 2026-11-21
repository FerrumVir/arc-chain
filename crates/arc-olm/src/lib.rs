#![allow(dead_code, unused_imports, unused_variables, clippy::never_loop)]

pub mod primitives;
pub mod search;

pub type Grid = Vec<Vec<u8>>;

pub type Color = u8;

pub type Pos = (usize, usize);

pub type PosSet = std::collections::BTreeSet<Pos>;

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
