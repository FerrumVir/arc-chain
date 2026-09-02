#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Local Inference Smoke Test
#
# Runs inference on an explicitly loopback-only development node and displays
# the endpoint's returned result/attestation fields. It is not payment proof.
#
# Usage:
#   ./scripts/test-inference.sh                         # Test your local node
#   ./scripts/test-inference.sh "What is 2+2?"          # Custom prompt
#   ./scripts/test-inference.sh "Hi" 127.0.0.1:19090   # Another local port
# ─────────────────────────────────────────────────────────────────────────────
set -e

PROMPT="${1:-What is the capital of France?}"
NODE="${2:-127.0.0.1:9944}"
if ! [[ "$NODE" =~ ^127[.]0[.]0[.]1:([0-9]{1,5})$ ]]; then
    printf 'RETIRED REMOTE PATH: test-inference.sh accepts only 127.0.0.1.\n' >&2
    exit 78
fi
NODE_PORT="${BASH_REMATCH[1]}"
if [ "$NODE_PORT" -lt 1 ] || [ "$NODE_PORT" -gt 65535 ]; then
    printf 'test-inference.sh: invalid loopback port: %s\n' "$NODE_PORT" >&2
    exit 2
fi

GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

echo -e "${CYAN}${BOLD}"
echo "  ╔══════════════════════════════════════════╗"
echo "  ║     ARC Chain - Inference Test           ║"
echo "  ╚══════════════════════════════════════════╝"
echo -e "${NC}"

echo -e "${YELLOW}[1/3] Running inference on ${NODE}...${NC}"
echo "  Prompt: \"${PROMPT}\""
echo ""

RESPONSE=$(curl -sf -X POST "http://${NODE}/inference/run" \
    -H 'Content-Type: application/json' \
    -d "{\"input\":\"[INST] ${PROMPT} [/INST]\",\"max_tokens\":64}" 2>&1)

if [ -z "$RESPONSE" ]; then
    echo -e "${YELLOW}Failed to reach ${NODE}${NC}"
    echo "  Is your node running? Check: curl http://${NODE}/health"
    exit 1
fi

# Parse with python for clean output
echo "$RESPONSE" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    if not d.get('success'):
        print('ERROR:', d.get('error', 'unknown'))
        sys.exit(1)

    inf = d['inference']
    att = d['attestation']

    print('${GREEN}[2/3] Inference result:${NC}')
    print(f'  Output:       {inf[\"output\"][:200]}')
    print(f'  Model:        {inf[\"model\"]}')
    print(f'  Engine:       {inf[\"engine\"]}')
    print(f'  Speed:        {inf[\"ms_per_token\"]} ms/token')
    print(f'  Deterministic: {inf[\"deterministic\"]}')
    print('')
    print('${GREEN}[3/3] Reported commitment fields:${NC}')
    print(f'  Input hash:   {inf[\"input_hash\"]}')
    print(f'  Output hash:  {inf[\"output_hash\"]}')
    print(f'  Model hash:   {inf[\"model_hash\"]}')
    print('')
    print('${CYAN}Returned attestation record (not a payment receipt):${NC}')
    print(f'  Tx hash:      {att[\"tx_hash\"]}')
    print(f'  Bond:         {att[\"bond\"]} ARC')
    print(f'  Status:       {att[\"status\"]}')
    print('')
    print('${BOLD}Verify on any node:${NC}')
    print(f'  curl http://${NODE}/inference/attestations?limit=5')
    print(f'  curl http://${NODE}/tx/{att[\"tx_hash\"]}')
except Exception as e:
    print('Failed to parse response:', e)
    print(sys.stdin.read() if False else '')
    sys.exit(1)
" || { echo -e "${YELLOW}Raw response:${NC}"; echo "$RESPONSE"; exit 1; }

echo ""
echo -e "${GREEN}${BOLD}Local inference response received.${NC}"
echo "A raw 0x16 attestation is not payment; only a successful mined 0x25 reward receipt proves earnings."
