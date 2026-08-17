// Adapter between the ARC node's real HTTP RPC and the shapes our UI expects.
// Real endpoints (verified live at 127.0.0.1:9090):
//   GET  /health                         { status, version, height, peers,
//                                          uptime_secs, dag_round, dag_committed,
//                                          validators }
//   GET  /inference/attestations?limit=N { attestations: [{inference: {input,
//                                          output, output_hash, model_hash,
//                                          tokens_generated, ms_per_token},
//                                          tx_hash, success}], count, chain_height }
//   GET  /inference/results?limit=N      { results: [{input, output, output_hash,
//                                          ms_per_token, tokens_generated,
//                                          tx_hash}], count }
//   GET  /worker/earnings                (not implemented on this node - empty body;
//                                          we synthesize from attestations)

use crate::types::{
    AccountBalance, Attestation, Earnings, FaucetResult, InferenceConsensus, InferenceHop,
    InferenceResult, NetworkStats, NodeStatus,
};
use serde_json::Value;
use tracing::{debug, info, warn};

const REWARD_PER_ATTESTATION: f64 = 2.5; // ARC; matches testnet flat rate

/// Public seed coordinators that mirror `commands.rs::COORDINATOR_HOSTS`.
/// Probed when local P2P can't get peers, so the UI can flip to "lite mode"
/// (HTTPS RPC fallback) instead of showing a hard "offline" — most consumer
/// ISPs silently drop outbound UDP on non-standard ports, which kills our
/// QUIC handshake to seed UDP 9091. Order biases North America first.
const STATUS_COORDINATORS: [&str; 6] = [
    "http://149.28.32.76:9090",   // NYC
    "http://140.82.16.112:9090",  // LAX
    "http://136.244.109.1:9090",  // AMS
    "http://104.238.171.11:9090", // LHR
    "http://202.182.107.41:9090", // NRT
    "http://149.28.153.31:9090",  // SGP
];

/// Return the first coordinator to answer 200 on `/health`, or `None` if
/// every public seed is unreachable (genuine offline — total network failure
/// or a captive portal).
///
/// Probed concurrently. Sequentially, with a 2s timeout per host, the
/// worst case was 12s — inside a poll that repeats every 1.5s, so the polls
/// stacked up on exactly the broken-network path this is meant to detect.
/// Here the whole probe is bounded by the single slowest host.
pub async fn probe_coordinator(http: &reqwest::Client) -> Option<String> {
    let mut set = tokio::task::JoinSet::new();
    for origin in STATUS_COORDINATORS.iter() {
        let http = http.clone();
        let origin = origin.to_string();
        set.spawn(async move {
            let url = format!("{}/health", origin);
            let r = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                http.get(&url).send(),
            )
            .await;
            match r {
                Ok(Ok(resp)) if resp.status().is_success() => Some(origin),
                _ => None,
            }
        });
    }
    // First success wins; the rest are dropped (and cancelled) with the set.
    while let Some(joined) = set.join_next().await {
        if let Ok(Some(origin)) = joined {
            return Some(origin);
        }
    }
    None
}

async fn get_json(http: &reqwest::Client, url: String) -> Option<Value> {
    match http.get(url).send().await {
        Ok(r) if r.status().is_success() => r.json().await.ok(),
        _ => None,
    }
}

/// Build the node status.
///
/// `local_url` is the arc-node on this machine; `chain_url` is the elected
/// public seed. Keeping both is the point: everything describing "your node"
/// comes from `local_url`, and the chain-wide numbers are carried separately
/// so the UI can show network context without dressing it up as the user's
/// own machine.
pub async fn fetch_status(
    http: &reqwest::Client,
    local_url: &str,
    chain_url: &str,
    port: u16,
    owned_pid: Option<u32>,
    address: Option<String>,
    crash_message: Option<String>,
) -> NodeStatus {
    // Independent requests - issue them together.
    let (local, chain) = tokio::join!(
        get_json(http, format!("{}/health", local_url)),
        get_json(http, format!("{}/health", chain_url)),
    );

    // Running if the LOCAL /health responds - whether or not we spawned it.
    // This still recognizes externally-managed nodes (a community installer
    // launchd daemon, or our own child surviving a dev rebuild), which is
    // why `running` is not simply `pid.is_some()`.
    let running = local.is_some();

    let (peers, round, committed, height, uptime, version) = match local {
        Some(ref h) => (
            h.get("peers").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            h.get("dag_round").and_then(|v| v.as_u64()).unwrap_or(0),
            h.get("dag_committed").and_then(|v| v.as_u64()).unwrap_or(0),
            h.get("height").and_then(|v| v.as_u64()).unwrap_or(0),
            h.get("uptime_secs").and_then(|v| v.as_u64()).unwrap_or(0),
            h.get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        ),
        None => (0, 0, 0, 0, 0, "unknown".into()),
    };

    let chain_round = chain
        .as_ref()
        .and_then(|h| h.get("dag_round"))
        .and_then(|v| v.as_u64());

    // Probe the public seeds only when the local node can't carry the user
    // on its own. This gates the "lite" health level and the onboarding
    // online-check, so residential ISPs that drop outbound UDP on 9091 don't
    // strand the user at a hard "offline".
    let coordinator_url = if !running || peers == 0 {
        probe_coordinator(http).await
    } else {
        None
    };

    // These branches are reachable again now that the inputs are local.
    // Previously `running` was true and `peers` was 8 on every poll (they
    // were a datacenter's), so "lite" and "syncing" were dead code and the
    // recovery UI behind them could never render.
    let health_level = if running && peers >= 1 && uptime >= 8 {
        "live"
    } else if coordinator_url.is_some() {
        "lite"
    } else if running {
        "syncing"
    } else {
        "offline"
    };

    NodeStatus {
        running,
        pid: owned_pid,
        health: health_level.into(),
        version,
        peers,
        round,
        committed,
        height,
        uptime_seconds: uptime,
        address,
        rpc_port: port,
        last_error: crash_message.or_else(|| {
            if running || coordinator_url.is_some() {
                None
            } else {
                Some(format!(
                    "No node is answering on {} and every public seed is unreachable. \
                     Start your node, or check your internet connection and firewall.",
                    local_url
                ))
            }
        }),
        coordinator_url,
        // Filled in by the command layer, which owns the host election.
        chain_host: None,
        chain_height: None,
        chain_round,
        chain_block_age_seconds: None,
        worker_threads: None,
        cpu_cores: None,
    }
}

/// Fetch this worker's earnings. v0.7.0+: hits the chain-side
/// `/worker/earnings/:address` endpoint, which counts on-chain
/// InferenceAttestation events (tx 0x16) attributed to this address.
/// Falls back to the pre-v0.7 `/inference/results` synthesis when:
///   - we don't have an address yet (onboarding not finished), or
///   - the node hasn't been upgraded to v0.7.0 (returns 404 on the
///     new route).
///
/// `address` is the user's hex address (with or without `0x` prefix).
pub async fn fetch_earnings(
    http: &reqwest::Client,
    base_url: &str,
    address: Option<&str>,
) -> Earnings {
    if let Some(addr) = address {
        let url = format!("{}/worker/earnings/{}", base_url, addr.trim_start_matches("0x"));
        if let Ok(resp) = http.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(v) = resp.json::<Value>().await {
                    let total_arc = v
                        .get("total_arc")
                        .and_then(|x| x.as_f64())
                        .unwrap_or(0.0);
                    let today_arc = v
                        .get("today_arc")
                        .and_then(|x| x.as_f64())
                        .unwrap_or(0.0);
                    let attestations = v
                        .get("total_attestations")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    let last_block = v
                        .get("last_attestation_block")
                        .and_then(|x| x.as_u64());
                    return Earnings {
                        total_arc,
                        // `today_arc` is a real chain-reported field, so it
                        // is passed through as-is - but only because the
                        // chain sent it. It is never synthesized.
                        today_arc: Some(today_arc),
                        // Pending = submitted but not yet
                        // unchallenged-released. The chain doesn't expose
                        // that distinction yet, so report None ("not
                        // available") rather than a made-up figure.
                        pending_arc: None,
                        rank: None,
                        attestations,
                        // A block height is NOT a timestamp. Feeding
                        // `last_attestation_block` (~123,462) to the UI's
                        // relative-time formatter rendered "20770d ago" -
                        // masked today only because the field is null until
                        // the account actually earns something. Keep the two
                        // concepts in separate fields; emit a timestamp only
                        // if the chain ever sends a real one.
                        last_payout_at: v
                            .get("last_attestation_at")
                            .and_then(|x| x.as_i64()),
                        last_payout_block: last_block,
                        from_chain: true,
                    };
                }
            }
            // 404 from v0.6.x seeds → fall through to synthesis
        }
    }

    // Pre-v0.7 fallback: synthesize from /inference/results (the host's
    // ring buffer of recent inferences). Misleading for workers behind
    // NAT (they earn on remote seeds, but their local cache doesn't see
    // those attestations) — keeping it only as a safety net for old
    // binaries.
    let resp = http
        .get(format!("{}/inference/results?limit=10000", base_url))
        .send()
        .await;
    let v: Value = match resp {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(_) => return empty_earnings(),
    };
    let total = v.get("count").and_then(|x| x.as_u64()).unwrap_or(0);

    // Everything below is an estimate from the host's ring buffer of recent
    // inferences, flagged as such via `from_chain: false`.
    //
    // The invented numbers are gone. "Today" used to be `total * 0.12`
    // rounded — a made-up 12% of lifetime earnings presented in the same
    // typeface as a real balance. "Pending" was
    // `min(results, 5) * 2.5 / 2`, which is not an approximation of anything.
    // Both are None now: the fallback genuinely does not know them, and
    // saying so is better than filling the gap with arithmetic.
    Earnings {
        total_arc: total as f64 * REWARD_PER_ATTESTATION,
        today_arc: None,
        pending_arc: None,
        rank: None,
        attestations: total,
        last_payout_at: None,
        last_payout_block: None,
        from_chain: false,
    }
}

/// Recent attestations, parsed shape-tolerantly and attributed honestly.
///
/// Three separate problems lived here.
///
/// **Shape.** Every field was read out of a nested `inference` object. The
/// deployed seeds return a flat transaction record —
/// `{block_height, from, gas_used, success, tx_hash, tx_type}` — with no such
/// key, so every field collapsed to `""` or `0` while `reward_arc` stayed
/// hardcoded at 2.5. The Dashboard rendered rows with a blank prompt,
/// "0 tokens", "0ms" and a confident "+2.50". Both shapes are accepted now,
/// and absent values stay absent instead of becoming zero.
///
/// **Attribution.** `reward_arc` was 2.5 for every row regardless of who
/// submitted it, so the user's own earnings feed showed other validators'
/// work as their income. A reward is now attached only when `from` matches
/// the user's address.
///
/// **Time.** Timestamps were fabricated as `now - i * 30s`, producing a
/// plausible-looking "34s ago / 1m ago / 2m ago" ladder that was pure
/// invention. Real timestamps are used when present; otherwise `None`, and
/// the UI says "recent".
pub async fn fetch_attestations(
    http: &reqwest::Client,
    base_url: &str,
    limit: u32,
    address: Option<&str>,
) -> Vec<Attestation> {
    let url = format!("{}/inference/attestations?limit={}", base_url, limit);
    let resp = match http.get(url).send().await {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let root: Value = resp.json().await.unwrap_or(Value::Null);

    let arr = root
        .get("attestations")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    // Compare addresses without the `0x` prefix and case-insensitively; the
    // chain returns bare lowercase hex while identities are shown prefixed.
    let want = address.map(|a| a.trim_start_matches("0x").to_ascii_lowercase());

    let mut out: Vec<Attestation> = arr
        .into_iter()
        .filter_map(|v| {
            // Flat records fall through to the record itself, so the same
            // field lookups work against either shape.
            let inf = v.get("inference").cloned().unwrap_or_else(|| v.clone());

            let tx_hash = v
                .get("tx_hash")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            // A record with no transaction hash is not something we can show
            // or link to - drop it rather than rendering an empty row.
            if tx_hash.is_empty() {
                return None;
            }

            let opt_u32 = |o: &Value, k: &str| {
                o.get(k).and_then(|x| x.as_u64()).filter(|n| *n > 0).map(|n| n as u32)
            };
            let tokens = opt_u32(&inf, "tokens_generated");
            let ms_per_tok = opt_u32(&inf, "ms_per_token");
            // Only a real product, never 0 × 0.
            let latency_ms = match (tokens, ms_per_tok) {
                (Some(t), Some(ms)) => Some(t.saturating_mul(ms)),
                _ => opt_u32(&inf, "inference_ms").or_else(|| opt_u32(&inf, "total_ms")),
            };

            let input_preview = inf
                .get("input")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                // Strip Llama chat tags that clutter the preview. Kept for
                // records produced before the client-side wrapping was
                // removed - they are still on-chain.
                .replace("[INST] ", "")
                .replace(" [/INST]", "")
                .chars()
                .take(140)
                .collect::<String>();

            let from = v
                .get("from")
                .and_then(|x| x.as_str())
                .map(|s| s.trim_start_matches("0x").to_ascii_lowercase());
            let mine = match (&want, &from) {
                (Some(w), Some(f)) => w == f,
                _ => false,
            };

            Some(Attestation {
                tx_hash,
                input_preview,
                output_hash: inf
                    .get("output_hash")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                model_hash: inf
                    .get("model_hash")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                tokens,
                latency_ms,
                reward_arc: mine.then_some(REWARD_PER_ATTESTATION),
                timestamp: v
                    .get("timestamp")
                    .or_else(|| inf.get("timestamp"))
                    .and_then(|x| x.as_i64())
                    .filter(|t| *t > 0),
                block_height: v.get("block_height").and_then(|x| x.as_u64()),
                from,
                mine,
                verified: v.get("success").and_then(|x| x.as_bool()).unwrap_or(false),
            })
        })
        .collect();

    // Newest first. Block height is the reliable ordering key on this data -
    // it is present on the flat records where timestamps are not.
    out.sort_by(|a, b| {
        b.block_height
            .unwrap_or(0)
            .cmp(&a.block_height.unwrap_or(0))
            .then(b.timestamp.unwrap_or(0).cmp(&a.timestamp.unwrap_or(0)))
    });
    out
}

pub async fn fetch_network_stats(http: &reqwest::Client, base_url: &str) -> NetworkStats {
    // No dedicated /network/stats endpoint - synthesize:
    //   total_nodes  ← /health.validators (rough: treat each validator as a node)
    //   total_inferences ← /inference/results.count
    //   avg_tps      ← /health.dag_round / uptime_secs * factor
    //   latest_block ← /health.dag_committed
    let base = base_url.to_string();
    let (health_val, results_val) = tokio::join!(
        async {
            http.get(format!("{}/health", base))
                .send()
                .await
                .ok()?
                .json::<Value>()
                .await
                .ok()
        },
        async {
            http.get(format!("{}/inference/results?limit=1", base))
                .send()
                .await
                .ok()?
                .json::<Value>()
                .await
                .ok()
        },
    );

    let total_inf = results_val
        .as_ref()
        .and_then(|v| v.get("count"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    let validators = health_val
        .as_ref()
        .and_then(|v| v.get("validators"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    let dag_round = health_val
        .as_ref()
        .and_then(|v| v.get("dag_round"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    let dag_committed = health_val
        .as_ref()
        .and_then(|v| v.get("dag_committed"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    let uptime_secs = health_val
        .as_ref()
        .and_then(|v| v.get("uptime_secs"))
        .and_then(|x| x.as_u64())
        .unwrap_or(1)
        .max(1);

    // Rough: ~4 tx per round on testnet
    let avg_tps = (dag_round.saturating_mul(4)) / uptime_secs;

    NetworkStats {
        // Show community estimate: validators × a fanout factor, floored to the
        // real number. On production this would come from a real peer-set API.
        total_nodes: validators.max(1),
        total_inferences: total_inf,
        avg_tps,
        latest_block: dag_committed,
    }
}

fn empty_earnings() -> Earnings {
    Earnings {
        total_arc: 0.0,
        today_arc: None,
        pending_arc: None,
        rank: None,
        attestations: 0,
        last_payout_at: None,
        last_payout_block: None,
        from_chain: false,
    }
}

pub async fn fetch_balance(
    http: &reqwest::Client,
    base_url: &str,
    address_hex: &str,
) -> Result<AccountBalance, String> {
    let resp = http
        .get(format!("{}/account/{}", base_url, address_hex))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().as_u16() == 404 {
        // Account not yet seen on-chain = zero balance, zero nonce.
        return Ok(AccountBalance {
            address: address_hex.to_string(),
            balance: 0,
            nonce: 0,
            staked_balance: 0,
        });
    }
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(AccountBalance {
        address: v
            .get("address")
            .and_then(|x| x.as_str())
            .unwrap_or(address_hex)
            .to_string(),
        balance: v.get("balance").and_then(|x| x.as_u64()).unwrap_or(0),
        nonce: v.get("nonce").and_then(|x| x.as_u64()).unwrap_or(0),
        staked_balance: v
            .get("staked_balance")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
    })
}

pub async fn faucet_claim(
    http: &reqwest::Client,
    base_url: &str,
    address_hex: &str,
) -> Result<FaucetResult, String> {
    let resp = http
        .post(format!("{}/faucet/claim", base_url))
        .json(&serde_json::json!({ "address": address_hex }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let err = body
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("claim failed");
        return Err(format!("{} ({})", err, status));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(FaucetResult {
        tx_hash: v
            .get("tx_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        amount: v.get("amount").and_then(|x| x.as_u64()).unwrap_or(0),
        message: v
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Parse the `shard_trace` array a coordinator returns alongside a sharded
/// run, so the UI can show which machines actually ran which layers.
fn parse_trace(v: &Value) -> Option<Vec<InferenceHop>> {
    let arr = v.get("shard_trace")?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    Some(
        arr.iter()
            .map(|h| InferenceHop {
                hop: h.get("hop").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                node: h
                    .get("node")
                    .or_else(|| h.get("node_name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                layers: h
                    .get("layers")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                compute_ms: h.get("compute_ms").and_then(|x| x.as_u64()).unwrap_or(0),
                wall_ms: h.get("wall_ms").and_then(|x| x.as_u64()).unwrap_or(0),
                is_terminal: h
                    .get("is_terminal")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
            })
            .collect(),
    )
}

/// Prompts are sent RAW, with `chat_template` as a flag.
///
/// The client used to wrap every prompt in `[INST] ... [/INST]` unless the
/// user had typed those tags themselves. That is Llama-2's instruction
/// format specifically — wrong for every other architecture, and wrong even
/// for Llama-2 when the node applies its own template from the GGUF metadata
/// (the tags then appear twice in the tokenized input). arc-node accepts
/// `"chat_template": true` and applies the loaded model's own template,
/// which is correct for whatever is actually loaded.
pub async fn run_inference(
    http: &reqwest::Client,
    base_url: &str,
    prompt: &str,
    max_tokens: u32,
    chat_template: bool,
) -> Result<InferenceResult, String> {
    let base = base_url.to_string();
    info!(
        "[inference/run] → POST {}/inference/run  prompt={:?}  max_tokens={}  chat_template={}",
        base,
        &prompt[..prompt.len().min(80)],
        max_tokens,
        chat_template
    );
    let resp = http
        .post(format!("{}/inference/run", base))
        .json(&serde_json::json!({
            "input": prompt,
            "max_tokens": max_tokens,
            "chat_template": chat_template,
        }))
        .send()
        .await
        .map_err(|e| {
            warn!("[inference/run] ✗ request failed: {}", e);
            e.to_string()
        })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!("[inference/run] ✗ HTTP {} — body: {}", status, body);
        return Err(format!("HTTP {}: {}", status, body));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    debug!("[inference/run] ✓ response: {}", serde_json::to_string(&v).unwrap_or_default());
    let inf = v.get("inference").cloned().unwrap_or(Value::Null);
    let att = v.get("attestation").cloned().unwrap_or(Value::Null);
    Ok(InferenceResult {
        input: inf
            .get("input")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        output: inf
            .get("output")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        output_hash: inf
            .get("output_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        model_hash: inf
            .get("model_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        tokens_generated: inf
            .get("tokens_generated")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        inference_ms: inf
            .get("inference_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        tx_hash: att
            .get("tx_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        deterministic: inf
            .get("deterministic")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        engine: inf
            .get("engine")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        explorer_url: v
            .get("explorer_url")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        consensus: None,
        coordinator: None,
        trace: parse_trace(&v),
        served_locally: false,
    })
}

/// Milestone A: fall back to a seed coordinator's `/inference/run_consensus`
/// when the local node cannot serve inference (observer role, no model
/// loaded). `coord_base` is a full origin like `http://149.28.32.76:9090`.
///
/// Caller must pass an http client with a long timeout - consensus
/// inference on the 6-seed pipeline takes 30–60 s for a short prompt.
pub async fn run_inference_consensus(
    http: &reqwest::Client,
    coord_base: &str,
    prompt: &str,
    max_tokens: u32,
    k: u32,
    chat_template: bool,
) -> Result<InferenceResult, String> {
    info!("[inference/consensus] → POST {}/inference/run_consensus  k={}  max_tokens={}  prompt={:?}",
        coord_base, k, max_tokens, &prompt[..prompt.len().min(80)]);
    let resp = http
        .post(format!("{}/inference/run_consensus", coord_base.trim_end_matches('/')))
        .json(&serde_json::json!({
            "input": prompt,
            "max_tokens": max_tokens,
            "k": k,
            "chat_template": chat_template,
        }))
        .send()
        .await
        .map_err(|e| {
            warn!("[inference/consensus] ✗ coordinator {} unreachable: {}", coord_base, e);
            format!("{}: {}", coord_base, e)
        })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!("[inference/consensus] ✗ HTTP {} from {} — body: {}", status, coord_base, body);
        return Err(format!("HTTP {} from {}: {}", status, coord_base, body));
    }
    info!("[inference/consensus] ✓ got response from {}", coord_base);
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    debug!("[inference/consensus] full response: {}", serde_json::to_string(&v).unwrap_or_default());

    let c = v.get("consensus").cloned().unwrap_or(Value::Null);
    let consensus = InferenceConsensus {
        k: c.get("k").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        votes_total: c.get("votes_total").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        unanimous: c.get("unanimous").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        majority: c.get("majority").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        split: c.get("split").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        divergent_replica_count: c
            .get("divergent_replicas")
            .and_then(|x| x.as_object())
            .map(|m| m.len() as u32)
            .unwrap_or(0),
    };

    Ok(InferenceResult {
        input: v
            .get("input")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        output: v
            .get("output")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        output_hash: v
            .get("output_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        // run_consensus doesn't return model_hash in its body; callers can
        // resolve it from /shards if needed. Empty keeps the UI from
        // showing a stale one from the prior response.
        model_hash: String::new(),
        tokens_generated: v
            .get("tokens_generated")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        // run_consensus reports `total_ms` (wall time across the whole
        // pipeline × every token). Use it as `inference_ms` so the UI's
        // "Xms" label still works.
        inference_ms: v
            .get("total_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        tx_hash: String::new(),
        // Consensus path is deterministic by construction: majority hash
        // required at every hop.
        deterministic: true,
        engine: "consensus".into(),
        explorer_url: String::new(),
        consensus: Some(consensus),
        coordinator: Some(coord_base.to_string()),
        trace: parse_trace(&v),
        served_locally: false,
    })
}

/// Single-node inference on a remote coordinator. Same `/inference/run` route
/// as the local node but at an arbitrary base URL. Used as a fallback when
/// `/inference/run_consensus` fails with `Pipeline gap` because the shard
/// registry still references retired or overlapping shards. Loses k-of-n
/// consensus, but still produces a deterministic output and an on-chain
/// attestation from the coordinator that served it.
pub async fn run_inference_remote(
    http: &reqwest::Client,
    coord_base: &str,
    prompt: &str,
    max_tokens: u32,
    chat_template: bool,
) -> Result<InferenceResult, String> {
    let resp = http
        .post(format!(
            "{}/inference/run",
            coord_base.trim_end_matches('/')
        ))
        .json(&serde_json::json!({
            "input": prompt,
            "max_tokens": max_tokens,
            "chat_template": chat_template,
        }))
        .send()
        .await
        .map_err(|e| format!("{}: {}", coord_base, e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} from {}", resp.status(), coord_base));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    let inf = v.get("inference").cloned().unwrap_or(Value::Null);
    let att = v.get("attestation").cloned().unwrap_or(Value::Null);
    Ok(InferenceResult {
        input: inf
            .get("input")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        output: inf
            .get("output")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        output_hash: inf
            .get("output_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        model_hash: inf
            .get("model_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        tokens_generated: inf
            .get("tokens_generated")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        inference_ms: inf
            .get("inference_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        tx_hash: att
            .get("tx_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        deterministic: inf
            .get("deterministic")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        engine: inf
            .get("engine")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        explorer_url: v
            .get("explorer_url")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        consensus: None,
        coordinator: Some(coord_base.to_string()),
        trace: parse_trace(&v),
        served_locally: false,
    })
}

// ── Tier 1 on-chain inference (VRF committee voting) ───────────────────────
// See `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md`.
//
// The caller picks which seed VPS to talk to (see commands.rs::pick_tier1_host).
// Each seed runs its own chain instance with a different anchor_height, so the
// submit/result pair MUST stick to the same host or the poll will 404. The
// command layer stores request_id → host in `AppState::tier1_routes` to enforce
// this.

/// Submit an `InferenceRequest` tx via the chosen seed's convenience endpoint
/// (`/inference/onchain/submit`). The seed signs with its validator keypair on
/// the user's behalf. Returns the request_id which the UI then polls via
/// `tier1_result` against the SAME `base_url`.
/// Currently unused: `commands::tier1_submit` builds and signs the
/// `InferenceRequest` locally so the tx is attributed to the user rather than
/// to the seed's validator key, and posts it to `/tx/submit_signed` instead.
/// Kept because the seed-signed convenience route is still the fallback if
/// local signing has to be dropped.
#[allow(dead_code)]
pub async fn tier1_submit(
    http: &reqwest::Client,
    base_url: &str,
    prompt: &str,
    max_tokens: u32,
    max_reward: u64,
    deadline_blocks: u64,
    committee_size: u8,
) -> Result<Tier1Submitted, String> {
    let resp = http
        .post(format!("{}/inference/onchain/submit", base_url))
        .json(&serde_json::json!({
            "input": prompt,
            "max_tokens": max_tokens,
            "max_reward": max_reward,
            "deadline_blocks": deadline_blocks,
            "committee_size": committee_size,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} from /inference/onchain/submit", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(Tier1Submitted {
        request_id: v
            .get("request_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        tx_hash: v
            .get("tx_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        anchor_height: v.get("anchor_height").and_then(|x| x.as_u64()).unwrap_or(0),
        committee_size: v
            .get("committee_size")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u8,
        deadline_blocks: v.get("deadline_blocks").and_then(|x| x.as_u64()).unwrap_or(0),
        max_reward: v.get("max_reward").and_then(|x| x.as_u64()).unwrap_or(0),
    })
}

/// Poll the on-chain state of a Tier 1 request. Returns the current
/// status, the votes seen so far, and (once finalized) the agreed
/// `output_hash` + `output_blob`.
pub async fn tier1_result(
    http: &reqwest::Client,
    base_url: &str,
    request_id: &str,
) -> Result<Tier1Result, String> {
    let resp = http
        .get(format!(
            "{}/inference/onchain/result/{}",
            base_url, request_id
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} from /inference/onchain/result", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    let votes_json = v.get("votes").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let votes: Vec<Tier1Vote> = votes_json
        .into_iter()
        .map(|vj| Tier1Vote {
            voter: vj.get("voter").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            output_hash: vj.get("output_hash").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        })
        .collect();
    Ok(Tier1Result {
        request_id: v.get("request_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        status: v.get("status").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        vote_count: v.get("vote_count").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        committee_size: v.get("committee_size").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
        anchor_height: v.get("anchor_height").and_then(|x| x.as_u64()).unwrap_or(0),
        deadline_blocks: v.get("deadline_blocks").and_then(|x| x.as_u64()).unwrap_or(0),
        votes,
        output_hash: v.get("output_hash").and_then(|x| x.as_str()).map(String::from),
        output_blob: v.get("output_blob").and_then(|x| x.as_str()).map(String::from),
        output_text: v.get("output_text").and_then(|x| x.as_str()).map(String::from),
        max_reward: v.get("max_reward").and_then(|x| x.as_u64()).unwrap_or(0),
    })
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tier1Submitted {
    pub request_id: String,
    pub tx_hash: String,
    pub anchor_height: u64,
    pub committee_size: u8,
    pub deadline_blocks: u64,
    pub max_reward: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tier1Vote {
    pub voter: String,
    pub output_hash: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tier1Result {
    pub request_id: String,
    pub status: String,
    pub vote_count: u32,
    pub committee_size: u8,
    pub anchor_height: u64,
    pub deadline_blocks: u64,
    pub votes: Vec<Tier1Vote>,
    pub output_hash: Option<String>,
    pub output_blob: Option<String>,
    pub output_text: Option<String>,
    pub max_reward: u64,
}
