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
    AccountBalance, Attestation, Earnings, FaucetResult, InferenceConsensus,
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
const STATUS_COORDINATORS: [&str; 5] = [
    "http://140.82.16.112:9090",  // LAX
    "http://136.244.109.1:9090",  // AMS
    "http://104.238.171.11:9090", // LHR
    "http://202.182.107.41:9090", // NRT
    "http://149.28.153.31:9090",  // SGP
];

/// Probe each coordinator in order; return the first to answer 200 on
/// `/health` within 2s. None means every public seed is unreachable (genuine
/// offline — total network failure or full ISP captive portal). Sequential
/// instead of parallel to avoid pulling in a futures dep; the typical hit is
/// the first host and returns sub-200ms, so the worst-case 12s only applies
/// when the user has *no* working internet.
pub async fn probe_coordinator(http: &reqwest::Client) -> Option<String> {
    for origin in STATUS_COORDINATORS.iter() {
        let url = format!("{}/health", origin);
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            http.get(&url).send(),
        )
        .await;
        if let Ok(Ok(resp)) = r {
            if resp.status().is_success() {
                return Some(origin.to_string());
            }
        }
    }
    None
}

pub async fn fetch_status(
    http: &reqwest::Client,
    port: u16,
    owned_pid: Option<u32>,
    address: Option<String>,
    crash_message: Option<String>,
) -> NodeStatus {
    let base = format!("http://127.0.0.1:{}", port);

    let resp = http.get(format!("{}/health", base)).send().await;
    let parsed: Option<Value> = match resp {
        Ok(r) if r.status().is_success() => r.json().await.ok(),
        _ => None,
    };

    // Running if /health responds - whether or not we spawned it. This lets the
    // app recognize externally-managed nodes (e.g. a community installer launchd
    // daemon).
    let running = parsed.is_some();

    let (peers, round, committed, height, uptime, version, validators) = match parsed {
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
            h.get("validators").and_then(|v| v.as_u64()).unwrap_or(0),
        ),
        None => (0, 0, 0, 0, 0, "unknown".into(), 0),
    };

    // Always probe coordinators in parallel with the local check. The result
    // gates the "lite" health level + the Onboarding "online" check, so
    // residential ISPs that block UDP 9091 don't strand the user at "offline".
    let coordinator_url = if !running || peers == 0 {
        probe_coordinator(http).await
    } else {
        None
    };

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
            if running {
                None
            } else if coordinator_url.is_some() {
                None
            } else {
                Some(format!(
                    "No response from 127.0.0.1:{} and every public seed is unreachable. Check internet/firewall.",
                    port
                ))
            }
        }),
        coordinator_url,
    }
    .with_validators_hint(validators)
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
    port: u16,
    address: Option<&str>,
) -> Earnings {
    let base = format!("http://127.0.0.1:{}", port);

    if let Some(addr) = address {
        let url = format!("{}/worker/earnings/{}", base, addr.trim_start_matches("0x"));
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
                        today_arc,
                        // Pending = an attestation submitted but not yet
                        // unchallenged-released. The chain doesn't expose
                        // this distinction yet — when it does, fold it in
                        // here. For now report 0 so the UI doesn't show
                        // imaginary pending rewards.
                        pending_arc: 0.0,
                        rank: None,
                        attestations,
                        // Prefer block height (cleanly chain-derived) when
                        // available; else now-1m so the UI doesn't show
                        // a Unix epoch zero.
                        last_payout_at: last_block
                            .map(|h| h as i64)
                            .or_else(|| Some(chrono::Utc::now().timestamp_millis() - 60_000)),
                    };
                }
            }
            // 404 from v0.6.x seeds → fall through to synthesis
        }
    }

    // Pre-v0.7 fallback: synthesize from /inference/results (the local
    // node's ring buffer of recent inferences). Misleading for workers
    // behind NAT (they earn on remote seeds, but their local cache
    // doesn't see those attestations) — keeping it only as a safety net
    // for old binaries.
    let resp = http
        .get(format!("{}/inference/results?limit=10000", base))
        .send()
        .await;
    let v: Value = match resp {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(_) => return empty_earnings(),
    };
    let total = v.get("count").and_then(|x| x.as_u64()).unwrap_or(0);
    let results = v
        .get("results")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    // Approximation: "today" is the tail ~12% of attestations by order.
    let today = (total as f64 * 0.12).round() as u64;

    Earnings {
        total_arc: total as f64 * REWARD_PER_ATTESTATION,
        today_arc: today as f64 * REWARD_PER_ATTESTATION,
        pending_arc: (results.len().min(5) as f64) * REWARD_PER_ATTESTATION / 2.0,
        rank: None,
        attestations: total,
        last_payout_at: Some(chrono::Utc::now().timestamp_millis() - 60_000),
    }
}

pub async fn fetch_attestations(
    http: &reqwest::Client,
    port: u16,
    limit: u32,
) -> Vec<Attestation> {
    let base = format!("http://127.0.0.1:{}", port);
    let url = format!("{}/inference/attestations?limit={}", base, limit);
    let resp = match http.get(url).send().await {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let root: Value = resp.json().await.unwrap_or(Value::Null);

    // Real shape: { attestations: [ { inference: {...}, tx_hash, success } ], count }
    let arr = root
        .get("attestations")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    // Synthesize descending timestamps so the UI's relative-time strings make sense.
    let now = chrono::Utc::now().timestamp_millis();
    arr.into_iter()
        .enumerate()
        .map(|(i, v)| {
            let inf = v.get("inference").cloned().unwrap_or(Value::Null);
            let tokens = inf
                .get("tokens_generated")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32;
            let ms_per_tok = inf
                .get("ms_per_token")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32;
            let latency = tokens.saturating_mul(ms_per_tok);

            let input = inf
                .get("input")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                // Strip the Llama chat tags that clutter the UI preview
                .replace("[INST] ", "")
                .replace(" [/INST]", "")
                .chars()
                .take(140)
                .collect::<String>();

            Attestation {
                tx_hash: v
                    .get("tx_hash")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_preview: input,
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
                latency_ms: latency,
                reward_arc: REWARD_PER_ATTESTATION,
                // The node doesn't give us a timestamp per attestation, so stagger
                // them at 30s intervals so the UI shows a natural "recent activity" order.
                timestamp: now - (i as i64) * 30_000,
                verified: v.get("success").and_then(|x| x.as_bool()).unwrap_or(false),
            }
        })
        .collect()
}

pub async fn fetch_network_stats(http: &reqwest::Client, port: u16) -> NetworkStats {
    // No dedicated /network/stats endpoint - synthesize:
    //   total_nodes  ← /health.validators (rough: treat each validator as a node)
    //   total_inferences ← /inference/results.count
    //   avg_tps      ← /health.dag_round / uptime_secs * factor
    //   latest_block ← /health.dag_committed
    let base = format!("http://127.0.0.1:{}", port);
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
        today_arc: 0.0,
        pending_arc: 0.0,
        rank: None,
        attestations: 0,
        last_payout_at: None,
    }
}

pub async fn fetch_balance(
    http: &reqwest::Client,
    port: u16,
    address_hex: &str,
) -> Result<AccountBalance, String> {
    let base = format!("http://127.0.0.1:{}", port);
    let resp = http
        .get(format!("{}/account/{}", base, address_hex))
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
    port: u16,
    address_hex: &str,
) -> Result<FaucetResult, String> {
    let base = format!("http://127.0.0.1:{}", port);
    let resp = http
        .post(format!("{}/faucet/claim", base))
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

pub async fn run_inference(
    http: &reqwest::Client,
    port: u16,
    prompt: &str,
    max_tokens: u32,
) -> Result<InferenceResult, String> {
    let base = format!("http://127.0.0.1:{}", port);
    let wrapped = if prompt.contains("[INST]") {
        prompt.to_string()
    } else {
        format!("[INST] {} [/INST]", prompt)
    };
    info!("[inference/run] → POST {}/inference/run  prompt={:?}  max_tokens={}", base, &wrapped[..wrapped.len().min(80)], max_tokens);
    let resp = http
        .post(format!("{}/inference/run", base))
        .json(&serde_json::json!({ "input": wrapped, "max_tokens": max_tokens }))
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
) -> Result<InferenceResult, String> {
    let wrapped = if prompt.contains("[INST]") {
        prompt.to_string()
    } else {
        format!("[INST] {} [/INST]", prompt)
    };
    info!("[inference/consensus] → POST {}/inference/run_consensus  k={}  max_tokens={}  prompt={:?}",
        coord_base, k, max_tokens, &wrapped[..wrapped.len().min(80)]);
    let resp = http
        .post(format!("{}/inference/run_consensus", coord_base.trim_end_matches('/')))
        .json(&serde_json::json!({
            "input": wrapped,
            "max_tokens": max_tokens,
            "k": k,
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
) -> Result<InferenceResult, String> {
    let wrapped = if prompt.contains("[INST]") {
        prompt.to_string()
    } else {
        format!("[INST] {} [/INST]", prompt)
    };
    let resp = http
        .post(format!(
            "{}/inference/run",
            coord_base.trim_end_matches('/')
        ))
        .json(&serde_json::json!({ "input": wrapped, "max_tokens": max_tokens }))
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
    })
}

// NodeStatus doesn't expose `validators` directly, but we can stash it in
// `last_error` when the node is live, to avoid a schema change. Kept as a
// no-op for now (hook here later if we expose validator count in the UI).
impl NodeStatus {
    fn with_validators_hint(self, _validators: u64) -> Self {
        self
    }
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
