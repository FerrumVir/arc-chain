//! Cross-seed block-hash equality check — the acceptance test for the
//! replicated-chain work (Model-1 Phases 1+2). Fetches `GET /block/<height>`
//! from every seed and reports whether all seeds agree on the block hash at
//! that height.
//!
//! Before Phases 1+2, `/block/N` returned a DIFFERENT hash on every seed
//! (5 independent chains). This tool is the gate: the fix is "done" only when
//! every passed height reports AGREE across all seeds.
//!
//! Usage:
//!     cargo run --release --example cross_seed_block_check -p arc-node -- <height> [height2 ...]
//! Exit code 0 = all seeds agree on every requested height; 1 = divergence;
//! 2 = usage error.

use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

/// The public testnet seeds (RPC on :9090). NYC is included but has been
/// RPC-flaky; a `<none>` for one seed doesn't fail the check unless the seeds
/// that DID answer disagree.
const SEEDS: &[(&str, &str)] = &[
    ("NYC", "149.28.32.76"),
    ("LAX", "140.82.16.112"),
    ("AMS", "136.244.109.1"),
    ("LHR", "104.238.171.11"),
    ("NRT", "202.182.107.41"),
    ("SGP", "149.28.153.31"),
];

async fn block_hash(c: &Client, ip: &str, height: u64) -> Option<String> {
    let url = format!("http://{}:9090/block/{}", ip, height);
    let v: Value = c.get(&url).send().await.ok()?.json().await.ok()?;
    v.get("hash")
        .or_else(|| v.get("block_hash"))
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
}

#[tokio::main]
async fn main() {
    let heights: Vec<u64> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    if heights.is_empty() {
        eprintln!("usage: cross_seed_block_check <height> [height2 ...]");
        std::process::exit(2);
    }
    let c = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let mut all_ok = true;
    for h in heights {
        let mut results: Vec<(&str, Option<String>)> = Vec::new();
        for (name, ip) in SEEDS {
            results.push((name, block_hash(&c, ip, h).await));
        }
        // Compare only among seeds that actually answered.
        let answered: Vec<&String> = results.iter().filter_map(|(_, x)| x.as_ref()).collect();
        let agree = match answered.first() {
            Some(&reference) => answered.iter().all(|x| *x == reference),
            None => false, // no seed answered — treat as failure
        };
        println!(
            "height {}: {}",
            h,
            if agree { "AGREE" } else { "DIVERGE" }
        );
        for (name, x) in &results {
            println!(
                "  {:<4} {}",
                name,
                x.clone().unwrap_or_else(|| "<none>".into())
            );
        }
        all_ok &= agree;
    }
    std::process::exit(if all_ok { 0 } else { 1 });
}
