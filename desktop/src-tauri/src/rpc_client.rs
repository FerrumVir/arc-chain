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
//   GET  /worker/earnings/:address       (candidate: successful mined 0x25
//                                          receipt index + readiness fields)

use crate::types::{
    AccountBalance, Attestation, BlockSummary, ConfirmedRewardReceipt, Earnings,
    EarningsProjection, FaucetResult, InferenceConsensus, InferenceHop, InferenceResult,
    InferenceSettlement, NetworkOverview, NetworkStats, NodeContribution, NodeStatus, RecentBlocks,
    RewardEconomics, TxLookup, ValidatorInfo, WalletTxResult,
};
use arc_crypto::Hash256;
use arc_types::Transaction;
use serde_json::Value;
use tracing::{debug, info, warn};

/// Public seed coordinators that mirror `commands.rs::COORDINATOR_HOSTS`.
/// Probed when local P2P can't get peers, so the UI can flip to "lite mode"
/// (HTTPS RPC fallback) instead of showing a hard "offline" — most consumer
/// ISPs silently drop outbound UDP on non-standard ports, which kills our
/// QUIC handshake to seed UDP 9091. Order biases North America first.
pub(crate) const PRODUCTION_RPC_ORIGINS: [&str; 6] = [
    "https://149.28.32.76",   // NYC
    "https://140.82.16.112",  // LAX
    "https://136.244.109.1",  // AMS
    "https://104.238.171.11", // LHR
    "https://202.182.107.41", // NRT
    "https://149.28.153.31",  // SGP
];
const STATUS_COORDINATORS: [&str; 6] = PRODUCTION_RPC_ORIGINS;

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
            let r = tokio::time::timeout(std::time::Duration::from_secs(2), http.get(&url).send())
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

/// Parse only the v0.8.0 mined-community-reward receipt contract.
///
/// Public v2 also answers `/worker/earnings/:address` with HTTP 200, but that
/// older body is raw-0x16 count × constant display arithmetic. HTTP success is
/// therefore not a semantics/version signal. The candidate contract is
/// identified by its explicit receipt count, retained-window note, last-reward
/// evidence fields, and all three rollout/readiness booleans. Any missing,
/// ambiguous, or internally inconsistent field fails closed.
const RETAINED_EARNINGS_SOURCE: &str = "scan of this node's in-memory full_transactions map";

fn confirmed_earnings_from_value(v: &Value) -> Option<Earnings> {
    let normalized_hash = |value: &Value| {
        let bare = strip_0x(value.as_str()?);
        is_tx_hash(&bare).then(|| format!("0x{bare}"))
    };
    let total_rewards = v.get("total_rewards")?.as_u64()?;
    let confirmed_count = v.get("confirmed_receipt_count")?.as_u64()?;
    if confirmed_count != total_rewards {
        return None;
    }
    let total_arc = v.get("confirmed_gross_earnings_arc")?.as_f64()?;
    if total_arc < 0.0 {
        return None;
    }
    // The backend cannot yet join every retained reward receipt to a wall
    // clock day. Accepting a numeric `today_arc` would reintroduce an
    // estimate that is not mined-receipt evidence.
    if !v.get("today_arc")?.is_null() {
        return None;
    }
    let note = v.get("estimated_total_arc_note")?.as_str()?;
    if !note.contains("CommunityInferenceReward") {
        return None;
    }
    let receipt_source = v.get("source")?.as_str()?;
    if receipt_source != RETAINED_EARNINGS_SOURCE {
        return None;
    }
    let archive_mode = v.get("archive_mode")?.as_bool()?;

    let effective = v.get("community_rewards_v1_enabled")?.as_bool()?;
    let protocol_active = v.get("community_rewards_v1_protocol_active")?.as_bool()?;
    let approval_ready = v
        .get("community_rewards_v1_approval_collection_ready")?
        .as_bool()?;
    if effective && !(protocol_active && approval_ready) {
        return None;
    }

    let receipt_values = v.get("confirmed_receipts")?.as_array()?;
    if receipt_values.len() != usize::try_from(total_rewards).ok()? {
        return None;
    }
    let mut confirmed_receipts = Vec::with_capacity(receipt_values.len());
    let mut receipt_base_sum = 0u64;
    let mut receipt_tx_hashes = std::collections::HashSet::new();
    let mut receipt_job_ids = std::collections::HashSet::new();
    for receipt in receipt_values {
        if receipt.get("tx_type")?.as_str()? != "0x25" || !receipt.get("success")?.as_bool()? {
            return None;
        }
        let reward_arc = receipt.get("reward_arc")?.as_f64()?;
        if !reward_arc.is_finite() || (reward_arc - 2.5).abs() > f64::EPSILON {
            return None;
        }
        let reward_base = receipt.get("reward_base")?.as_u64()?;
        if reward_base != arc_types::economics::INFERENCE_ATTESTATION_REWARD {
            return None;
        }
        receipt_base_sum = receipt_base_sum.checked_add(reward_base)?;
        let tx_hash = normalized_hash(receipt.get("tx_hash")?)?;
        let job_id = normalized_hash(receipt.get("job_id")?)?;
        let block_hash = normalized_hash(receipt.get("block_hash")?)?;
        if !receipt_tx_hashes.insert(tx_hash.clone()) || !receipt_job_ids.insert(job_id.clone()) {
            return None;
        }
        confirmed_receipts.push(ConfirmedRewardReceipt {
            tx_hash,
            job_id,
            block_height: receipt.get("block_height")?.as_u64()?,
            block_hash,
            reward_base,
            reward_arc,
            recovery_epoch: receipt.get("recovery_epoch").and_then(Value::as_u64),
            validator_set_id: receipt.get("validator_set_id").and_then(Value::as_u64),
        });
    }
    if receipt_base_sum != v.get("confirmed_gross_earnings_base")?.as_u64()? {
        return None;
    }
    let receipt_sum: f64 = confirmed_receipts
        .iter()
        .map(|receipt| receipt.reward_arc)
        .sum();
    if (receipt_sum - total_arc).abs() > 1e-9 {
        return None;
    }

    let host_projected_daily_arc = match v.get("projected_daily_arc")? {
        Value::Null => None,
        value => {
            let projected = value.as_f64()?;
            if !projected.is_finite() || projected < 0.0 {
                return None;
            }
            Some(projected)
        }
    };
    let host_projection_reason = match v.get("projected_daily_unavailable_reason")? {
        Value::Null => None,
        value => Some(value.as_str()?.trim().to_string()).filter(|reason| !reason.is_empty()),
    };
    // Exactly one side of the projection contract is populated: either a
    // receipt-derived forecast or a concrete explanation for its absence.
    if host_projected_daily_arc.is_some() == host_projection_reason.is_some() {
        return None;
    }
    // The host's numeric field is only a candidate. Exact, unique successful
    // mined 0x25 rows above establish the observation count used by the local
    // minimum-history gate; a raw count alone never unlocks a projection.
    let observation_gate = projection_observation_gate(
        v,
        u64::try_from(confirmed_receipts.len()).ok()?,
    );
    let projected_daily_arc = observation_gate
        .is_none()
        .then_some(host_projected_daily_arc)
        .flatten();
    let projected_daily_unavailable_reason = observation_gate.or(host_projection_reason);

    // Both keys are part of the candidate contract even when their value is
    // null because no successful receipt exists yet.
    let last_reward_block_value = v.get("last_reward_block")?;
    let last_reward_hash_value = v.get("last_reward_tx_hash")?;
    let last_reward_block = last_reward_block_value.as_u64();
    let last_reward_hash = normalized_hash(last_reward_hash_value);
    if total_rewards > 0 && (last_reward_block.is_none() || last_reward_hash.is_none()) {
        return None;
    }
    if total_rewards > 0
        && !confirmed_receipts.iter().any(|receipt| {
            Some(receipt.block_height) == last_reward_block
                && Some(receipt.tx_hash.as_str()) == last_reward_hash.as_deref()
        })
    {
        return None;
    }

    Some(Earnings {
        total_arc,
        // Null means unavailable, not zero. The candidate intentionally does
        // not invent a daily split from a lifetime receipt count.
        today_arc: None,
        pending_arc: None,
        rank: None,
        attestations: total_rewards,
        last_payout_at: v.get("last_reward_at").and_then(Value::as_i64),
        last_payout_block: last_reward_block,
        confirmed_receipts,
        projected_daily_arc,
        projected_daily_unavailable_reason,
        recovery_epoch: v.get("recovery_epoch").and_then(Value::as_u64),
        validator_set_id: v.get("validator_set_id").and_then(Value::as_u64),
        unavailable_reason: None,
        receipt_source: Some(receipt_source.to_string()),
        archive_mode: Some(archive_mode),
        from_chain: true,
    })
}

/// Fetch this worker's confirmed community rewards.
///
/// Only the v0.8.0 receipt/readiness response above can produce numeric
/// earnings. Older endpoints and inference caches are deliberately unavailable
/// rather than converted into ARC.
///
/// `address` is the user's hex address (with or without `0x` prefix).
pub async fn fetch_earnings(
    http: &reqwest::Client,
    base_url: &str,
    address: Option<&str>,
) -> Earnings {
    let Some(addr) = address.filter(|addr| !addr.trim().is_empty()) else {
        return unavailable_earnings(
            "No identity exists on this device yet, so retained reward receipts cannot be queried.",
        );
    };
    let path = format!("/worker/earnings/{}", addr.trim_start_matches("0x"));
    let fetched = get_detailed(http, &format!("{}{}", base_url, path)).await;
    earnings_from_fetched(base_url, &path, fetched)
}

/// Recent attestations, parsed shape-tolerantly and attributed honestly.
///
/// Three separate problems lived here.
///
/// **Shape.** Every field was read out of a nested `inference` object. The
/// deployed seeds return a flat transaction record —
/// `{block_height, from, gas_used, success, tx_hash, tx_type}` — with no such
/// key, so every field collapsed to `""` or `0` while the old adapter still
/// attached a hardcoded +2.5. The Dashboard rendered rows with a blank prompt,
/// "0 tokens", "0ms" and a confident "+2.50". Both shapes are accepted now,
/// and absent values stay absent instead of becoming zero.
///
/// **Payment.** Protocol-v3 activity rows distinguish a mined computation-only
/// `0x16` claim from the authoritative `0x25` community reward receipt. Worker
/// ownership comes from the reward body's `worker`, never its coordinator
/// `from`; failed, pending, local-only, or unreceipted rows are excluded.
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

    attestations_from_value(&root, address)
}

fn attestations_from_value(root: &Value, address: Option<&str>) -> Vec<Attestation> {
    let activity_rows = root.get("activities").and_then(|x| x.as_array());
    let strict_activity = activity_rows.is_some();
    let arr = activity_rows
        .or_else(|| root.get("attestations").and_then(|x| x.as_array()))
        .cloned()
        .unwrap_or_default();

    // Compare addresses without the `0x` prefix and case-insensitively; the
    // chain returns bare lowercase hex while identities are shown prefixed.
    let want = address.map(|a| a.trim_start_matches("0x").to_ascii_lowercase());

    let mut out: Vec<Attestation> = arr
        .into_iter()
        .filter_map(|v| {
            if strict_activity {
                let common = v.get("schema").and_then(Value::as_str)
                    == Some("arc.inference.activity.v1")
                    && v.get("source").and_then(Value::as_str) == Some("chain_receipt")
                    && v.get("mined").and_then(Value::as_bool) == Some(true)
                    && v.get("success").and_then(Value::as_bool) == Some(true)
                    && v.get("computed").and_then(Value::as_bool) == Some(true);
                let typed = match v.get("record_kind").and_then(Value::as_str) {
                    Some("mined_community_inference_reward") => {
                        v.get("tx_type").and_then(Value::as_str) == Some("CommunityInferenceReward")
                            && v.get("tx_type_code").and_then(Value::as_str) == Some("0x25")
                            && v.get("paid").and_then(Value::as_bool) == Some(true)
                            && v.get("earned").and_then(Value::as_bool) == Some(true)
                    }
                    Some("mined_inference_attestation") => {
                        v.get("tx_type").and_then(Value::as_str) == Some("InferenceAttestation")
                            && v.get("tx_type_code").and_then(Value::as_str) == Some("0x16")
                            && v.get("paid").and_then(Value::as_bool) == Some(false)
                            && v.get("earned").and_then(Value::as_bool) == Some(false)
                    }
                    _ => false,
                };
                if !(common && typed) {
                    return None;
                }
            }
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
                o.get(k)
                    .and_then(|x| x.as_u64())
                    .filter(|n| *n > 0)
                    .map(|n| n as u32)
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
                .get("worker")
                .or_else(|| v.get("from"))
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
                timestamp: v
                    .get("timestamp")
                    .or_else(|| inf.get("timestamp"))
                    .and_then(|x| x.as_i64())
                    .filter(|t| *t > 0),
                block_height: v.get("block_height").and_then(|x| x.as_u64()),
                tx_type: v
                    .get("tx_type")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
                record_kind: v
                    .get("record_kind")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                computed: if strict_activity {
                    v.get("computed").and_then(Value::as_bool) == Some(true)
                } else {
                    v.get("success").and_then(Value::as_bool).unwrap_or(false)
                },
                paid: strict_activity && v.get("paid").and_then(Value::as_bool) == Some(true),
                earned: strict_activity && v.get("earned").and_then(Value::as_bool) == Some(true),
                from,
                mine,
                verified: if strict_activity {
                    v.get("mined").and_then(Value::as_bool) == Some(true)
                        && v.get("success").and_then(Value::as_bool) == Some(true)
                } else {
                    v.get("success").and_then(|x| x.as_bool()).unwrap_or(false)
                },
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

fn unavailable_earnings(reason: impl Into<String>) -> Earnings {
    Earnings {
        total_arc: 0.0,
        today_arc: None,
        pending_arc: None,
        rank: None,
        attestations: 0,
        last_payout_at: None,
        last_payout_block: None,
        confirmed_receipts: Vec::new(),
        projected_daily_arc: None,
        projected_daily_unavailable_reason: Some(
            "confirmed mined reward receipts are unavailable".to_string(),
        ),
        recovery_epoch: None,
        validator_set_id: None,
        unavailable_reason: Some(reason.into()),
        receipt_source: None,
        archive_mode: None,
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
            balance_base: "0".to_string(),
            balance_arc: "0".to_string(),
            nonce: 0,
            staked_balance_base: "0".to_string(),
            staked_balance_arc: "0".to_string(),
        });
    }
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    let balance = v
        .get("balance")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| "account response is missing an exact u64 balance".to_string())?;
    let staked_balance = v
        .get("staked_balance")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| "account response is missing an exact u64 staked_balance".to_string())?;
    let nonce = v
        .get("nonce")
        .and_then(Value::as_u64)
        .ok_or_else(|| "account response is missing an exact u64 nonce".to_string())?;
    let response_address = v
        .get("address")
        .and_then(Value::as_str)
        .unwrap_or(address_hex);
    if strip_0x(response_address) != strip_0x(address_hex) {
        return Err("account response address does not match the requested wallet".to_string());
    }
    Ok(AccountBalance {
        address: strip_0x(response_address),
        balance_base: balance.to_string(),
        balance_arc: crate::wallet::format_arc_amount(balance),
        nonce,
        staked_balance_base: staked_balance.to_string(),
        staked_balance_arc: crate::wallet::format_arc_amount(staked_balance),
    })
}

/// Fetch the transaction-signing domain from the same pinned host that will
/// receive the signed transaction. Missing or inconsistent recovery metadata
/// fails closed: signing for the wrong domain weakens replay protection.
pub async fn transaction_signing_domain(
    http: &reqwest::Client,
    base_url: &str,
) -> Result<Option<Hash256>, String> {
    let resp = http
        .get(format!("{}/network/info", base_url))
        .send()
        .await
        .map_err(|e| format!("could not read transaction domain from {base_url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "{} cannot establish the chain transaction domain (GET /network/info returned HTTP {})",
            base_url,
            resp.status()
        ));
    }
    let value: Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid /network/info response from {base_url}: {e}"))?;
    transaction_signing_domain_from_value(&value, base_url)
}

fn transaction_signing_domain_from_value(
    value: &Value,
    base_url: &str,
) -> Result<Option<Hash256>, String> {
    let active = value
        .get("recovery_active")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            format!("{base_url} did not state whether recovery-domain signing is active")
        })?;
    let raw_domain = value
        .get("recovery_domain")
        .or_else(|| value.get("transaction_domain"))
        .and_then(Value::as_str);

    if !active {
        if raw_domain.is_some() {
            return Err(format!(
                "{base_url} returned a transaction domain while recovery_active=false"
            ));
        }
        if value
            .get("protocol_version")
            .and_then(Value::as_str)
            .is_some_and(|version| version.starts_with("3."))
        {
            return Err(format!(
                "{base_url} reports protocol v3 without an active recovery transaction domain"
            ));
        }
        return Ok(None);
    }

    let raw_domain = raw_domain
        .ok_or_else(|| format!("{base_url} reports recovery_active=true but no recovery_domain"))?;
    let clean = raw_domain.strip_prefix("0x").unwrap_or(raw_domain);
    let domain = Hash256::from_hex(clean)
        .map_err(|_| format!("{base_url} returned a malformed 32-byte recovery_domain"))?;
    if domain == Hash256::ZERO {
        return Err(format!("{base_url} returned an all-zero recovery_domain"));
    }
    Ok(Some(domain))
}

fn wallet_result_from_lookup(
    tx_hash: String,
    amount_base: u64,
    lookup: TxLookup,
    flow: &str,
) -> WalletTxResult {
    let (receipt_status, mined, unavailable, message) =
        match (lookup.status.as_str(), lookup.success) {
            ("mined", Some(true)) => (
                "mined_success",
                true,
                None,
                lookup
                    .block_height
                    .map(|height| format!("{flow} confirmed in block #{height}."))
                    .unwrap_or_else(|| format!("{flow} has a successful mined receipt.")),
            ),
            ("mined", Some(false)) => (
                "mined_failed",
                true,
                None,
                lookup
                    .block_height
                    .map(|height| format!("{flow} was mined but failed in block #{height}."))
                    .unwrap_or_else(|| format!("{flow} was mined but execution failed.")),
            ),
            ("not_found", _) => (
                "pending",
                false,
                None,
                format!("{flow} was accepted and is waiting for a mined receipt."),
            ),
            ("mined", None) => (
                "receipt_unavailable",
                true,
                Some("the mined receipt did not include an execution result".to_string()),
                format!("{flow} is in a block, but its success result is unavailable."),
            ),
            _ => (
                "receipt_unavailable",
                false,
                lookup.unavailable.clone().or_else(|| {
                    Some("the receipt lookup could not establish transaction status".to_string())
                }),
                format!("{flow} was accepted, but its mined receipt could not be verified yet."),
            ),
        };

    WalletTxResult {
        tx_hash,
        amount_base: amount_base.to_string(),
        amount_arc: crate::wallet::format_arc_amount(amount_base),
        receipt_status: receipt_status.to_string(),
        mined,
        success: lookup.success,
        block_height: lookup.block_height,
        block_hash: lookup.block_hash,
        source_host: lookup.source_host,
        unavailable,
        message,
    }
}

pub async fn submit_signed_transfer(
    http: &reqwest::Client,
    base_url: &str,
    tx: &Transaction,
    amount_base: u64,
) -> Result<WalletTxResult, String> {
    let local_hash = tx.hash.to_hex();
    let resp = http
        .post(format!("{}/tx/submit_signed", base_url))
        .json(tx)
        .send()
        .await
        .map_err(|e| format!("transfer submission failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "transfer was rejected by {} (HTTP {}): {}",
            base_url,
            status,
            body.trim()
        ));
    }
    let value: Value = serde_json::from_str(&body)
        .map_err(|e| format!("invalid transfer submission response from {base_url}: {e}"))?;
    if value.get("status").and_then(Value::as_str) != Some("pending") {
        return Err(format!(
            "{} did not acknowledge the signed transfer as pending",
            base_url
        ));
    }
    let remote_hash = value
        .get("tx_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{base_url} omitted tx_hash from the submission response"))?;
    if strip_0x(remote_hash) != local_hash {
        return Err(format!(
            "{} acknowledged a different transaction hash; refusing to track it",
            base_url
        ));
    }

    let lookup = lookup_tx(http, base_url, &local_hash).await;
    Ok(wallet_result_from_lookup(
        local_hash,
        amount_base,
        lookup,
        "Transfer",
    ))
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
    let tx_hash = v
        .get("tx_hash")
        .and_then(Value::as_str)
        .map(strip_0x)
        .filter(|hash| is_tx_hash(hash))
        .ok_or_else(|| {
            "faucet accepted the request but returned no valid transaction hash".to_string()
        })?;
    let amount = v
        .get("amount")
        .and_then(Value::as_u64)
        .ok_or_else(|| "faucet response omitted its exact base-unit amount".to_string())?;
    if v.get("status").and_then(Value::as_str) != Some("pending") {
        return Err("faucet did not acknowledge the claim as pending".to_string());
    }
    let lookup = lookup_tx(http, base_url, &tx_hash).await;
    Ok(wallet_result_from_lookup(
        tx_hash,
        amount,
        lookup,
        "Faucet claim",
    ))
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

fn parse_inference_settlement(v: &Value) -> Option<InferenceSettlement> {
    let settlement = v.get("settlement")?.as_object()?;
    let status = settlement.get("status")?.as_str()?.trim();
    if status.is_empty() {
        return None;
    }
    let reward_arc = settlement
        .get("reward_arc")
        .and_then(Value::as_f64)
        .filter(|amount| amount.is_finite() && *amount >= 0.0);
    Some(InferenceSettlement {
        status: status.to_string(),
        tx_type: settlement
            .get("tx_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        tx_hash: settlement
            .get("tx_hash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        job_id: settlement
            .get("job_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        submitted: settlement
            .get("submitted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        included: settlement
            .get("included")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        confirmed: settlement
            .get("confirmed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        reward_arc,
        receipt_url: settlement
            .get("receipt_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Parse the common `/inference/run` envelope. Community verification and
/// reward settlement live at the top level, while local inference evidence
/// lives under `inference`; preserving that distinction prevents a verified
/// community result from being displayed as unverified and prevents a pending
/// 0x25 reward from being mislabeled as an unpaid 0x16 claim.
fn parse_inference_run_value(
    v: &Value,
    coordinator: Option<&str>,
    served_locally: bool,
) -> Result<InferenceResult, String> {
    if v.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("inference endpoint reported success=false")
            .to_string());
    }
    let inf = v
        .get("inference")
        .and_then(Value::as_object)
        // `/inference/run` delegates partial-model seeds to run_sharded,
        // whose successful response is flat rather than nested.
        .or_else(|| {
            v.get("output")
                .and_then(Value::as_str)
                .and_then(|_| v.as_object())
        })
        .ok_or_else(|| "inference response omitted inference output".to_string())?;
    let att = v.get("attestation").and_then(Value::as_object);
    let verification = v.get("verification").and_then(Value::as_object);

    let profile_bound = verification
        .and_then(|value| value.get("profile_bound"))
        .and_then(Value::as_bool)
        .or_else(|| inf.get("profile_bound").and_then(Value::as_bool))
        .unwrap_or(false);
    let quorum_verified = verification
        .and_then(|value| value.get("quorum_verified"))
        .and_then(Value::as_bool)
        .or_else(|| inf.get("quorum_verified").and_then(Value::as_bool))
        .unwrap_or(false);
    let execution_profile = verification
        .and_then(|value| value.get("execution_profile"))
        .and_then(Value::as_str)
        .or_else(|| inf.get("execution_profile").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();

    Ok(InferenceResult {
        input: inf
            .get("input")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        output: inf
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        output_hash: inf
            .get("output_hash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        model_hash: inf
            .get("model_hash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        tokens_generated: inf
            .get("tokens_generated")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        inference_ms: inf
            .get("inference_ms")
            .and_then(Value::as_u64)
            .or_else(|| inf.get("total_ms").and_then(Value::as_u64))
            .unwrap_or(0) as u32,
        tx_hash: att
            .and_then(|value| value.get("tx_hash"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        deterministic: inf
            .get("deterministic")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        profile_bound,
        quorum_verified,
        execution_profile,
        engine: inf
            .get("engine")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        explorer_url: v
            .get("explorer_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        routed_via: v
            .get("routed_via")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        settlement: parse_inference_settlement(v),
        consensus: None,
        coordinator: coordinator.map(ToString::to_string),
        trace: parse_trace(v),
        served_locally,
    })
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
    debug!(
        "[inference/run] ✓ response: {}",
        serde_json::to_string(&v).unwrap_or_default()
    );
    parse_inference_run_value(&v, None, false)
}

/// Milestone A: fall back to a seed coordinator's `/inference/run_consensus`
/// when the local node cannot serve inference (observer role, no model
/// loaded). `coord_base` is a full origin like `https://149.28.32.76`.
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
    info!(
        "[inference/consensus] → POST {}/inference/run_consensus  k={}  max_tokens={}  prompt={:?}",
        coord_base,
        k,
        max_tokens,
        &prompt[..prompt.len().min(80)]
    );
    let resp = http
        .post(format!(
            "{}/inference/run_consensus",
            coord_base.trim_end_matches('/')
        ))
        .json(&serde_json::json!({
            "input": prompt,
            "max_tokens": max_tokens,
            "k": k,
            "chat_template": chat_template,
        }))
        .send()
        .await
        .map_err(|e| {
            warn!(
                "[inference/consensus] ✗ coordinator {} unreachable: {}",
                coord_base, e
            );
            format!("{}: {}", coord_base, e)
        })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!(
            "[inference/consensus] ✗ HTTP {} from {} — body: {}",
            status, coord_base, body
        );
        return Err(format!("HTTP {} from {}: {}", status, coord_base, body));
    }
    info!("[inference/consensus] ✓ got response from {}", coord_base);
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    debug!(
        "[inference/consensus] full response: {}",
        serde_json::to_string(&v).unwrap_or_default()
    );

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
        model_hash: v
            .get("model_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        tokens_generated: v
            .get("tokens_generated")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32,
        // run_consensus reports `total_ms` (wall time across the whole
        // pipeline × every token). Use it as `inference_ms` so the UI's
        // "Xms" label still works.
        inference_ms: v.get("total_ms").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        tx_hash: String::new(),
        deterministic: v
            .get("deterministic")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        profile_bound: v
            .get("profile_bound")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        quorum_verified: v
            .get("quorum_verified")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        execution_profile: v
            .get("execution_profile")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        engine: v
            .get("engine")
            .and_then(|x| x.as_str())
            .unwrap_or("consensus")
            .to_string(),
        explorer_url: String::new(),
        routed_via: "sharded_consensus".to_string(),
        settlement: None,
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
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {} from {}: {}", status, coord_base, body));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    parse_inference_run_value(&v, Some(coord_base), false)
}

// ── Historical Tier 1 reads ───────────────────────────────────────────────
// New Tier 1 request writes are disabled by commands.rs. The former
// seed-signed convenience submitter was removed as well: server-selected
// replica names and VRF membership are not validator authorization. Keep only
// the read path so an operator can inspect a request ID created by an older
// build without creating a new request.

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
        return Err(format!(
            "HTTP {} from /inference/onchain/result",
            resp.status()
        ));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    let votes_json = v
        .get("votes")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let votes: Vec<Tier1Vote> = votes_json
        .into_iter()
        .map(|vj| Tier1Vote {
            voter: vj
                .get("voter")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            output_hash: vj
                .get("output_hash")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect();
    Ok(Tier1Result {
        request_id: v
            .get("request_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        status: v
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        vote_count: v.get("vote_count").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        committee_size: v
            .get("committee_size")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u8,
        anchor_height: v.get("anchor_height").and_then(|x| x.as_u64()).unwrap_or(0),
        deadline_blocks: v
            .get("deadline_blocks")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        votes,
        output_hash: v
            .get("output_hash")
            .and_then(|x| x.as_str())
            .map(String::from),
        output_blob: v
            .get("output_blob")
            .and_then(|x| x.as_str())
            .map(String::from),
        output_text: v
            .get("output_text")
            .and_then(|x| x.as_str())
            .map(String::from),
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

// ═════════════════════════════════════════════════════════════════════════
// Chain visibility + projection reads (v0.8.0)
// ═════════════════════════════════════════════════════════════════════════

/// ARC base units per whole ARC. Balances and stakes cross the wire as
/// integers in these units.
const ARC_BASE_UNITS: f64 = 1_000_000_000.0;

/// Outcome of a GET, keeping distinctions the UI has to phrase differently.
///
/// A collapsed `Option<Value>` cannot tell "this seed is running a build
/// without that endpoint" from "this seed is unreachable" — and those degrade
/// to two different sentences. Several of the endpoints below only exist on
/// builds newer than what is deployed, so 404 is the *expected* path and has
/// to read as a statement about the host, not as an error.
enum Fetched {
    Ok(Value),
    /// HTTP 404 — the host does not serve this path.
    NotFound,
    /// HTTP 400 — the host rejected the request as malformed.
    BadRequest,
    Status(u16),
    Unreachable(String),
    /// 200, but the body was not JSON we could parse.
    Unparseable,
}

async fn get_detailed(http: &reqwest::Client, url: &str) -> Fetched {
    match http.get(url).send().await {
        Ok(r) => {
            let code = r.status().as_u16();
            if r.status().is_success() {
                match r.json::<Value>().await {
                    Ok(v) => Fetched::Ok(v),
                    Err(_) => Fetched::Unparseable,
                }
            } else if code == 404 {
                Fetched::NotFound
            } else if code == 400 {
                Fetched::BadRequest
            } else {
                Fetched::Status(code)
            }
        }
        Err(e) => Fetched::Unreachable(e.to_string()),
    }
}

/// Phrase a failed read as a sentence naming the host and the path.
///
/// Deliberately states only what was observed. It does not guess *why* a host
/// lacks an endpoint (an old build? a proxy? a feature flag?) because the
/// desktop cannot know, and a confident wrong explanation is worse than the
/// bare fact.
fn unavailable_reason(host: &str, path: &str, f: &Fetched) -> String {
    match f {
        Fetched::Ok(_) => String::new(),
        Fetched::NotFound => format!("{} does not serve {} (HTTP 404).", host, path),
        Fetched::BadRequest => format!("{} rejected {} as malformed (HTTP 400).", host, path),
        Fetched::Status(c) => format!("{} answered {} with HTTP {}.", host, path, c),
        Fetched::Unreachable(e) => format!("Could not reach {} — {}", host, e),
        Fetched::Unparseable => format!(
            "{} answered {} with a response this build could not parse.",
            host, path
        ),
    }
}

fn earnings_from_fetched(host: &str, path: &str, fetched: Fetched) -> Earnings {
    match fetched {
        Fetched::Ok(value) => confirmed_earnings_from_value(&value).unwrap_or_else(|| {
            unavailable_earnings(format!(
                "{} answered {}, but did not provide the candidate mined-0x25 retained-receipt contract. Legacy or malformed inference-count arithmetic is not earnings.",
                host, path
            ))
        }),
        other => unavailable_earnings(unavailable_reason(host, path, &other)),
    }
}

/// First present key out of `keys`, as f64. Accepts ints and floats.
fn pick_f64(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_f64()))
}

fn pick_u64(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_u64()))
}

/// Follow a dotted path, e.g. `threads.in_use`.
///
/// `/node/contribution` groups its figures into `threads`, `shards` and
/// `own_compute_ms` objects rather than emitting them flat. A flat lookup for
/// `threads` returns the OBJECT, which silently yields `None` from `pick_u64`
/// and made the whole panel look unreported.
fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

fn nested_u64(v: &Value, path: &str) -> Option<u64> {
    at(v, path).and_then(|x| x.as_u64())
}

fn nested_f64(v: &Value, path: &str) -> Option<f64> {
    at(v, path).and_then(|x| x.as_f64())
}

fn nested_str(v: &Value, path: &str) -> Option<String> {
    at(v, path)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn nested_bool(v: &Value, path: &str) -> Option<bool> {
    at(v, path).and_then(|x| x.as_bool())
}

fn pick_str(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        v.get(*k)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    })
}

/// Strip an optional `0x` and lowercase. The node's own handlers are
/// inconsistent about the prefix — `/worker/earnings` and
/// `/inference/attestations` emit it, `/blocks`, `/validators` and `/tx/{hash}`
/// do not — so everything is normalised on ingest.
pub fn strip_0x(s: &str) -> String {
    s.trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_lowercase()
}

/// Whether `s` (already `strip_0x`ed) is a 32-byte hash.
///
/// Separate from the lookup so the "that is not a hash" answer can be tested
/// without a network, and so the check is stated once. Being able to reject a
/// bad paste locally is what keeps a typo from being reported as a pending
/// transaction.
pub fn is_tx_hash(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// `GET /economics/rewards` — the finite reward treasury.
///
/// Not present on the deployed seeds, so the 404 path is the one that runs
/// today and is the one that has to read well.
pub async fn fetch_reward_economics(http: &reqwest::Client, base_url: &str) -> RewardEconomics {
    let path = "/economics/rewards";
    let f = get_detailed(http, &format!("{}{}", base_url, path)).await;
    let v = match &f {
        Fetched::Ok(v) => v,
        other => {
            return RewardEconomics {
                source_host: base_url.to_string(),
                unavailable: Some(unavailable_reason(base_url, path, other)),
                reward_per_attestation: None,
                treasury_balance_arc: None,
                treasury_balance_unavailable_reason: None,
                attestations_remaining: None,
                attestations_remaining_unavailable_reason: None,
                treasury_is_finite: None,
                bond_per_attestation: None,
                challenge_period_blocks: None,
                bond_refunded_after_challenge_period: None,
                funding_detail: None,
            };
        }
    };

    // Prefer the exact `_base` integer and divide, falling back to the
    // host's own `_arc` float. The floats are produced by dividing by 1e9 and
    // carry rounding; the integers do not.
    let base_or_arc = |base_key: &str, arc_key: &str| -> Option<f64> {
        pick_u64(v, &[base_key])
            .map(|b| b as f64 / ARC_BASE_UNITS)
            .or_else(|| pick_f64(v, &[arc_key]))
    };

    RewardEconomics {
        source_host: base_url.to_string(),
        unavailable: None,
        reward_per_attestation: base_or_arc(
            "reward_per_attestation_base",
            "reward_per_attestation_arc",
        ),
        treasury_balance_arc: base_or_arc("treasury_balance_base", "treasury_balance_arc"),
        treasury_balance_unavailable_reason: pick_str(v, &["treasury_balance_unavailable_reason"]),
        // `rewards_remaining` is a COUNT of fundable reward receipts — NOT an
        // ARC amount. Renamed on the way in so no call site can mistake it for
        // currency.
        attestations_remaining: pick_u64(v, &["rewards_remaining"]),
        attestations_remaining_unavailable_reason: pick_str(
            v,
            &["rewards_remaining_unavailable_reason"],
        ),
        treasury_is_finite: v.get("treasury_is_finite").and_then(|x| x.as_bool()),
        // Community reward certificates are a separate transaction shape
        // from the local coordinator attestation. Never subtract the latter's
        // bond from a worker's projected reward.
        bond_per_attestation: base_or_arc(
            "community_worker_certificate_bond_base",
            "community_worker_certificate_bond_arc",
        ),
        challenge_period_blocks: None,
        bond_refunded_after_challenge_period: None,
        funding_detail: pick_str(v, &["funding_detail", "funding"]),
    }
}

/// `GET /worker/earnings/{addr}` — projection inputs for one address.
///
/// The rate is the delicate part. `attestations_per_day` is populated ONLY if
/// the host measured it. This function never derives a rate itself: doing so
/// would mean assuming a block time, and on this network block production has
/// been stalled on four of six seeds for days, so any block-time assumption is
/// wrong by an unknown factor.
const MIN_PROJECTION_RECEIPTS: u64 = 3;
const MIN_PROJECTION_WINDOW_MS: u64 = 86_400_000;
const PROJECTION_COLLECTING_REASON: &str = "collecting data: a projection needs at least 3 successful mined reward receipts spanning at least 24 hours, not the initial one or two rollout canaries";

fn projection_observation_gate(value: &Value, receipt_count: u64) -> Option<String> {
    if receipt_count < MIN_PROJECTION_RECEIPTS {
        return Some(PROJECTION_COLLECTING_REASON.to_string());
    }
    let first = pick_u64(value, &["observed_window_first_timestamp_ms"]);
    let last = pick_u64(value, &["observed_window_last_timestamp_ms"]);
    match (first, last) {
        (Some(first), Some(last))
            if last > first && last - first >= MIN_PROJECTION_WINDOW_MS =>
        {
            None
        }
        _ => Some(
            "collecting data: this host has not supplied a valid confirmed-receipt window spanning at least 24 hours"
                .to_string(),
        ),
    }
}

pub async fn fetch_earnings_projection(
    http: &reqwest::Client,
    base_url: &str,
    address: Option<&str>,
) -> EarningsProjection {
    let addr = match address {
        Some(a) if !a.trim().is_empty() => strip_0x(a),
        _ => {
            return EarningsProjection {
                source_host: base_url.to_string(),
                unavailable: Some(
                    "No identity on this device yet, so there is nothing to project.".to_string(),
                ),
                reward_per_attestation: None,
                reward_rate_source: "unknown".to_string(),
                community_rewards_enabled: None,
                projected_daily_arc: None,
                projected_daily_unavailable_reason: None,
                reward_policy_hash: None,
                reward_budget_epoch: None,
                rewards_remaining_this_epoch: None,
                worker_rewards_remaining_this_epoch: None,
                coordinator_rewards_remaining_this_epoch: None,
                issuance_ready_for_worker: None,
                reward_program: None,
                reward_is_customer_demand: None,
                attestations_total: 0,
                first_attestation_block: None,
                attestations_per_day: None,
                rate_unavailable_reason: None,
                observed_over_blocks: None,
                rate_caveat: None,
            };
        }
    };

    let path = format!("/worker/earnings/{}", addr);
    let f = get_detailed(http, &format!("{}{}", base_url, path)).await;
    let v = match &f {
        Fetched::Ok(v) => v,
        other => {
            return EarningsProjection {
                source_host: base_url.to_string(),
                unavailable: Some(unavailable_reason(base_url, &path, other)),
                reward_per_attestation: None,
                reward_rate_source: "unknown".to_string(),
                community_rewards_enabled: None,
                projected_daily_arc: None,
                projected_daily_unavailable_reason: None,
                reward_policy_hash: None,
                reward_budget_epoch: None,
                rewards_remaining_this_epoch: None,
                worker_rewards_remaining_this_epoch: None,
                coordinator_rewards_remaining_this_epoch: None,
                issuance_ready_for_worker: None,
                reward_program: None,
                reward_is_customer_demand: None,
                attestations_total: 0,
                first_attestation_block: None,
                attestations_per_day: None,
                rate_unavailable_reason: None,
                observed_over_blocks: None,
                rate_caveat: None,
            };
        }
    };

    let Some(confirmed) = confirmed_earnings_from_value(v) else {
        return EarningsProjection {
            source_host: base_url.to_string(),
            unavailable: Some(format!(
                "{} answered {}, but did not provide the candidate mined-0x25 receipt and reward-readiness contract. Legacy inference-count arithmetic is not projected as earnings.",
                base_url, path
            )),
            reward_per_attestation: None,
            reward_rate_source: "unknown".to_string(),
            community_rewards_enabled: None,
            projected_daily_arc: None,
            projected_daily_unavailable_reason: None,
            reward_policy_hash: None,
            reward_budget_epoch: None,
            rewards_remaining_this_epoch: None,
            worker_rewards_remaining_this_epoch: None,
            coordinator_rewards_remaining_this_epoch: None,
            issuance_ready_for_worker: None,
            reward_program: None,
            reward_is_customer_demand: None,
            attestations_total: 0,
            first_attestation_block: None,
            attestations_per_day: None,
            rate_unavailable_reason: None,
            observed_over_blocks: None,
            rate_caveat: None,
        };
    };

    let chain_rate = pick_u64(v, &["reward_per_attestation_base"])
        .map(|b| b as f64 / ARC_BASE_UNITS)
        .or_else(|| pick_f64(v, &["reward_per_attestation_arc"]))
        .filter(|rate| *rate >= 0.0);
    let (reward_per_attestation, reward_rate_source) = match chain_rate {
        Some(r) => (Some(r), "chain"),
        // A payable amount must come from the selected coordinator. A local
        // constant cannot prove that the remote rollout is active or aligned.
        None => (None, "unknown"),
    };
    let community_rewards_enabled = v
        .get("community_rewards_v1_enabled")
        .and_then(|value| value.as_bool());

    // The parser above has already reconciled this count against every exact
    // successful mined 0x25 receipt row. Do not re-trust a parallel host count
    // when receipt identities are available.
    let attestations_total = confirmed.attestations;
    let observation_gate = projection_observation_gate(v, attestations_total);
    let first_attestation_block = pick_u64(v, &["first_attestation_block"]);
    // `blocks_observed` on the wire — the inclusive span the rate covers.
    let observed_over_blocks = pick_u64(v, &["blocks_observed"]);
    let attestations_per_day = if observation_gate.is_none() {
        pick_f64(v, &["attestations_per_day_observed"]).filter(|rate| *rate >= 0.0)
    } else {
        None
    };

    // A rate the host explicitly declined to give carries its own reason;
    // otherwise say precisely which input is missing.
    let rate_unavailable_reason = if attestations_per_day.is_some() {
        None
    } else {
        observation_gate.clone().or_else(|| {
            Some(
                pick_str(
                v,
                &[
                    "attestations_per_day_unavailable_reason",
                    "attestations_per_day_reason",
                    "rate_unavailable_reason",
                ],
            )
                .unwrap_or_else(|| {
                    if attestations_total == 0 {
                        "No successful mined reward receipts are retained for this address, so there is no history to measure a rate from.".to_string()
                    } else {
                        format!(
                            "{} reports {} successful mined reward receipt(s) for this address but no observed rate, so a per-day figure cannot be measured here.",
                            base_url, attestations_total
                        )
                    }
                }),
            )
        })
    };

    // The host's own caveat about how it derived the rate. Shown verbatim
    // rather than paraphrased — it knows its method and this build does not.
    let rate_caveat = pick_str(v, &["attestations_per_day_caveat"]);
    let budget = v.get("reward_budget").and_then(Value::as_object);

    EarningsProjection {
        source_host: base_url.to_string(),
        unavailable: None,
        reward_per_attestation,
        reward_rate_source: reward_rate_source.to_string(),
        community_rewards_enabled,
        projected_daily_arc: if observation_gate.is_none() {
            confirmed.projected_daily_arc
        } else {
            None
        },
        projected_daily_unavailable_reason: observation_gate
            .or(confirmed.projected_daily_unavailable_reason),
        reward_policy_hash: pick_str(v, &["reward_issuance_policy_hash"]),
        reward_budget_epoch: budget
            .and_then(|value| value.get("epoch"))
            .and_then(Value::as_u64),
        rewards_remaining_this_epoch: budget
            .and_then(|value| value.get("remaining_this_epoch"))
            .and_then(Value::as_u64),
        worker_rewards_remaining_this_epoch: budget
            .and_then(|value| value.get("worker_remaining_this_epoch"))
            .and_then(Value::as_u64),
        coordinator_rewards_remaining_this_epoch: budget
            .and_then(|value| value.get("coordinator_remaining_this_epoch"))
            .and_then(Value::as_u64),
        issuance_ready_for_worker: v.get("issuance_ready_for_worker").and_then(Value::as_bool),
        reward_program: pick_str(v, &["reward_program"]),
        reward_is_customer_demand: v.get("reward_is_customer_demand").and_then(Value::as_bool),
        attestations_total,
        first_attestation_block,
        attestations_per_day,
        rate_unavailable_reason,
        observed_over_blocks,
        // The bond is NOT on this endpoint. It comes from /economics/rewards,
        // which is fetched separately and joined in the UI.
        rate_caveat,
    }
}

/// What the LOCAL node is contributing.
///
/// Prefers `GET /node/contribution`. Where that is absent it composes the same
/// picture from `GET /node/threads` and `GET /stats`, both of which the shipped
/// binary serves — so "not available" is reserved for a node that genuinely
/// isn't answering, rather than shown next to numbers we could have read.
pub async fn fetch_node_contribution(
    http: &reqwest::Client,
    local_url: &str,
    cpu_cores: Option<u32>,
) -> NodeContribution {
    let path = "/node/contribution";
    if let Fetched::Ok(v) = get_detailed(http, &format!("{}{}", local_url, path)).await {
        // This endpoint is NESTED: `threads`, `shards` and `own_compute_ms` are
        // objects. A flat lookup for "threads" returns the object itself, which
        // reads as absent and made the whole panel look unreported.
        let ranges = at(&v, "shards.ranges")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        let a = r.get("start_layer")?.as_u64()?;
                        let b = r.get("end_layer")?.as_u64()?;
                        Some(format!("{}..{}", a, b))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        return NodeContribution {
            source_host: local_url.to_string(),
            unavailable: None,
            source: "contribution".to_string(),
            threads_in_use: nested_u64(&v, "threads.in_use").map(|x| x as u32),
            threads_available: nested_u64(&v, "threads.available_parallelism")
                // The host reports 0 when it could not read the core count.
                // Zero cores is not a measurement, so fall back to ours.
                .filter(|x| *x > 0)
                .map(|x| x as u32)
                .or(cpu_cores),
            layers_held: if ranges.is_empty() {
                None
            } else {
                Some(ranges.join(", "))
            },
            // A UNION of the layers held, which the host computes. Summing the
            // ranges would double-count replicated layers.
            layer_count: nested_u64(&v, "shards.layers_held").map(|x| x as u32),
            total_layers: nested_u64(&v, "shards.total_layers").map(|x| x as u32),
            runs_served: pick_u64(&v, &["sharded_runs_total"]),
            // `sharded_cache_hits` — no `_total` suffix here, unlike
            // `sharded_runs_total` and `sharded_bytes_total`.
            cache_hits: pick_u64(&v, &["sharded_cache_hits"]),
            hop_ms_mean: nested_f64(&v, "own_compute_ms.mean_ms"),
            hop_samples: nested_u64(&v, "own_compute_ms.samples"),
            hop_unavailable_reason: nested_str(&v, "own_compute_ms.unavailable_reason"),
        };
    }

    // Composed fallback. Both reads are against 127.0.0.1 — this describes the
    // user's own machine, never a seed's.
    let threads = get_detailed(http, &format!("{}/node/threads", local_url)).await;
    let stats = get_detailed(http, &format!("{}/stats", local_url)).await;

    let (threads_in_use, threads_available) = match &threads {
        Fetched::Ok(v) => (
            pick_u64(v, &["threads", "threads_in_use", "worker_threads"]).map(|x| x as u32),
            pick_u64(v, &["available", "cpu_cores", "max_threads"])
                .map(|x| x as u32)
                .or(cpu_cores),
        ),
        _ => (None, cpu_cores),
    };

    let (runs_served, cache_hits) = match &stats {
        Fetched::Ok(v) => (
            pick_u64(v, &["sharded_runs_total"]),
            // /stats spells it with `_total`; /node/contribution does not.
            pick_u64(v, &["sharded_cache_hits_total", "sharded_cache_hits"]),
        ),
        _ => (None, None),
    };

    // Nothing answered at all — say so once, naming the local node.
    if threads_in_use.is_none() && runs_served.is_none() {
        return NodeContribution {
            source_host: local_url.to_string(),
            unavailable: Some(format!(
                "Your node did not answer {}, /node/threads or /stats, so what it is contributing cannot be read right now.",
                path
            )),
            source: "none".to_string(),
            threads_in_use: None,
            threads_available: cpu_cores,
            layers_held: None,
            layer_count: None,
            total_layers: None,
            runs_served: None,
            cache_hits: None,
            hop_ms_mean: None,
            hop_samples: None,
            hop_unavailable_reason: None,
        };
    }

    NodeContribution {
        source_host: local_url.to_string(),
        unavailable: None,
        source: "composed".to_string(),
        threads_in_use,
        threads_available,
        // Neither /node/threads nor /stats reports the layer range. Absent,
        // not guessed — /shards would answer it but describes the whole
        // network's tiling, not specifically this node's slice.
        layers_held: None,
        layer_count: None,
        total_layers: None,
        runs_served,
        cache_hits,
        // No endpoint on the older binary measures a per-hop mean for this
        // node. /inference/latency_stats is the network's EWMA, which is
        // known-poisoned and 9-11h stale, so it is deliberately not used.
        hop_ms_mean: None,
        hop_samples: None,
        hop_unavailable_reason: None,
    }
}

/// The Network screen's chain view, from the ONE pinned host.
///
/// Four independent reads against the same host. `/network/info` is allowed to
/// be missing without taking the rest down: a host that cannot name its
/// network can still report a height and a block age, and those are the
/// numbers a user needs to see that a chain has stopped.
pub async fn fetch_network_overview(http: &reqwest::Client, base_url: &str) -> NetworkOverview {
    let (info_url, health_url, latest_url, validators_url) = (
        format!("{}/network/info", base_url),
        format!("{}/health", base_url),
        format!("{}/block/latest", base_url),
        format!("{}/validators", base_url),
    );
    let (info, health, latest, validators) = tokio::join!(
        get_detailed(http, &info_url),
        get_detailed(http, &health_url),
        get_detailed(http, &latest_url),
        get_detailed(http, &validators_url),
    );

    // Only /network/info may name the network. `/info` is NOT consulted: its
    // `chain` field is the constant string "ARC Chain" on every deployment, so
    // it distinguishes nothing and would be an invented answer to "which
    // network am I on".
    let info_body = match &info {
        Fetched::Ok(v) => Some(v),
        _ => None,
    };
    let network_name = info_body.and_then(|v| pick_str(v, &["network"]));
    let chain_id = info_body.and_then(|v| pick_str(v, &["chain_id"]));
    let network_name_unavailable_reason = info_body
        .and_then(|v| pick_str(v, &["network_unavailable_reason"]))
        .or_else(|| match &info {
            Fetched::Ok(_) => None,
            other => Some(unavailable_reason(base_url, "/network/info", other)),
        });
    // The ONLY input allowed to make this app describe a network as mainnet.
    let declares_mainnet = info_body.and_then(|v| nested_bool(v, "declares_mainnet"));
    let is_block_producing = info_body.and_then(|v| nested_bool(v, "is_block_producing"));
    let is_block_producing_basis =
        info_body.and_then(|v| pick_str(v, &["is_block_producing_basis"]));

    let (host_version, height, dag_round, dag_committed, peers) = match &health {
        Fetched::Ok(v) => (
            pick_str(v, &["version"]),
            pick_u64(v, &["height", "block_height"]),
            pick_u64(v, &["dag_round"]),
            pick_u64(v, &["dag_committed"]),
            pick_u64(v, &["peers", "connected_peers"]).map(|x| x as u32),
        ),
        _ => (None, None, None, None, None),
    };

    // Block age comes from the block header's own timestamp, not from
    // /health. A stalled seed still reports status "ok" with a healthy peer
    // count, because its DAG round keeps advancing while height stands still.
    let last_block_age_secs = info_body
        .and_then(|v| pick_u64(v, &["last_block_age_secs"]))
        .or_else(|| match &latest {
            Fetched::Ok(v) => v
                .get("header")
                .and_then(|h| h.get("timestamp"))
                .and_then(|t| t.as_u64())
                .filter(|t| *t > 0)
                .map(|ts_ms| {
                    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
                    now.saturating_sub(ts_ms) / 1000
                }),
            _ => None,
        });

    // Active vs registered is DERIVED here by counting stake > 0, because
    // /validators exposes no such distinction. Zero-stake entries are counted
    // by /health and by `count`, which is what inflates the reported set.
    let reported_active = info_body.and_then(|v| pick_u64(v, &["validators_active"]));
    let reported_registered = info_body.and_then(|v| pick_u64(v, &["validators_registered"]));
    let min_active_stake = info_body.and_then(|v| pick_u64(v, &["min_active_stake"]));

    let (validator_list, derived_active, derived_registered) = match &validators {
        Fetched::Ok(v) => {
            let list: Vec<ValidatorInfo> = v
                .get("validators")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|entry| {
                            let address = pick_str(entry, &["address"])?;
                            let stake = pick_u64(entry, &["stake"]).unwrap_or(0);
                            Some(ValidatorInfo {
                                address: strip_0x(&address),
                                stake,
                                active: stake > 0,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let active = list.iter().filter(|x| x.active).count() as u32;
            let registered = pick_u64(v, &["count"])
                .map(|x| x as u32)
                .unwrap_or(list.len() as u32);
            (list, Some(active), Some(registered))
        }
        _ => (Vec::new(), None, None),
    };

    // `/network/info` applies the real `min_active_stake` threshold; counting
    // stake > 0 from `/validators` only approximates it. Prefer the reported
    // figures and record which was used, so an approximation is never shown as
    // the host's own number.
    let validator_split_derived = reported_active.is_none();
    let validators_active = reported_active.map(|x| x as u32).or(derived_active);
    let validators_registered = reported_registered.map(|x| x as u32).or(derived_registered);

    // Only a total blackout is "unavailable". A missing network name is
    // reported by `network_name: None` and phrased by the UI, which names the
    // host it is reading.
    let unavailable =
        if height.is_none() && last_block_age_secs.is_none() && validator_list.is_empty() {
            Some(unavailable_reason(base_url, "/health", &health))
        } else {
            None
        };

    NetworkOverview {
        source_host: base_url.to_string(),
        unavailable,
        network_name,
        network_name_unavailable_reason,
        chain_id,
        declares_mainnet,
        is_block_producing,
        is_block_producing_basis,
        host_version,
        height,
        last_block_age_secs,
        dag_round,
        dag_committed,
        peers,
        validators_active,
        validators_registered,
        min_active_stake,
        validator_split_derived,
        validators: validator_list,
    }
}

/// The `from`/`to` window covering the newest `limit` blocks at `tip`.
///
/// Extracted so the off-by-one is testable. Getting it wrong is not a cosmetic
/// error: asking `/blocks` for a limit with no `from` returns genesis, whose
/// timestamp is 0, which the UI's relative-time formatter renders as
/// "20770d ago".
fn recent_block_range(tip: u64, limit: u32) -> (u64, u64) {
    let span = (limit.max(1) as u64).saturating_sub(1);
    (tip.saturating_sub(span), tip)
}

/// `GET /blocks` — the most recent blocks on the pinned host.
///
/// The range has to be computed, not defaulted. `/blocks` takes `from`, `to`
/// and `limit`, and `from` defaults to **0** — so `?limit=10` returns the ten
/// OLDEST blocks, starting at genesis, not the newest ten. Verified against the
/// live NYC seed, which answered `?limit=2` with height 0. The height is read
/// first so the window can be anchored to the tip.
pub async fn fetch_recent_blocks(
    http: &reqwest::Client,
    base_url: &str,
    limit: u32,
) -> RecentBlocks {
    // The handler caps limit at 100; ask for no more than it will give.
    let limit = limit.clamp(1, 100);

    // Anchor to the tip. Without a height we cannot ask for "recent", so say
    // so rather than silently returning genesis blocks.
    let health = get_detailed(http, &format!("{}/health", base_url)).await;
    let tip = match &health {
        Fetched::Ok(v) => pick_u64(v, &["height", "block_height"]),
        _ => None,
    };
    let tip = match tip {
        Some(h) => h,
        None => {
            return RecentBlocks {
                source_host: base_url.to_string(),
                unavailable: Some(format!(
                    "Could not read the current height from {}, so the newest blocks cannot be located.",
                    base_url
                )),
                blocks: Vec::new(),
            };
        }
    };
    let (from, to) = recent_block_range(tip, limit);

    let path = format!("/blocks?from={}&to={}&limit={}", from, to, limit);
    let f = get_detailed(http, &format!("{}{}", base_url, path)).await;
    let v = match &f {
        Fetched::Ok(v) => v,
        other => {
            return RecentBlocks {
                source_host: base_url.to_string(),
                unavailable: Some(unavailable_reason(base_url, &path, other)),
                blocks: Vec::new(),
            };
        }
    };

    let mut blocks: Vec<BlockSummary> = v
        .get("blocks")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    let height = pick_u64(b, &["height"])?;
                    Some(BlockSummary {
                        height,
                        hash: pick_str(b, &["hash"])
                            .map(|h| strip_0x(&h))
                            .unwrap_or_default(),
                        // A zero timestamp is not a time. Genesis carries
                        // `timestamp: 0`, and feeding that to the UI's
                        // relative-time formatter renders "20770d ago" — the
                        // same class of bug as passing a block height to it.
                        timestamp_ms: pick_u64(b, &["timestamp"]).filter(|t| *t > 0),
                        tx_count: pick_u64(b, &["tx_count"]).map(|x| x as u32),
                        // An all-zero producer is a placeholder, not an address.
                        proposer: pick_str(b, &["producer", "proposer"])
                            .map(|p| strip_0x(&p))
                            .filter(|p| p.chars().any(|c| c != '0')),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // Newest first. The handler returns ascending by height.
    blocks.sort_by_key(|block| std::cmp::Reverse(block.height));

    RecentBlocks {
        source_host: base_url.to_string(),
        unavailable: None,
        blocks,
    }
}

/// `GET /tx/{hash}` on the pinned host.
///
/// The three outcomes are kept apart deliberately. A 404 here does NOT mean
/// the hash is bogus: `/tx/{hash}` returns a *receipt*, and a transaction
/// still in the mempool has none. Calling that "invalid" would tell a user who
/// just submitted an attestation that their hash was fake.
pub async fn lookup_tx(http: &reqwest::Client, base_url: &str, hash: &str) -> TxLookup {
    let normalised = strip_0x(hash);
    let base = TxLookup {
        source_host: base_url.to_string(),
        unavailable: None,
        hash: normalised.clone(),
        status: "error".to_string(),
        block_height: None,
        block_hash: None,
        tx_index: None,
        success: None,
        gas_used: None,
    };

    // Reject a malformed paste locally rather than spending a round trip to
    // be told the same thing.
    if !is_tx_hash(&normalised) {
        return TxLookup {
            status: "invalid_hash".to_string(),
            unavailable: Some(format!(
                "A transaction hash is 64 hex characters (an optional 0x prefix is fine). That one is {}.",
                normalised.len()
            )),
            ..base
        };
    }

    let path = format!("/tx/{}", normalised);
    match get_detailed(http, &format!("{}{}", base_url, path)).await {
        Fetched::Ok(v) => TxLookup {
            status: "mined".to_string(),
            block_height: pick_u64(&v, &["block_height"]),
            block_hash: pick_str(&v, &["block_hash"]).map(|h| strip_0x(&h)),
            tx_index: pick_u64(&v, &["index"]).map(|x| x as u32),
            success: v.get("success").and_then(|x| x.as_bool()),
            gas_used: pick_u64(&v, &["gas_used"]),
            ..base
        },
        Fetched::NotFound => TxLookup {
            status: "not_found".to_string(),
            ..base
        },
        Fetched::BadRequest => TxLookup {
            status: "invalid_hash".to_string(),
            unavailable: Some(format!("{} rejected that hash as malformed.", base_url)),
            ..base
        },
        other => TxLookup {
            status: "error".to_string(),
            unavailable: Some(unavailable_reason(base_url, &path, &other)),
            ..base
        },
    }
}

/// `GET /block/{height}/txs` — the transactions in one block.
///
/// Fetched on demand (when the user expands a block), never on the polling
/// path. Expanding every visible block on each poll would mean ten extra
/// requests per interval against the single pinned host.
pub async fn fetch_block_txs(
    http: &reqwest::Client,
    base_url: &str,
    height: u64,
    limit: u32,
) -> crate::types::BlockTxs {
    use crate::types::{BlockTx, BlockTxs};
    let path = format!("/block/{}/txs?limit={}", height, limit.clamp(1, 1000));
    let f = get_detailed(http, &format!("{}{}", base_url, path)).await;
    let v = match &f {
        Fetched::Ok(v) => v,
        other => {
            return BlockTxs {
                source_host: base_url.to_string(),
                unavailable: Some(unavailable_reason(base_url, &path, other)),
                height,
                tx_count: None,
                txs: Vec::new(),
            };
        }
    };

    let txs: Vec<BlockTx> = v
        .get("transactions")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let hash = pick_str(t, &["hash"])?;
                    Some(BlockTx {
                        index: pick_u64(t, &["index"]).unwrap_or(0) as u32,
                        hash: strip_0x(&hash),
                        tx_type: pick_str(t, &["tx_type"]),
                        from: pick_str(t, &["from"]).map(|s| strip_0x(&s)),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    BlockTxs {
        source_host: base_url.to_string(),
        unavailable: None,
        height,
        tx_count: pick_u64(v, &["tx_count"]).map(|x| x as u32),
        txs,
    }
}

#[cfg(test)]
mod chain_read_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn community_inference_parser_preserves_top_level_proof_and_reward_state() {
        let reward_hash = format!("0x{}", "ab".repeat(32));
        let value = json!({
            "success": true,
            "routed_via": format!("community:0x{}", "11".repeat(32)),
            "inference": {
                "input": "hello",
                "output": "verified community output",
                "output_hash": format!("0x{}", "22".repeat(32)),
                "model_hash": format!("0x{}", "33".repeat(32)),
                "tokens_generated": 3,
                "inference_ms": 42,
                "deterministic": true,
                "engine": "integer community worker"
            },
            "attestation": {
                "status": "worker_certificate_handled_by_settlement"
            },
            "verification": {
                "method": "authenticated_shard_quorum_2_of_3_per_range",
                "profile_bound": true,
                "quorum_verified": true,
                "execution_profile": "canonical-reward-profile-v3"
            },
            "settlement": {
                "status": "pending_mined_receipt",
                "tx_type": "0x25",
                "tx_hash": reward_hash,
                "job_id": format!("0x{}", "44".repeat(32)),
                "submitted": true,
                "included": false,
                "confirmed": false,
                "reward_arc": 2.5,
                "receipt_url": "/community/reward_receipt/test"
            }
        });

        let parsed = parse_inference_run_value(&value, Some("https://seed.example"), false)
            .expect("valid community response");
        assert_eq!(parsed.output, "verified community output");
        assert!(parsed.profile_bound);
        assert!(parsed.quorum_verified);
        assert_eq!(parsed.execution_profile, "canonical-reward-profile-v3");
        assert!(parsed.routed_via.starts_with("community:"));
        assert_eq!(
            parsed.tx_hash, "",
            "0x25 must not be labeled as a 0x16 claim"
        );
        let settlement = parsed.settlement.expect("typed reward settlement");
        assert_eq!(settlement.tx_type, "0x25");
        assert_eq!(settlement.tx_hash, reward_hash);
        assert!(settlement.submitted);
        assert!(!settlement.included);
        assert!(!settlement.confirmed);
        assert_eq!(settlement.reward_arc, Some(2.5));
    }

    #[test]
    fn inference_success_false_is_an_error_not_an_empty_success() {
        let error = parse_inference_run_value(
            &json!({"success": false, "error": "No model loaded"}),
            None,
            true,
        )
        .expect_err("failed endpoint result must remain an error");
        assert_eq!(error, "No model loaded");
    }

    #[test]
    fn inference_parser_accepts_flat_sharded_fallback_without_inventing_settlement() {
        let value = json!({
            "success": true,
            "routed_via": "sharded_seed_pipeline",
            "input": "hello",
            "output": "sharded output",
            "output_hash": format!("0x{}", "66".repeat(32)),
            "model_hash": format!("0x{}", "77".repeat(32)),
            "tokens_generated": 4,
            "total_ms": 512,
            "deterministic": true,
            "profile_bound": true,
            "quorum_verified": false,
            "execution_profile": "canonical-sharded-profile",
            "engine": "integer sharded pipeline",
            "attestation": { "tx_hash": format!("0x{}", "88".repeat(32)) },
            "shard_trace": [{
                "hop": 0,
                "node": "NYC",
                "layers": "0..6",
                "compute_ms": 10,
                "wall_ms": 12,
                "is_terminal": false
            }]
        });

        let parsed = parse_inference_run_value(&value, Some("https://seed.example"), false)
            .expect("flat sharded response");
        assert_eq!(parsed.output, "sharded output");
        assert_eq!(parsed.inference_ms, 512);
        assert_eq!(parsed.routed_via, "sharded_seed_pipeline");
        assert!(parsed.profile_bound);
        assert!(!parsed.quorum_verified);
        assert!(parsed.settlement.is_none());
        assert_eq!(parsed.trace.as_ref().map(Vec::len), Some(1));
    }

    // ── Hash normalisation ──────────────────────────────────────────────
    // The node's handlers disagree about the `0x` prefix, so every hash is
    // normalised on ingest. Getting this wrong makes a lookup miss a tx that
    // is genuinely in a block.

    #[test]
    fn strip_0x_removes_either_prefix_case_and_lowercases() {
        assert_eq!(strip_0x("0xABCD"), "abcd");
        assert_eq!(strip_0x("0XABCD"), "abcd");
        assert_eq!(strip_0x("abcd"), "abcd");
        assert_eq!(strip_0x("  0xAbCd  "), "abcd");
    }

    #[test]
    fn strip_0x_leaves_an_embedded_0x_alone() {
        // Only a LEADING prefix is a prefix.
        assert_eq!(strip_0x("ab0xcd"), "ab0xcd");
    }

    #[test]
    fn is_tx_hash_accepts_exactly_64_hex_chars() {
        assert!(is_tx_hash(&"a".repeat(64)));
        assert!(is_tx_hash(&"0".repeat(64)));
        assert!(is_tx_hash(&"0123456789abcdef".repeat(4)));
    }

    #[test]
    fn is_tx_hash_rejects_wrong_length_and_non_hex() {
        assert!(!is_tx_hash(""));
        assert!(!is_tx_hash(&"a".repeat(63)));
        assert!(!is_tx_hash(&"a".repeat(65)));
        // 'g' is not hex. A 64-char non-hex string is the paste most likely to
        // be mistaken for a valid hash.
        assert!(!is_tx_hash(&"g".repeat(64)));
        assert!(!is_tx_hash("not-a-hash"));
    }

    // ── Degradation copy ────────────────────────────────────────────────
    // These sentences are the product when an endpoint is missing, which on
    // the deployed seeds is most of the time. They must name the host and the
    // path, and must not speculate about the cause.

    #[test]
    fn not_found_names_the_host_the_path_and_the_status() {
        let r = unavailable_reason(
            "http://1.2.3.4:9090",
            "/economics/rewards",
            &Fetched::NotFound,
        );
        assert!(r.contains("http://1.2.3.4:9090"), "{r}");
        assert!(r.contains("/economics/rewards"), "{r}");
        assert!(r.contains("404"), "{r}");
    }

    #[test]
    fn degradation_copy_never_speculates_about_the_cause() {
        // A confident wrong explanation ("your node is out of date") is worse
        // than the bare observed fact, because the desktop cannot know.
        let forbidden = [
            "old build",
            "out of date",
            "outdated",
            "upgrade",
            "probably",
            "likely",
        ];
        let cases = [
            Fetched::NotFound,
            Fetched::BadRequest,
            Fetched::Status(500),
            Fetched::Unparseable,
            Fetched::Unreachable("connection refused".into()),
        ];
        for f in &cases {
            let r = unavailable_reason("http://1.2.3.4:9090", "/x", f).to_lowercase();
            for word in forbidden {
                assert!(!r.contains(word), "reason {r:?} speculates with {word:?}");
            }
        }
    }

    #[test]
    fn unreachable_carries_the_transport_error() {
        let r = unavailable_reason(
            "http://1.2.3.4:9090",
            "/health",
            &Fetched::Unreachable("connection refused".into()),
        );
        assert!(r.contains("Could not reach"), "{r}");
        assert!(r.contains("connection refused"), "{r}");
    }

    #[test]
    fn every_failure_mode_produces_a_non_empty_sentence() {
        // A blank reason would render as an empty box where an explanation
        // should be.
        for f in [
            Fetched::NotFound,
            Fetched::BadRequest,
            Fetched::Status(503),
            Fetched::Unparseable,
            Fetched::Unreachable("timeout".into()),
        ] {
            let r = unavailable_reason("http://h", "/p", &f);
            assert!(!r.trim().is_empty());
        }
    }

    // ── Field picking ───────────────────────────────────────────────────
    // Endpoints spell the same quantity differently (`height` vs
    // `block_height`), and the projection fields may land under either an
    // `_arc` or a `_base` name. Absent must stay absent rather than becoming 0.

    #[test]
    fn pick_takes_the_first_present_key_in_order() {
        let v = json!({ "second": 2, "first": 1 });
        assert_eq!(pick_u64(&v, &["first", "second"]), Some(1));
        assert_eq!(pick_u64(&v, &["second", "first"]), Some(2));
    }

    #[test]
    fn pick_returns_none_for_a_missing_key_never_zero() {
        let v = json!({ "other": 5 });
        assert_eq!(pick_u64(&v, &["height"]), None);
        assert_eq!(pick_f64(&v, &["rate"]), None);
        assert_eq!(pick_str(&v, &["name"]), None);
    }

    #[test]
    fn pick_f64_accepts_an_integer_encoding() {
        // Reward figures arrive as either 2.5 or 2, depending on the handler.
        let v = json!({ "reward": 2 });
        assert_eq!(pick_f64(&v, &["reward"]), Some(2.0));
    }

    #[test]
    fn pick_str_treats_an_empty_string_as_absent() {
        // An empty network name must fall through to "unknown", not render as
        // a blank network label.
        let v = json!({ "network_name": "", "name": "arc-testnet-1" });
        assert_eq!(
            pick_str(&v, &["network_name", "name"]),
            Some("arc-testnet-1".to_string())
        );
    }

    // ── Recent-block window ─────────────────────────────────────────────

    fn candidate_earnings_value() -> Value {
        json!({
            "total_rewards": 2,
            "estimated_total_arc": 5.0,
            "confirmed_receipt_count": 2,
            "confirmed_gross_earnings_base": 5_000_000_000u64,
            "confirmed_gross_earnings_arc": 5.0,
            "confirmed_receipts": [
                {
                    "tx_type": "0x25",
                    "tx_hash": format!("0x{}", "aa".repeat(32)),
                    "job_id": format!("0x{}", "01".repeat(32)),
                    "block_height": 123_461,
                    "block_hash": format!("0x{}", "10".repeat(32)),
                    "success": true,
                    "reward_base": 2_500_000_000u64,
                    "reward_arc": 2.5,
                    "recovery_epoch": 1,
                    "validator_set_id": 7,
                },
                {
                    "tx_type": "0x25",
                    "tx_hash": format!("0x{}", "ab".repeat(32)),
                    "job_id": format!("0x{}", "02".repeat(32)),
                    "block_height": 123_462,
                    "block_hash": format!("0x{}", "11".repeat(32)),
                    "success": true,
                    "reward_base": 2_500_000_000u64,
                    "reward_arc": 2.5,
                    "recovery_epoch": 1,
                    "validator_set_id": 7,
                }
            ],
            "estimated_total_arc_note":
                "retained-window gross rewards = successful CommunityInferenceReward receipts × reward_per_attestation_arc",
            "source": RETAINED_EARNINGS_SOURCE,
            "archive_mode": false,
            "today_arc": Value::Null,
            "projected_daily_arc": Value::Null,
            "projected_daily_unavailable_reason": "a single receipt window cannot establish a forecast",
            "recovery_epoch": 1,
            "validator_set_id": 7,
            "community_rewards_v1_enabled": true,
            "community_rewards_v1_protocol_active": true,
            "community_rewards_v1_approval_collection_ready": true,
            "last_reward_block": 123_462,
            "last_reward_tx_hash": format!("0x{}", "ab".repeat(32)),
        })
    }

    fn candidate_projection_value(receipt_count: usize, window_ms: u64) -> Value {
        assert!(receipt_count <= 3);
        let mut value = candidate_earnings_value();
        value["confirmed_receipts"]
            .as_array_mut()
            .expect("receipt fixture is an array")
            .push(json!({
                "tx_type": "0x25",
                "tx_hash": format!("0x{}", "ac".repeat(32)),
                "job_id": format!("0x{}", "03".repeat(32)),
                "block_height": 123_463,
                "block_hash": format!("0x{}", "12".repeat(32)),
                "success": true,
                "reward_base": 2_500_000_000u64,
                "reward_arc": 2.5,
                "recovery_epoch": 1,
                "validator_set_id": 7,
            }));
        value["confirmed_receipts"]
            .as_array_mut()
            .expect("receipt fixture is an array")
            .truncate(receipt_count);
        let last = value["confirmed_receipts"]
            .as_array()
            .and_then(|receipts| receipts.last())
            .cloned();
        let count = u64::try_from(receipt_count).expect("small fixture count");
        value["total_rewards"] = json!(count);
        value["estimated_total_arc"] = json!(count as f64 * 2.5);
        value["confirmed_receipt_count"] = json!(count);
        value["confirmed_gross_earnings_base"] = json!(count * 2_500_000_000u64);
        value["confirmed_gross_earnings_arc"] = json!(count as f64 * 2.5);
        value["last_reward_block"] = last
            .as_ref()
            .map(|receipt| receipt["block_height"].clone())
            .unwrap_or(Value::Null);
        value["last_reward_tx_hash"] = last
            .as_ref()
            .map(|receipt| receipt["tx_hash"].clone())
            .unwrap_or(Value::Null);
        value["projected_daily_arc"] = json!(7.5);
        value["projected_daily_unavailable_reason"] = Value::Null;
        value["observed_window_first_timestamp_ms"] = json!(1_700_000_000_000u64);
        value["observed_window_last_timestamp_ms"] =
            json!(1_700_000_000_000u64 + window_ms);
        value
    }

    #[test]
    fn typed_activity_prefers_real_0x25_rows_and_uses_worker_ownership() {
        let worker = "ab".repeat(32);
        let common = json!({
            "schema": "arc.inference.activity.v1",
            "source": "chain_receipt",
            "mined": true,
            "success": true,
            "computed": true,
            "block_height": 42,
            "inference": {"model_hash": format!("0x{}", "11".repeat(32))},
        });
        let mut paid = common.clone();
        paid["record_kind"] = json!("mined_community_inference_reward");
        paid["tx_type"] = json!("CommunityInferenceReward");
        paid["tx_type_code"] = json!("0x25");
        paid["paid"] = json!(true);
        paid["earned"] = json!(true);
        paid["tx_hash"] = json!(format!("0x{}", "22".repeat(32)));
        paid["worker"] = json!(format!("0x{worker}"));
        paid["from"] = json!(format!("0x{}", "ff".repeat(32)));

        let mut claim = common.clone();
        claim["record_kind"] = json!("mined_inference_attestation");
        claim["tx_type"] = json!("InferenceAttestation");
        claim["tx_type_code"] = json!("0x16");
        claim["paid"] = json!(false);
        claim["earned"] = json!(false);
        claim["tx_hash"] = json!(format!("0x{}", "33".repeat(32)));
        claim["from"] = json!(format!("0x{worker}"));

        let mut failed = paid.clone();
        failed["success"] = json!(false);
        failed["tx_hash"] = json!(format!("0x{}", "44".repeat(32)));
        let mut mislabeled = paid.clone();
        mislabeled["paid"] = json!(false);
        mislabeled["tx_hash"] = json!(format!("0x{}", "55".repeat(32)));

        let parsed = attestations_from_value(
            &json!({
                "activities": [paid, claim, failed, mislabeled],
                "attestations": [{"tx_hash": format!("0x{}", "66".repeat(32)), "success": true}],
            }),
            Some(&format!("0x{worker}")),
        );
        assert_eq!(parsed.len(), 2);
        let reward = parsed
            .iter()
            .find(|row| row.record_kind.as_deref() == Some("mined_community_inference_reward"))
            .expect("successful 0x25 activity must remain visible");
        assert!(
            reward.mine,
            "ownership must use reward.worker before coordinator from"
        );
        assert!(reward.computed && reward.paid && reward.earned && reward.verified);
        let claim = parsed
            .iter()
            .find(|row| row.record_kind.as_deref() == Some("mined_inference_attestation"))
            .expect("successful 0x16 computation evidence must remain visible");
        assert!(claim.mine && claim.computed && !claim.paid && !claim.earned);
    }

    #[test]
    fn candidate_reward_receipt_contract_is_confirmed_without_inventing_today() {
        let earnings = confirmed_earnings_from_value(&candidate_earnings_value())
            .expect("candidate receipt contract should parse");
        assert!(earnings.from_chain);
        assert_eq!(earnings.attestations, 2);
        assert_eq!(earnings.total_arc, 5.0);
        assert_eq!(earnings.today_arc, None);
        assert_eq!(earnings.last_payout_block, Some(123_462));
        assert_eq!(earnings.confirmed_receipts.len(), 2);
        assert_eq!(earnings.recovery_epoch, Some(1));
        assert_eq!(earnings.validator_set_id, Some(7));
        assert_eq!(earnings.unavailable_reason, None);
        assert_eq!(
            earnings.receipt_source.as_deref(),
            Some(RETAINED_EARNINGS_SOURCE)
        );
        assert_eq!(earnings.archive_mode, Some(false));
    }

    #[test]
    fn desktop_rejects_two_canary_receipts_even_if_a_host_projects_them() {
        let mut value = candidate_earnings_value();
        value["projected_daily_arc"] = json!(216_000.0);
        value["projected_daily_unavailable_reason"] = Value::Null;
        value["observed_window_first_timestamp_ms"] = json!(1_700_000_000_000u64);
        value["observed_window_last_timestamp_ms"] = json!(1_700_000_000_001u64);
        let earnings = confirmed_earnings_from_value(&value)
            .expect("fixture represents an otherwise valid but over-eager host response");
        assert_eq!(earnings.projected_daily_arc, None);
        assert!(earnings
            .projected_daily_unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("at least 3")));

        let reason = projection_observation_gate(&value, 2)
            .expect("two rollout receipts must remain collecting data");
        assert!(reason.contains("collecting data"));
        assert!(reason.contains("at least 3"));
        assert!(reason.contains("at least 24 hours"));
    }

    #[test]
    fn desktop_rejects_a_zero_receipt_host_projection() {
        let mut value = candidate_earnings_value();
        value["total_rewards"] = json!(0);
        value["estimated_total_arc"] = json!(0.0);
        value["confirmed_receipt_count"] = json!(0);
        value["confirmed_gross_earnings_base"] = json!(0);
        value["confirmed_gross_earnings_arc"] = json!(0.0);
        value["confirmed_receipts"] = json!([]);
        value["last_reward_block"] = Value::Null;
        value["last_reward_tx_hash"] = Value::Null;
        value["projected_daily_arc"] = json!(216_000.0);
        value["projected_daily_unavailable_reason"] = Value::Null;

        let earnings = confirmed_earnings_from_value(&value)
            .expect("zero retained receipts are valid confirmed history, but not a projection");
        assert_eq!(earnings.attestations, 0);
        assert_eq!(earnings.projected_daily_arc, None);
        assert!(earnings
            .projected_daily_unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("at least 3")));
    }

    #[test]
    fn zero_one_and_two_exact_receipt_rows_all_keep_projection_unavailable() {
        for receipt_count in 0..=2 {
            let value = candidate_projection_value(receipt_count, MIN_PROJECTION_WINDOW_MS);
            let earnings = confirmed_earnings_from_value(&value)
                .expect("exact retained receipt history remains valid");
            assert_eq!(earnings.attestations, receipt_count as u64);
            assert_eq!(earnings.projected_daily_arc, None);
            assert!(earnings
                .projected_daily_unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("at least 3")));
        }
    }

    #[test]
    fn omitted_or_optimistic_receipt_counts_cannot_unlock_projection() {
        let mut omitted = candidate_projection_value(3, MIN_PROJECTION_WINDOW_MS);
        omitted
            .as_object_mut()
            .expect("fixture is an object")
            .remove("confirmed_receipt_count");
        assert!(confirmed_earnings_from_value(&omitted).is_none());

        let mut optimistic = candidate_projection_value(2, MIN_PROJECTION_WINDOW_MS);
        optimistic["total_rewards"] = json!(3);
        optimistic["confirmed_receipt_count"] = json!(3);
        optimistic["confirmed_gross_earnings_base"] = json!(7_500_000_000u64);
        optimistic["confirmed_gross_earnings_arc"] = json!(7.5);
        assert!(confirmed_earnings_from_value(&optimistic).is_none());
    }

    #[test]
    fn desktop_requires_a_full_day_even_after_the_minimum_receipt_count() {
        let value = json!({
            "observed_window_first_timestamp_ms": 1_700_000_000_000u64,
            "observed_window_last_timestamp_ms": 1_700_003_600_000u64,
        });
        let reason = projection_observation_gate(&value, 3)
            .expect("a one-hour window must not become a per-day forecast");
        assert!(reason.contains("at least 24 hours"));

        let day = json!({
            "observed_window_first_timestamp_ms": 1_700_000_000_000u64,
            "observed_window_last_timestamp_ms": 1_700_086_400_000u64,
        });
        assert_eq!(projection_observation_gate(&day, 3), None);

        let earnings = confirmed_earnings_from_value(&candidate_projection_value(3, 3_600_000))
            .expect("exact receipts remain valid while their projection is gated");
        assert_eq!(earnings.projected_daily_arc, None);
        assert!(earnings
            .projected_daily_unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("at least 24 hours")));
    }

    #[test]
    fn three_exact_receipts_over_a_full_day_can_retain_the_host_projection() {
        let mut value = candidate_earnings_value();
        value["total_rewards"] = json!(3);
        value["estimated_total_arc"] = json!(7.5);
        value["confirmed_receipt_count"] = json!(3);
        value["confirmed_gross_earnings_base"] = json!(7_500_000_000u64);
        value["confirmed_gross_earnings_arc"] = json!(7.5);
        value["confirmed_receipts"]
            .as_array_mut()
            .expect("receipt fixture is an array")
            .push(json!({
                "tx_type": "0x25",
                "tx_hash": format!("0x{}", "ac".repeat(32)),
                "job_id": format!("0x{}", "03".repeat(32)),
                "block_height": 123_463,
                "block_hash": format!("0x{}", "12".repeat(32)),
                "success": true,
                "reward_base": 2_500_000_000u64,
                "reward_arc": 2.5,
                "recovery_epoch": 1,
                "validator_set_id": 7,
            }));
        value["projected_daily_arc"] = json!(7.5);
        value["projected_daily_unavailable_reason"] = Value::Null;
        value["observed_window_first_timestamp_ms"] = json!(1_700_000_000_000u64);
        value["observed_window_last_timestamp_ms"] = json!(1_700_086_400_000u64);

        let earnings = confirmed_earnings_from_value(&value)
            .expect("three exact receipts and a full-day window satisfy the local gate");
        assert_eq!(earnings.attestations, 3);
        assert_eq!(earnings.projected_daily_arc, Some(7.5));
        assert_eq!(earnings.projected_daily_unavailable_reason, None);
    }

    #[test]
    fn public_v2_count_times_constant_body_is_not_confirmed_earnings() {
        let legacy = json!({
            "total_attestations": 7,
            "total_arc": 17.5,
            "today_arc": 2.5,
            "last_attestation_block": 123_462,
        });
        assert!(confirmed_earnings_from_value(&legacy).is_none());
    }

    #[test]
    fn candidate_shape_without_all_readiness_fields_fails_closed() {
        let mut value = candidate_earnings_value();
        value
            .as_object_mut()
            .unwrap()
            .remove("community_rewards_v1_approval_collection_ready");
        assert!(confirmed_earnings_from_value(&value).is_none());
    }

    #[test]
    fn candidate_shape_with_negative_reward_total_fails_closed() {
        let mut value = candidate_earnings_value();
        value["confirmed_gross_earnings_arc"] = json!(-5.0);
        assert!(confirmed_earnings_from_value(&value).is_none());
    }

    #[test]
    fn internally_inconsistent_effective_reward_gate_fails_closed() {
        let mut value = candidate_earnings_value();
        value["community_rewards_v1_approval_collection_ready"] = json!(false);
        assert!(confirmed_earnings_from_value(&value).is_none());
    }

    #[test]
    fn positive_reward_count_requires_last_successful_receipt_evidence() {
        let mut value = candidate_earnings_value();
        value["last_reward_tx_hash"] = Value::Null;
        assert!(confirmed_earnings_from_value(&value).is_none());
    }

    #[test]
    fn failed_or_non_reward_rows_cannot_be_parsed_as_earnings() {
        let mut failed = candidate_earnings_value();
        failed["confirmed_receipts"][0]["success"] = json!(false);
        assert!(confirmed_earnings_from_value(&failed).is_none());

        let mut wrong_type = candidate_earnings_value();
        wrong_type["confirmed_receipts"][0]["tx_type"] = json!("0x16");
        assert!(confirmed_earnings_from_value(&wrong_type).is_none());
    }

    #[test]
    fn reward_amounts_and_receipt_hashes_are_exact_and_fail_closed() {
        let mut wrong_base = candidate_earnings_value();
        wrong_base["confirmed_receipts"][0]["reward_base"] = json!(2_499_999_999u64);
        wrong_base["confirmed_gross_earnings_base"] = json!(4_999_999_999u64);
        assert!(confirmed_earnings_from_value(&wrong_base).is_none());

        let mut wrong_arc = candidate_earnings_value();
        wrong_arc["confirmed_receipts"][0]["reward_arc"] = json!(2.49);
        wrong_arc["confirmed_gross_earnings_arc"] = json!(4.99);
        assert!(confirmed_earnings_from_value(&wrong_arc).is_none());

        for field in ["tx_hash", "job_id", "block_hash"] {
            let mut malformed = candidate_earnings_value();
            malformed["confirmed_receipts"][0][field] = json!("0xdeadbeef");
            assert!(
                confirmed_earnings_from_value(&malformed).is_none(),
                "malformed {field} was accepted"
            );
        }
    }

    #[test]
    fn receipt_count_and_exact_base_sum_must_reconcile() {
        let mut count_mismatch = candidate_earnings_value();
        count_mismatch["confirmed_receipt_count"] = json!(1);
        assert!(confirmed_earnings_from_value(&count_mismatch).is_none());

        let mut amount_mismatch = candidate_earnings_value();
        amount_mismatch["confirmed_gross_earnings_base"] = json!(4_999_999_999u64);
        assert!(confirmed_earnings_from_value(&amount_mismatch).is_none());
    }

    #[test]
    fn duplicate_receipt_or_job_identity_fails_closed() {
        let mut duplicate_tx = candidate_earnings_value();
        let first_tx = duplicate_tx["confirmed_receipts"][0]["tx_hash"].clone();
        duplicate_tx["confirmed_receipts"][1]["tx_hash"] = first_tx;
        assert!(confirmed_earnings_from_value(&duplicate_tx).is_none());

        let mut duplicate_job = candidate_earnings_value();
        let first_job = duplicate_job["confirmed_receipts"][0]["job_id"].clone();
        duplicate_job["confirmed_receipts"][1]["job_id"] = first_job;
        assert!(confirmed_earnings_from_value(&duplicate_job).is_none());
    }

    #[test]
    fn unmined_daily_estimates_and_ambiguous_projections_fail_closed() {
        let mut invented_today = candidate_earnings_value();
        invented_today["today_arc"] = json!(2.5);
        assert!(confirmed_earnings_from_value(&invented_today).is_none());

        let mut both_projection_states = candidate_earnings_value();
        both_projection_states["projected_daily_arc"] = json!(1.0);
        assert!(confirmed_earnings_from_value(&both_projection_states).is_none());

        let mut malformed_projection = candidate_earnings_value();
        malformed_projection["projected_daily_arc"] = json!("1.0");
        assert!(confirmed_earnings_from_value(&malformed_projection).is_none());
    }

    #[test]
    fn zero_candidate_receipts_allow_null_last_receipt_but_remain_confirmed_zero() {
        let mut value = candidate_earnings_value();
        value["total_rewards"] = json!(0);
        value["estimated_total_arc"] = json!(0.0);
        value["confirmed_receipt_count"] = json!(0);
        value["confirmed_gross_earnings_base"] = json!(0);
        value["confirmed_gross_earnings_arc"] = json!(0.0);
        value["confirmed_receipts"] = json!([]);
        value["last_reward_block"] = Value::Null;
        value["last_reward_tx_hash"] = Value::Null;
        let earnings = confirmed_earnings_from_value(&value)
            .expect("explicit candidate zero is distinct from legacy ambiguity");
        assert!(earnings.from_chain);
        assert_eq!(earnings.total_arc, 0.0);
        assert_eq!(earnings.today_arc, None);
        assert_eq!(earnings.unavailable_reason, None);
    }

    #[test]
    fn retained_zero_is_distinct_from_malformed_or_unreachable_earnings() {
        let mut zero = candidate_earnings_value();
        zero["total_rewards"] = json!(0);
        zero["estimated_total_arc"] = json!(0.0);
        zero["confirmed_receipt_count"] = json!(0);
        zero["confirmed_gross_earnings_base"] = json!(0);
        zero["confirmed_gross_earnings_arc"] = json!(0.0);
        zero["confirmed_receipts"] = json!([]);
        zero["last_reward_block"] = Value::Null;
        zero["last_reward_tx_hash"] = Value::Null;

        let confirmed = earnings_from_fetched(
            "https://seed.example",
            "/worker/earnings/abc",
            Fetched::Ok(zero),
        );
        assert!(confirmed.from_chain);
        assert_eq!(confirmed.total_arc, 0.0);
        assert_eq!(confirmed.unavailable_reason, None);

        let malformed = earnings_from_fetched(
            "https://seed.example",
            "/worker/earnings/abc",
            Fetched::Ok(json!({"total_attestations": 0, "total_arc": 0.0})),
        );
        assert!(!malformed.from_chain);
        assert_eq!(malformed.total_arc, 0.0);
        assert!(malformed
            .unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("malformed")));

        let unreachable = earnings_from_fetched(
            "https://seed.example",
            "/worker/earnings/abc",
            Fetched::Unreachable("connection refused".to_string()),
        );
        assert!(!unreachable.from_chain);
        let reason = unreachable
            .unavailable_reason
            .expect("transport failure must remain visible");
        assert!(reason.contains("Could not reach"));
        assert!(reason.contains("connection refused"));
    }

    #[test]
    fn recent_block_range_anchors_to_the_tip_not_to_genesis() {
        // The bug this guards: `/blocks?limit=10` with no `from` returns
        // heights 0..9, whose timestamps are 0.
        assert_eq!(recent_block_range(123_469, 10), (123_460, 123_469));
        assert_eq!(recent_block_range(123_469, 1), (123_469, 123_469));
    }

    #[test]
    fn recent_block_range_clamps_at_genesis_on_a_short_chain() {
        // A chain shorter than the window must not underflow to u64::MAX.
        assert_eq!(recent_block_range(3, 10), (0, 3));
        assert_eq!(recent_block_range(0, 10), (0, 0));
    }

    #[test]
    fn recent_block_range_treats_a_zero_limit_as_one() {
        assert_eq!(recent_block_range(500, 0), (500, 500));
    }

    #[test]
    fn pick_ignores_an_explicit_json_null() {
        // `/worker/earnings` sends `today_arc: null` deliberately.
        let v = json!({ "today_arc": Value::Null });
        assert_eq!(pick_f64(&v, &["today_arc"]), None);
    }

    #[test]
    fn protocol_v3_signing_domain_is_required_and_exact() {
        let domain = "ab".repeat(32);
        let parsed = transaction_signing_domain_from_value(
            &json!({
                "protocol_version": "3.0.0",
                "recovery_active": true,
                "recovery_domain": format!("0x{domain}"),
            }),
            "https://seed.example.com",
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.to_hex(), domain);

        for bad in [
            json!({"protocol_version":"3.0.0", "recovery_active":true, "recovery_domain":null}),
            json!({"protocol_version":"3.0.0", "recovery_active":true, "recovery_domain":"0x00"}),
            json!({"protocol_version":"3.0.0", "recovery_active":true, "recovery_domain":format!("0x{}", "00".repeat(32))}),
            json!({"protocol_version":"3.0.0", "recovery_active":false, "recovery_domain":null}),
        ] {
            assert!(
                transaction_signing_domain_from_value(&bad, "https://seed.example.com").is_err()
            );
        }
    }

    #[test]
    fn legacy_domain_is_allowed_only_when_explicitly_inactive() {
        assert_eq!(
            transaction_signing_domain_from_value(
                &json!({
                    "protocol_version": "2.0.0",
                    "recovery_active": false,
                    "recovery_domain": null,
                }),
                "http://127.0.0.1:9090",
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn wallet_result_never_calls_submission_a_confirmation() {
        let pending = wallet_result_from_lookup(
            "aa".repeat(32),
            1_000_000_000,
            TxLookup {
                source_host: "https://seed.example.com".to_string(),
                unavailable: None,
                hash: "aa".repeat(32),
                status: "not_found".to_string(),
                block_height: None,
                block_hash: None,
                tx_index: None,
                success: None,
                gas_used: None,
            },
            "Transfer",
        );
        assert_eq!(pending.amount_base, "1000000000");
        assert_eq!(pending.amount_arc, "1");
        assert_eq!(pending.receipt_status, "pending");
        assert!(!pending.mined);
        assert_eq!(pending.success, None);
        assert!(!pending.message.to_ascii_lowercase().contains("confirmed"));
    }

    #[test]
    fn only_a_successful_mined_receipt_is_confirmed() {
        for (success, expected) in [(true, "mined_success"), (false, "mined_failed")] {
            let result = wallet_result_from_lookup(
                "bb".repeat(32),
                10_000,
                TxLookup {
                    source_host: "https://seed.example.com".to_string(),
                    unavailable: None,
                    hash: "bb".repeat(32),
                    status: "mined".to_string(),
                    block_height: Some(42),
                    block_hash: Some("cc".repeat(32)),
                    tx_index: Some(1),
                    success: Some(success),
                    gas_used: Some(21_000),
                },
                "Faucet claim",
            );
            assert!(result.mined);
            assert_eq!(result.receipt_status, expected);
            assert_eq!(result.amount_arc, "0.00001");
            assert_eq!(result.success, Some(success));
            assert_eq!(result.block_height, Some(42));
        }
    }
}
