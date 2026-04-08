#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Single vs Distributed Inference Benchmark
#
# Runs the same N prompts in two modes:
#   1. SEQUENTIAL on a single node (the "old" way)
#   2. PARALLEL across N inference-enabled nodes (the "new" way)
#
# Reports wall time, throughput, and the speedup factor.
#
# Usage:
#   ./scripts/inference-benchmark.sh [N]
#
# Examples:
#   ./scripts/inference-benchmark.sh           # 10 prompts default
#   ./scripts/inference-benchmark.sh 20        # 20 prompts
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

N="${1:-10}"
PROMPT="What is the largest planet?"
MAX_TOKENS=15

ALL_NODES=(
    "NYC:149.28.32.76:9090"
    "LAX:140.82.16.112:9090"
    "AMS:136.244.109.1:9090"
    "LHR:104.238.171.11:9090"
    "NRT:202.182.107.41:9090"
    "SGP:149.28.153.31:9090"
    "SAO:216.238.120.27:9090"
    "JNB:139.84.237.49:9090"
)

# Auto-discover live inference nodes
echo "Discovering inference-capable nodes..."
NODES=()
for entry in "${ALL_NODES[@]}"; do
    name="${entry%%:*}"
    host="${entry#*:}"
    if curl -sf -m 30 -X POST "http://${host}/inference/run" \
        -H 'Content-Type: application/json' \
        -d '{"input":"hi","max_tokens":1}' 2>/dev/null | grep -q '"output_hash"'; then
        NODES+=("$entry")
        echo "  ✓ $name"
    else
        echo "  ✗ $name (skipped)"
    fi
done

if [ ${#NODES[@]} -lt 2 ]; then
    echo "ERROR: Need at least 2 inference-capable nodes"
    exit 1
fi

# Pick LAX if available, otherwise first node
SINGLE_NODE="${NODES[0]}"
for n in "${NODES[@]}"; do
    if [[ "$n" == LAX:* ]]; then SINGLE_NODE="$n"; break; fi
done
SINGLE_HOST="${SINGLE_NODE#*:}"
NUM_NODES=${#NODES[@]}
echo ""

BOLD=$'\033[1m' GREEN=$'\033[32m' CYAN=$'\033[36m' YELLOW=$'\033[33m' RED=$'\033[31m' RESET=$'\033[0m'

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

printf "%s%s╔══════════════════════════════════════════════════════════════╗%s\n" "$CYAN" "$BOLD" "$RESET"
printf "%s║   ARC Chain — Single vs Distributed Benchmark                ║%s\n" "$CYAN$BOLD" "$RESET"
printf "%s╚══════════════════════════════════════════════════════════════╝%s\n" "$CYAN$BOLD" "$RESET"
echo ""
printf "  Running %s%s%s identical inference requests in two modes\n" "$BOLD" "$N" "$RESET"
printf "  Prompt: \"%s\"\n\n" "$PROMPT"

# ── PHASE 1: SEQUENTIAL on single node ─────────────────────────────────────
printf "%s%s── PHASE 1: SEQUENTIAL (single node, old way) ──%s\n" "$CYAN" "$BOLD" "$RESET"
printf "  Target: %s%s%s (one device handles all requests)\n\n" "$BOLD" "${SINGLE_NODE%%:*}" "$RESET"

SEQ_HASH_FILE=$(mktemp)
SEQ_START=$(now_ms)
for i in $(seq 1 $N); do
    RESPONSE=$(curl -sf -X POST "http://${SINGLE_HOST}/inference/run" \
        -H 'Content-Type: application/json' \
        -d "{\"input\":\"[INST] ${PROMPT} [/INST]\",\"max_tokens\":${MAX_TOKENS}}" 2>/dev/null)
    if [ -n "$RESPONSE" ]; then
        HASH=$(echo "$RESPONSE" | grep -o '"output_hash":"[^"]*"' | cut -d'"' -f4)
        echo "$HASH" >> "$SEQ_HASH_FILE"
        printf "  [%2d] %s✓%s done (hash=%s...)\n" "$i" "$GREEN" "$RESET" "${HASH:0:18}"
    else
        printf "  [%2d] %s✗%s failed\n" "$i" "$RED" "$RESET"
    fi
done
SEQ_END=$(now_ms)
SEQ_MS=$(( SEQ_END - SEQ_START ))

echo ""
printf "  %sSequential total: %dms%s (%.2f sec)\n" "$BOLD" "$SEQ_MS" "$RESET" "$(echo "scale=2; $SEQ_MS / 1000" | bc)"
printf "  Throughput:        %s req/sec\n" "$(echo "scale=2; $N * 1000 / $SEQ_MS" | bc)"

# ── PHASE 2: PARALLEL across N nodes ────────────────────────────────────────
echo ""
printf "%s%s── PHASE 2: DISTRIBUTED (across %d devices, new way) ──%s\n" "$CYAN" "$BOLD" "$NUM_NODES" "$RESET"
printf "  Targets: %s%s%s (round-robin distribution)\n\n" "$BOLD" "$(echo "${NODES[@]}" | sed 's/:[^ ]*//g')" "$RESET"

DIST_HASH_FILE=$(mktemp)
DIST_START=$(now_ms)
PIDS=()
for i in $(seq 1 $N); do
    NODE_IDX=$(( (i - 1) % NUM_NODES ))
    NODE_ENTRY="${NODES[$NODE_IDX]}"
    NODE_NAME="${NODE_ENTRY%%:*}"
    NODE_HOST="${NODE_ENTRY#*:}"
    (
        RESPONSE=$(curl -sf -X POST "http://${NODE_HOST}/inference/run" \
            -H 'Content-Type: application/json' \
            -d "{\"input\":\"[INST] ${PROMPT} [/INST]\",\"max_tokens\":${MAX_TOKENS}}" 2>/dev/null)
        if [ -n "$RESPONSE" ]; then
            HASH=$(echo "$RESPONSE" | grep -o '"output_hash":"[^"]*"' | cut -d'"' -f4)
            echo "$HASH" >> "$DIST_HASH_FILE"
            printf "  [%2d] %s✓%s %-4s done\n" "$i" "$GREEN" "$RESET" "$NODE_NAME"
        else
            printf "  [%2d] %s✗%s %-4s failed\n" "$i" "$RED" "$RESET" "$NODE_NAME"
        fi
    ) &
    PIDS+=($!)
done
for pid in "${PIDS[@]}"; do wait $pid; done
DIST_END=$(now_ms)
DIST_MS=$(( DIST_END - DIST_START ))

echo ""
printf "  %sDistributed total: %dms%s (%.2f sec)\n" "$BOLD" "$DIST_MS" "$RESET" "$(echo "scale=2; $DIST_MS / 1000" | bc)"
printf "  Throughput:        %s req/sec\n" "$(echo "scale=2; $N * 1000 / $DIST_MS" | bc)"

# ── PHASE 3: Comparison ─────────────────────────────────────────────────────
echo ""
printf "%s%s═══ RESULTS ═══%s\n" "$CYAN" "$BOLD" "$RESET"
echo ""
SPEEDUP=$(echo "scale=2; $SEQ_MS / $DIST_MS" | bc)
TIME_SAVED=$(( SEQ_MS - DIST_MS ))
printf "  Sequential (1 node):  %s%6dms%s\n" "$YELLOW" "$SEQ_MS" "$RESET"
printf "  Distributed (%d nodes): %s%6dms%s\n" "$NUM_NODES" "$GREEN" "$DIST_MS" "$RESET"
printf "  %sSpeedup: %sx faster%s\n" "$BOLD" "$SPEEDUP" "$RESET"
printf "  Time saved: %dms (%.1f sec)\n" "$TIME_SAVED" "$(echo "scale=1; $TIME_SAVED / 1000" | bc)"
echo ""

# ── PHASE 4: Determinism check ──────────────────────────────────────────────
SEQ_UNIQUE=$(sort -u "$SEQ_HASH_FILE" | wc -l | tr -d ' ')
DIST_UNIQUE=$(sort -u "$DIST_HASH_FILE" | wc -l | tr -d ' ')
SEQ_HASH=$(head -1 "$SEQ_HASH_FILE")
DIST_HASH=$(head -1 "$DIST_HASH_FILE")

printf "%s%sDeterminism check:%s\n" "$CYAN" "$BOLD" "$RESET"
printf "  Sequential mode:  %d unique hashes across %d responses\n" "$SEQ_UNIQUE" "$N"
printf "  Distributed mode: %d unique hashes across %d responses\n" "$DIST_UNIQUE" "$N"

if [ "$SEQ_UNIQUE" = "1" ] && [ "$DIST_UNIQUE" = "1" ] && [ "$SEQ_HASH" = "$DIST_HASH" ]; then
    printf "  %s✓ ALL %d responses across BOTH modes produced IDENTICAL hash%s\n" "$GREEN" "$((N*2))" "$RESET"
    printf "  Hash: %s\n" "$SEQ_HASH"
    printf "  %sCryptographic proof of cross-device deterministic inference%s\n" "$BOLD" "$RESET"
else
    printf "  %s⚠ Hashes diverge — investigate%s\n" "$YELLOW" "$RESET"
fi

rm -f "$SEQ_HASH_FILE" "$DIST_HASH_FILE"
