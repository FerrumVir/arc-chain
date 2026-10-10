//! NON-CANONICAL, STUDY ONLY: ARC's algorithm with f32 accumulation.
//!
//! This module exists only when the crate is built with the
//! `float-accumulation-study` feature, which no workspace crate enables: no
//! node, worker or release binary contains it (see the feature's note in
//! `Cargo.toml` and the `cargo tree` check in
//! `.github/workflows/draft-verify-bench.yml`). Even when compiled in, it is
//! off until a study binary calls [`set_enabled`], or a test turns it on for
//! its own thread with [`on_this_thread`].
//!
//! When on, every canonical per-row I8 weight projection (the one-row and the
//! batched path) computes the same quantities from the same operands as the
//! exact engine, the I8 weight row and the Q16 activations, but forms the dot
//! product in f32: both operands are converted to f32 and accumulated with
//! fused multiply-adds over 16 lanes, then the lanes are summed in order and
//! the sum is rounded to an integer. The per-row scale and requantisation that
//! follow are the exact engine's (`(acc * scale) >> FRAC_BITS`), and so is
//! everything else: norms, RoPE, attention, activation, residuals and the
//! repetition penalty. It stands in for a fast float implementation of ARC's
//! pipeline (a GPU GEMM accumulating in f32), to measure how often such an
//! engine would choose the exact engine's tokens. Its outputs are not ARC
//! outputs and must never be signed, served or compared as such.

use crate::integer_lut::FRAC_BITS;
use rayon::prelude::*;
use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// f32 accumulation for the projections this thread calls.
    static ON_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Turns f32 accumulation on or off for every projection in this process.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::SeqCst);
}

/// Whether the projection about to run on this thread accumulates in f32:
/// on for the whole process, or for this thread ([`on_this_thread`]).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::SeqCst) || ON_THIS_THREAD.with(Cell::get)
}

/// Turns f32 accumulation on for the projections this thread calls until the
/// returned guard drops, leaving every other thread exact: for tests that
/// compare against this engine while other tests run. A projection checks
/// the switch on the thread that calls it, before any of its own parallel
/// work, so its rayon workers follow the caller.
#[must_use = "f32 accumulation is on only while the guard lives"]
pub fn on_this_thread() -> ThisThread {
    ON_THIS_THREAD.with(|on| on.set(true));
    ThisThread {
        _not_send: PhantomData,
    }
}

/// See [`on_this_thread`]. Dropping it turns this thread's switch off.
pub struct ThisThread {
    /// The switch belongs to the thread that set it.
    _not_send: PhantomData<*const ()>,
}

impl Drop for ThisThread {
    fn drop(&mut self) {
        ON_THIS_THREAD.with(|on| on.set(false));
    }
}

const LANES: usize = 16;

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

/// `output[t * n_rows + i] = (round(sum_j w[i][j] * x[t][j]) * scales[i]) >>
/// FRAC_BITS`, the sum formed in f32. The layouts are the exact batched
/// projection's.
pub(crate) fn matmul_batched_f32(
    data: &[i8],
    scales: &[i64],
    n_rows: usize,
    inputs: &[i64],
    n_tokens: usize,
    in_size: usize,
    output: &mut [i64],
) {
    let xs: Vec<f32> = inputs.iter().map(|&x| x as f32).collect();
    let mut by_row = vec![0i64; n_rows * n_tokens];
    by_row
        .par_chunks_mut(n_tokens)
        .enumerate()
        .for_each(|(i, column)| {
            let w: Vec<f32> = data[i * in_size..(i + 1) * in_size]
                .iter()
                .map(|&v| f32::from(v))
                .collect();
            for (t, out) in column.iter_mut().enumerate() {
                let acc = dot_f32(&w, &xs[t * in_size..(t + 1) * in_size]);
                *out = ((acc.round() as i64) * scales[i]) >> FRAC_BITS;
            }
        });
    for (i, column) in by_row.chunks_exact(n_tokens).enumerate() {
        for (t, &value) in column.iter().enumerate() {
            output[t * n_rows + i] = value;
        }
    }
}
