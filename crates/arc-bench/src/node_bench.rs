/// ARC Chain — Multi-Node Live Benchmark
///
/// Modes:
///   live     — Live dashboard + WebSocket coordinator
///   worker   — Benchmark worker (standalone HTTP or reporting to coordinator)
///   coord    — One-shot aggregation of worker nodes
///   local    — Run benchmark locally, output JSON
///
/// Usage:
///   arc-bench-node live [--port 8080]
///   arc-bench-node worker [--port 9090] [--coord http://coordinator:8080]
///   arc-bench-node coord --nodes http://node1:9090,http://node2:9090 [--n 1000000]
///   arc-bench-node local [--n 1000000]

use arc_crypto::*;
use arc_gpu::{cpu_batch_commit, gpu_batch_commit, probe_gpu};
use arc_state::StateDB;
use arc_types::*;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::HeaderMap,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;

// ─────────────────────────────────────────────────────────────────
//  Types
// ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct NodeResult {
    node_id: String,
    hostname: String,
    cpu_cores: usize,
    gpu_name: String,
    gpu_available: bool,
    cpu_hash_tps: f64,
    gpu_hash_tps: f64,
    state_exec_tps: f64,
    compact_pipeline_tps: f64,
    best_single_node_tps: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ClusterResult {
    nodes: Vec<NodeResult>,
    total_nodes: usize,
    combined_tps: f64,
    projected_128_nodes: f64,
    projected_256_nodes: f64,
    hits_1b: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct LiveSnapshot {
    total_tps: f64,
    node_count: usize,
    avg_node_tps: f64,
    nodes: Vec<NodeResult>,
    uptime_secs: f64,
    projected_1000_nodes: f64,
    hits_1b: bool,
}

#[derive(Deserialize)]
struct BenchParams {
    #[serde(default = "default_n")]
    n: usize,
}

fn default_n() -> usize {
    1_000_000
}

// ─────────────────────────────────────────────────────────────────
//  Live coordinator state
// ─────────────────────────────────────────────────────────────────

struct TrackedNode {
    result: NodeResult,
    last_seen: Instant,
}

struct LiveState {
    nodes: RwLock<HashMap<String, TrackedNode>>,
    start_time: Instant,
}

impl LiveState {
    fn new() -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
            start_time: Instant::now(),
        }
    }

    async fn snapshot(&self) -> LiveSnapshot {
        let nodes = self.nodes.read().await;
        let active: Vec<&TrackedNode> = nodes
            .values()
            .filter(|n| n.last_seen.elapsed() < Duration::from_secs(60))
            .collect();

        let total_tps: f64 = active.iter().map(|n| n.result.compact_pipeline_tps).sum();
        let node_count = active.len();
        let avg = if node_count > 0 {
            total_tps / node_count as f64
        } else {
            0.0
        };

        LiveSnapshot {
            total_tps,
            node_count,
            avg_node_tps: avg,
            nodes: active.iter().map(|n| n.result.clone()).collect(),
            uptime_secs: self.start_time.elapsed().as_secs_f64(),
            projected_1000_nodes: avg * 1000.0 * 0.88,
            hits_1b: avg * 1000.0 * 0.88 >= 1_000_000_000.0,
        }
    }
}

// ─────────────────────────────────────────────────────────────────
//  Benchmark logic
// ─────────────────────────────────────────────────────────────────

fn run_benchmark(n: usize, node_id: Option<String>) -> NodeResult {
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let gpu = probe_gpu();
    let num_cores = rayon::current_num_threads();
    let nid = node_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    println!("  Running benchmark (n={}) on {}...", n, hostname);

    // 1. CPU parallel BLAKE3
    let data: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut buf = vec![0u8; 256];
            buf[..8].copy_from_slice(&(i as u64).to_le_bytes());
            for j in (8..256).step_by(8) {
                let val = (i as u64)
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(j as u64);
                buf[j..j + 8].copy_from_slice(&val.to_le_bytes());
            }
            buf
        })
        .collect();
    let refs: Vec<&[u8]> = data.iter().map(|d| d.as_slice()).collect();

    let _ = cpu_batch_commit(&refs[..1000.min(n)]);
    let start = Instant::now();
    let _ = cpu_batch_commit(&refs);
    let cpu_hash_tps = n as f64 / start.elapsed().as_secs_f64();
    println!("    CPU BLAKE3:     {:>12.0} TPS", cpu_hash_tps);

    // 2. GPU BLAKE3
    let gpu_hash_tps = if gpu.available && n >= 5000 {
        let _ = gpu_batch_commit(&refs[..5000]);
        let start = Instant::now();
        match gpu_batch_commit(&refs) {
            Ok(_) => {
                let tps = n as f64 / start.elapsed().as_secs_f64();
                println!("    GPU BLAKE3:     {:>12.0} TPS", tps);
                tps
            }
            Err(e) => {
                println!("    GPU BLAKE3:     error: {e}");
                0.0
            }
        }
    } else {
        0.0
    };

    // 3. State execution
    let num_agents = 10_000u32;
    let agent_accounts: Vec<(Hash256, u64)> = (0..num_agents)
        .map(|i| (hash_bytes(&i.to_le_bytes()), u64::MAX / 2))
        .collect();

    let state = StateDB::with_genesis(&agent_accounts);
    let txs_per_agent = (n as u32) / num_agents;
    let actual_n = (txs_per_agent * num_agents) as usize;

    let transactions: Vec<Transaction> = (0..num_agents)
        .flat_map(|agent_id| {
            let from = hash_bytes(&agent_id.to_le_bytes());
            let to = hash_bytes(&((agent_id + 1) % num_agents).to_le_bytes());
            (0..txs_per_agent as u64).map(move |nonce| Transaction::new_transfer(from, to, 1, nonce))
        })
        .collect();

    let start = Instant::now();
    let (success, total) = state.execute_optimistic(&transactions);
    let state_exec_tps = actual_n as f64 / start.elapsed().as_secs_f64();
    println!(
        "    State exec:     {:>12.0} TPS  ({}/{})",
        state_exec_tps, success, total
    );

    // 4. Compact pipeline
    let start = Instant::now();

    let compact_txs: Vec<[u8; COMPACT_TX_SIZE]> = transactions
        .par_iter()
        .map(|tx| {
            if let TxBody::Transfer(ref body) = tx.body {
                CompactTransfer::new(tx.from, body.to, body.amount, tx.nonce).to_bytes()
            } else {
                [0u8; COMPACT_TX_SIZE]
            }
        })
        .collect();
    let compact_refs: Vec<&[u8]> = compact_txs.iter().map(|d| d.as_slice()).collect();

    if gpu_hash_tps > cpu_hash_tps {
        let _ = gpu_batch_commit(&compact_refs);
    } else {
        let _ = cpu_batch_commit(&compact_refs);
    }

    let state2 = StateDB::with_genesis(&agent_accounts);
    let (_, _) = state2.execute_optimistic(&transactions);

    let compact_pipeline_tps = actual_n as f64 / start.elapsed().as_secs_f64();
    println!("    Pipeline:       {:>12.0} TPS", compact_pipeline_tps);

    NodeResult {
        node_id: nid,
        hostname,
        cpu_cores: num_cores,
        gpu_name: gpu.name,
        gpu_available: gpu.available,
        cpu_hash_tps,
        gpu_hash_tps,
        state_exec_tps,
        compact_pipeline_tps,
        best_single_node_tps: compact_pipeline_tps,
    }
}

// ─────────────────────────────────────────────────────────────────
//  Standalone worker handlers (no shared state)
// ─────────────────────────────────────────────────────────────────

async fn handle_health() -> &'static str {
    "ok"
}

async fn handle_info() -> Json<serde_json::Value> {
    let gpu = probe_gpu();
    Json(serde_json::json!({
        "hostname": hostname::get().map(|h| h.to_string_lossy().to_string()).unwrap_or_default(),
        "cpu_cores": rayon::current_num_threads(),
        "gpu": gpu.name,
        "gpu_available": gpu.available,
        "gpu_backend": gpu.backend,
    }))
}

async fn handle_benchmark(Query(params): Query<BenchParams>) -> Json<NodeResult> {
    let result = tokio::task::spawn_blocking(move || run_benchmark(params.n, None))
        .await
        .unwrap();
    Json(result)
}

// ─────────────────────────────────────────────────────────────────
//  Live coordinator handlers (with shared state)
// ─────────────────────────────────────────────────────────────────

async fn live_dashboard(State(_): State<Arc<LiveState>>) -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

async fn live_report(
    State(state): State<Arc<LiveState>>,
    Json(report): Json<NodeResult>,
) -> &'static str {
    let mut nodes = state.nodes.write().await;
    nodes.insert(
        report.node_id.clone(),
        TrackedNode {
            result: report,
            last_seen: Instant::now(),
        },
    );
    "ok"
}

async fn live_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<LiveState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| dashboard_loop(socket, state))
}

async fn dashboard_loop(mut socket: WebSocket, state: Arc<LiveState>) {
    loop {
        let snapshot = state.snapshot().await;
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        if socket.send(Message::Text(json.into())).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn live_api_nodes(State(state): State<Arc<LiveState>>) -> Json<LiveSnapshot> {
    Json(state.snapshot().await)
}

async fn live_join_sh(headers: HeaderMap) -> impl IntoResponse {
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8080");

    let script = format!(
        r#"#!/bin/sh
set -e
echo ""
echo "  ARC Chain Benchmark — Joining the Swarm"
echo "  ========================================="
echo ""

COORD="http://{host}"

# Check dependencies
if ! command -v cargo >/dev/null 2>&1; then
    echo "  Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    . "$HOME/.cargo/env"
fi

if ! command -v git >/dev/null 2>&1; then
    echo "  ERROR: git is required"
    exit 1
fi

TMPDIR=$(mktemp -d)
echo "  Cloning arc-chain..."
git clone --depth 1 https://github.com/FerrumVir/arc-chain.git "$TMPDIR/arc-chain" 2>&1 | tail -1

echo "  Building (release mode)..."
cd "$TMPDIR/arc-chain"
cargo build --release -p arc-bench 2>&1 | tail -3

echo ""
echo "  Starting worker → $COORD"
echo ""
./target/release/arc-bench-node worker --coord "$COORD" --n 500000
"#
    );

    ([("content-type", "text/plain; charset=utf-8")], script)
}

// ─────────────────────────────────────────────────────────────────
//  Live mode — coordinator + dashboard + self-benchmark
// ─────────────────────────────────────────────────────────────────

async fn run_live(port: u16) {
    let state = Arc::new(LiveState::new());

    // Background: self-benchmark
    let self_state = state.clone();
    tokio::spawn(async move {
        let node_id = uuid::Uuid::new_v4().to_string();
        loop {
            let nid = node_id.clone();
            let result = tokio::task::spawn_blocking(move || run_benchmark(500_000, Some(nid)))
                .await
                .unwrap();

            self_state.nodes.write().await.insert(
                result.node_id.clone(),
                TrackedNode {
                    result,
                    last_seen: Instant::now(),
                },
            );
        }
    });

    // Background: prune stale nodes
    let prune_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let mut nodes = prune_state.nodes.write().await;
            nodes.retain(|_, n| n.last_seen.elapsed() < Duration::from_secs(60));
        }
    });

    let app = Router::new()
        .route("/", get(live_dashboard))
        .route("/health", get(handle_health))
        .route("/api/report", post(live_report))
        .route("/api/nodes", get(live_api_nodes))
        .route("/ws/dashboard", get(live_ws))
        .route("/join.sh", get(live_join_sh))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  ARC Chain — Live Benchmark Coordinator                    ║");
    println!(
        "║  Dashboard:   http://{:<40}║",
        format!("localhost:{port}")
    );
    println!(
        "║  API:         http://{:<40}║",
        format!("localhost:{port}/api/nodes")
    );
    println!(
        "║  WebSocket:   ws://{:<42}║",
        format!("localhost:{port}/ws/dashboard")
    );
    println!(
        "║  Join:        curl -sSL http://{:<30}║",
        format!("localhost:{port}/join.sh | sh")
    );
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Self-benchmarking in background...");
    println!("  Workers can POST results to /api/report");
    println!();

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────
//  Worker mode — standalone HTTP or reporting to coordinator
// ─────────────────────────────────────────────────────────────────

async fn run_worker(port: u16, coord_url: Option<String>) {
    if let Some(coord) = coord_url {
        run_worker_reporting(&coord).await;
        return;
    }

    // Standalone HTTP server
    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/info", get(handle_info))
        .route("/benchmark", get(handle_benchmark));

    let addr = format!("0.0.0.0:{port}");
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  ARC Chain — Benchmark Worker Node                         ║");
    println!(
        "║  Listening on {:<46}║",
        &addr
    );
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn run_worker_reporting(coord_url: &str) {
    let client = reqwest::Client::new();
    let node_id = uuid::Uuid::new_v4().to_string();
    let n = 500_000usize;

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  ARC Chain — Benchmark Worker (Reporting Mode)             ║");
    println!(
        "║  Coordinator: {:<46}║",
        coord_url
    );
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    let mut iteration = 0u64;
    loop {
        iteration += 1;
        println!("  [Iteration {}]", iteration);

        let nid = node_id.clone();
        let result = tokio::task::spawn_blocking(move || run_benchmark(n, Some(nid)))
            .await
            .unwrap();

        println!(
            "    Pipeline: {:.0} TPS — reporting to coordinator...",
            result.compact_pipeline_tps
        );

        match client
            .post(format!("{coord_url}/api/report"))
            .json(&result)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                println!("    Reported OK");
            }
            Ok(resp) => {
                eprintln!("    Report failed: HTTP {}", resp.status());
            }
            Err(e) => {
                eprintln!("    Report failed: {e}");
            }
        }

        println!();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ─────────────────────────────────────────────────────────────────
//  Coordinator mode — one-shot aggregation (existing)
// ─────────────────────────────────────────────────────────────────

async fn run_coordinator(node_urls: Vec<String>, n: usize) {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  ARC Chain — Multi-Node Benchmark Coordinator              ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Nodes: {}", node_urls.join(", "));
    println!("  Transactions per node: {}", n);
    println!();

    let client = reqwest::Client::new();
    let mut results: Vec<NodeResult> = Vec::new();

    println!("  Checking node health...");
    for url in &node_urls {
        match client.get(format!("{url}/health")).send().await {
            Ok(resp) if resp.status().is_success() => println!("    {} — OK", url),
            Ok(resp) => {
                eprintln!("    {} — ERROR: status {}", url, resp.status());
                return;
            }
            Err(e) => {
                eprintln!("    {} — ERROR: {}", url, e);
                return;
            }
        }
    }
    println!();

    println!("  Starting benchmarks on all nodes simultaneously...");
    let start = Instant::now();

    let mut handles = Vec::new();
    for url in &node_urls {
        let client = client.clone();
        let url = url.clone();
        let handle = tokio::spawn(async move {
            let resp = client
                .get(format!("{url}/benchmark?n={n}"))
                .timeout(Duration::from_secs(300))
                .send()
                .await;
            match resp {
                Ok(r) => r.json::<NodeResult>().await.ok(),
                Err(e) => {
                    eprintln!("    Error from {url}: {e}");
                    None
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        if let Ok(Some(result)) = handle.await {
            results.push(result);
        }
    }

    let total_elapsed = start.elapsed();
    println!();

    let combined_tps: f64 = results.iter().map(|r| r.best_single_node_tps).sum();
    let network_efficiency = 0.88;
    let avg_node_tps = combined_tps / results.len().max(1) as f64;
    let projected_128 = avg_node_tps * 128.0 * network_efficiency;
    let projected_256 = avg_node_tps * 256.0 * network_efficiency;

    let cluster = ClusterResult {
        total_nodes: results.len(),
        combined_tps,
        projected_128_nodes: projected_128,
        projected_256_nodes: projected_256,
        hits_1b: projected_256 >= 1_000_000_000.0,
        nodes: results.clone(),
    };

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║              MULTI-NODE BENCHMARK RESULTS                  ║");
    println!("╠══════════════════════════════════════════════════════════════╣");

    for (i, node) in results.iter().enumerate() {
        println!("║                                                            ║");
        println!(
            "║  Node {}: {} ({} cores, {})",
            i + 1,
            node.hostname,
            node.cpu_cores,
            if node.gpu_available {
                &node.gpu_name
            } else {
                "no GPU"
            }
        );
        println!(
            "║    CPU hash:     {:>12.0} TPS",
            node.cpu_hash_tps
        );
        if node.gpu_hash_tps > 0.0 {
            println!(
                "║    GPU hash:     {:>12.0} TPS",
                node.gpu_hash_tps
            );
        }
        println!(
            "║    State exec:   {:>12.0} TPS",
            node.state_exec_tps
        );
        println!(
            "║    Pipeline:     {:>12.0} TPS",
            node.compact_pipeline_tps
        );
        println!(
            "║    Best:         {:>12.0} TPS",
            node.best_single_node_tps
        );
    }

    println!("║                                                            ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║                                                            ║");
    println!(
        "║  COMBINED ({} nodes):           {:>12.0} TPS",
        results.len(),
        combined_tps
    );
    println!(
        "║  Avg per node:                {:>12.0} TPS",
        avg_node_tps
    );
    println!(
        "║  Elapsed:                     {:>12.2}s",
        total_elapsed.as_secs_f64()
    );
    println!("║                                                            ║");
    println!("║  Projections (88% efficiency):                             ║");
    for nodes in [32, 64, 128, 256] {
        let p = avg_node_tps * nodes as f64 * network_efficiency;
        let m = if p >= 1_000_000_000.0 { " 1B+" } else { "" };
        println!("║    {:>3} nodes: {:>14.0} TPS{}", nodes, p, m);
    }
    println!("║                                                            ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    let json = serde_json::to_string_pretty(&cluster).unwrap();
    std::fs::write("benchmark-results.json", &json).unwrap();
    println!("\n  Results: benchmark-results.json");
}

// ─────────────────────────────────────────────────────────────────
//  Main
// ─────────────────────────────────────────────────────────────────

fn get_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("live") => {
            let port = get_arg(&args, "--port")
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(8080);
            run_live(port).await;
        }
        Some("worker") => {
            let port = get_arg(&args, "--port")
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(9090);
            let coord = get_arg(&args, "--coord");
            run_worker(port, coord).await;
        }
        Some("coord") => {
            let nodes_str = get_arg(&args, "--nodes").expect("--nodes required");
            let nodes: Vec<String> = nodes_str.split(',').map(|s| s.trim().to_string()).collect();
            let n = get_arg(&args, "--n")
                .and_then(|p| p.parse::<usize>().ok())
                .unwrap_or(1_000_000);
            run_coordinator(nodes, n).await;
        }
        Some("local") => {
            let n = get_arg(&args, "--n")
                .and_then(|p| p.parse::<usize>().ok())
                .unwrap_or(1_000_000);
            let result = run_benchmark(n, None);
            println!("\n{}", serde_json::to_string_pretty(&result).unwrap());
        }
        _ => {
            eprintln!("ARC Chain — Multi-Node Benchmark");
            eprintln!();
            eprintln!("Usage:");
            eprintln!("  arc-bench-node live     [--port 8080]                             Live dashboard coordinator");
            eprintln!("  arc-bench-node worker   [--port 9090] [--coord http://host:8080]  Benchmark worker");
            eprintln!("  arc-bench-node coord    --nodes url1,url2 [--n 1000000]           One-shot aggregation");
            eprintln!("  arc-bench-node local    [--n 1000000]                             Local benchmark");
        }
    }
}
