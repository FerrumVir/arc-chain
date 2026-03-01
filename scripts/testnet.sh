#!/bin/bash
set -euo pipefail

# ══════════════════════════════════════════════════════════════════════════════
# ARC Chain — Testnet Launcher
# Spin up a local multi-node testnet for development and benchmarking
# ══════════════════════════════════════════════════════════════════════════════

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BIN="$PROJECT_DIR/target/release/arc-node"
TESTNET_DIR="/tmp/arc-testnet"
BASE_RPC_PORT=9090
BASE_P2P_PORT=9190
STAKE=5000000

# Genesis account 0 — prefunded with 1T ARC
GENESIS_FROM="af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
GENESIS_TO="2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"

# ── Colors ────────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m' # No Color

# ── Banner ────────────────────────────────────────────────────────────────────
banner() {
    echo ""
    echo -e "${BLUE}${BOLD}╔═══════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}${BOLD}║                                                       ║${NC}"
    echo -e "${BLUE}${BOLD}║   █████╗ ██████╗  ██████╗     ██████╗██╗  ██╗ █████╗  ║${NC}"
    echo -e "${BLUE}${BOLD}║  ██╔══██╗██╔══██╗██╔════╝    ██╔════╝██║  ██║██╔══██╗ ║${NC}"
    echo -e "${BLUE}${BOLD}║  ███████║██████╔╝██║         ██║     ███████║███████║  ║${NC}"
    echo -e "${BLUE}${BOLD}║  ██╔══██║██╔══██╗██║         ██║     ██╔══██║██╔══██║  ║${NC}"
    echo -e "${BLUE}${BOLD}║  ██║  ██║██║  ██║╚██████╗    ╚██████╗██║  ██║██║  ██║  ║${NC}"
    echo -e "${BLUE}${BOLD}║  ╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝     ╚═════╝╚═╝  ╚═╝╚═╝  ╚═╝  ║${NC}"
    echo -e "${BLUE}${BOLD}║                                                       ║${NC}"
    echo -e "${BLUE}${BOLD}║   ${CYAN}Testnet Launcher${BLUE}              ${DIM}v0.1.0${BLUE}${BOLD}              ║${NC}"
    echo -e "${BLUE}${BOLD}╚═══════════════════════════════════════════════════════╝${NC}"
    echo ""
}

# ── Helpers ───────────────────────────────────────────────────────────────────
info()    { echo -e "  ${CYAN}INFO${NC}  $*"; }
ok()      { echo -e "  ${GREEN} OK ${NC}  $*"; }
warn()    { echo -e "  ${YELLOW}WARN${NC}  $*"; }
fail()    { echo -e "  ${RED}FAIL${NC}  $*"; }
section() { echo -e "\n${BOLD}── $* ──${NC}"; }

usage() {
    banner
    echo -e "${BOLD}Usage:${NC}"
    echo "  testnet.sh start [N]     Start N validator nodes (default: 4)"
    echo "  testnet.sh stop          Stop all testnet nodes"
    echo "  testnet.sh status        Check health of all running nodes"
    echo "  testnet.sh bench [TPS]   Benchmark with a burst of transactions (default: 1000)"
    echo ""
    echo -e "${BOLD}Examples:${NC}"
    echo "  testnet.sh start         # 4-node testnet on ports 9090-9093"
    echo "  testnet.sh start 8       # 8-node testnet on ports 9090-9097"
    echo "  testnet.sh status        # Health-check all running nodes"
    echo "  testnet.sh bench 5000    # Send 5000 transactions to node 0"
    echo ""
    echo -e "${BOLD}Environment:${NC}"
    echo "  STAKE        Validator stake per node (default: $STAKE)"
    echo "  RUST_LOG     Log filter for nodes (default: arc=info)"
    echo ""
    echo -e "${DIM}Logs:  $TESTNET_DIR/node-N.log${NC}"
    echo -e "${DIM}PIDs:  $TESTNET_DIR/node-N.pid${NC}"
    echo ""
    exit 0
}

ensure_dir() {
    mkdir -p "$TESTNET_DIR"
}

# ── Build ─────────────────────────────────────────────────────────────────────
build() {
    section "Building arc-node (release)"
    cd "$PROJECT_DIR"
    cargo build --release -p arc-node 2>&1 | tail -5

    if [ ! -f "$BIN" ]; then
        fail "Binary not found at $BIN"
        exit 1
    fi
    ok "Binary ready: $BIN"
}

# ── Start ─────────────────────────────────────────────────────────────────────
cmd_start() {
    local num_nodes="${1:-4}"
    banner
    ensure_dir

    # Check for already-running nodes
    local running=0
    for i in $(seq 0 $((num_nodes - 1))); do
        local pidfile="$TESTNET_DIR/node-${i}.pid"
        if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
            running=$((running + 1))
        fi
    done
    if [ "$running" -gt 0 ]; then
        warn "$running node(s) already running. Run 'testnet.sh stop' first."
        exit 1
    fi

    build

    section "Launching $num_nodes validator nodes"
    echo ""

    for i in $(seq 0 $((num_nodes - 1))); do
        local rpc_port=$((BASE_RPC_PORT + i))
        local p2p_port=$((BASE_P2P_PORT + i))
        local seed="arc-validator-${i}"
        local logfile="$TESTNET_DIR/node-${i}.log"
        local pidfile="$TESTNET_DIR/node-${i}.pid"
        local datadir="$TESTNET_DIR/data-${i}"

        mkdir -p "$datadir"

        # Build peer list — connect to all previously started nodes
        local peers=""
        if [ "$i" -gt 0 ]; then
            for p in $(seq 0 $((i - 1))); do
                local peer_p2p=$((BASE_P2P_PORT + p))
                if [ -n "$peers" ]; then
                    peers="${peers},127.0.0.1:${peer_p2p}"
                else
                    peers="127.0.0.1:${peer_p2p}"
                fi
            done
        fi

        # Compose args
        local args="--rpc 0.0.0.0:${rpc_port} --p2p-port ${p2p_port} --stake ${STAKE} --validator-seed ${seed} --data-dir ${datadir}"
        if [ -n "$peers" ]; then
            args="$args --peers $peers"
        fi

        # Launch node in background
        RUST_LOG="${RUST_LOG:-arc=info}" "$BIN" $args > "$logfile" 2>&1 &
        local pid=$!
        echo "$pid" > "$pidfile"

        echo -e "  ${GREEN}Node $i${NC}  RPC=${BOLD}:${rpc_port}${NC}  P2P=:${p2p_port}  Seed=${DIM}${seed}${NC}  PID=${pid}"
    done

    echo ""

    # Wait for nodes to come up
    info "Waiting for nodes to start..."
    sleep 3

    # Quick health check
    local healthy=0
    for i in $(seq 0 $((num_nodes - 1))); do
        local rpc_port=$((BASE_RPC_PORT + i))
        local health
        health=$(curl -sf "http://127.0.0.1:${rpc_port}/health" 2>/dev/null || echo "")
        if echo "$health" | grep -q '"status":"ok"'; then
            healthy=$((healthy + 1))
        fi
    done

    if [ "$healthy" -eq "$num_nodes" ]; then
        ok "All $num_nodes nodes healthy"
    else
        warn "$healthy/$num_nodes nodes responding (some may still be starting)"
    fi

    # Optional: simulated network latency with tc (requires root)
    if [ "$(id -u)" -eq 0 ] && command -v tc &>/dev/null; then
        section "Applying simulated network latency (50ms jitter)"
        for i in $(seq 0 $((num_nodes - 1))); do
            local p2p_port=$((BASE_P2P_PORT + i))
            tc qdisc add dev lo root netem delay 25ms 10ms distribution normal 2>/dev/null || true
        done
        ok "Latency simulation active on loopback"
    else
        info "Skipping network latency simulation (requires root + tc)"
    fi

    echo ""
    echo -e "${BOLD}Testnet is running.${NC}"
    echo -e "  Node 0 RPC:  ${CYAN}http://127.0.0.1:${BASE_RPC_PORT}${NC}"
    echo -e "  Logs:        ${DIM}$TESTNET_DIR/node-*.log${NC}"
    echo -e "  Stop:        ${DIM}testnet.sh stop${NC}"
    echo ""
}

# ── Stop ──────────────────────────────────────────────────────────────────────
cmd_stop() {
    banner
    section "Stopping testnet nodes"

    local stopped=0
    local already=0

    for pidfile in "$TESTNET_DIR"/node-*.pid; do
        [ -f "$pidfile" ] || continue
        local pid
        pid=$(cat "$pidfile")
        local node_name
        node_name=$(basename "$pidfile" .pid)

        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            # Wait briefly for graceful shutdown
            local waited=0
            while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 5 ]; do
                sleep 1
                waited=$((waited + 1))
            done
            # Force kill if still running
            if kill -0 "$pid" 2>/dev/null; then
                kill -9 "$pid" 2>/dev/null || true
                warn "$node_name (PID $pid) force-killed"
            else
                ok "$node_name (PID $pid) stopped"
            fi
            stopped=$((stopped + 1))
        else
            already=$((already + 1))
        fi
        rm -f "$pidfile"
    done

    # Remove tc rules if root
    if [ "$(id -u)" -eq 0 ] && command -v tc &>/dev/null; then
        tc qdisc del dev lo root 2>/dev/null || true
        info "Cleared network latency simulation"
    fi

    echo ""
    if [ "$stopped" -eq 0 ] && [ "$already" -eq 0 ]; then
        info "No testnet nodes found"
    else
        ok "Stopped $stopped node(s)"
        if [ "$already" -gt 0 ]; then
            info "$already node(s) were already stopped"
        fi
    fi
    echo ""
}

# ── Status ────────────────────────────────────────────────────────────────────
cmd_status() {
    banner
    section "Testnet Node Status"
    echo ""

    local total=0
    local healthy=0
    local dead=0

    printf "  ${BOLD}%-8s  %-8s  %-12s  %-10s  %-8s  %s${NC}\n" \
        "NODE" "PORT" "STATUS" "HEIGHT" "UPTIME" "PID"
    printf "  %-8s  %-8s  %-12s  %-10s  %-8s  %s\n" \
        "--------" "--------" "------------" "----------" "--------" "------"

    for pidfile in "$TESTNET_DIR"/node-*.pid; do
        [ -f "$pidfile" ] || continue
        total=$((total + 1))

        local pid
        pid=$(cat "$pidfile")
        local node_name
        node_name=$(basename "$pidfile" .pid)
        local node_idx="${node_name#node-}"
        local rpc_port=$((BASE_RPC_PORT + node_idx))

        if ! kill -0 "$pid" 2>/dev/null; then
            printf "  %-8s  %-8s  ${RED}%-12s${NC}  %-10s  %-8s  %s\n" \
                "$node_name" ":$rpc_port" "DEAD" "-" "-" "$pid"
            dead=$((dead + 1))
            continue
        fi

        local health
        health=$(curl -sf "http://127.0.0.1:${rpc_port}/health" 2>/dev/null || echo "")

        if echo "$health" | grep -q '"status":"ok"'; then
            local height
            height=$(echo "$health" | python3 -c "import sys,json; print(json.load(sys.stdin)['height'])" 2>/dev/null || echo "?")
            local uptime
            uptime=$(echo "$health" | python3 -c "import sys,json; s=json.load(sys.stdin)['uptime_secs']; print(f'{s//3600}h{(s%3600)//60}m{s%60}s')" 2>/dev/null || echo "?")

            printf "  %-8s  %-8s  ${GREEN}%-12s${NC}  %-10s  %-8s  %s\n" \
                "$node_name" ":$rpc_port" "HEALTHY" "$height" "$uptime" "$pid"
            healthy=$((healthy + 1))
        else
            printf "  %-8s  %-8s  ${YELLOW}%-12s${NC}  %-10s  %-8s  %s\n" \
                "$node_name" ":$rpc_port" "UNREACHABLE" "-" "-" "$pid"
            dead=$((dead + 1))
        fi
    done

    echo ""
    if [ "$total" -eq 0 ]; then
        info "No testnet nodes found. Run 'testnet.sh start' to launch."
    else
        echo -e "  ${BOLD}Total:${NC} $total  ${GREEN}Healthy:${NC} $healthy  ${RED}Down:${NC} $dead"
    fi
    echo ""
}

# ── Bench ─────────────────────────────────────────────────────────────────────
cmd_bench() {
    local num_txs="${1:-1000}"
    local rpc="http://127.0.0.1:${BASE_RPC_PORT}"

    banner
    section "Benchmark: $num_txs transactions"

    # Verify node 0 is running
    local health
    health=$(curl -sf "$rpc/health" 2>/dev/null || echo "")
    if ! echo "$health" | grep -q '"status":"ok"'; then
        fail "Node 0 is not running at $rpc"
        echo "  Start the testnet first: testnet.sh start"
        exit 1
    fi

    local initial_height
    initial_height=$(curl -sf "$rpc/info" | python3 -c "import sys,json; print(json.load(sys.stdin)['block_height'])" 2>/dev/null || echo "0")
    info "Initial block height: $initial_height"

    # Build batch payload
    section "Generating $num_txs transactions"
    local batch_size=500
    local total_sent=0
    local total_accepted=0
    local total_rejected=0

    local start_time
    start_time=$(python3 -c "import time; print(time.time())")

    while [ "$total_sent" -lt "$num_txs" ]; do
        local remaining=$((num_txs - total_sent))
        local this_batch=$batch_size
        if [ "$remaining" -lt "$batch_size" ]; then
            this_batch=$remaining
        fi

        # Generate batch JSON
        local txs="["
        for j in $(seq 0 $((this_batch - 1))); do
            local nonce=$((total_sent + j))
            if [ "$j" -gt 0 ]; then
                txs="${txs},"
            fi
            txs="${txs}{\"from\":\"$GENESIS_FROM\",\"to\":\"$GENESIS_TO\",\"amount\":1,\"nonce\":${nonce}}"
        done
        txs="${txs}]"

        local result
        result=$(curl -sf -X POST "$rpc/tx/submit_batch" \
            -H "Content-Type: application/json" \
            -d "{\"transactions\":$txs}" 2>/dev/null || echo '{"accepted":0,"rejected":0}')

        local accepted
        accepted=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin).get('accepted',0))" 2>/dev/null || echo "0")
        local rejected
        rejected=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin).get('rejected',0))" 2>/dev/null || echo "0")

        total_accepted=$((total_accepted + accepted))
        total_rejected=$((total_rejected + rejected))
        total_sent=$((total_sent + this_batch))

        # Progress indicator
        local pct=$((total_sent * 100 / num_txs))
        printf "\r  ${CYAN}Sending:${NC} %d/%d (%d%%)  " "$total_sent" "$num_txs" "$pct"
    done

    local end_time
    end_time=$(python3 -c "import time; print(time.time())")

    echo ""
    echo ""

    # Wait for blocks to be produced
    info "Waiting for block production..."
    sleep 5

    local final_height
    final_height=$(curl -sf "$rpc/info" | python3 -c "import sys,json; print(json.load(sys.stdin)['block_height'])" 2>/dev/null || echo "0")
    local blocks_produced=$((final_height - initial_height))

    # Calculate TPS
    local elapsed
    elapsed=$(python3 -c "print(round($end_time - $start_time, 2))")
    local tps
    tps=$(python3 -c "print(round($total_accepted / max($elapsed, 0.001), 1))")

    # Results
    section "Benchmark Results"
    echo ""
    echo -e "  ${BOLD}Transactions${NC}"
    echo -e "    Submitted:   $num_txs"
    echo -e "    Accepted:    ${GREEN}$total_accepted${NC}"
    echo -e "    Rejected:    ${RED}$total_rejected${NC}"
    echo ""
    echo -e "  ${BOLD}Performance${NC}"
    echo -e "    Duration:    ${elapsed}s"
    echo -e "    Throughput:  ${CYAN}${BOLD}${tps} TPS${NC} (submission rate)"
    echo ""
    echo -e "  ${BOLD}Chain State${NC}"
    echo -e "    Height:      $initial_height -> $final_height (+$blocks_produced blocks)"
    echo -e "    Mempool:     $(curl -sf "$rpc/info" | python3 -c "import sys,json; print(json.load(sys.stdin)['mempool_size'])" 2>/dev/null || echo "?")"
    echo ""
}

# ── Main ──────────────────────────────────────────────────────────────────────
CMD="${1:-}"
shift || true

case "$CMD" in
    start)
        cmd_start "$@"
        ;;
    stop)
        cmd_stop
        ;;
    status)
        cmd_status
        ;;
    bench)
        cmd_bench "$@"
        ;;
    -h|--help|help|"")
        usage
        ;;
    *)
        fail "Unknown command: $CMD"
        echo ""
        usage
        ;;
esac
