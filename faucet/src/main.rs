//! ARC Chain Testnet Faucet
//!
//! A simple HTTP service that distributes test ARC tokens to developers.
//! Rate-limited to 1 claim per address per hour.

use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

/// Faucet account index — deterministic derivation from the same genesis seed.
/// Index 99 is reserved for the faucet.
const FAUCET_ACCOUNT_INDEX: u8 = 99;

/// Amount of ARC tokens per claim.
const CLAIM_AMOUNT: u64 = 1000;

/// Rate limit window (1 hour).
const RATE_LIMIT_SECS: u64 = 3600;

/// Derive the faucet address deterministically (same as genesis accounts in arc-bench).
fn faucet_address() -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[FAUCET_ACCOUNT_INDEX]);
    let hash = hasher.finalize();
    hex::encode(hash.as_bytes())
}

/// Hex encoding helper (inline to avoid another dependency).
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

#[derive(Parser)]
#[command(name = "arc-faucet", about = "ARC Chain testnet faucet")]
struct Cli {
    /// ARC node RPC URL
    #[arg(long, default_value = "http://localhost:9090")]
    node_url: String,

    /// Faucet HTTP listen port
    #[arg(long, default_value = "3001")]
    port: u16,
}

/// Shared application state.
struct AppState {
    /// Node RPC URL.
    node_url: String,
    /// Rate limit tracker: address -> last claim time.
    claims: Mutex<HashMap<String, Instant>>,
    /// Faucet wallet address.
    faucet_address: String,
    /// Total claims today (resets on restart).
    total_claims: Mutex<u64>,
    /// Current nonce for the faucet account (incremented per claim).
    nonce: Mutex<u64>,
    /// HTTP client for RPC calls.
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct ClaimRequest {
    address: String,
}

#[derive(Serialize)]
struct ClaimResponse {
    tx_hash: String,
    amount: u64,
    message: String,
}


#[derive(Serialize)]
struct StatusResponse {
    address: String,
    node_url: String,
    claims_today: u64,
    claim_amount: u64,
    rate_limit_secs: u64,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    faucet_address: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    let cli = Cli::parse();
    let addr = faucet_address();

    tracing::info!("ARC Chain Faucet starting");
    tracing::info!("  Node URL: {}", cli.node_url);
    tracing::info!("  Faucet address: {}", addr);
    tracing::info!("  Claim amount: {} ARC", CLAIM_AMOUNT);
    tracing::info!("  Rate limit: 1 claim per {} seconds", RATE_LIMIT_SECS);

    let state = Arc::new(AppState {
        node_url: cli.node_url,
        claims: Mutex::new(HashMap::new()),
        faucet_address: addr,
        total_claims: Mutex::new(0),
        nonce: Mutex::new(0),
        http: reqwest::Client::new(),
    });

    let app = Router::new()
        .route("/", get(index_page))
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/claim", post(claim))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listen_addr = format!("0.0.0.0:{}", cli.port);
    tracing::info!("Listening on {}", listen_addr);

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .expect("Failed to bind");

    axum::serve(listener, app).await.expect("Server error");
}

/// GET /health
async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        faucet_address: state.faucet_address.clone(),
    })
}

/// GET /status
async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let claims_today = *state.total_claims.lock().await;
    Json(StatusResponse {
        address: state.faucet_address.clone(),
        node_url: state.node_url.clone(),
        claims_today,
        claim_amount: CLAIM_AMOUNT,
        rate_limit_secs: RATE_LIMIT_SECS,
    })
}

/// POST /claim
async fn claim(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ClaimRequest>,
) -> impl IntoResponse {
    let address = req.address.trim().to_lowercase();

    // Validate address format (64 hex chars)
    if address.len() != 64 || !address.chars().all(|c| c.is_ascii_hexdigit()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Invalid address. Must be 64 hex characters."
            })),
        );
    }

    // Check rate limit
    {
        let mut claims = state.claims.lock().await;
        if let Some(last_claim) = claims.get(&address) {
            let elapsed = last_claim.elapsed();
            if elapsed < Duration::from_secs(RATE_LIMIT_SECS) {
                let remaining = RATE_LIMIT_SECS - elapsed.as_secs();
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(serde_json::json!({
                        "error": format!(
                            "Rate limited. Try again in {} minutes.",
                            remaining / 60 + 1
                        )
                    })),
                );
            }
        }
        // Record this claim
        claims.insert(address.clone(), Instant::now());
    }

    // Get next nonce
    let nonce = {
        let mut n = state.nonce.lock().await;
        let current = *n;
        *n += 1;
        current
    };

    // Submit transfer transaction to the node
    let payload = serde_json::json!({
        "from": state.faucet_address,
        "to": address,
        "amount": CLAIM_AMOUNT,
        "nonce": nonce
    });

    let url = format!("{}/tx/submit", state.node_url);
    let result = state.http.post(&url).json(&payload).send().await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let tx_hash = body["tx_hash"].as_str().unwrap_or("unknown").to_string();

            // Increment total claims
            {
                let mut total = state.total_claims.lock().await;
                *total += 1;
            }

            tracing::info!("Claimed {} ARC -> {} (tx: {})", CLAIM_AMOUNT, &address[..16], &tx_hash[..16.min(tx_hash.len())]);

            (
                StatusCode::OK,
                Json(serde_json::json!(ClaimResponse {
                    tx_hash,
                    amount: CLAIM_AMOUNT,
                    message: format!("{} ARC sent!", CLAIM_AMOUNT),
                })),
            )
        }
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            tracing::error!("Node returned {}: {}", status, text);

            // Roll back the nonce on failure
            {
                let mut n = state.nonce.lock().await;
                if *n > 0 {
                    *n -= 1;
                }
            }

            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("Node error: {}", text)
                })),
            )
        }
        Err(e) => {
            tracing::error!("Failed to connect to node: {}", e);

            // Roll back nonce
            {
                let mut n = state.nonce.lock().await;
                if *n > 0 {
                    *n -= 1;
                }
            }

            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "Failed to connect to ARC node. Is it running?"
                })),
            )
        }
    }
}

/// GET / — Simple HTML faucet page.
async fn index_page(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ARC Testnet Faucet</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    background: #03030A;
    color: #fff;
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
  }}
  .container {{
    max-width: 480px;
    width: 100%;
    padding: 2rem;
  }}
  h1 {{
    font-size: 1.5rem;
    font-weight: 500;
    margin-bottom: 0.5rem;
  }}
  h1 span {{ color: #6F7CF4; }}
  .subtitle {{
    color: #8E8E9D;
    font-size: 0.875rem;
    margin-bottom: 2rem;
  }}
  label {{
    display: block;
    font-size: 0.75rem;
    color: #8E8E9D;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    margin-bottom: 0.5rem;
  }}
  input {{
    width: 100%;
    padding: 0.75rem 1rem;
    background: #0A0A14;
    border: 1px solid #1E1E2E;
    color: #fff;
    font-family: 'SF Mono', 'Fira Code', monospace;
    font-size: 0.8125rem;
    outline: none;
    transition: border-color 0.15s;
  }}
  input:focus {{ border-color: #6F7CF4; }}
  input::placeholder {{ color: #777785; }}
  button {{
    width: 100%;
    margin-top: 1rem;
    padding: 0.75rem;
    background: #fff;
    color: #03030A;
    border: 1px solid #03030A;
    font-size: 0.875rem;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.15s;
  }}
  button:hover {{ background: #03030A; color: #fff; border-color: #fff; }}
  button:disabled {{ opacity: 0.5; cursor: not-allowed; }}
  .result {{
    margin-top: 1.5rem;
    padding: 1rem;
    font-size: 0.8125rem;
    border: 1px solid #1E1E2E;
    background: #0A0A14;
    display: none;
  }}
  .result.success {{ border-color: #51EB8E; }}
  .result.error {{ border-color: #FF0040; }}
  .result .label {{ color: #8E8E9D; font-size: 0.6875rem; text-transform: uppercase; }}
  .result .value {{ color: #fff; font-family: monospace; word-break: break-all; margin-top: 0.25rem; }}
  .info {{
    margin-top: 2rem;
    padding-top: 1.5rem;
    border-top: 1px solid #1E1E2E;
    font-size: 0.75rem;
    color: #777785;
  }}
  .info p {{ margin-bottom: 0.25rem; }}
</style>
</head>
<body>
<div class="container">
  <h1><span>ARC</span> Testnet Faucet</h1>
  <p class="subtitle">Request {amount} test ARC tokens per hour</p>

  <form id="faucet-form" onsubmit="claim(event)">
    <label for="address">Wallet Address</label>
    <input
      type="text"
      id="address"
      name="address"
      placeholder="64-character hex address"
      maxlength="64"
      required
    />
    <button type="submit" id="btn">Request Tokens</button>
  </form>

  <div id="result" class="result">
    <div class="label">Transaction Hash</div>
    <div class="value" id="result-text"></div>
  </div>

  <div class="info">
    <p>Faucet address: <code>{faucet_addr_short}...</code></p>
    <p>Claim amount: {amount} ARC</p>
    <p>Rate limit: 1 claim per hour per address</p>
  </div>
</div>

<script>
async function claim(e) {{
  e.preventDefault();
  const btn = document.getElementById('btn');
  const result = document.getElementById('result');
  const resultText = document.getElementById('result-text');
  const address = document.getElementById('address').value.trim();

  btn.disabled = true;
  btn.textContent = 'Sending...';
  result.style.display = 'none';

  try {{
    const res = await fetch('/claim', {{
      method: 'POST',
      headers: {{ 'Content-Type': 'application/json' }},
      body: JSON.stringify({{ address }}),
    }});
    const data = await res.json();
    result.style.display = 'block';
    if (res.ok) {{
      result.className = 'result success';
      resultText.textContent = data.tx_hash || data.message;
    }} else {{
      result.className = 'result error';
      resultText.textContent = data.error || 'Unknown error';
    }}
  }} catch (err) {{
    result.style.display = 'block';
    result.className = 'result error';
    resultText.textContent = 'Network error: ' + err.message;
  }} finally {{
    btn.disabled = false;
    btn.textContent = 'Request Tokens';
  }}
}}
</script>
</body>
</html>"#,
        amount = CLAIM_AMOUNT,
        faucet_addr_short = &state.faucet_address[..16],
    ))
}
