#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Check On-Chain Inference Attestations
#
# Lists recent inference-attestation records returned by one selected node.
# This is a node-local retained view, not proof of every inference or payment.
#
# Usage:
#   ./scripts/check-attestations.sh                   # Your local node
#   ./scripts/check-attestations.sh 140.82.16.112:9090 # Remote node
#   ./scripts/check-attestations.sh "" 20             # Last 20 attestations
# ─────────────────────────────────────────────────────────────────────────────
set -e

NODE="${1:-localhost:9944}"
LIMIT="${2:-10}"

[ -z "$NODE" ] && NODE="localhost:9944"

GREEN='\033[0;32m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

echo -e "${CYAN}${BOLD}ARC Chain - Recent Inference Attestations${NC}"
echo "  Node: ${NODE}"
echo "  Note: raw 0x16 attestations pay nothing; earnings require a successful mined 0x25 receipt."
echo ""

curl -sf "http://${NODE}/inference/attestations?limit=${LIMIT}" 2>/dev/null | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    atts = data.get('attestations', [])
    if not atts:
        print('  No attestations yet. Run inference first:')
        print('  ./scripts/test-inference.sh \"your prompt\"')
        sys.exit(0)

    for i, a in enumerate(atts, 1):
        inf = a.get('inference', {})
        print(f'${BOLD}[{i}]${NC} Tx: {a.get(\"tx_hash\", \"?\")}')
        print(f'    Input:    {inf.get(\"input\", \"?\")[:80]}')
        print(f'    Output:   {inf.get(\"output\", \"?\")[:80]}')
        print(f'    Model:    {inf.get(\"model\", \"?\")}')
        print(f'    Hash:     {inf.get(\"output_hash\", \"?\")}')
        print(f'    Speed:    {inf.get(\"ms_per_token\", \"?\")} ms/tok')
        print(f'    Node reports deterministic: {inf.get(\"deterministic\", False)}')
        print('')

    print(f'${GREEN}Node-retained rows: {data.get(\"count\", len(atts))} at reported chain height {data.get(\"chain_height\", \"?\")}${NC}')
except Exception as e:
    print(f'Error: {e}')
    sys.exit(1)
" || { echo "Failed to reach ${NODE}. Is the node running?"; exit 1; }
