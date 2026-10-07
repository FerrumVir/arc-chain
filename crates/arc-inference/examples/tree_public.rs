//! Public sampled ENG-8 evidence. Full target outputs for 60 cases; lookup
//! replay for all; shared-node lookup/recycle/hybrid verification on the first
//! two sampled cases per class. No recycling replay or hidden-head claims.
use arc_inference::modern::{
    arith,
    bpe::ByteLevelBpe,
    chat::{ChatPrompt, render},
    model::GenerationRequest,
    package,
    serving::{
        dense::DenseModel,
        tree::{self, LookupTree, RecycleTree, TreeDrafter},
    },
};
use serde_json::{Value, json};
use std::{io::Write, path::Path, time::Instant};

fn timings(out: &tree::TreeGeneration) -> Value {
    json!({"prefill_seconds":out.prefill_seconds,"decode_seconds":out.decode_seconds,
        "draft_seconds":out.draft_seconds,"verify_seconds":out.verify_seconds})
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let value = |key: &str| -> Result<&str, String> {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
            .ok_or_else(|| format!("missing {key}"))
    };
    rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build_global()?;
    arc_inference::canonical_simd::set_fast_canonical_kernel(true);
    let path = Path::new(value("--package")?);
    let digest = package::digest_file(path)?;
    let model = package::load_package(path)?;
    let dense = DenseModel::new(&model, *blake3::hash(digest.sha256.as_bytes()).as_bytes());
    let tokenizer_bytes = std::fs::read(value("--tokenizer")?)?;
    let tokenizer = ByteLevelBpe::from_json(&tokenizer_bytes)?;
    let cases_bytes = std::fs::read(value("--cases")?)?;
    let cases: Value = serde_json::from_slice(&cases_bytes)?;
    let mut file = std::fs::File::create(value("--out")?)?;
    let mut emit = |entry: Value| -> Result<(), Box<dyn std::error::Error>> {
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        file.flush()?;
        Ok(())
    };
    emit(
        json!({"kind":"metadata","schema":"arc.tree-public-evidence.v1",
        "head":std::env::var("GITHUB_SHA").unwrap_or_default(),
        "pr_head":std::env::var("PR_HEAD_SHA").unwrap_or_default(),
        "package":digest.to_json(),"cases_blake3":blake3::hash(&cases_bytes).to_hex().as_str(),
        "tokenizer_blake3":blake3::hash(&tokenizer_bytes).to_hex().as_str(),
        "os":std::env::consts::OS,"arch":std::env::consts::ARCH,"threads":4,
        "depth":8,"lookup_nodes":32,"recycle_nodes":48,"top_k":8,"hybrid_min_ngram":2,
        "sampling":cases["sampling"],"sources":cases["sources"]}),
    )?;
    let lookup = LookupTree {
        max_nodes: 32,
        ..LookupTree::default()
    };
    for case in cases["cases"].as_array().ok_or("missing cases")? {
        let id = case["id"].as_str().ok_or("missing id")?;
        let text = render(&ChatPrompt {
            system: Some("Be concise. /no_think"),
            user: case["user"].as_str().ok_or("missing user")?,
            thinking: false,
            today: "07 October 2026",
        });
        let prompt = tokenizer.encode(&text)?;
        let max_tokens = case["max_tokens"].as_u64().ok_or("missing max_tokens")? as usize;
        if max_tokens < 128 {
            return Err("sample output allowance below128".into());
        }
        let req = GenerationRequest {
            prompt: &prompt,
            max_tokens,
            eos: &[128012],
            selection: arith::Selection::Argmax,
        };
        // Depth zero is single-row greedy decode with exactly the SAME batched
        // prefill, feature extraction, hashing and observer boundaries.
        let baseline = tree::generate_tree(&dense, &req, &mut { lookup }, 0)?;
        let replay_start = Instant::now();
        let replay = tree::replay_lookup(&prompt, &baseline.tokens, max_tokens, lookup, 8)?;
        let replay_seconds = replay_start.elapsed().as_secs_f64();
        let full = case["full_tree_identity"].as_bool().unwrap_or(false);
        if full {
            // Independent serial reference on the predeclared identity subset.
            // Its total time is NOT compared with batched-prefill generation.
            let reference = model.generate(&req)?;
            if baseline.tokens != reference.tokens
                || baseline.logits_hashes != reference.logits_hashes
            {
                return Err(format!("{id}: batched greedy differs from serial reference").into());
            }
        }
        emit(json!({"kind":"baseline","id":id,"traffic":case["traffic"],
            "source_id":case["source_id"],"full_tree_identity":full,"prompt_tokens":prompt.len(),
            "prompt_token_ids":prompt,"rendered_prompt_blake3":blake3::hash(text.as_bytes()).to_hex().as_str(),
            "max_tokens":max_tokens,"eos":[128012],"output_tokens":baseline.tokens,
            "output_text":tokenizer.decode(&baseline.tokens,false),"output_blake3":hex::encode(arith::tokens_hash(&baseline.tokens)),
            "logits_hashes":baseline.logits_hashes.iter().map(hex::encode).collect::<Vec<_>>(),
            "kv_digest":hex::encode(baseline.kv_digest),"timing":timings(&baseline),
            "decode_tokens":baseline.tokens.len()-1,"verification_passes":baseline.verification_passes,
            "lookup_replay":{"passes":replay.passes,"nodes":replay.nodes,"expanded_rows":replay.expanded_rows,"seconds":replay_seconds}}))?;
        eprintln!(
            "{id}: baseline {} tokens, {:.1}s decode; full={full}",
            baseline.tokens.len(),
            baseline.decode_seconds
        );
        if !full {
            continue;
        }
        let mut drafters: Vec<(&str, Box<dyn TreeDrafter>)> = vec![
            ("lookup", Box::new(lookup)),
            ("recycle", Box::new(RecycleTree::new(8, 48, None))),
            (
                "hybrid",
                Box::new(RecycleTree::new(
                    8,
                    48,
                    Some(LookupTree {
                        min_ngram: 2,
                        ..lookup
                    }),
                )),
            ),
        ];
        for (name, drafter) in &mut drafters {
            let out = tree::generate_tree(&dense, &req, drafter.as_mut(), 8)?;
            if out.tokens != baseline.tokens
                || out.logits_hashes != baseline.logits_hashes
                || out.kv_digest != baseline.kv_digest
            {
                return Err(format!("{id}/{name}: target identity mismatch").into());
            }
            if *name == "lookup"
                && (out.verification_passes != replay.passes
                    || out.verified_rows != replay.nodes
                    || out.expanded_rows != replay.expanded_rows)
            {
                return Err(format!("{id}: lookup replay differs from full verification").into());
            }
            emit(
                json!({"kind":"full_tree","id":id,"traffic":case["traffic"],"drafter":name,
                "decode_tokens":out.tokens.len()-1,"verification_passes":out.verification_passes,
                "physical_rows":out.verified_rows,"logical_nodes":out.logical_nodes,"path_lowered_rows":out.expanded_rows,
                "tokens_logits_kv_identical":true,"timing":timings(&out),
                "output_blake3":hex::encode(arith::tokens_hash(&out.tokens)),"kv_digest":hex::encode(out.kv_digest)}),
            )?;
            eprintln!(
                "{id}/{name}: {} passes; {:.1}s decode; exact",
                out.verification_passes, out.decode_seconds
            );
        }
    }
    emit(json!({"kind":"complete","cases":60,"full_tree_cases":6,"full_tree_comparisons":18}))?;
    Ok(())
}
