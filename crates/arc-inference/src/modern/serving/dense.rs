//! Batched forward pass of the dyadic model, equal row by row to
//! [`ModernModel::forward`].
//!
//! A step's rows may mix sequences, prefill chunks and decode tokens. Each
//! projection runs once over all rows ([`project_rows`]), so every weight row is
//! read once per step instead of once per token. Everything else is per row:
//! RMSNorm, RoPE at the row's own position, the KV append, and two-pass
//! attention over the row's own sequence up to its own position. The
//! arithmetic is the profile's ([`crate::modern::arith`]); only the loop order
//! changes.
//!
//! The pass is split into stages so a model can be cut between devices: an
//! island pipeline runs [`embed_rows`], then [`forward_layers`] for its layer
//! range on each device, then [`final_logits`]. The only data crossing a stage
//! boundary is the hidden state of each row (`d_model` values of `i64`) and the
//! per-row error flags. The feed-forward block is a [`FeedForward`], so a
//! mixture-of-experts layer reuses the same attention path.
//!
//! [`ModernModel::forward`]: crate::modern::model::ModernModel::forward

use std::ops::Range;

use rayon::prelude::*;

use super::gemm::project_rows;
use super::{BatchModel, Row, SeqKv, StepOutput, check_rows};
use crate::modern::ModernError;
use crate::modern::arith::{
    HeadCache, add_residual, attention_head, embed_row, gated_silu, rms_norm, rope_split_half,
};
use crate::modern::model::ModernModel;
use crate::modern::tables::attention_lambda;

/// The feed-forward block of a layer, applied to every row of a step at once.
pub trait FeedForward: Sync {
    /// Write `ffn_layer(normed[r])` into `out[r]` for every row without an
    /// error, recording a row's first error in `errors[r]`. Rows hold
    /// `d_model` values each. A row's result may depend on that row alone.
    fn forward(
        &self,
        layer: usize,
        normed: &[i64],
        out: &mut [i64],
        errors: &mut [Option<ModernError>],
    );
}

/// The dense SwiGLU block of the dyadic profile (spec §5.7).
pub struct DenseFfn<'a>(pub &'a ModernModel);

impl FeedForward for DenseFfn<'_> {
    fn forward(
        &self,
        layer: usize,
        normed: &[i64],
        out: &mut [i64],
        errors: &mut [Option<ModernError>],
    ) {
        let weights = &self.0.layers[layer];
        let n = errors.len();
        let d_ff = self.0.config.d_ff;
        let mut gate = vec![0i64; n * d_ff];
        let mut up = vec![0i64; n * d_ff];
        project_rows(&weights.w_gate, normed, &mut gate, errors);
        project_rows(&weights.w_up, normed, &mut up, errors);
        silu_rows(&mut gate, &up, d_ff, errors);
        project_rows(&weights.w_down, &gate, out, errors);
    }
}

/// A loaded dense model served by the batched scheduler.
pub struct DenseModel<'a> {
    model: &'a ModernModel,
    identity: [u8; 32],
}

impl<'a> DenseModel<'a> {
    /// Serve `model`. `identity` names the computed function, for example
    /// BLAKE3 of the profile and the package SHA-256; prefix-cache keys
    /// commit to it.
    pub fn new(model: &'a ModernModel, identity: [u8; 32]) -> Self {
        Self { model, identity }
    }

    /// The underlying model.
    pub fn model(&self) -> &ModernModel {
        self.model
    }
}

impl BatchModel for DenseModel<'_> {
    fn vocab_size(&self) -> usize {
        self.model.config.vocab_size
    }

    fn max_positions(&self) -> usize {
        self.model.config.max_seq
    }

    fn kv_widths(&self) -> Vec<usize> {
        dense_kv_widths(self.model)
    }

    fn identity(&self) -> [u8; 32] {
        self.identity
    }

    fn forward_rows(&self, rows: &[Row], kvs: &mut [&mut SeqKv]) -> StepOutput {
        layered_forward(self.model, rows, kvs, &DenseFfn(self.model))
    }
}

/// Plane widths of a dense model's cache: keys then values for each layer.
pub fn dense_kv_widths(model: &ModernModel) -> Vec<usize> {
    vec![model.config.d_kv(); 2 * model.config.n_layers]
}

/// A full step: embeddings, every layer with `ffn` as the feed-forward block,
/// logits, and the per-sequence commit or rollback.
pub fn layered_forward(
    model: &ModernModel,
    rows: &[Row],
    kvs: &mut [&mut SeqKv],
    ffn: &dyn FeedForward,
) -> StepOutput {
    let base = match check_rows(rows, kvs) {
        Ok(base) => base,
        Err(e) => return failed_step(rows.len(), kvs.len(), &e),
    };
    let mut errors: Vec<Option<ModernError>> = rows.iter().map(|_| None).collect();
    let mut hidden = embed_rows(model, rows, &mut errors);
    forward_layers(
        model,
        0..model.config.n_layers,
        rows,
        kvs,
        &mut hidden,
        &mut errors,
        ffn,
    );
    let logits = final_logits(model, rows, &hidden, &mut errors);
    finish_step(rows, kvs, &base, errors, logits)
}

/// A step that failed its contract check: every sequence gets the error.
pub fn failed_step(rows: usize, sequences: usize, error: &ModernError) -> StepOutput {
    let message = error.to_string();
    StepOutput {
        logits: (0..rows).map(|_| None).collect(),
        errors: (0..sequences)
            .map(|_| Some(ModernError::Invalid(message.clone())))
            .collect(),
    }
}

/// Embedding rows (spec §5.3), refusing positions outside the context.
pub fn embed_rows(
    model: &ModernModel,
    rows: &[Row],
    errors: &mut [Option<ModernError>],
) -> Vec<i64> {
    let c = &model.config;
    let d = c.d_model;
    let mut hidden = vec![0i64; rows.len() * d];
    for ((row, error), out) in rows.iter().zip(errors.iter_mut()).zip(hidden.chunks_mut(d)) {
        if row.position >= c.max_seq {
            *error = Some(ModernError::Domain(format!(
                "position {} is outside the {}-position context",
                row.position, c.max_seq
            )));
            continue;
        }
        match embed_row(&model.embed, row.token as usize) {
            Ok(values) => out.copy_from_slice(&values),
            Err(e) => *error = Some(e),
        }
    }
    hidden
}

/// Layers `layers` for every row (spec §5.8). Each row reads and appends KV
/// planes `2l` and `2l + 1` of its sequence; hidden states enter and leave
/// through `hidden`. A row that fails stops computing and appends zeros, so
/// the planes of its sequence stay aligned until the step rolls it back.
pub fn forward_layers(
    model: &ModernModel,
    layers: Range<usize>,
    rows: &[Row],
    kvs: &mut [&mut SeqKv],
    hidden: &mut [i64],
    errors: &mut [Option<ModernError>],
    ffn: &dyn FeedForward,
) {
    let c = &model.config;
    let n = rows.len();
    let (d, dq, dkv, dh) = (c.d_model, c.d_q(), c.d_kv(), c.d_head);
    let half = dh / 2;
    let group = c.n_heads / c.n_kv_heads;
    let lambda = attention_lambda(dh);
    let mut normed = vec![0i64; n * d];
    let mut q = vec![0i64; n * dq];
    let mut k = vec![0i64; n * dkv];
    let mut v = vec![0i64; n * dkv];
    let mut attended = vec![0i64; n * dq];
    let mut projected = vec![0i64; n * d];
    for l in layers {
        let layer = &model.layers[l];
        norm_rows(hidden, &layer.attn_norm, c.rms_eps_q32, &mut normed, errors);
        project_rows(&layer.wq, &normed, &mut q, errors);
        project_rows(&layer.wk, &normed, &mut k, errors);
        project_rows(&layer.wv, &normed, &mut v, errors);
        if c.rope_layers[l] {
            for (r, row) in rows.iter().enumerate() {
                if errors[r].is_some() {
                    continue;
                }
                let cos = &model.rope_cos[row.position * half..(row.position + 1) * half];
                let sin = &model.rope_sin[row.position * half..(row.position + 1) * half];
                let rotated = q[r * dq..(r + 1) * dq]
                    .chunks_exact_mut(dh)
                    .chain(k[r * dkv..(r + 1) * dkv].chunks_exact_mut(dh))
                    .try_for_each(|head| rope_split_half(head, cos, sin));
                if let Err(e) = rotated {
                    errors[r] = Some(e);
                }
            }
        }
        for (r, row) in rows.iter().enumerate() {
            let narrowed = if errors[r].is_some() {
                None
            } else {
                match (
                    narrow_kv(&k[r * dkv..(r + 1) * dkv]),
                    narrow_kv(&v[r * dkv..(r + 1) * dkv]),
                ) {
                    (Ok(keys), Ok(values)) => Some((keys, values)),
                    (Err(e), _) | (_, Err(e)) => {
                        errors[r] = Some(e);
                        None
                    }
                }
            };
            let (keys, values) = narrowed.unwrap_or_else(|| (vec![0; dkv], vec![0; dkv]));
            kvs[row.seq].extend_plane(2 * l, &keys);
            kvs[row.seq].extend_plane(2 * l + 1, &values);
        }
        let caches: &[&mut SeqKv] = kvs;
        let failed: &[Option<ModernError>] = errors;
        let results: Vec<Result<(), ModernError>> = attended
            .par_chunks_mut(dh)
            .enumerate()
            .map(|(index, out)| {
                let r = index / c.n_heads;
                let head = index % c.n_heads;
                if failed[r].is_some() {
                    return Ok(());
                }
                let row = rows[r];
                let cache = &caches[row.seq];
                let view = HeadCache {
                    keys: cache.plane(2 * l),
                    values: cache.plane(2 * l + 1),
                    positions: row.position + 1,
                    stride: dkv,
                    offset: (head / group) * dh,
                };
                let start = r * dq + head * dh;
                attention_head(&q[start..start + dh], view, lambda, out)
            })
            .collect();
        for (index, result) in results.into_iter().enumerate() {
            let r = index / c.n_heads;
            if let Err(e) = result
                && errors[r].is_none()
            {
                errors[r] = Some(e);
            }
        }
        project_rows(&layer.wo, &attended, &mut projected, errors);
        residual_rows(hidden, &projected, d, errors);
        norm_rows(hidden, &layer.ffn_norm, c.rms_eps_q32, &mut normed, errors);
        ffn.forward(l, &normed, &mut projected, errors);
        residual_rows(hidden, &projected, d, errors);
    }
}

/// Final RMSNorm and the tied LM head (spec §5.8) for rows that asked for
/// logits. Rows that did not ask skip both, which is why a prefill chunk costs
/// one LM head instead of one per token.
pub fn final_logits(
    model: &ModernModel,
    rows: &[Row],
    hidden: &[i64],
    errors: &mut [Option<ModernError>],
) -> Vec<Option<Vec<i64>>> {
    let c = &model.config;
    let d = c.d_model;
    let vocab = c.vocab_size;
    let mut logits: Vec<Option<Vec<i64>>> = rows.iter().map(|_| None).collect();
    let wanted: Vec<usize> = (0..rows.len())
        .filter(|&r| rows[r].logits && errors[r].is_none())
        .collect();
    if wanted.is_empty() {
        return logits;
    }
    let mut normed = vec![0i64; wanted.len() * d];
    let mut wanted_errors: Vec<Option<ModernError>> = wanted.iter().map(|_| None).collect();
    for ((&r, out), error) in wanted
        .iter()
        .zip(normed.chunks_mut(d))
        .zip(wanted_errors.iter_mut())
    {
        match rms_norm(
            &hidden[r * d..(r + 1) * d],
            &model.final_norm,
            c.rms_eps_q32,
        ) {
            Ok(values) => out.copy_from_slice(&values),
            Err(e) => *error = Some(e),
        }
    }
    let mut values = vec![0i64; wanted.len() * vocab];
    project_rows(&model.embed, &normed, &mut values, &mut wanted_errors);
    for ((&r, error), row_values) in wanted.iter().zip(wanted_errors).zip(values.chunks(vocab)) {
        match error {
            Some(e) => errors[r] = Some(e),
            None => logits[r] = Some(row_values.to_vec()),
        }
    }
    logits
}

/// Close a step: each sequence commits its new positions, or rolls back to
/// `base` and reports the first error among its rows.
pub fn finish_step(
    rows: &[Row],
    kvs: &mut [&mut SeqKv],
    base: &[usize],
    errors: Vec<Option<ModernError>>,
    mut logits: Vec<Option<Vec<i64>>>,
) -> StepOutput {
    let mut seq_errors: Vec<Option<ModernError>> = kvs.iter().map(|_| None).collect();
    let mut grown = vec![0usize; kvs.len()];
    for (row, error) in rows.iter().zip(errors) {
        grown[row.seq] += 1;
        if let Some(e) = error
            && seq_errors[row.seq].is_none()
        {
            seq_errors[row.seq] = Some(e);
        }
    }
    for (seq, kv) in kvs.iter_mut().enumerate() {
        if grown[seq] == 0 {
            continue;
        }
        if seq_errors[seq].is_none()
            && let Err(e) = kv.commit(base[seq] + grown[seq])
        {
            seq_errors[seq] = Some(e);
        }
        if seq_errors[seq].is_some() {
            kv.rollback(base[seq]);
        }
    }
    for (row, slot) in rows.iter().zip(logits.iter_mut()) {
        if seq_errors[row.seq].is_some() {
            *slot = None;
        }
    }
    StepOutput {
        logits,
        errors: seq_errors,
    }
}

/// Per-row RMSNorm (spec §5.4).
fn norm_rows(
    x: &[i64],
    gain: &[i64],
    eps_q32: i64,
    out: &mut [i64],
    errors: &mut [Option<ModernError>],
) {
    let d = gain.len();
    out.par_chunks_mut(d)
        .zip(x.par_chunks(d))
        .zip(errors.par_iter_mut())
        .for_each(|((y, row), error)| {
            if error.is_some() {
                return;
            }
            match rms_norm(row, gain, eps_q32) {
                Ok(values) => y.copy_from_slice(&values),
                Err(e) => *error = Some(e),
            }
        });
}

/// Per-row residual add.
fn residual_rows(h: &mut [i64], delta: &[i64], d: usize, errors: &mut [Option<ModernError>]) {
    h.par_chunks_mut(d)
        .zip(delta.par_chunks(d))
        .zip(errors.par_iter_mut())
        .for_each(|((row, add), error)| {
            if error.is_none()
                && let Err(e) = add_residual(row, add)
            {
                *error = Some(e);
            }
        });
}

/// Per-row gated SiLU, in place on `gate` (spec §5.7).
pub fn silu_rows(gate: &mut [i64], up: &[i64], width: usize, errors: &mut [Option<ModernError>]) {
    gate.par_chunks_mut(width)
        .zip(up.par_chunks(width))
        .zip(errors.par_iter_mut())
        .for_each(|((g, u), error)| {
            if error.is_some() {
                return;
            }
            for (a, &b) in g.iter_mut().zip(u) {
                match gated_silu(*a, b) {
                    Ok(value) => *a = value,
                    Err(e) => {
                        *error = Some(e);
                        return;
                    }
                }
            }
        });
}

/// Narrow one row's keys or values to the cache's `i32` (spec §5.8).
fn narrow_kv(values: &[i64]) -> Result<Vec<i32>, ModernError> {
    values
        .iter()
        .map(|&x| {
            i32::try_from(x)
                .map_err(|_| ModernError::Domain("KV value outside i32 (|v| >= 2^31)".into()))
        })
        .collect()
}
