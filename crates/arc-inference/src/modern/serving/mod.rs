//! Serving layer for the dyadic profile: continuous batching, a
//! content-addressed prefix cache and exact speculative decoding.
//!
//! Every technique here changes how often weights and cached keys are read,
//! never a value. The contract that makes this true is
//! [`BatchModel::forward_rows`]: a batched step computes each row exactly as
//! [`ModernModel::forward`] computes that token alone. Every operator of the
//! profile is a pure function of one row: exact integer sums, per-row
//! RMSNorm, and a two-pass softmax over the row's own sequence. So:
//!
//! * **batch invariance**: a request's bytes cannot depend on the batch size or
//!   on the other requests in the batch;
//! * **phase invariance**: a prompt processed in one prefill, in chunks or one
//!   token at a time gives the same logits and the same KV cache;
//! * the **prefix cache** may hand any request the KV of an identical prefix,
//!   whoever computed it and in whichever phase;
//! * **speculative decoding** verifies drafted tokens in one batched pass and
//!   keeps exactly the tokens plain greedy decoding produces.
//!
//! The scheduler, the prefix cache and speculation see a model only through
//! [`BatchModel`] and the plane-structured [`SeqKv`]. A mixture-of-experts
//! model, an MLA latent cache or a model split across devices (an island
//! pipeline) plugs in without changing them; the tests run both a routed
//! mixture-of-experts fixture and a two-stage pipeline through the same
//! scheduler. `docs/serving-optimizations.md` has the design, the measured CI
//! numbers and the mapping to GPU kernels.
//!
//! [`ModernModel::forward`]: crate::modern::model::ModernModel::forward

pub mod dense;
pub mod gemm;
pub mod prefix;
pub mod scheduler;
pub mod spec;
pub mod tree;

#[cfg(test)]
mod tests;

use super::ModernError;

/// One token of one sequence in a batched step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// Index of the row's sequence in the step's KV slice.
    pub seq: usize,
    /// Token fed at `position`.
    pub token: u32,
    /// Absolute position; a sequence's first row sits at its committed length.
    pub position: usize,
    /// Whether the step returns this row's logits.
    pub logits: bool,
}

/// What a batched step returns.
#[derive(Debug, Default)]
pub struct StepOutput {
    /// Logits per row, for rows that asked for them, succeeded, and precede
    /// any failure of their sequence.
    pub logits: Vec<Option<Vec<i64>>>,
    /// Per sequence, its first failing row, if any. The rows before it are
    /// kept: their logits are returned and their positions committed. It and
    /// the rows after it, whose caches would include it, are dropped. Other
    /// sequences are unaffected.
    pub errors: Vec<Option<Failure>>,
}

/// The first failing row of one sequence in a step.
#[derive(Debug)]
pub struct Failure {
    /// Rows of the sequence, in step order, that succeeded before this one
    /// and whose positions stay committed.
    pub kept: usize,
    /// The row's error: the one the model raises for that token alone.
    pub error: ModernError,
}

/// A model the serving layer can drive.
///
/// # Contract
///
/// In `forward_rows(rows, kvs)` the rows of one sequence are contiguous and in
/// position order, and the first of them sits at `kvs[seq].len()`. On success
/// the sequence's cache grows by its row count. Each row's logits and each
/// appended KV position must equal what the model computes for that token
/// alone, given only that sequence's cache. No value may depend on the other
/// rows of the step, on how rows are grouped, or on the step's size.
/// Mixture-of-experts routing must therefore be a function of the row alone:
/// no expert capacity limit and no token dropping.
///
/// A row that fails fails alone, with the error the model raises for that
/// token alone. Its sequence keeps the rows before it (logits returned,
/// positions committed), drops it and the rows after it, and reports the
/// failure with the number of rows kept ([`Failure`]). The scheduler decides
/// whether plain decoding would have run the failing row at all.
pub trait BatchModel: Sync {
    /// Token ids are below this.
    fn vocab_size(&self) -> usize;
    /// Positions one sequence may hold.
    fn max_positions(&self) -> usize;
    /// Values per position in each KV plane of a sequence.
    fn kv_widths(&self) -> Vec<usize>;
    /// Names the computed function; prefix-cache keys commit to it.
    fn identity(&self) -> [u8; 32];
    /// Run one batched step (see the trait contract).
    fn forward_rows(&self, rows: &[Row], kvs: &mut [&mut SeqKv]) -> StepOutput;

    /// An empty cache for one sequence.
    fn new_kv(&self) -> SeqKv {
        SeqKv::new(self.kv_widths())
    }
}

/// One sequence's cache as planes of `i32`: plane `p` holds `widths[p]` values
/// per position.
///
/// The dense profile stores keys then values for each layer, which is the
/// layout [`KvCache::digest`] hashes. An MLA layer would hold one latent plane,
/// and a pipeline stage holds the planes of its own layers.
///
/// [`KvCache::digest`]: crate::modern::model::KvCache::digest
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqKv {
    widths: Vec<usize>,
    planes: Vec<Vec<i32>>,
    len: usize,
}

impl SeqKv {
    /// An empty cache with the given plane widths.
    pub fn new(widths: Vec<usize>) -> Self {
        let planes = vec![Vec::new(); widths.len()];
        Self {
            widths,
            planes,
            len: 0,
        }
    }

    /// Committed positions.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no position is committed.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Plane widths.
    pub fn widths(&self) -> &[usize] {
        &self.widths
    }

    /// One plane, including values a step in progress has appended.
    pub fn plane(&self, index: usize) -> &[i32] {
        &self.planes[index]
    }

    /// Bytes per committed position across all planes.
    pub fn bytes_per_position(&self) -> usize {
        self.widths.iter().sum::<usize>() * 4
    }

    /// Append values to one plane during a step; [`Self::commit`] or
    /// [`Self::rollback`] ends the step.
    pub fn extend_plane(&mut self, index: usize, values: &[i32]) {
        self.planes[index].extend_from_slice(values);
    }

    /// Commit `len` positions. Every plane must hold exactly that many.
    pub fn commit(&mut self, len: usize) -> Result<(), ModernError> {
        let consistent = self
            .planes
            .iter()
            .zip(&self.widths)
            .all(|(plane, &width)| plane.len() == len * width);
        if !consistent {
            return Err(ModernError::Invalid(format!(
                "KV planes do not hold {len} positions"
            )));
        }
        self.len = len;
        Ok(())
    }

    /// Keep at most `len` committed positions and drop everything after them:
    /// a failed step, or drafted positions that verification rejected.
    pub fn rollback(&mut self, len: usize) {
        let len = len.min(self.len);
        for (plane, &width) in self.planes.iter_mut().zip(&self.widths) {
            plane.truncate(len * width);
        }
        self.len = len;
    }

    /// Copy committed positions `start..end` of every plane (a prefix block).
    pub fn export(&self, start: usize, end: usize) -> Vec<Vec<i32>> {
        let end = end.min(self.len);
        let start = start.min(end);
        self.planes
            .iter()
            .zip(&self.widths)
            .map(|(plane, &width)| plane[start * width..end * width].to_vec())
            .collect()
    }

    /// Append a block exported from a cache with the same widths, returning
    /// the number of positions appended.
    pub fn append(&mut self, block: &[Vec<i32>]) -> Result<usize, ModernError> {
        let positions = match (block.first(), self.widths.first()) {
            (Some(plane), Some(&width)) if width > 0 => plane.len() / width,
            _ => 0,
        };
        let shaped = block.len() == self.planes.len()
            && block
                .iter()
                .zip(&self.widths)
                .all(|(plane, &width)| plane.len() == positions * width);
        let settled = self
            .planes
            .iter()
            .zip(&self.widths)
            .all(|(plane, &width)| plane.len() == self.len * width);
        if !shaped || !settled {
            return Err(ModernError::Invalid(
                "KV block does not fit this cache".into(),
            ));
        }
        for (plane, values) in self.planes.iter_mut().zip(block) {
            plane.extend_from_slice(values);
        }
        self.len += positions;
        Ok(positions)
    }

    /// BLAKE3 over every committed value, plane by plane, as little-endian
    /// `i32`. For the dense layout this equals [`KvCache::digest`].
    ///
    /// [`KvCache::digest`]: crate::modern::model::KvCache::digest
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for (plane, &width) in self.planes.iter().zip(&self.widths) {
            let bytes: Vec<u8> = plane[..self.len * width]
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            hasher.update(&bytes);
        }
        *hasher.finalize().as_bytes()
    }
}

/// Check the batch contract of [`BatchModel::forward_rows`] and return each
/// sequence's committed length before the step.
pub fn check_rows(rows: &[Row], kvs: &[&mut SeqKv]) -> Result<Vec<usize>, ModernError> {
    let base: Vec<usize> = kvs.iter().map(|kv| kv.len()).collect();
    let mut next: Vec<Option<usize>> = vec![None; kvs.len()];
    let mut previous: Option<usize> = None;
    for row in rows {
        let Some(&start) = base.get(row.seq) else {
            return Err(ModernError::Invalid(format!(
                "a row names sequence {} of {}",
                row.seq,
                kvs.len()
            )));
        };
        let expected = if previous == Some(row.seq) {
            next[row.seq]
        } else if next[row.seq].is_some() {
            return Err(ModernError::Invalid(
                "the rows of a sequence must be contiguous".into(),
            ));
        } else {
            Some(start)
        };
        if expected != Some(row.position) {
            return Err(ModernError::Invalid(format!(
                "sequence {} expects position {expected:?}, the row has {}",
                row.seq, row.position
            )));
        }
        next[row.seq] = Some(row.position + 1);
        previous = Some(row.seq);
    }
    Ok(base)
}
