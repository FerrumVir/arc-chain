#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Test Inference (dead simple verification)
#
# Runs inference locally, shows result, fetches on-chain attestation.
# Proves: inference works, output is hashed, attestation is on-chain.
#
# Usage:
#   ./scripts/test-inference.sh                         # Test your local node
#   ./scripts/test-inference.sh "What is 2+2?"          # Custom prompt
#   ./scripts/test-inference.sh "Hi" 140.82.16.112:9090 # Test a remote node
# ─────────────────────────────────────────────────────────────────────────────
set -e

PROMPT="${1:-What is the capital of France?}"
NODE="${2:-localhost:9944}"

GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

echo -e "${CYAN}${BOLD}"
echo "  ╔══════════════════════════════════════════╗"
echo "  ║     ARC Chain — Inference Test           ║"
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
    print('${GREEN}[3/3] Cryptographic proof:${NC}')
    print(f'  Input hash:   {inf[\"input_hash\"]}')
    print(f'  Output hash:  {inf[\"output_hash\"]}')
    print(f'  Model hash:   {inf[\"model_hash\"]}')
    print('')
    print('${CYAN}On-chain attestation:${NC}')
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
echo -e "${GREEN}${BOLD}Proof of verifiable inference. Same prompt = same output hash on any machine.${NC}"
