//! Read-only GETs for the live network panel (`arc.network-stats.v1`).
//!
//! The webview's CSP cannot reach the validators, so the panel asks this
//! module for one typed read at a time. The path is built here from typed
//! fields, never from a caller's string, so the panel can issue exactly these
//! five GETs:
//!
//! | read         | path                                  |
//! |--------------|---------------------------------------|
//! | `health`     | `/health`                             |
//! | `finality`   | `/finality/latest`                    |
//! | `blocks`     | `/blocks?from=F&to=T&limit=N` (N ≤ 100) |
//! | `scoreboard` | `/workers/scoreboard?limit=0`         |
//! | `twinStats`  | `/community/twin_stats`               |
//!
//! It can never reach `GET /community/list`, whose handler prunes the worker
//! registry as a side effect, and it never sends anything but a GET. The
//! scoreboard read asks for zero worker rows, so no worker names come back.
//!
//! Scheduling, backoff, failover and every statistic live in the TypeScript
//! poller (desktop/src/lib/network-stats/). This module adds one native
//! backstop on top: a read budget that caps the panel at a short burst and a
//! low sustained rate across all validators, so a UI bug cannot hammer them.

use crate::rpc_client::PRODUCTION_RPC_ORIGINS;
use crate::AppState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;
use std::time::Instant;
use tauri::State;

/// City labels, index-aligned with `PRODUCTION_RPC_ORIGINS` (and with
/// `PUBLIC_VALIDATORS` in desktop/src/lib/network-stats/read.ts).
pub(crate) const VALIDATOR_LABELS: [&str; 6] = ["NYC", "LAX", "AMS", "LHR", "NRT", "SGP"];

/// `GET /blocks` returns at most 100 blocks per call (arc-node caps `limit`).
pub(crate) const MAX_BLOCKS_PER_READ: u64 = 100;

/// Bodies larger than this are refused, not parsed. 100 block headers are
/// about 30 KB.
pub(crate) const MAX_BODY_BYTES: usize = 512 * 1024;

/// Read budget: a burst of this many reads...
pub(crate) const BUDGET_BURST: f64 = 30.0;
/// ...refilled at this many reads per second across all validators. The
/// panel's schedule needs about one read per second at start-up and well
/// under one per second after that.
pub(crate) const BUDGET_PER_SECOND: f64 = 2.0;

/// One read the panel may ask for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LiveRead {
    Health { validator: u8 },
    Finality { validator: u8 },
    Blocks { validator: u8, from: u64, to: u64 },
    Scoreboard { validator: u8 },
    TwinStats { validator: u8 },
}

impl LiveRead {
    pub(crate) fn validator(&self) -> u8 {
        match self {
            Self::Health { validator }
            | Self::Finality { validator }
            | Self::Blocks { validator, .. }
            | Self::Scoreboard { validator }
            | Self::TwinStats { validator } => *validator,
        }
    }

    /// The exact path this read requests, or why the request is malformed.
    pub(crate) fn path(&self) -> Result<String, String> {
        match self {
            Self::Health { .. } => Ok("/health".to_string()),
            Self::Finality { .. } => Ok("/finality/latest".to_string()),
            Self::Blocks { from, to, .. } => {
                if from > to {
                    return Err(format!("block range {from}-{to} is reversed"));
                }
                let span = to - from;
                if span >= MAX_BLOCKS_PER_READ {
                    return Err(format!(
                        "block range {from}-{to} asks for more than {MAX_BLOCKS_PER_READ} blocks"
                    ));
                }
                Ok(format!("/blocks?from={from}&to={to}&limit={}", span + 1))
            }
            Self::Scoreboard { .. } => Ok("/workers/scoreboard?limit=0".to_string()),
            Self::TwinStats { .. } => Ok("/community/twin_stats".to_string()),
        }
    }
}

/// What happened to one read. A 404 is kept apart from a failure: it says the
/// validator does not serve that path, which is the expected answer for
/// endpoints newer than the deployed release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Outcome {
    #[serde(rename = "ok")]
    Answered,
    #[serde(rename = "notFound")]
    NotFound,
    #[serde(rename = "badRequest")]
    BadRequest,
    #[serde(rename = "status")]
    OtherStatus,
    #[serde(rename = "unreachable")]
    Unreachable,
    #[serde(rename = "unparseable")]
    Unparseable,
    #[serde(rename = "tooLarge")]
    TooLarge,
    #[serde(rename = "throttled")]
    Throttled,
}

/// Mirrors `LiveReadResult` in desktop/src/lib/network-stats/read.ts.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveReadResult {
    pub validator: u8,
    pub label: String,
    pub origin: String,
    pub path: String,
    pub outcome: Outcome,
    pub http_status: Option<u16>,
    /// The parsed JSON body; present only when `outcome` is `ok`.
    pub body: Option<Value>,
    /// What went wrong, as observed.
    pub detail: Option<String>,
    pub fetched_at_unix_ms: u64,
    pub elapsed_ms: u64,
}

/// A token bucket: `BUDGET_BURST` reads at once, refilled at
/// `BUDGET_PER_SECOND`.
#[derive(Debug)]
pub(crate) struct ReadBudget {
    tokens: f64,
    last: Option<Instant>,
}

impl ReadBudget {
    pub(crate) const fn full() -> Self {
        Self {
            tokens: BUDGET_BURST,
            last: None,
        }
    }

    pub(crate) fn try_take(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last {
            let elapsed = now.saturating_duration_since(last).as_secs_f64();
            self.tokens = (self.tokens + elapsed * BUDGET_PER_SECOND).min(BUDGET_BURST);
        }
        self.last = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

static READ_BUDGET: Mutex<ReadBudget> = Mutex::new(ReadBudget::full());

fn take_read_budget() -> bool {
    let mut budget = READ_BUDGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    budget.try_take(Instant::now())
}

fn now_unix_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Classify a finished response. `body` is `None` for non-2xx statuses,
/// whose bodies are not read.
pub(crate) fn classify(
    status: u16,
    body: Option<&[u8]>,
) -> (Outcome, Option<Value>, Option<String>) {
    if (200..300).contains(&status) {
        return match body.map(|bytes| serde_json::from_slice::<Value>(bytes)) {
            Some(Ok(value)) => (Outcome::Answered, Some(value), None),
            Some(Err(error)) => (Outcome::Unparseable, None, Some(error.to_string())),
            None => (
                Outcome::Unparseable,
                None,
                Some("empty response".to_string()),
            ),
        };
    }
    match status {
        404 => (Outcome::NotFound, None, None),
        400 => (Outcome::BadRequest, None, None),
        _ => (Outcome::OtherStatus, None, None),
    }
}

enum BodyError {
    TooLarge,
    Transport(String),
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| BodyError::Transport(error.to_string()))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// One GET of `origin` + `path`. Never retries and never follows a redirect
/// (the shared desktop client is built that way in lib.rs).
pub(crate) async fn read_from_origin(
    http: &reqwest::Client,
    validator: u8,
    label: &str,
    origin: &str,
    path: &str,
) -> LiveReadResult {
    let started = Instant::now();
    let (outcome, http_status, body, detail) =
        match http.get(format!("{origin}{path}")).send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                if response.status().is_success() {
                    match read_capped(response).await {
                        Ok(bytes) => {
                            let (outcome, body, detail) =
                                classify(status, Some(bytes.as_slice()));
                            (outcome, Some(status), body, detail)
                        }
                        Err(BodyError::TooLarge) => (
                            Outcome::TooLarge,
                            Some(status),
                            None,
                            Some(format!("body over {MAX_BODY_BYTES} bytes")),
                        ),
                        Err(BodyError::Transport(error)) => {
                            (Outcome::Unreachable, Some(status), None, Some(error))
                        }
                    }
                } else {
                    let (outcome, body, detail) = classify(status, None);
                    (outcome, Some(status), body, detail)
                }
            }
            Err(error) => (Outcome::Unreachable, None, None, Some(error.to_string())),
        };
    LiveReadResult {
        validator,
        label: label.to_string(),
        origin: origin.to_string(),
        path: path.to_string(),
        outcome,
        http_status,
        body,
        detail,
        fetched_at_unix_ms: now_unix_ms(),
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    }
}

/// Resolve the read to a compiled-in public validator and its path.
pub(crate) fn resolve(
    request: &LiveRead,
) -> Result<(u8, &'static str, &'static str, String), String> {
    let validator = request.validator();
    let index = usize::from(validator);
    let origin: &'static str = PRODUCTION_RPC_ORIGINS
        .get(index)
        .copied()
        .ok_or_else(|| format!("unknown validator {validator}"))?;
    let label: &'static str = VALIDATOR_LABELS
        .get(index)
        .copied()
        .ok_or_else(|| format!("unknown validator {validator}"))?;
    Ok((validator, label, origin, request.path()?))
}

/// One read for the live network panel. Read-only, allowlisted, budgeted.
#[tauri::command]
pub async fn network_live_read(
    state: State<'_, AppState>,
    request: LiveRead,
) -> Result<LiveReadResult, String> {
    let (validator, label, origin, path) = resolve(&request)?;
    if !take_read_budget() {
        return Ok(LiveReadResult {
            validator,
            label: label.to_string(),
            origin: origin.to_string(),
            path,
            outcome: Outcome::Throttled,
            http_status: None,
            body: None,
            detail: Some("the live panel's read budget is used up for the moment".to_string()),
            fetched_at_unix_ms: now_unix_ms(),
            elapsed_ms: 0,
        });
    }
    Ok(read_from_origin(&state.http, validator, label, origin, &path).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn all_reads() -> Vec<LiveRead> {
        vec![
            LiveRead::Health { validator: 0 },
            LiveRead::Finality { validator: 1 },
            LiveRead::Blocks {
                validator: 2,
                from: 6_105_000,
                to: 6_105_099,
            },
            LiveRead::Scoreboard { validator: 3 },
            LiveRead::TwinStats { validator: 5 },
        ]
    }

    #[test]
    fn reads_deserialize_from_the_panel_request_shape() {
        let cases = [
            (
                serde_json::json!({"kind": "health", "validator": 0}),
                LiveRead::Health { validator: 0 },
            ),
            (
                serde_json::json!({"kind": "finality", "validator": 4}),
                LiveRead::Finality { validator: 4 },
            ),
            (
                serde_json::json!({"kind": "blocks", "validator": 1, "from": 10, "to": 19}),
                LiveRead::Blocks {
                    validator: 1,
                    from: 10,
                    to: 19,
                },
            ),
            (
                serde_json::json!({"kind": "scoreboard", "validator": 2}),
                LiveRead::Scoreboard { validator: 2 },
            ),
            (
                serde_json::json!({"kind": "twinStats", "validator": 5}),
                LiveRead::TwinStats { validator: 5 },
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(serde_json::from_value::<LiveRead>(json).unwrap(), expected);
        }
        // Anything else is not a read the panel can make.
        for json in [
            serde_json::json!({"kind": "communityList", "validator": 0}),
            serde_json::json!({"kind": "path", "validator": 0, "path": "/community/list"}),
            serde_json::json!({"kind": "health", "validator": 300}),
            serde_json::json!({"validator": 0}),
        ] {
            assert!(serde_json::from_value::<LiveRead>(json).is_err());
        }
    }

    #[test]
    fn paths_are_built_from_typed_reads_and_never_reach_community_list() {
        let paths: Vec<String> = all_reads().iter().map(|r| r.path().unwrap()).collect();
        assert_eq!(
            paths,
            vec![
                "/health",
                "/finality/latest",
                "/blocks?from=6105000&to=6105099&limit=100",
                "/workers/scoreboard?limit=0",
                "/community/twin_stats",
            ]
        );
        assert!(paths.iter().all(|p| !p.contains("/community/list")));
    }

    #[test]
    fn block_ranges_are_bounded_to_one_hundred_blocks() {
        let read = |from, to| LiveRead::Blocks {
            validator: 0,
            from,
            to,
        };
        assert_eq!(
            read(7, 7).path().unwrap(),
            "/blocks?from=7&to=7&limit=1".to_string()
        );
        assert!(read(10, 9).path().is_err());
        assert!(read(0, 100).path().is_err());
        assert!(read(0, u64::MAX).path().is_err());
    }

    #[test]
    fn only_the_six_compiled_validators_resolve() {
        let (validator, label, origin, path) =
            resolve(&LiveRead::Scoreboard { validator: 1 }).unwrap();
        assert_eq!(
            (validator, label, origin, path.as_str()),
            (
                1,
                "LAX",
                "https://140.82.16.112",
                "/workers/scoreboard?limit=0"
            )
        );
        assert_eq!(VALIDATOR_LABELS.len(), PRODUCTION_RPC_ORIGINS.len());
        assert!(resolve(&LiveRead::Health { validator: 6 }).is_err());
    }

    #[test]
    fn the_read_budget_allows_a_burst_then_refills_slowly() {
        let start = Instant::now();
        let mut budget = ReadBudget::full();
        for _ in 0..30 {
            assert!(budget.try_take(start));
        }
        assert!(!budget.try_take(start));
        // One second later: two more reads, then refused again.
        let later = start + Duration::from_secs(1);
        assert!(budget.try_take(later));
        assert!(budget.try_take(later));
        assert!(!budget.try_take(later));
        // Never above the burst, however long it sat idle.
        let much_later = later + Duration::from_secs(3_600);
        for _ in 0..30 {
            assert!(budget.try_take(much_later));
        }
        assert!(!budget.try_take(much_later));
    }

    #[test]
    fn a_missing_endpoint_is_not_reported_as_a_failure_or_a_zero() {
        let (outcome, body, _) = classify(404, None);
        assert_eq!(outcome, Outcome::NotFound);
        assert!(body.is_none());

        let answered: &[u8] = br#"{"eligible_inference_workers":3}"#;
        let (outcome, body, _) = classify(200, Some(answered));
        assert_eq!(outcome, Outcome::Answered);
        assert_eq!(body.unwrap()["eligible_inference_workers"], 3);

        let page: &[u8] = b"<html>";
        assert_eq!(classify(200, Some(page)).0, Outcome::Unparseable);
        assert_eq!(classify(400, None).0, Outcome::BadRequest);
        assert_eq!(classify(503, None).0, Outcome::OtherStatus);
    }

    #[test]
    fn outcomes_serialize_to_the_panel_names() {
        let names: Vec<String> = [
            Outcome::Answered,
            Outcome::NotFound,
            Outcome::BadRequest,
            Outcome::OtherStatus,
            Outcome::Unreachable,
            Outcome::Unparseable,
            Outcome::TooLarge,
            Outcome::Throttled,
        ]
        .iter()
        .map(|o| serde_json::to_value(o).unwrap().as_str().unwrap().to_string())
        .collect();
        assert_eq!(
            names,
            vec![
                "ok",
                "notFound",
                "badRequest",
                "status",
                "unreachable",
                "unparseable",
                "tooLarge",
                "throttled"
            ]
        );
    }

    /// Serve exactly one request, record its request line, and answer with
    /// `status` and `body`.
    async fn serve_once(
        status_line: &'static str,
        body: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let read = stream.read(&mut request).await.unwrap();
            let head = String::from_utf8_lossy(&request[..read]).to_string();
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            head.lines().next().unwrap_or_default().to_string()
        });
        (origin, server)
    }

    #[tokio::test]
    async fn a_read_is_one_get_of_the_built_path() {
        let (origin, server) = serve_once(
            "200 OK",
            br#"{"eligible_inference_workers":3,"workers":[]}"#.to_vec(),
        )
        .await;
        let result = read_from_origin(
            &reqwest::Client::new(),
            3,
            "LHR",
            &origin,
            "/workers/scoreboard?limit=0",
        )
        .await;
        assert_eq!(
            server.await.unwrap(),
            "GET /workers/scoreboard?limit=0 HTTP/1.1"
        );
        assert_eq!(result.outcome, Outcome::Answered);
        assert_eq!(result.http_status, Some(200));
        assert_eq!(result.body.unwrap()["eligible_inference_workers"], 3);
        assert_eq!((result.validator, result.label.as_str()), (3, "LHR"));
    }

    #[tokio::test]
    async fn a_404_comes_back_as_not_found_without_a_body() {
        let (origin, server) = serve_once("404 Not Found", b"{}".to_vec()).await;
        let result = read_from_origin(
            &reqwest::Client::new(),
            1,
            "LAX",
            &origin,
            "/community/twin_stats",
        )
        .await;
        assert_eq!(server.await.unwrap(), "GET /community/twin_stats HTTP/1.1");
        assert_eq!(result.outcome, Outcome::NotFound);
        assert_eq!(result.http_status, Some(404));
        assert!(result.body.is_none());
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_not_parsed() {
        let (origin, server) = serve_once("200 OK", vec![b' '; MAX_BODY_BYTES + 1]).await;
        let result =
            read_from_origin(&reqwest::Client::new(), 0, "NYC", &origin, "/health").await;
        // The server may still be writing the refused body; leave it.
        drop(server);
        assert_eq!(result.outcome, Outcome::TooLarge);
        assert!(result.body.is_none());
    }

    #[tokio::test]
    async fn an_unreachable_validator_is_reported_as_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let result = read_from_origin(
            &reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            4,
            "NRT",
            &origin,
            "/health",
        )
        .await;
        assert_eq!(result.outcome, Outcome::Unreachable);
        assert!(result.body.is_none());
        assert!(result.detail.is_some());
    }
}
