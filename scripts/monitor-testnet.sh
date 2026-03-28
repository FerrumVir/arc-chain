#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# ARC Chain — Live Testnet Monitor
# Shows all 8 nodes, TPS, block height, consensus status.
# Usage: ./scripts/monitor-testnet.sh
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail

POLL_INTERVAL="${1:-5}"

# Live testnet nodes
NODES=(
    "NYC:149.28.32.76"
    "LAX:140.82.16.112"
    "AMS:136.244.109.1"
    "LHR:104.238.171.11"
    "NRT:202.182.107.41"
    "SGP:149.28.153.31"
    "SAO:216.238.120.27"
    "JNB:139.84.237.49"
)

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

cleanup() { tput cnorm 2>/dev/null; echo ""; exit 0; }
trap cleanup INT TERM

tput civis 2>/dev/null || true

while true; do
    tput clear 2>/dev/null || printf '\033[2J\033[H'

    echo -e "${BOLD}${CYAN}"
    echo "  ╔══════════════════════════════════════════╗"
    echo "  ║       ARC Chain — Testnet Monitor        ║"
    echo "  ╚══════════════════════════════════════════╝"
    echo -e "${NC}"

    nodes_up=0
    max_round=0
    max_committed=0
    bench_tps=0

    printf "  ${BOLD}%-6s %-16s %8s %10s %6s %6s${NC}\n" "NODE" "IP" "BLOCK" "FINALIZED" "PEERS" "STATUS"
    echo "  $(printf '%.0s─' {1..60})"

    for entry in "${NODES[@]}"; do
        name="${entry%%:*}"
        ip="${entry##*:}"

        health=$(curl -sf --connect-timeout 2 --max-time 3 "http://${ip}:9090/health" 2>/dev/null || echo "")

        if [ -z "$health" ]; then
            printf "  ${RED}%-6s %-16s %8s %10s %6s %6s${NC}\n" "$name" "$ip" "—" "—" "—" "DOWN"
            continue
        fi

        round=$(echo "$health" | python3 -c "import sys,json; print(json.load(sys.stdin).get('dag_round','?'))" 2>/dev/null || echo "?")
        committed=$(echo "$health" | python3 -c "import sys,json; print(json.load(sys.stdin).get('dag_committed','?'))" 2>/dev/null || echo "?")
        peers=$(echo "$health" | python3 -c "import sys,json; print(json.load(sys.stdin).get('peers','?'))" 2>/dev/null || echo "?")

        printf "  ${GREEN}%-6s${NC} %-16s %8s %10s %6s ${GREEN}%6s${NC}\n" "$name" "$ip" "$round" "$committed" "$peers" "UP"

        nodes_up=$((nodes_up + 1))
        if [[ "$round" =~ ^[0-9]+$ ]] && [ "$round" -gt "$max_round" ]; then max_round=$round; fi
        if [[ "$committed" =~ ^[0-9]+$ ]] && [ "$committed" -gt "$max_committed" ]; then max_committed=$committed; fi
    done

    echo ""

    # Get TPS from LAX (benchmark node)
    stats=$(curl -sf --connect-timeout 2 --max-time 3 "http://140.82.16.112:9090/stats" 2>/dev/null || echo "")
    if [ -n "$stats" ]; then
        bench_tps=$(echo "$stats" | python3 -c "import sys,json; print(json.load(sys.stdin).get('benchmark_tps',0))" 2>/dev/null || echo "0")
        total_tx=$(echo "$stats" | python3 -c "import sys,json; print(json.load(sys.stdin).get('total_transactions',0))" 2>/dev/null || echo "0")
        validators=$(echo "$stats" | python3 -c "import sys,json; print(json.load(sys.stdin).get('validators',0))" 2>/dev/null || echo "0")
    else
        total_tx="?"
        validators="?"
    fi

    echo -e "  $(printf '%.0s─' {1..60})"
    echo -e "  ${BOLD}Network:${NC}    ${nodes_up}/8 nodes up, ${validators} validators"
    echo -e "  ${BOLD}Block:${NC}      ${max_round}"
    echo -e "  ${BOLD}Finalized:${NC}  ${max_committed}"
    echo -e "  ${BOLD}TPS:${NC}        ${bench_tps}"
    echo -e "  ${BOLD}Total TXs:${NC}  ${total_tx}"
    echo ""

    if [ "$nodes_up" -eq 8 ]; then
        echo -e "  ${GREEN}${BOLD}All 8 nodes healthy${NC}"
    elif [ "$nodes_up" -gt 0 ]; then
        echo -e "  ${YELLOW}${BOLD}Degraded: ${nodes_up}/8 nodes up${NC}"
    else
        echo -e "  ${RED}${BOLD}All nodes unreachable${NC}"
    fi

    echo ""
    echo -e "  ${BOLD}Dashboard:${NC}  http://140.82.16.112:3200"
    echo -e "  ${BOLD}Wallet:${NC}     http://140.82.16.112:3100"
    echo ""
    echo -e "${DIM}  Refreshing every ${POLL_INTERVAL}s | Ctrl+C to exit${NC}"

    sleep "$POLL_INTERVAL"
done
