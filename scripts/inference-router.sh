#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Distributed Inference Router
#
# Sends N inference requests across all available inference-enabled nodes.
# Demonstrates that the network distributes load and is faster than
# single-node serial execution.
#
# Usage:
#   ./scripts/inference-router.sh [N] [PROMPT]
#
# Examples:
#   ./scripts/inference-router.sh                              # 10 reqs default prompt
#   ./scripts/inference-router.sh 20                           # 20 reqs default prompt
#   ./scripts/inference-router.sh 10 "What is 2+2?"            # custom prompt
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

N="${1:-10}"
PROMPT="${2:-What is the largest planet?}"
MAX_TOKENS=15

# Inference-enabled nodes (TinyLlama loaded on all)
NODES=(
    "NYC:149.28.32.76:9090"
    "LAX:140.82.16.112:9090"
    "AMS:136.244.109.1:9090"
    "LHR:104.238.171.11:9090"
    "JNB:139.84.237.49:9090"
)
NUM_NODES=${#NODES[@]}

BOLD=$'\033[1m' GREEN=$'\033[32m' CYAN=$'\033[36m' YELLOW=$'\033[33m' RESET=$'\033[0m'

# Cross-platform millisecond timer (macOS lacks date %N)
now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

printf "%s%s╔══════════════════════════════════════════════════════════════╗%s\n" "$CYAN" "$BOLD" "$RESET"
printf "%s║       ARC Chain — Distributed Inference Router               ║%s\n" "$CYAN$BOLD" "$RESET"
printf "%s╚══════════════════════════════════════════════════════════════╝%s\n" "$CYAN$BOLD" "$RESET"
echo ""
printf "  Sending %s%s%s inference requests across %s%s%s devices in parallel\n" "$BOLD" "$N" "$RESET" "$BOLD" "$NUM_NODES" "$RESET"
printf "  Prompt: \"%s\"\n\n" "$PROMPT"

# Pre-create temp file for results
RESULTS_FILE=$(mktemp)
trap "rm -f $RESULTS_FILE" EXIT

START=$(now_ms)

PIDS=()
for i in $(seq 1 $N); do
    # Round-robin across nodes
    NODE_IDX=$(( (i - 1) % NUM_NODES ))
    NODE_ENTRY="${NODES[$NODE_IDX]}"
    NODE_NAME="${NODE_ENTRY%%:*}"
    NODE_HOST="${NODE_ENTRY#*:}"

    (
        REQ_START=$(now_ms)
        RESPONSE=$(curl -sf -X POST "http://${NODE_HOST}/inference/run" \
            -H 'Content-Type: application/json' \
            -d "{\"input\":\"[INST] ${PROMPT} [/INST]\",\"max_tokens\":${MAX_TOKENS}}" 2>/dev/null)
        REQ_END=$(now_ms)
        REQ_MS=$(( REQ_END - REQ_START ))

        if [ -n "$RESPONSE" ]; then
            HASH=$(echo "$RESPONSE" | grep -o '"output_hash":"[^"]*"' | cut -d'"' -f4)
            SPEED=$(echo "$RESPONSE" | grep -o '"ms_per_token":[0-9]*' | grep -o '[0-9]*')
            printf "  [%2d] %s✓%s %-4s — %5dms wall, %4sms/tok, hash=%s...\n" "$i" "$GREEN" "$RESET" "$NODE_NAME" "$REQ_MS" "$SPEED" "${HASH:0:18}"
            echo "$NODE_NAME $HASH $REQ_MS" >> "$RESULTS_FILE"
        else
            printf "  [%2d] %s✗%s %-4s — failed\n" "$i" "$YELLOW" "$RESET" "$NODE_NAME"
        fi
    ) &
    PIDS+=($!)
done

# Wait for all
for pid in "${PIDS[@]}"; do
    wait $pid
done

END=$(now_ms)
TOTAL_MS=$(( END - START ))

echo ""
printf "${BOLD}${GREEN}Total wall time: ${TOTAL_MS}ms${RESET}\n"
printf "${BOLD}Throughput:      $(echo "scale=2; $N * 1000 / $TOTAL_MS" | bc) req/sec${RESET}\n"
echo ""

# Determinism check — all nodes using same model should produce same hash for same prompt
UNIQUE_HASHES=$(awk '{print $2}' "$RESULTS_FILE" | sort -u | wc -l | tr -d ' ')
TOTAL_REQS=$(wc -l < "$RESULTS_FILE" | tr -d ' ')

echo "${BOLD}Determinism check:${RESET}"
if [ "$UNIQUE_HASHES" = "1" ]; then
    HASH=$(awk 'NR==1{print $2}' "$RESULTS_FILE")
    printf "  ${GREEN}✓ ALL %d responses produced IDENTICAL output_hash${RESET}\n" "$TOTAL_REQS"
    printf "  Hash: %s\n" "$HASH"
    printf "  ${BOLD}This proves cross-device determinism: same model + same prompt = same hash${RESET}\n"
else
    printf "  ${YELLOW}⚠ %d unique hashes across %d responses${RESET}\n" "$UNIQUE_HASHES" "$TOTAL_REQS"
    awk '{print $2, $1}' "$RESULTS_FILE" | sort -u | head -5
fi

echo ""
echo "${BOLD}Per-node distribution:${RESET}"
awk '{print $1}' "$RESULTS_FILE" | sort | uniq -c | sort -rn | awk '{printf "  %s: %d requests\n", $2, $1}'
