#!/usr/bin/env bash
set -euo pipefail

# ══════════════════════════════════════════════════════════════════════════════
# ARC Chain — Testnet Monitor Dashboard
# Polls validator /health and /stats endpoints and displays a live dashboard.
# Requires: curl, jq
# ══════════════════════════════════════════════════════════════════════════════

NUM_NODES="${1:-4}"
POLL_INTERVAL="${2:-5}"

# RPC ports follow the convention: 9090, 9190, 9290, 9390
BASE_RPC_PORT=9090
RPC_PORT_STEP=100

# ── Colors ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

# ── Dependency check ────────────────────────────────────────────────────────
for cmd in curl jq; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "Error: '$cmd' is required but not found. Install it and retry."
        exit 1
    fi
done

# ── Cleanup on exit ─────────────────────────────────────────────────────────
cleanup() {
    tput cnorm 2>/dev/null || true  # restore cursor
    echo ""
    exit 0
}
trap cleanup INT TERM

# ── Main loop ────────────────────────────────────────────────────────────────
tput civis 2>/dev/null || true  # hide cursor

while true; do
    # Clear screen
    tput clear 2>/dev/null || printf '\033[2J\033[H'

    echo -e "${BOLD}${CYAN}ARC Testnet Monitor${NC}"
    echo -e "${BOLD}$(printf '%.0s=' {1..50})${NC}"
    echo ""

    total_tps=0
    nodes_up=0
    nodes_total="$NUM_NODES"
    latest_block_time=""
    max_height=0

    for i in $(seq 0 $((NUM_NODES - 1))); do
        rpc_port=$((BASE_RPC_PORT + i * RPC_PORT_STEP))
        url="http://127.0.0.1:${rpc_port}"

        # Fetch /health
        health_json=$(curl -sf --connect-timeout 2 --max-time 3 "${url}/health" 2>/dev/null || echo "")

        if [ -z "$health_json" ]; then
            echo -e "  ${RED}Node ${i}: DOWN                                    x${NC}"
            continue
        fi

        # Parse health response
        height=$(echo "$health_json" | jq -r '.height // .block_height // "?"' 2>/dev/null || echo "?")
        peers=$(echo "$health_json" | jq -r '.peers // .peer_count // "?"' 2>/dev/null || echo "?")

        # Fetch /stats for TPS (some nodes may expose it here)
        stats_json=$(curl -sf --connect-timeout 2 --max-time 3 "${url}/stats" 2>/dev/null || echo "")
        if [ -n "$stats_json" ]; then
            tps=$(echo "$stats_json" | jq -r '.tps // .throughput // 0' 2>/dev/null || echo "0")
            block_time=$(echo "$stats_json" | jq -r '.last_block_time // .block_time // empty' 2>/dev/null || echo "")
        else
            # Fall back to /health for tps
            tps=$(echo "$health_json" | jq -r '.tps // 0' 2>/dev/null || echo "0")
            block_time=$(echo "$health_json" | jq -r '.last_block_time // empty' 2>/dev/null || echo "")
        fi

        # Ensure tps is a number
        if ! [[ "$tps" =~ ^[0-9]+\.?[0-9]*$ ]]; then
            tps=0
        fi

        # Track totals
        total_tps=$(echo "$total_tps + $tps" | bc 2>/dev/null || echo "$total_tps")
        nodes_up=$((nodes_up + 1))

        # Track max height for block-time calculation
        if [[ "$height" =~ ^[0-9]+$ ]] && [ "$height" -gt "$max_height" ]; then
            max_height=$height
        fi

        if [ -n "$block_time" ] && [ "$block_time" != "null" ]; then
            latest_block_time="$block_time"
        fi

        # Format TPS with padding
        tps_display=$(printf "%.0f" "$tps" 2>/dev/null || echo "$tps")

        echo -e "  ${GREEN}Node ${i}:${NC} height=${BOLD}${height}${NC}  tps=${BOLD}${tps_display}${NC}  peers=${peers}  ${GREEN}ok${NC}"
    done

    echo ""
    echo -e "${BOLD}$(printf '%.0s-' {1..50})${NC}"

    # Network summary
    total_tps_display=$(printf "%.0f" "$total_tps" 2>/dev/null || echo "$total_tps")

    # Calculate time since last block if we have a timestamp
    block_age_str="N/A"
    if [ -n "$latest_block_time" ] && [ "$latest_block_time" != "null" ]; then
        # Try to parse as epoch seconds
        now=$(date +%s)
        if [[ "$latest_block_time" =~ ^[0-9]+\.?[0-9]*$ ]]; then
            block_epoch=$(printf "%.0f" "$latest_block_time" 2>/dev/null || echo "$now")
            age=$((now - block_epoch))
            if [ "$age" -ge 0 ] && [ "$age" -lt 3600 ]; then
                block_age_str="${age}.0s ago"
            fi
        fi
    fi

    echo -e "  ${BOLD}Network:${NC} ${nodes_up}/${nodes_total} nodes, ${total_tps_display} total TPS, latest block: ${block_age_str}"

    # Health indicator
    if [ "$nodes_up" -eq "$nodes_total" ]; then
        echo -e "  ${BOLD}Status:${NC}  ${GREEN}All nodes healthy${NC}"
    elif [ "$nodes_up" -gt 0 ]; then
        echo -e "  ${BOLD}Status:${NC}  ${YELLOW}Degraded (${nodes_up}/${nodes_total} up)${NC}"
    else
        echo -e "  ${BOLD}Status:${NC}  ${RED}All nodes down${NC}"
    fi

    echo ""
    echo -e "${DIM}Refreshing every ${POLL_INTERVAL}s | Ctrl+C to exit${NC}"

    sleep "$POLL_INTERVAL"
done
