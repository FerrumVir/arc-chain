//! TEST ONLY and NON-CANONICAL: the MLA + MoE profile's algorithm with f32
//! accumulation, for an agreement study.
//!
//! This module is compiled only for arc-inference's own unit tests
//! (`#[cfg(test)]` in `mla/mod.rs`, and every hook that calls it is
//! `#[cfg(test)]` as well): no library, binary, node, worker or release build
//! contains it, and nothing outside this crate's tests can reach it. Even in a
//! test build the hooks are inert until a study turns them on with
//! [`F32Accumulation::on`].
//!
//! When on, every dot product of quantised weights with activations is formed
//! in f32, from the same operands as the exact engine: the INT16 projections
//! (`precision::project_i16`), the INT8 projections (`ops::QView::project`),
//! every 32-value group of the INT4 expert projections (`ops::Q4View`) and the
//! router's logits (`ops::router_logits`). Both operands are converted to f32
//! and accumulated with fused multiply-adds over 16 lanes, the lanes are summed
//! in order and the sum is rounded to an integer: the 7B study's definition
//! (`float_accumulation_study.rs`, #191). Everything after the dot (the dyadic
//! epilogues, the INT4 group scales and their exact combination, the router's
//! shifts) and everything else (norms, RoPE, attention, sigmoid routing and
//! expert selection, the expert combine, residuals, token selection) is the
//! exact engine's. It stands in for a fast float implementation of ARC's
//! algorithm. Its outputs are not ARC outputs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use rayon::prelude::*;

static ON: AtomicBool = AtomicBool::new(false);
static ROUTES: Mutex<Option<Vec<Vec<usize>>>> = Mutex::new(None);

/// Whether f32 accumulation is on (only ever inside a study).
pub(crate) fn enabled() -> bool {
    ON.load(Ordering::SeqCst)
}

/// f32 accumulation on, process-wide, until dropped. Hold
/// `canonical_simd::kernel_switch_guard()` as well.
pub(crate) struct F32Accumulation;

impl F32Accumulation {
    pub(crate) fn on() -> Self {
        ON.store(true, Ordering::SeqCst);
        F32Accumulation
    }
}

impl Drop for F32Accumulation {
    fn drop(&mut self) {
        ON.store(false, Ordering::SeqCst);
    }
}

/// Records every MoE layer's selected experts, in call order, until dropped.
pub(crate) struct Routes;

impl Routes {
    pub(crate) fn record() -> Self {
        *ROUTES.lock().unwrap_or_else(PoisonError::into_inner) = Some(Vec::new());
        Routes
    }

    /// The selections recorded since the last call.
    pub(crate) fn take(&self) -> Vec<Vec<usize>> {
        ROUTES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }
}

impl Drop for Routes {
    fn drop(&mut self) {
        *ROUTES.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Called by `moe_forward` with every selection; recorded only inside
/// [`Routes::record`].
pub(crate) fn record_route(chosen: &[usize]) {
    if let Some(routes) = ROUTES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        routes.push(chosen.to_vec());
    }
}

const LANES: usize = 16;

/// `sum_j w_j x_j` in f32: fused multiply-adds over 16 lanes, the lanes summed
/// in order, then the tail (as the 7B study).
fn dot_f32(weights: &[f32], inputs: &[f32]) -> f32 {
    let mut lanes = [0f32; LANES];
    let whole = weights.len() / LANES * LANES;
    for (w, x) in weights[..whole]
        .chunks_exact(LANES)
        .zip(inputs[..whole].chunks_exact(LANES))
    {
        for ((lane, &wv), &xv) in lanes.iter_mut().zip(w).zip(x) {
            *lane = wv.mul_add(xv, *lane);
        }
    }
    let mut acc = 0f32;
    for lane in lanes {
        acc += lane;
    }
    for (&wv, &xv) in weights[whole..].iter().zip(&inputs[whole..]) {
        acc = wv.mul_add(xv, acc);
    }
    acc
}

fn floats(x: &[i64]) -> Vec<f32> {
    x.iter().map(|&v| v as f32).collect()
}

/// Every row's dot of a row-major little-endian INT16 matrix with `x`, in
/// f32, rounded to an integer.
pub(crate) fn dots_i16(weights: &[u8], cols: usize, x: &[i64], out: &mut [i64]) {
    let xs = floats(x);
    out.par_iter_mut().enumerate().for_each(|(r, slot)| {
        let row: Vec<f32> = weights[r * cols * 2..(r + 1) * cols * 2]
            .chunks_exact(2)
            .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])))
            .collect();
        *slot = dot_f32(&row, &xs).round() as i64;
    });
}

/// Every row's dot of a row-major INT8 matrix with `x`, in f32, rounded.
pub(crate) fn dots_i8(weights: &[i8], cols: usize, x: &[i64], out: &mut [i64]) {
    let xs = floats(x);
    out.par_iter_mut().enumerate().for_each(|(r, slot)| {
        let row: Vec<f32> = weights[r * cols..(r + 1) * cols]
            .iter()
            .map(|&w| f32::from(w))
            .collect();
        *slot = dot_f32(&row, &xs).round() as i64;
    });
}

/// One dot of `weights` (any integer values exact in f32) with `x`, in f32,
/// rounded to an integer.
pub(crate) fn dot(weights: &[f32], x: &[i64]) -> i64 {
    dot_f32(weights, &floats(x)).round() as i64
}
