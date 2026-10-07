//! Reproducible ENG-8 acceptance measurement on a supplied integer package.
//! Run with --package PKG --tokenizer tokenizer.json --cases CASES.json
//! --out RESULT.json --summary RESULT.md [--draft-package PKG]. The optional
//! draft package must share the target's tokenizer; it is never downloaded by
//! this program. No nodes, network endpoints or signing material are used.
use arc_inference::modern::serving::{
    dense::DenseModel,
    tree::{self, LocalModelTree, LookupTree, TreeDrafter},
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

#[derive(Default)]
struct Totals {
    cases: usize,
    tokens: usize,
    passes: usize,
    rows: usize,
    matched: usize,
    seconds: f64,
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
    rayon::ThreadPoolBuilder::new()
        .num_threads(4)
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
    let local = draft_dense.as_ref().map(|m| LocalModelTree {
        model: m,
        top_k: 2,
        max_nodes: 32,
    });
    let lookup = LookupTree::default();
    let mut drafters: Vec<(&str, &dyn TreeDrafter)> = vec![("lookup-tree", &lookup)];
    if let Some(local) = &local {
        drafters.push(("local-model-head-tree", local));
    }
    let mut records = Vec::new();
    // totals: cases, generated decode tokens, verification passes, physical
    // rows, matched draft transitions, elapsed (includes drafting + prefill).
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
        for &(drafter_name, drafter) in &drafters {
            let start = Instant::now();
            let out = tree::generate_tree(&dense, &request, drafter, 8)?;
            let seconds = start.elapsed().as_secs_f64();
            if out.tokens != baseline.tokens
                || out.logits_hashes != baseline.logits_hashes
                || arith::tokens_hash(&out.tokens) != baseline.output_hash
            {
                return Err(format!("{id}/{drafter_name}: differs from greedy").into());
            }
            // Decode numerator excludes the first token selected at prefill.
            let decode_tokens = out.tokens.len() - 1;
            let matched = decode_tokens - out.verification_passes;
            let total = totals
                .entry((traffic.into(), drafter_name.into()))
                .or_default();
            total.cases += 1;
            total.tokens += decode_tokens;
            total.passes += out.verification_passes;
            total.rows += out.verified_rows;
            total.matched += matched;
            total.seconds += seconds;
            records.push(json!({"id":id,"traffic":traffic,"drafter":drafter_name,"prompt_tokens":prompt,
                "output_tokens":out.tokens,"output_text":tokenizer.decode(&out.tokens,false),
                "output_blake3":hex::encode(baseline.output_hash),"logits_digest":hex::encode(baseline.logits_digest),
                "kv_digest":hex::encode(out.kv_digest),"identical":true,"verification_passes":out.verification_passes,
                "decode_tokens":decode_tokens,"accepted_draft_transitions":matched,
                "logical_nodes":out.logical_nodes,"physical_rows":out.verified_rows,"tree_seconds_including_prefill_and_draft":seconds,
                "greedy_prefill_seconds":baseline.prefill_seconds,"greedy_decode_seconds":baseline.decode_seconds}));
            eprintln!(
                "{id}/{drafter_name}: {decode_tokens} decode tokens / {} passes; identical",
                out.verification_passes
            );
        }
    }
    let mut summary = String::from(
        "ENG-8 tree speculation evidence\n\nMeasured on GitHub Actions only when this file comes from that workflow; platform is recorded in JSON. Authored representative fixtures (two coding, two agent transcript, two chat), not sampled production traffic. Plain integer argmax; lookup drafts see only the current prompt and generated prefix. First prefill token excluded from acceptance numerator and verification denominator. Accepted transitions = decode tokens minus passes; each pass also supplies one greedy fallback/bonus token.\n\n| Traffic | Drafter | Cases | Decode tokens | Passes | Tokens/pass | Accepted transitions | Physical rows | Identical |\n|---|---|---:|---:|---:|---:|---:|---:|---|\n",
    );
    let mut acceptance = Vec::new();
    let mut projections = Vec::new();
    for (
        (traffic, drafter),
        Totals {
            cases: n,
            tokens,
            passes,
            rows,
            matched,
            seconds,
        },
    ) in &totals
    {
        let rate = if *passes == 0 {
            0.0
        } else {
            *tokens as f64 / *passes as f64
        };
        summary.push_str(&format!("| {traffic} | {drafter} | {n} | {tokens} | {passes} | {rate:.3} | {matched} | {rows} | yes |\n"));
        acceptance.push(json!({"traffic":traffic,"drafter":drafter,"cases":n,"decode_tokens":tokens,"passes":passes,"tokens_per_pass":rate,"physical_rows":rows,"accepted_draft_transitions":matched,"seconds_including_prefill":seconds}));
        if *passes > 0 {
            for hops in [8, 16, 32, 60] {
                for hop_ms in [10.0, 30.0, 60.0] {
                    for compute_ms in [0.0, 50.0] {
                        projections.push(json!({"traffic":traffic,"drafter":drafter,"hops":hops,"hop_ms":hop_ms,"assumed_compute_ms":compute_ms,"tokens_per_pass":rate,"projected_tok_s":tree::projected_tokens_per_second(rate,hops,hop_ms,compute_ms)?}));
                    }
                }
            }
        }
    }
    summary.push_str("\nPROJECTION (not measured network performance): tok/s = measured SmolLM tokens/pass / ((hops × hop_ms + assumed compute_ms) / 1000). SmolLM acceptance does not establish Kimi acceptance. A pass assumes one sequential traversal, counting all one-way hops; additional return latency, draft time, serialization and tree compute must be included in compute_ms. 0 ms is a network-only upper bound; 50 ms is an illustrative assumption, not a Kimi measurement. Shared ancestors and prefix caches are duplicated in this portable implementation. JSON contains the full traffic × hops × latency × compute grid.\n\n| Hops | ms/hop | Traversal ms | Tokens/pass needed for 59 tok/s (0 ms compute) | Needed (50 ms compute) |\n|---:|---:|---:|---:|---:|\n");
    for hops in [8, 16, 32, 60] {
        for hop in [10, 30, 60] {
            let ms = hops * hop;
            summary.push_str(&format!(
                "| {hops} | {hop} | {ms} | {:.2} | {:.2} |\n",
                59.0 * ms as f64 / 1000.0,
                59.0 * (ms + 50) as f64 / 1000.0
            ));
        }
    }
    summary.push_str("\n| Traffic / drafter | 8 × 10ms, +50ms compute | 16 × 30ms, +50ms compute | 32 × 30ms, +50ms compute | 60 × 60ms, +50ms compute |\n|---|---:|---:|---:|---:|\n");
    for a in &acceptance {
        let rate = a["tokens_per_pass"].as_f64().unwrap();
        if rate == 0.0 {
            continue;
        }
        summary.push_str(&format!(
            "| {} / {} | {:.3} | {:.3} | {:.3} | {:.3} |\n",
            a["traffic"].as_str().unwrap(),
            a["drafter"].as_str().unwrap(),
            tree::projected_tokens_per_second(rate, 8, 10.0, 50.0)?,
            tree::projected_tokens_per_second(rate, 16, 30.0, 50.0)?,
            tree::projected_tokens_per_second(rate, 32, 30.0, 50.0)?,
            tree::projected_tokens_per_second(rate, 60, 60.0, 50.0)?
        ));
    }
    let result = json!({"schema":"arc.tree-speculation.v1","model_package":digest.to_json(),
        "draft_package":draft_digest.map(|d|d.to_json()),"tokenizer_blake3":blake3::hash(&tokenizer_bytes).to_hex().as_str(),
        "cases_blake3":blake3::hash(&cases_bytes).to_hex().as_str(),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "threads":4,"kernel":"simd when supported, otherwise scalar fallback","depth":8,"max_nodes":32,
        "records":records,"acceptance":acceptance,"projections":projections});
    std::fs::write(
        required("--out")?,
        serde_json::to_string_pretty(&result)? + "\n",
    )?;
    std::fs::write(required("--summary")?, summary)?;
    Ok(())
}
