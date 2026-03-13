#!/usr/bin/env bash
# =========================================================================
# ARC Chain — Testnet Monitor
# =========================================================================
# Checks health, block height, peer count, and uptime for all 4 nodes.
#
# Usage:
#   ./monitor.sh              # One-shot status check
#   ./monitor.sh --watch      # Continuous monitoring (refreshes every 10s)
#   ./monitor.sh --json       # JSON output for scripting
# =========================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IP_FILE="${SCRIPT_DIR}/.node-ips"
NODE_COUNT=4
WATCH_MODE=false
JSON_MODE=false

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

# Parse args
for arg in "$@"; do
    case "$arg" in
        --watch|-w) WATCH_MODE=true ;;
        --json|-j)  JSON_MODE=true ;;
        --help|-h)
            echo "Usage: $0 [--watch|-w] [--json|-j]"
            exit 0
            ;;
    esac
done

# -- Load Node IPs --------------------------------------------------------

load_ips() {
    if [ -f "$IP_FILE" ]; then
        mapfile -t NODE_IPS < "$IP_FILE"
    else
        # Fallback: query hcloud
        if command -v hcloud &>/dev/null; then
            NODE_IPS=()
            for i in $(seq 0 $((NODE_COUNT - 1))); do
                IP=$(hcloud server ip "arc-node-${i}" 2>/dev/null || echo "")
                NODE_IPS+=("$IP")
            done
        else
            echo -e "${RED}ERROR: No .node-ips file and hcloud not installed.${NC}"
            echo "Run setup-testnet.sh first, or install hcloud CLI."
            exit 1
        fi
    fi
}

# -- Query Node ------------------------------------------------------------

query_node() {
    local ip="$1"
    local timeout=5

    if [ -z "$ip" ]; then
        echo "UNREACHABLE|||"
        return
    fi

    HEALTH=$(curl -sf --connect-timeout "$timeout" "http://${ip}:9090/health" 2>/dev/null || echo "")

    if [ -z "$HEALTH" ]; then
        echo "UNREACHABLE|||"
        return
    fi

    # Parse health response — adapt based on actual /health JSON format
    BLOCK_HEIGHT=$(echo "$HEALTH" | jq -r '.block_height // .height // "N/A"' 2>/dev/null || echo "N/A")
    PEER_COUNT=$(echo "$HEALTH" | jq -r '.peer_count // .peers // "N/A"' 2>/dev/null || echo "N/A")
    UPTIME=$(echo "$HEALTH" | jq -r '.uptime // .uptime_seconds // "N/A"' 2>/dev/null || echo "N/A")

    echo "HEALTHY|${BLOCK_HEIGHT}|${PEER_COUNT}|${UPTIME}"
}

# -- Format Uptime ---------------------------------------------------------

format_uptime() {
    local seconds="$1"
    if [ "$seconds" = "N/A" ] || [ -z "$seconds" ]; then
        echo "N/A"
        return
    fi

    # Handle non-numeric
    if ! [[ "$seconds" =~ ^[0-9]+$ ]]; then
        echo "$seconds"
        return
    fi

    local days=$((seconds / 86400))
    local hours=$(( (seconds % 86400) / 3600 ))
    local mins=$(( (seconds % 3600) / 60 ))

    if [ "$days" -gt 0 ]; then
        printf "%dd %dh %dm" "$days" "$hours" "$mins"
    elif [ "$hours" -gt 0 ]; then
        printf "%dh %dm" "$hours" "$mins"
    else
        printf "%dm %ds" "$mins" $((seconds % 60))
    fi
}

# -- Display ---------------------------------------------------------------

display_status() {
    local timestamp
    timestamp=$(date '+%Y-%m-%d %H:%M:%S')

    if [ "$JSON_MODE" = true ]; then
        echo "{"
        echo "  \"timestamp\": \"$timestamp\","
        echo "  \"nodes\": ["
    else
        echo ""
        echo -e "${BOLD}ARC Chain Testnet — Node Status${NC}"
        echo -e "${DIM}$timestamp${NC}"
        echo "============================================"
        printf "  %-14s %-12s %-10s %-8s %s\n" "NODE" "STATUS" "HEIGHT" "PEERS" "UPTIME"
        echo "  ----------------------------------------------------"
    fi

    local all_healthy=true
    local max_height=0

    for i in $(seq 0 $((NODE_COUNT - 1))); do
        local ip="${NODE_IPS[$i]:-}"
        local name="arc-node-${i}"
        local result
        result=$(query_node "$ip")

        IFS='|' read -r status height peers uptime <<< "$result"

        # Track max height for consensus check
        if [[ "$height" =~ ^[0-9]+$ ]] && [ "$height" -gt "$max_height" ]; then
            max_height="$height"
        fi

        if [ "$JSON_MODE" = true ]; then
            local comma=""
            [ "$i" -lt $((NODE_COUNT - 1)) ] && comma=","
            echo "    {\"name\": \"$name\", \"ip\": \"$ip\", \"status\": \"$status\", \"height\": \"$height\", \"peers\": \"$peers\", \"uptime\": \"$uptime\"}${comma}"
        else
            local status_color
            if [ "$status" = "HEALTHY" ]; then
                status_color="${GREEN}HEALTHY${NC}"
            else
                status_color="${RED}DOWN${NC}"
                all_healthy=false
            fi

            local formatted_uptime
            formatted_uptime=$(format_uptime "$uptime")

            printf "  %-14s %-20b %-10s %-8s %s\n" \
                "$name" "$status_color" "$height" "$peers" "$formatted_uptime"
        fi
    done

    if [ "$JSON_MODE" = true ]; then
        echo "  ]"
        echo "}"
    else
        echo ""
        if [ "$all_healthy" = true ]; then
            echo -e "  ${GREEN}All nodes healthy.${NC} Max block height: ${BOLD}$max_height${NC}"
        else
            echo -e "  ${RED}Some nodes are unreachable.${NC}"
        fi
        echo ""
    fi
}

# -- Main ------------------------------------------------------------------

load_ips

if [ "$WATCH_MODE" = true ]; then
    echo -e "${CYAN}Monitoring mode — refresh every 10s. Press Ctrl+C to stop.${NC}"
    while true; do
        clear
        display_status
        sleep 10
    done
else
    display_status
fi
