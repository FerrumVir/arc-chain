//! NON-CANONICAL STUDY (feature `float-accumulation-study`, never in a node or
//! release binary): how often ARC's algorithm with f32 accumulation chooses
//! the exact engine's tokens.
//!
//! Teacher-forces the f32-accumulating engine on sequences the exact engine
//! generated on the worker path (the draft-agreement experiment's
//! `arc-*.json` records: every token forwarded, and the tokens chosen) and
//! writes, for every generated position, its raw and repetition-penalty
//! choices, the rank of the exact token after the penalty, and the logit
//! gaps, in the experiment's teacher format. The first sequence is also
//! teacher-forced with f32 accumulation off, which must reproduce every exact
//! token, or the run exits non-zero.
//!
//! usage: float_accumulation_study --model GGUF --profile legacy|interleaved
//!        --arc FILE.json[,FILE.json...] --out FILE.jsonl [--chunk 64]

use arc_inference::cached_integer_model::{
    CachedIntegerModel, KVCache, load_cached_model_canonical_i8,
    load_cached_model_canonical_i8_interleaved_rope, select_next_token_with_repetition_penalty,
};
use arc_inference::float_accumulation_study;
use arc_inference::integer_lut::argmax_i64;
use serde_json::{Value, json};
use std::io::Write;
use std::time::Instant;

const ONE: f64 = 65536.0;

struct Sequence {
    id: String,
    first: usize,
    fed: Vec<u32>,
    generated: Vec<u32>,
    arc_raw: Vec<u32>,
}

fn tokens(value: &Value) -> Result<Vec<u32>, String> {
    value
        .as_array()
        .ok_or("expected an array of tokens")?
        .iter()
        .map(|t| {
            t.as_u64()
                .and_then(|t| u32::try_from(t).ok())
                .ok_or_else(|| "bad token".to_string())
        })
        .collect()
}

fn read_sequences(paths: &str, profile: &str) -> Result<Vec<Sequence>, String> {
    let mut out = Vec::new();
    for path in paths.split(',').filter(|p| !p.is_empty()) {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        let run: Value = serde_json::from_str(&text).map_err(|e| format!("parse {path}: {e}"))?;
        if run["profile"].as_str() != Some(profile) {
            continue;
        }
        for rec in run["prompts"].as_array().ok_or("no prompts")? {
            let id = rec["id"].as_str().ok_or("no id")?;
            let seq = Sequence {
                id: format!("{profile}/{id}"),
                first: rec["first"].as_u64().ok_or("no first")? as usize,
                fed: tokens(&rec["fed"])?,
                generated: tokens(&rec["generated"])?,
                arc_raw: tokens(&rec["arc_raw_argmax"])?,
            };
            if seq.fed.len() != seq.first + seq.generated.len() {
                return Err(format!("{}: fed length mismatch", seq.id));
            }
            out.push(seq);
        }
    }
    Ok(out)
}

/// Logits after every fed token, by the batched prefill.
fn teacher_logits(
    model: &CachedIntegerModel,
    fed: &[u32],
    chunk: usize,
) -> Result<Vec<Vec<i64>>, String> {
    let mut cache = KVCache::new(model.config.n_layers);
    model
        .prefill_canonical_i8_batched(fed, &mut cache, chunk, true)
        .filter(|rows| rows.len() == fed.len())
        .ok_or_else(|| "the batched prefill refused the sequence".to_string())
}

/// Scores one sequence: per position the raw and penalised choices, the
/// rank of the exact token after the penalty (0 = first choice), the gaps
/// to it and the margin between the first two choices, in logit units.
fn score(seq: &Sequence, logits: &[Vec<i64>]) -> Value {
    let n = seq.generated.len();
    let (mut raw, mut pen, mut rank) = (Vec::new(), Vec::new(), Vec::new());
    let (mut raw_gap, mut pen_gap, mut margin) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..n {
        let row = &logits[seq.first + i];
        let want = seq.generated[i] as usize;
        let r = argmax_i64(row);
        raw.push(r as u32);
        raw_gap.push((row[r] - row[want]) as f64 / ONE);
        let mut penalised = row.clone();
        let p =
            select_next_token_with_repetition_penalty(&mut penalised, &seq.generated[..i]) as usize;
        pen.push(p as u32);
        pen_gap.push((penalised[p] - penalised[want]) as f64 / ONE);
        let target = penalised[want];
        let ahead = penalised
            .iter()
            .enumerate()
            .filter(|&(t, &v)| v > target || (v == target && t < want))
            .count();
        rank.push(ahead);
        let second = penalised
            .iter()
            .enumerate()
            .filter(|&(t, _)| t != p)
            .map(|(_, &v)| v)
            .max()
            .unwrap_or(penalised[p]);
        margin.push((penalised[p] - second) as f64 / ONE);
    }
    json!({
        "id": seq.id,
        "n_fed": seq.fed.len(),
        "raw": raw,
        "pen": pen,
        "raw_gap": raw_gap,
        "pen_gap": pen_gap,
        "pen_rank": rank,
        "pen_margin": margin,
    })
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let (mut model_path, mut profile, mut arc, mut out_path, mut chunk) = (
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        64usize,
    );
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => model_path = value()?,
            "--profile" => profile = value()?,
            "--arc" => arc = value()?,
            "--out" => out_path = value()?,
            "--chunk" => chunk = value()?.parse().map_err(|e| format!("{e}"))?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if profile != "legacy" && profile != "interleaved" {
        return Err("--profile must be legacy or interleaved".into());
    }
    let sequences = read_sequences(&arc, &profile)?;
    if sequences.is_empty() {
        return Err(format!("no {profile} sequences in {arc}"));
    }
    let started = Instant::now();
    let model = if profile == "interleaved" {
        load_cached_model_canonical_i8_interleaved_rope(&model_path)
    } else {
        load_cached_model_canonical_i8(&model_path)
    }
    .map_err(|e| format!("load {model_path}: {e}"))?;
    if !model.has_canonical_i8_profile() || !model.has_all_transformer_layers() {
        return Err("the model did not load as a complete canonical INT8 model".into());
    }
    println!(
        "{} sequences, profile {}, loaded in {:.0} s, {} rayon threads",
        sequences.len(),
        model.arithmetic_profile(),
        started.elapsed().as_secs_f64(),
        rayon::current_num_threads()
    );

    // The harness itself: exact arithmetic, teacher-forced, must reproduce the
    // exact engine's own tokens at every position.
    float_accumulation_study::set_enabled(false);
    let check = &sequences[0];
    let exact = score(check, &teacher_logits(&model, &check.fed, chunk)?);
    let same_pen = exact["pen"].as_array().is_some_and(|p| {
        p.iter()
            .zip(&check.generated)
            .all(|(a, &b)| a.as_u64() == Some(u64::from(b)))
    });
    let same_raw = exact["raw"].as_array().is_some_and(|p| {
        p.iter()
            .zip(&check.arc_raw)
            .all(|(a, &b)| a.as_u64() == Some(u64::from(b)))
    });
    println!(
        "harness check on {}: exact teacher-forcing reproduces tokens {same_pen}, raw argmax {same_raw}",
        check.id
    );
    if !same_pen || !same_raw {
        return Err("exact teacher-forcing does not reproduce the exact engine".into());
    }

    float_accumulation_study::set_enabled(true);
    let mut out = std::fs::File::create(&out_path).map_err(|e| format!("{out_path}: {e}"))?;
    for seq in &sequences {
        let seq_started = Instant::now();
        let logits = teacher_logits(&model, &seq.fed, chunk)?;
        let mut row = score(seq, &logits);
        let seconds = seq_started.elapsed().as_secs_f64();
        row["seconds"] = json!(seconds);
        let agree = row["pen"]
            .as_array()
            .map(|p| {
                p.iter()
                    .zip(&seq.generated)
                    .filter(|(a, b)| a.as_u64() == Some(u64::from(**b)))
                    .count()
            })
            .unwrap_or(0);
        println!(
            "{}: {} fed, {} generated, {agree} agree (f32 accumulation), {seconds:.0} s",
            seq.id,
            seq.fed.len(),
            seq.generated.len()
        );
        writeln!(out, "{row}").map_err(|e| format!("{out_path}: {e}"))?;
    }
    float_accumulation_study::set_enabled(false);
    println!("done in {:.0} s", started.elapsed().as_secs_f64());
    Ok(())
}
