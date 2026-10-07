//! Reproducible ENG-8 acceptance measurement on a supplied integer package.
//! Run with --package PKG --tokenizer tokenizer.json --cases CASES.json
//! --out RESULT.json --summary RESULT.md [--threads N] [--depth D]
//! [--drafters lookup,recycle,hybrid,hybrid-warm] [--recycle-nodes N]
//! [--top-k K] [--rank-cost C0,C1,..] [--hybrid-min-ngram N] [--draft-package PKG].
//! The optional draft package must share the target's tokenizer; it is never
//! downloaded by this program. No nodes, network endpoints or signing
//! material are used. Every tree output is checked against plain greedy
//! generation (tokens, every logits hash) before it is counted.
use arc_inference::modern::serving::{
    dense::DenseModel,
    tree::{self, LocalModelTree, LookupTree, RecycleTree, TreeDrafter},
};
use arc_inference::modern::{
    arith,
    bpe::ByteLevelBpe,
    chat::{ChatPrompt, render},
    model::GenerationRequest,
    package,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Instant};

/// Kimi K2.x hidden width (docs/protocol/kimi-k26-checkpoint.md, PR #156):
/// one row crossing a stage boundary is this many `i64` values.
const KIMI_D_MODEL: usize = 7168;
const HOPS: [usize; 4] = [8, 16, 32, 60];
const HOP_MS: [f64; 3] = [10.0, 30.0, 60.0];
const COMPUTE_MS: [f64; 2] = [0.0, 50.0];
/// Assumed per-link bandwidths for the activation-transfer column.
const LINK_GBPS: [f64; 2] = [1.0, 10.0];

#[derive(Default)]
struct Totals {
    cases: usize,
    tokens: usize,
    passes: usize,
    rows: usize,
    expanded: usize,
    nodes: usize,
    matched: usize,
    seconds: f64,
}

/// Milliseconds one traversal of `hops` links spends moving `rows` raw `i64`
/// Kimi rows at `gbps` per link (serialization only; latency is separate).
fn transfer_ms(rows: f64, hops: usize, gbps: f64) -> f64 {
    hops as f64 * rows * (KIMI_D_MODEL * 8 * 8) as f64 / (gbps * 1e6)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let value = |key: &str| {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let required = |key: &str| value(key).ok_or_else(|| format!("missing {key}"));
    let threads: usize = value("--threads").map_or(Ok(4), |v| v.parse())?;
    let depth: usize = value("--depth").map_or(Ok(8), |v| v.parse())?;
    let lookup_nodes: usize = value("--lookup-nodes").map_or(Ok(32), |v| v.parse())?;
    let recycle_nodes: usize = value("--recycle-nodes").map_or(Ok(48), |v| v.parse())?;
    let top_k: usize = value("--top-k").map_or(Ok(8), |v| v.parse())?;
    let rank_cost: Option<Vec<usize>> = value("--rank-cost")
        .map(|v| v.split(',').map(str::parse).collect())
        .transpose()?;
    let hybrid_min_ngram: usize = value("--hybrid-min-ngram").map_or(Ok(2), |v| v.parse())?;
    let wanted = value("--drafters").unwrap_or_else(|| "lookup,recycle,hybrid".into());
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()?;
    arc_inference::canonical_simd::set_fast_canonical_kernel(true);
    let path = required("--package")?;
    let digest = package::digest_file(Path::new(&path))?;
    let model = package::load_package(Path::new(&path))?;
    let dense = DenseModel::new(&model, *blake3::hash(digest.sha256.as_bytes()).as_bytes());
    let tokenizer_bytes = std::fs::read(required("--tokenizer")?)?;
    let tokenizer = ByteLevelBpe::from_json(&tokenizer_bytes)?;
    let cases_bytes = std::fs::read(required("--cases")?)?;
    let cases: Value = serde_json::from_slice(&cases_bytes)?;
    let draft_path = value("--draft-package");
    let draft_model = draft_path
        .as_ref()
        .map(|p| package::load_package(Path::new(p)))
        .transpose()?;
    let draft_digest = draft_path
        .as_ref()
        .map(|p| package::digest_file(Path::new(p)))
        .transpose()?;
    let draft_dense = draft_model.as_ref().map(|m| DenseModel::new(m, [0; 32]));
    let lookup = LookupTree {
        max_nodes: lookup_nodes,
        ..LookupTree::default()
    };
    let hybrid_lookup = LookupTree {
        min_ngram: hybrid_min_ngram,
        ..lookup
    };
    let recycler = |lookup: Option<LookupTree>| {
        let mut tree = RecycleTree::new(top_k, recycle_nodes, lookup);
        if let Some(cost) = &rank_cost {
            tree.rank_cost = cost.clone();
        }
        tree
    };
    let make = |name: &str| -> Result<Box<dyn TreeDrafter>, String> {
        Ok(match name {
            "lookup" => Box::new(lookup),
            "recycle" => Box::new(recycler(None)),
            "hybrid" | "hybrid-warm" => Box::new(recycler(Some(hybrid_lookup))),
            other => return Err(format!("unknown drafter {other}")),
        })
    };
    // (name, drafter, starts fresh on every request)
    let mut drafters: Vec<(String, Box<dyn TreeDrafter + '_>, bool)> = Vec::new();
    for name in wanted.split(',').map(str::trim) {
        drafters.push((name.into(), make(name)?, !name.ends_with("-warm")));
    }
    if let Some(m) = &draft_dense {
        drafters.push((
            "local-model".into(),
            Box::new(LocalModelTree {
                model: m,
                top_k: 2,
                max_nodes: 32,
            }),
            false,
        ));
    }
    let mut records = Vec::new();
    let mut totals: BTreeMap<(String, String), Totals> = BTreeMap::new();
    for case in cases["cases"].as_array().ok_or("missing cases")? {
        let id = case["id"].as_str().ok_or("missing id")?;
        let traffic = case["traffic"].as_str().ok_or("missing traffic")?;
        let prompt = tokenizer.encode(&render(&ChatPrompt {
            system: Some("Be concise. /no_think"),
            user: case["user"].as_str().ok_or("missing user")?,
            thinking: false,
            today: "07 October 2026",
        }))?;
        let max_tokens = case["max_tokens"].as_u64().ok_or("missing max_tokens")? as usize;
        let eos = [128012];
        let request = GenerationRequest {
            prompt: &prompt,
            max_tokens,
            eos: &eos,
            selection: arith::Selection::Argmax,
        };
        let baseline = model.generate(&request)?;
        for (name, drafter, fresh) in &mut drafters {
            // A fresh drafter per request, except the warm variant, which keeps
            // candidates learned on earlier requests (a serving node's history).
            if *fresh {
                *drafter = make(name)?;
            }
            let start = Instant::now();
            let out = tree::generate_tree(&dense, &request, drafter.as_mut(), depth)?;
            let seconds = start.elapsed().as_secs_f64();
            if out.tokens != baseline.tokens
                || out.logits_hashes != baseline.logits_hashes
                || arith::tokens_hash(&out.tokens) != baseline.output_hash
            {
                return Err(format!("{id}/{name}: differs from greedy").into());
            }
            // Decode numerator excludes the first token selected at prefill.
            let decode_tokens = out.tokens.len() - 1;
            let matched = decode_tokens - out.verification_passes;
            let total = totals.entry((traffic.into(), name.clone())).or_default();
            total.cases += 1;
            total.tokens += decode_tokens;
            total.passes += out.verification_passes;
            total.rows += out.verified_rows;
            total.expanded += out.expanded_rows;
            total.nodes += out.logical_nodes;
            total.matched += matched;
            total.seconds += seconds;
            records.push(json!({"id":id,"traffic":traffic,"drafter":name,"prompt_tokens":prompt.len(),
                "output_tokens":out.tokens,"output_text":tokenizer.decode(&out.tokens,false),
                "output_blake3":hex::encode(baseline.output_hash),"logits_digest":hex::encode(baseline.logits_digest),
                "kv_digest":hex::encode(out.kv_digest),"identical":true,"verification_passes":out.verification_passes,
                "decode_tokens":decode_tokens,"accepted_draft_transitions":matched,
                "logical_nodes":out.logical_nodes,"physical_rows":out.verified_rows,
                "path_lowered_rows":out.expanded_rows,
                "tree_seconds_including_prefill_and_draft":seconds,
                "greedy_prefill_seconds":baseline.prefill_seconds,"greedy_decode_seconds":baseline.decode_seconds}));
            eprintln!(
                "{id}/{name}: {decode_tokens} decode tokens / {} passes ({:.2}/pass), {} rows; identical; {seconds:.1} s",
                out.verification_passes,
                decode_tokens as f64 / out.verification_passes.max(1) as f64,
                out.verified_rows
            );
        }
    }
    let mut summary = format!(
        "ENG-8 tree speculation evidence ({os} {arch}, {threads} threads)\n\nAuthored representative fixtures, not sampled production traffic. Plain integer argmax. Every output was checked against plain greedy generation (tokens and every logits hash) before being counted. Drafters see only the current prompt, the emitted output and the target logits of rows already verified; recycle/hybrid start cold on every request. Tokens/pass = decode tokens / verification passes (each pass yields its accepted drafts plus one target token); the first token, chosen at prefill, is excluded. Rows/pass is what the target computed per pass with shared-node trees; path-lowered is what one-sequence-per-leaf lowering would send for the same trees.\n\n| Traffic | Drafter | Cases | Decode tokens | Passes | Tokens/pass | Nodes/pass | Rows/pass (shared) | Rows/pass (path-lowered) | Identical |\n|---|---|---:|---:|---:|---:|---:|---:|---:|---|\n",
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
    );
    let mut acceptance = Vec::new();
    let mut projections = Vec::new();
    for ((traffic, drafter), t) in &totals {
        let per_pass = |x: usize| x as f64 / t.passes.max(1) as f64;
        let rate = per_pass(t.tokens);
        summary.push_str(&format!(
            "| {traffic} | {drafter} | {} | {} | {} | {rate:.3} | {:.1} | {:.1} | {:.1} | yes |\n",
            t.cases,
            t.tokens,
            t.passes,
            per_pass(t.nodes),
            per_pass(t.rows),
            per_pass(t.expanded)
        ));
        acceptance.push(json!({"traffic":traffic,"drafter":drafter,"cases":t.cases,"decode_tokens":t.tokens,
            "passes":t.passes,"tokens_per_pass":rate,"physical_rows":t.rows,"path_lowered_rows":t.expanded,
            "logical_nodes":t.nodes,"rows_per_pass":per_pass(t.rows),"path_lowered_rows_per_pass":per_pass(t.expanded),
            "accepted_draft_transitions":t.matched,"seconds_including_prefill":t.seconds}));
        if t.passes == 0 {
            continue;
        }
        for hops in HOPS {
            for hop_ms in HOP_MS {
                for compute_ms in COMPUTE_MS {
                    let mut entry = json!({"traffic":traffic,"drafter":drafter,"hops":hops,"hop_ms":hop_ms,
                        "assumed_compute_ms":compute_ms,"tokens_per_pass":rate,
                        "projected_tok_s":tree::projected_tokens_per_second(rate,hops,hop_ms,compute_ms)?});
                    for gbps in LINK_GBPS {
                        let shared = transfer_ms(per_pass(t.rows), hops, gbps);
                        let lowered = transfer_ms(per_pass(t.expanded), hops, gbps);
                        entry[format!("projected_tok_s_raw_i64_{gbps}gbps_shared")] =
                            json!(tree::projected_tokens_per_second(
                                rate,
                                hops,
                                hop_ms,
                                compute_ms + shared
                            )?);
                        entry[format!("projected_tok_s_raw_i64_{gbps}gbps_path_lowered")] =
                            json!(tree::projected_tokens_per_second(
                                rate,
                                hops,
                                hop_ms,
                                compute_ms + lowered
                            )?);
                    }
                    projections.push(entry);
                }
            }
        }
    }
    summary.push_str("\nPROJECTION, not a measurement: tok/s = tokens/pass ÷ ((hops × ms/hop + compute_ms) / 1000). Tokens/pass is SmolLM3-3B's, measured above; Kimi's acceptance is unmeasured. One pass = one sequential traversal of all hops. compute_ms (0 = network-only bound, 50 = an assumption) must also cover drafting, serialization and the return path.\n\n| Hops | ms/hop | Traversal ms | No speculation (1 token/pass) | Tokens/pass needed for 59 tok/s, 0 ms compute | Needed, 50 ms compute |\n|---:|---:|---:|---:|---:|---:|\n");
    for hops in HOPS {
        for hop in HOP_MS {
            let ms = hops as f64 * hop;
            summary.push_str(&format!(
                "| {hops} | {hop} | {ms} | {:.2} | {:.2} | {:.2} |\n",
                1000.0 / ms,
                59.0 * ms / 1000.0,
                59.0 * (ms + 50.0) / 1000.0
            ));
        }
    }
    summary.push_str("\nProjected tok/s per answer, 50 ms assumed compute, latency only:\n\n| Traffic / drafter | Tokens/pass | 8 × 10 ms | 16 × 10 ms | 16 × 30 ms | 32 × 30 ms | 60 × 60 ms |\n|---|---:|---:|---:|---:|---:|---:|\n");
    for ((traffic, drafter), t) in &totals {
        if t.passes == 0 {
            continue;
        }
        let rate = t.tokens as f64 / t.passes as f64;
        let mut row = format!("| {traffic} / {drafter} | {rate:.2} |");
        for (hops, hop) in [(8, 10.0), (16, 10.0), (16, 30.0), (32, 30.0), (60, 60.0)] {
            row.push_str(&format!(
                " {:.2} |",
                tree::projected_tokens_per_second(rate, hops, hop, 50.0)?
            ));
        }
        summary.push_str(&row);
        summary.push('\n');
    }
    summary.push_str(&format!("\nThe same at 8 × 10 ms and 50 ms compute, adding the time to serialize each pass's rows on every hop as raw Kimi i64 hidden states ({} bytes/row; ENG-9's lossless codec is not counted because its real-weight ratio is unmeasured):\n\n| Traffic / drafter | Rows/pass shared | tok/s shared, 1 Gbit/s | tok/s path-lowered, 1 Gbit/s | tok/s shared, 10 Gbit/s | tok/s path-lowered, 10 Gbit/s |\n|---|---:|---:|---:|---:|---:|\n", KIMI_D_MODEL * 8));
    for ((traffic, drafter), t) in &totals {
        if t.passes == 0 {
            continue;
        }
        let rate = t.tokens as f64 / t.passes as f64;
        let (rows, expanded) = (
            t.rows as f64 / t.passes as f64,
            t.expanded as f64 / t.passes as f64,
        );
        let at = |r: f64, gbps: f64| {
            tree::projected_tokens_per_second(rate, 8, 10.0, 50.0 + transfer_ms(r, 8, gbps))
        };
        summary.push_str(&format!(
            "| {traffic} / {drafter} | {rows:.1} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
            at(rows, 1.0)?,
            at(expanded, 1.0)?,
            at(rows, 10.0)?,
            at(expanded, 10.0)?
        ));
    }
    let result = json!({"schema":"arc.tree-speculation.v2","model_package":digest.to_json(),
        "draft_package":draft_digest.map(|d|d.to_json()),"tokenizer_blake3":blake3::hash(&tokenizer_bytes).to_hex().as_str(),
        "cases_blake3":blake3::hash(&cases_bytes).to_hex().as_str(),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "threads":threads,"kernel":"simd when supported, otherwise scalar fallback","depth":depth,
        "lookup_max_nodes":lookup_nodes,"recycle_max_nodes":recycle_nodes,"recycle_top_k":top_k,
        "recycle_rank_cost":recycler(None).rank_cost,"hybrid_lookup_min_ngram":hybrid_min_ngram,
        "projection_assumptions":{"kimi_d_model":KIMI_D_MODEL,"bytes_per_row":KIMI_D_MODEL*8,"link_gbps":LINK_GBPS,
            "compute_ms":COMPUTE_MS,"formula":"tok/s = tokens_per_pass / ((hops*hop_ms + compute_ms + hops*rows*bytes_per_row*8/link_bps*1000) / 1000)"},
        "records":records,"acceptance":acceptance,"projections":projections});
    std::fs::write(
        required("--out")?,
        serde_json::to_string_pretty(&result)? + "\n",
    )?;
    std::fs::write(required("--summary")?, summary)?;
    Ok(())
}
