#!/usr/bin/env bash
# ARC Community Node — Status Dashboard
set -euo pipefail

RPC="http://localhost:9090"
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'

# Check if node is running
if ! curl -sf "${RPC}/health" >/dev/null 2>&1; then
    echo -e "${RED}Node is not running.${NC}"
    echo "Start it with: bash scripts/arc-community.sh"
    exit 1
fi

# Fetch data
HEALTH=$(curl -sf "${RPC}/health" 2>/dev/null)
EARNINGS=$(curl -sf "${RPC}/worker/earnings" 2>/dev/null)

# Parse with python3
parse() { echo "$1" | python3 -c "import sys,json; d=json.load(sys.stdin); print($2)" 2>/dev/null || echo "?"; }

PEERS=$(parse "${HEALTH}" "d.get('peer_count',0)")
ROUND=$(parse "${HEALTH}" "d.get('dag_round',0)")
COMMITTED=$(parse "${HEALTH}" "d.get('dag_committed',0)")

ADDR=$(parse "${EARNINGS}" "d.get('address','?')")
MODE=$(parse "${EARNINGS}" "d.get('mode','?')")
INFERENCES=$(parse "${EARNINGS}" "d.get('total_inferences',0)")
EARNED=$(parse "${EARNINGS}" "d['earnings']['total_arc']")
UPTIME=$(parse "${EARNINGS}" "f\"{d.get('uptime_hours',0):.1f}\"")
STATUS=$(parse "${EARNINGS}" "d.get('status','?')")
MODEL=$(parse "${EARNINGS}" "d.get('model_loaded',False)")

# Display
echo ""
echo -e "${BOLD}  ARC Community Node Status${NC}"
echo -e "  ════════════════════════════════════════"
echo ""

if [[ "${STATUS}" == "active" ]]; then
    echo -e "  Status:       ${GREEN}ACTIVE${NC}"
elif [[ "${STATUS}" == "disconnected" ]]; then
    echo -e "  Status:       ${RED}DISCONNECTED${NC}"
else
    echo -e "  Status:       ${YELLOW}${STATUS}${NC}"
fi

echo -e "  Address:      ${BLUE}${ADDR}${NC}"
echo -e "  Mode:         ${MODE}"
echo -e "  Peers:        ${PEERS}"
echo -e "  Model:        $(if [[ "${MODEL}" == "True" ]]; then echo -e "${GREEN}loaded${NC}"; else echo -e "${RED}not loaded${NC}"; fi)"
echo -e "  Uptime:       ${UPTIME} hours"
echo ""
echo -e "  ${BOLD}Consensus${NC}"
echo -e "  Round:        ${ROUND}"
echo -e "  Committed:    ${COMMITTED} blocks"
echo ""
echo -e "  ${BOLD}Earnings${NC}"
echo -e "  Inferences:   ${INFERENCES}"
echo -e "  ARC Earned:   ${GREEN}${EARNED} ARC${NC}"
echo ""
