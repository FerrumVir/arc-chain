//! Draft-agreement experiment, ARC side (scratch branch, not for merge).
//!
//! Generates with ARC's exact engine on the worker path and records every
//! token the engine forwarded, so a fast non-exact engine can be
//! teacher-forced on exactly the same context
//! (`.github/workflows/draft-agreement.yml`).
//!
//! The loop mirrors `CachedIntegerModel::try_generate`, the community worker's
//! contract (legacy v1): one BOS forward, the prompt one token at a time, the
//! last prompt token forwarded again, then ARC's deterministic repetition
//! penalty and argmax at every step. It also records the raw argmax before
//! the penalty. The first prompt of every run is generated a second time with
//! `try_generate` itself; any difference in tokens or output hash exits
//! non-zero.
//!
//! usage: draft_agreement_arc --model GGUF --prompts FILE.json --out FILE.json
//!        [--profile legacy|interleaved] [--category NAME]
//!        [--max-new-tokens N] [--kernel scalar|simd]

use arc_inference::cached_integer_model::{
    CachedIntegerModel, KVCache, load_cached_model_canonical_i8,
    load_cached_model_canonical_i8_interleaved_rope, select_next_token_with_repetition_penalty,
};
use arc_inference::canonical_simd;
use arc_inference::integer_lut::argmax_i64;
use serde_json::{Value, json};
use std::time::Instant;

struct Args {
    model: String,
    prompts: String,
    out: String,
    profile: String,
    category: Option<String>,
    max_new_tokens: u32,
    kernel: String,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        model: String::new(),
        prompts: String::new(),
        out: String::new(),
        profile: "legacy".into(),
        category: None,
        max_new_tokens: 256,
        kernel: "simd".into(),
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = || iter.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => args.model = value()?,
            "--prompts" => args.prompts = value()?,
            "--out" => args.out = value()?,
            "--profile" => args.profile = value()?,
            "--category" => args.category = Some(value()?),
            "--max-new-tokens" => {
                args.max_new_tokens = value()?.parse().map_err(|e| format!("{e}"))?;
            }
            "--kernel" => args.kernel = value()?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.model.is_empty() || args.prompts.is_empty() || args.out.is_empty() {
        return Err("--model, --prompts and --out are required".into());
    }
    if args.profile != "legacy" && args.profile != "interleaved" {
        return Err("--profile must be legacy or interleaved".into());
    }
    if args.kernel != "scalar" && args.kernel != "simd" {
        return Err("--kernel must be scalar or simd".into());
    }
    Ok(args)
}

/// One generation, with the exact tokens the engine forwarded.
struct Generation {
    /// Every token forwarded, in order: BOS, the prompt, the last prompt
    /// token again, then every generated token except the last.
    fed: Vec<u32>,
    /// Index into `fed` whose logits chose the first generated token; the
    /// logits after `fed[first + i]` chose `generated[i]`.
    first: usize,
    generated: Vec<u32>,
    /// Argmax of the same logits before the repetition penalty.
    raw_argmax: Vec<u32>,
    output_hash: String,
}

/// `try_generate`'s loop (legacy v1 worker semantics), recording what it fed.
fn generate(model: &CachedIntegerModel, prompt: &[u32], max_new: u32, eos: &[u32]) -> Generation {
    let mut cache = KVCache::new(model.config.n_layers);
    let mut fed: Vec<u32> = Vec::with_capacity(prompt.len() + max_new as usize + 2);
    let bos = model.config.bos_token;
    let _ = model.forward_one_token(bos, &mut cache);
    fed.push(bos);
    for &tok in prompt {
        let _ = model.forward_one_token(tok, &mut cache);
        fed.push(tok);
    }
    let first = fed.len();
    let mut generated: Vec<u32> = Vec::new();
    let mut raw_argmax: Vec<u32> = Vec::new();
    for _ in 0..max_new {
        let last = generated
            .last()
            .copied()
            .unwrap_or(*prompt.last().unwrap_or(&0));
        let mut logits = model.forward_one_token(last, &mut cache);
        fed.push(last);
        raw_argmax.push(argmax_i64(&logits) as u32);
        let next = select_next_token_with_repetition_penalty(&mut logits, &generated);
        generated.push(next);
        if eos.contains(&next) {
            break;
        }
    }
    let bytes: Vec<u8> = generated.iter().flat_map(|t| t.to_le_bytes()).collect();
    let output_hash = hex::encode(arc_crypto::hash_bytes(&bytes).0);
    Generation {
        fed,
        first,
        generated,
        raw_argmax,
        output_hash,
    }
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let started = Instant::now();
    let simd = args.kernel == "simd";
    if simd && !canonical_simd::dotprod_available() {
        return Err("the vectorised kernel is not available on this CPU".into());
    }
    canonical_simd::set_fast_canonical_kernel(simd);

    let text = std::fs::read_to_string(&args.prompts)
        .map_err(|e| format!("read {}: {e}", args.prompts))?;
    let file: Value = serde_json::from_str(&text).map_err(|e| format!("parse prompts: {e}"))?;
    let mut prompts: Vec<(String, String, String)> = Vec::new();
    for item in file["prompts"].as_array().ok_or("prompts file has no prompts array")? {
        let id = item["id"].as_str().ok_or("prompt without id")?.to_string();
        let category = item["category"].as_str().ok_or("prompt without category")?.to_string();
        let body = item["text"].as_str().ok_or("prompt without text")?.to_string();
        if args.category.as_ref().is_none_or(|wanted| *wanted == category) {
            prompts.push((id, category, body));
        }
    }
    if prompts.is_empty() {
        return Err("no prompt matches --category".into());
    }

    let load_started = Instant::now();
    let model = if args.profile == "interleaved" {
        load_cached_model_canonical_i8_interleaved_rope(&args.model)
    } else {
        load_cached_model_canonical_i8(&args.model)
    }
    .map_err(|e| format!("load {}: {e}", args.model))?;
    let load_s = load_started.elapsed().as_secs_f64();
    if !model.has_canonical_i8_profile() || !model.has_all_transformer_layers() {
        return Err("the model did not load as a complete canonical INT8 model".into());
    }
    let eos = model.config.eos_tokens.clone();
    println!(
        "model: {} layers, vocab {}, profile {}, loaded in {load_s:.0} s, {} rayon threads, kernel {}, {} prompts",
        model.config.n_layers,
        model.config.vocab_size,
        model.arithmetic_profile(),
        rayon::current_num_threads(),
        args.kernel,
        prompts.len()
    );

    let mut records = Vec::new();
    let mut gate = Value::Null;
    for (index, (id, category, body)) in prompts.iter().enumerate() {
        let prompt = model.encode(&format!("[INST] {body} [/INST]"));
        if prompt.is_empty() {
            return Err(format!("{id}: the prompt encodes to no tokens"));
        }
        let run_started = Instant::now();
        let g = generate(&model, &prompt, args.max_new_tokens, &eos);
        let seconds = run_started.elapsed().as_secs_f64();
        if g.fed.len() != g.first + g.generated.len() {
            return Err(format!("{id}: fed {} tokens, expected {}", g.fed.len(), g.first + g.generated.len()));
        }
        if index == 0 {
            let (tokens, hash) = model
                .try_generate(&prompt, args.max_new_tokens, &eos)
                .map_err(|e| format!("{id}: try_generate: {e}"))?;
            let same = tokens == g.generated && hex::encode(hash.0) == g.output_hash;
            println!("{id}: try_generate identical = {same}");
            if !same {
                return Err(format!("{id}: the recording loop differs from try_generate"));
            }
            gate = json!({ "prompt": id, "identical": same, "output_hash": g.output_hash });
        }
        let stopped_at_eos = g.generated.last().is_some_and(|t| eos.contains(t));
        let text = model.decode(&g.generated);
        let preview: String = text.chars().take(160).collect();
        println!(
            "{id}: {} prompt tokens, {} generated, eos {stopped_at_eos}, {seconds:.0} s, hash {}: {preview:?}",
            prompt.len(),
            g.generated.len(),
            &g.output_hash[..12]
        );
        records.push(json!({
            "id": id,
            "category": category,
            "prompt_tokens": prompt.len(),
            "first": g.first,
            "fed": g.fed,
            "generated": g.generated,
            "arc_raw_argmax": g.raw_argmax,
            "stopped_at_eos": stopped_at_eos,
            "output_hash": g.output_hash,
            "text": text,
            "seconds": seconds,
        }));
    }

    let report = json!({
        "label": "MEASURED on a CI runner",
        "engine": "ARC exact integer engine, worker path (try_generate, legacy v1 semantics)",
        "profile": args.profile,
        "arithmetic_profile": model.arithmetic_profile(),
        "category": args.category,
        "kernel": args.kernel,
        "max_new_tokens": args.max_new_tokens,
        "bos_token": model.config.bos_token,
        "eos_tokens": eos,
        "vocab_size": model.config.vocab_size,
        "rayon_threads": rayon::current_num_threads(),
        "load_s": load_s,
        "total_s": started.elapsed().as_secs_f64(),
        "gate": gate,
        "prompts": records,
    });
    let out = serde_json::to_string(&report).map_err(|e| format!("{e}"))?;
    std::fs::write(&args.out, out).map_err(|e| format!("write {}: {e}", args.out))?;
    println!("wrote {} in {:.0} s", args.out, started.elapsed().as_secs_f64());
    Ok(())
}
