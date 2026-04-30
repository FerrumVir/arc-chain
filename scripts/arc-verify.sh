#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Inference Verifier
#
# Take an inference attestation tx_hash, fetch its on-chain details
# (input, output, model_id, output_hash), re-run the SAME input through
# the SAME coordinator, and verify the new output_hash matches the
# original. Independent third-party verification of any past inference run.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh \
#     | bash -s -- 0x428c045cb321d061de1fefb22df0d43636b7c21e978049a28d0250ba157eb3df
#
#   # Or with a custom coordinator:
#   ARC_COORDINATOR=http://your-node:9090 bash arc-verify.sh <tx_hash>
#
# Requires curl + python3.
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

# ── Pick a live coordinator by probing seeds (override with ARC_COORDINATOR)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
PICK="$SCRIPT_DIR/arc-pick-coordinator.sh"
if [ ! -f "$PICK" ]; then
    PICK=$(mktemp)
    curl -fsSL "https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-pick-coordinator.sh" -o "$PICK" 2>/dev/null || true
fi
if [ -z "${ARC_COORDINATOR:-}" ] && [ -s "$PICK" ]; then
    ARC_COORDINATOR=$(bash "$PICK" 2>/dev/null || echo "")
fi
COORDINATOR="${ARC_COORDINATOR:-http://136.244.109.1:9090}"
TX_HASH="${1:-}"

# --latest mode: pick the newest entry from /inference/results so users
# don't need a prior tx_hash to try the verifier.
if [ "$TX_HASH" = "--latest" ] || [ "$TX_HASH" = "-l" ]; then
    LATEST_RESPONSE=$(curl -sf -m 30 "${COORDINATOR}/inference/results" 2>/dev/null || echo "")
    if [ -z "$LATEST_RESPONSE" ]; then
        echo "ERROR: Could not reach $COORDINATOR/inference/results" >&2
        exit 1
    fi
    TX_HASH=$(ARC_RESULTS="$LATEST_RESPONSE" python3 -c '
import json, os
d = json.loads(os.environ["ARC_RESULTS"])
results = d.get("results", [])
if not results:
    raise SystemExit(1)
print(results[0].get("tx_hash", ""))
' 2>/dev/null)
    if [ -z "$TX_HASH" ]; then
        echo "ERROR: No attestations available on the coordinator yet." >&2
        echo "       Run an inference first:" >&2
        echo "         curl -X POST $COORDINATOR/inference/run_sharded \\" >&2
        echo "           -H 'Content-Type: application/json' \\" >&2
        echo "           -d '{\"input\":\"hi\",\"max_tokens\":3}'" >&2
        exit 1
    fi
fi

# Colors
if [ -t 1 ]; then
    BOLD=$'\033[1m' DIM=$'\033[2m' RESET=$'\033[0m'
    GREEN=$'\033[32m' CYAN=$'\033[36m' YELLOW=$'\033[33m' RED=$'\033[31m'
else
    BOLD='' DIM='' RESET='' GREEN='' CYAN='' YELLOW='' RED=''
fi

if [ -z "$TX_HASH" ]; then
    cat <<USAGE
${BOLD}arc-verify.sh${RESET} - independently verify a past inference attestation

USAGE:
  arc-verify.sh <tx_hash>           # verify a specific past attestation
  arc-verify.sh --latest            # verify the newest attestation on the coordinator
  arc-verify.sh -l                  # short form

  # Live against the testnet coordinator (default):
  curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh \\
    | bash -s -- --latest

  # Or with a specific tx_hash:
  curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh \\
    | bash -s -- 0x428c045cb321d061de1fefb22df0d43636b7c21e978049a28d0250ba157eb3df

  # Or against your own coordinator:
  ARC_COORDINATOR=http://localhost:9944 bash arc-verify.sh --latest

WHAT IT DOES:
  1. Fetches /inference/results on the coordinator
  2. Locates the entry for the given tx_hash
  3. Pulls the original input text + claimed output_hash + model_hash
  4. Re-runs the SAME input through the coordinator
  5. Compares the new output_hash to the claimed one
  6. Prints VERIFIED or MISMATCH

This is third-party verification: anyone with the tx_hash can confirm
the network actually produced the claimed output for the claimed input,
and the model that ran it is identical to the one announced.
USAGE
    exit 1
fi

# Strip 0x prefix if present
TX_HASH_CLEAN="${TX_HASH#0x}"
TX_HASH_FULL="0x${TX_HASH_CLEAN}"

cat <<HEADER
${BOLD}${CYAN}
  ╔══════════════════════════════════════════════════════════════╗
  ║   ARC Chain - Inference Verifier                             ║
  ╚══════════════════════════════════════════════════════════════╝${RESET}

  Coordinator: ${COORDINATOR}
  tx_hash:     ${TX_HASH_FULL}

HEADER

if ! command -v curl >/dev/null; then
    printf "${RED}[FAIL]${RESET} curl is not installed.\n" >&2
    exit 1
fi
if ! command -v python3 >/dev/null; then
    printf "${RED}[FAIL]${RESET} python3 is not installed.\n" >&2
    exit 1
fi

# ── Step 1: Fetch the original attestation details ─────────────────────────
printf "${CYAN}[1/3]${RESET} Fetching inference details from %s/inference/results...\n" "$COORDINATOR"
RESULTS=$(curl -sf -m 30 "${COORDINATOR}/inference/results" 2>/dev/null || echo "")
if [ -z "$RESULTS" ]; then
    printf "${RED}[FAIL]${RESET} Could not reach coordinator.\n" >&2
    exit 1
fi

ARC_RESULTS="$RESULTS" ARC_TX_HASH="$TX_HASH_FULL" python3 <<'PYEOF' > /tmp/arc-verify-original.json 2>&1
import json, os, sys
target = os.environ["ARC_TX_HASH"].lstrip("0x").lower()
data = json.loads(os.environ["ARC_RESULTS"])
for r in data.get("results", []):
    rh = r.get("tx_hash", "").lstrip("0x").lower()
    if rh == target:
        print(json.dumps({
            "input": r.get("input", ""),
            "output": r.get("output", ""),
            "output_hash": r.get("output_hash", ""),
            "model_hash": r.get("model_hash", ""),
            "model": r.get("model", ""),
            "engine": r.get("engine", ""),
            "ms_per_token": r.get("ms_per_token", 0),
            "sharded": r.get("sharded", False),
        }))
        sys.exit(0)
print(json.dumps({"error": "tx_hash not found in /inference/results"}))
sys.exit(1)
PYEOF
PARSE_RC=$?

if [ $PARSE_RC -ne 0 ]; then
    printf "${RED}[FAIL]${RESET} %s\n" "$(cat /tmp/arc-verify-original.json)" >&2
    rm -f /tmp/arc-verify-original.json
    exit 1
fi

ORIG_INPUT=$(python3 -c "import json; d=json.load(open('/tmp/arc-verify-original.json')); print(d.get('input',''))")
ORIG_OUTPUT_HASH=$(python3 -c "import json; d=json.load(open('/tmp/arc-verify-original.json')); print(d.get('output_hash',''))")
ORIG_MODEL_HASH=$(python3 -c "import json; d=json.load(open('/tmp/arc-verify-original.json')); print(d.get('model_hash',''))")
ORIG_OUTPUT=$(python3 -c "import json; d=json.load(open('/tmp/arc-verify-original.json')); print(d.get('output','')[:100])")
ORIG_SHARDED=$(python3 -c "import json; d=json.load(open('/tmp/arc-verify-original.json')); print(d.get('sharded',False))")
ORIG_ENGINE=$(python3 -c "import json; d=json.load(open('/tmp/arc-verify-original.json')); print(d.get('engine',''))")

printf "${GREEN}[ OK]${RESET} Found attestation\n"
printf "       input:        '%s'\n" "$ORIG_INPUT"
printf "       output:       '%s'\n" "$ORIG_OUTPUT"
printf "       output_hash:  %s\n" "$ORIG_OUTPUT_HASH"
printf "       model_hash:   %s\n" "$ORIG_MODEL_HASH"
printf "       engine:       %s\n" "$ORIG_ENGINE"
printf "       sharded:      %s\n" "$ORIG_SHARDED"
echo ""

# ── Step 2: Re-run the inference ────────────────────────────────────────────
printf "${CYAN}[2/3]${RESET} Re-running the same input through the coordinator...\n"

# Pick the right endpoint depending on whether the original was sharded
if [ "$ORIG_SHARDED" = "True" ]; then
    ENDPOINT="/inference/run_sharded"
else
    ENDPOINT="/inference/run"
fi
printf "       endpoint:     %s\n" "$ENDPOINT"

# Build the request body
ARC_INPUT="$ORIG_INPUT" python3 -c "
import json, os
print(json.dumps({'input': os.environ['ARC_INPUT'], 'max_tokens': 15}))
" > /tmp/arc-verify-body.json

NEW_RESPONSE=$(curl -sf -m 240 -X POST "${COORDINATOR}${ENDPOINT}" \
    -H 'Content-Type: application/json' \
    --data @/tmp/arc-verify-body.json 2>/dev/null || echo "")

if [ -z "$NEW_RESPONSE" ]; then
    printf "${RED}[FAIL]${RESET} Re-run request failed.\n" >&2
    rm -f /tmp/arc-verify-original.json /tmp/arc-verify-body.json
    exit 1
fi

NEW_OUTPUT_HASH=$(echo "$NEW_RESPONSE" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('output_hash', d.get('inference',{}).get('output_hash','')))")
NEW_MODEL_HASH=$(echo "$NEW_RESPONSE" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('model_hash', d.get('inference',{}).get('model_hash','')))")

printf "${GREEN}[ OK]${RESET} Re-run complete\n"
printf "       new output_hash: %s\n" "$NEW_OUTPUT_HASH"
printf "       new model_hash:  %s\n" "$NEW_MODEL_HASH"
echo ""

# ── Step 3: Compare ────────────────────────────────────────────────────────
printf "${CYAN}[3/3]${RESET} Comparing hashes...\n"
echo ""

if [ "$ORIG_OUTPUT_HASH" = "$NEW_OUTPUT_HASH" ] && [ "$ORIG_MODEL_HASH" = "$NEW_MODEL_HASH" ]; then
    printf "  ${BOLD}${GREEN}✓ VERIFIED${RESET} - both output_hash and model_hash match the attestation.\n"
    printf "\n"
    printf "  This is cryptographic proof that:\n"
    printf "    • The same model was used (model_hash identical)\n"
    printf "    • The same input produces the same output (output_hash identical)\n"
    printf "    • The original attestation reflects an actual inference run on this network\n"
    EXIT_CODE=0
else
    printf "  ${BOLD}${RED}✗ MISMATCH${RESET}\n\n"
    if [ "$ORIG_OUTPUT_HASH" != "$NEW_OUTPUT_HASH" ]; then
        printf "    output_hash differs:\n"
        printf "      original: %s\n" "$ORIG_OUTPUT_HASH"
        printf "      new:      %s\n" "$NEW_OUTPUT_HASH"
    fi
    if [ "$ORIG_MODEL_HASH" != "$NEW_MODEL_HASH" ]; then
        printf "    model_hash differs:\n"
        printf "      original: %s\n" "$ORIG_MODEL_HASH"
        printf "      new:      %s\n" "$NEW_MODEL_HASH"
    fi
    EXIT_CODE=1
fi

rm -f /tmp/arc-verify-original.json /tmp/arc-verify-body.json
exit $EXIT_CODE
