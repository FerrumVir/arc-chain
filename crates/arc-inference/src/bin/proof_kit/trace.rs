//! Locate where two backends first differ, operator by operator.
//!
//! A traced copy of the forward pass (spec §5.8) is built from the same
//! public operators `ModernModel::forward` uses and hashes every
//! intermediate. The kit teacher-forces the reference tokens through it, once
//! with a backend that matched the reference and once with the one that did
//! not, and reports the first operator whose output differs. The traced pass
//! checks itself first: with the matching backend it must reproduce the
//! reference logits hash at every position, or no location is reported.

use arc_inference::modern::arith::{
    self, HeadCache, add_residual, attention_head, embed_row, gated_silu, project, rms_norm,
    rope_split_half,
};
use arc_inference::modern::model::ModernModel;
use arc_inference::modern::proof::CaseTrace;
use arc_inference::modern::tables::attention_lambda;
use arc_inference::modern::{ModernError, hex_lower};

use super::Backend;

/// The hash of one operator's output.
struct OpHash {
    layer: Option<usize>,
    op: &'static str,
    hash: [u8; 32],
}

/// The first operator whose output differs.
pub(super) struct Hit {
    pub position: usize,
    pub layer: Option<usize>,
    pub op: &'static str,
}

struct Cache {
    keys: Vec<Vec<i32>>,
    values: Vec<Vec<i32>>,
    positions: usize,
}

fn record(ops: &mut Vec<OpHash>, layer: Option<usize>, op: &'static str, values: &[i64]) {
    ops.push(OpHash {
        layer,
        op,
        hash: arith::logits_hash(values),
    });
}

fn narrow(values: &[i64]) -> Result<Vec<i32>, ModernError> {
    values
        .iter()
        .map(|&x| {
            i32::try_from(x)
                .map_err(|_| ModernError::Domain("KV value outside i32 (|v| >= 2^31)".into()))
        })
        .collect()
}

/// One forward pass, mirroring `ModernModel::forward` and hashing each step.
fn forward(
    model: &ModernModel,
    token: u32,
    cache: &mut Cache,
    ops: &mut Vec<OpHash>,
) -> Result<Vec<i64>, ModernError> {
    let c = &model.config;
    let position = cache.positions;
    if position >= c.max_seq {
        return Err(ModernError::Domain(format!(
            "position {position} is outside the {}-position context",
            c.max_seq
        )));
    }
    let mut hidden = embed_row(&model.embed, token as usize)?;
    record(ops, None, "embed", &hidden);
    let half = c.d_head / 2;
    let cos = &model.rope_cos[position * half..(position + 1) * half];
    let sin = &model.rope_sin[position * half..(position + 1) * half];
    let lambda = attention_lambda(c.d_head);
    let group = c.n_heads / c.n_kv_heads;
    let mut q = vec![0i64; c.d_q()];
    let mut k = vec![0i64; c.d_kv()];
    let mut v = vec![0i64; c.d_kv()];
    let mut attended = vec![0i64; c.d_q()];
    let mut projected = vec![0i64; c.d_model];
    let mut gate = vec![0i64; c.d_ff];
    let mut up = vec![0i64; c.d_ff];
    for (l, layer) in model.layers.iter().enumerate() {
        let at = Some(l);
        let normed = rms_norm(&hidden, &layer.attn_norm, c.rms_eps_q32)?;
        record(ops, at, "attn_norm", &normed);
        project(&layer.wq, &normed, &mut q)?;
        record(ops, at, "wq", &q);
        project(&layer.wk, &normed, &mut k)?;
        record(ops, at, "wk", &k);
        project(&layer.wv, &normed, &mut v)?;
        record(ops, at, "wv", &v);
        if c.rope_layers[l] {
            for head in q.chunks_exact_mut(c.d_head) {
                rope_split_half(head, cos, sin)?;
            }
            for head in k.chunks_exact_mut(c.d_head) {
                rope_split_half(head, cos, sin)?;
            }
            record(ops, at, "rope_q", &q);
            record(ops, at, "rope_k", &k);
        }
        cache.keys[l].extend(narrow(&k)?);
        cache.values[l].extend(narrow(&v)?);
        for (head, (out, q_head)) in attended
            .chunks_mut(c.d_head)
            .zip(q.chunks(c.d_head))
            .enumerate()
        {
            let view = HeadCache {
                keys: &cache.keys[l],
                values: &cache.values[l],
                positions: position + 1,
                stride: c.d_kv(),
                offset: (head / group) * c.d_head,
            };
            attention_head(q_head, view, lambda, out)?;
        }
        record(ops, at, "attention", &attended);
        project(&layer.wo, &attended, &mut projected)?;
        record(ops, at, "wo", &projected);
        add_residual(&mut hidden, &projected)?;
        record(ops, at, "attn_residual", &hidden);
        let normed = rms_norm(&hidden, &layer.ffn_norm, c.rms_eps_q32)?;
        record(ops, at, "ffn_norm", &normed);
        project(&layer.w_gate, &normed, &mut gate)?;
        record(ops, at, "w_gate", &gate);
        project(&layer.w_up, &normed, &mut up)?;
        record(ops, at, "w_up", &up);
        for (g, &u) in gate.iter_mut().zip(&up) {
            *g = gated_silu(*g, u)?;
        }
        record(ops, at, "silu", &gate);
        project(&layer.w_down, &gate, &mut projected)?;
        record(ops, at, "w_down", &projected);
        add_residual(&mut hidden, &projected)?;
        record(ops, at, "ffn_residual", &hidden);
    }
    cache.positions = position + 1;
    let normed = rms_norm(&hidden, &model.final_norm, c.rms_eps_q32)?;
    record(ops, None, "final_norm", &normed);
    let mut logits = vec![0i64; c.vocab_size];
    project(&model.embed, &normed, &mut logits)?;
    record(ops, None, "lm_head", &logits);
    Ok(logits)
}

/// Per-position operator hashes and logits hashes of one token sequence.
struct Trace {
    ops: Vec<Vec<OpHash>>,
    logits_hashes: Vec<String>,
}

fn trace_sequence(model: &ModernModel, sequence: &[u32]) -> Result<Trace, ModernError> {
    let layers = model.config.n_layers;
    let mut cache = Cache {
        keys: vec![Vec::new(); layers],
        values: vec![Vec::new(); layers],
        positions: 0,
    };
    let mut trace = Trace {
        ops: Vec::with_capacity(sequence.len()),
        logits_hashes: Vec::with_capacity(sequence.len()),
    };
    for &token in sequence {
        let mut ops = Vec::new();
        let logits = forward(model, token, &mut cache, &mut ops)?;
        trace
            .logits_hashes
            .push(hex_lower(&arith::logits_hash(&logits)));
        trace.ops.push(ops);
    }
    Ok(trace)
}

/// Find the first operator where `bad` differs from `good` on `case`, up to
/// forward `position`. `None` when the trace cannot be trusted or finds no
/// difference.
pub(super) fn locate(
    model: &ModernModel,
    good: Backend,
    bad: Backend,
    case: &CaseTrace,
    position: usize,
) -> Result<Option<Hit>, ModernError> {
    let Some(sequence) = (0..=position)
        .map(|p| case.input_token(p))
        .collect::<Option<Vec<u32>>>()
    else {
        return Ok(None);
    };
    let Some(expected) = case.logits_hashes.get(..=position) else {
        return Ok(None);
    };
    good.activate()?;
    let reference = trace_sequence(model, &sequence)?;
    if reference.logits_hashes != expected {
        eprintln!(
            "  the traced pass did not reproduce the reference on {}, so no layer is reported",
            good.name()
        );
        return Ok(None);
    }
    bad.activate()?;
    let differing = match trace_sequence(model, &sequence) {
        Ok(trace) => trace,
        Err(error) => {
            eprintln!("  {} stopped during the trace: {error}", bad.name());
            return Ok(None);
        }
    };
    for (index, (a, b)) in reference.ops.iter().zip(&differing.ops).enumerate() {
        if let Some(first) = a.iter().zip(b).find(|(x, y)| x.hash != y.hash) {
            eprintln!(
                "  first difference: position {index}, layer {}, {}",
                first
                    .0
                    .layer
                    .map_or_else(|| "-".to_string(), |l| l.to_string()),
                first.0.op
            );
            return Ok(Some(Hit {
                position: index,
                layer: first.0.layer,
                op: first.0.op,
            }));
        }
    }
    eprintln!("  the traced pass found no differing operator");
    Ok(None)
}
