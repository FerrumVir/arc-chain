#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Continuous TPS Generator
#
# Pumps real signed transfer transactions across all 8 seed nodes via the
# faucet endpoint. Each request creates an on-chain transfer that is gossip
# propagated, included in a DAG block, and committed via the two-round rule.
#
# This is what populates the dashboard's "Total Transactions" counter and
# proves the chain is processing real, ordered, finalized transactions —
# not just inference attestations.
#
# Usage:
#   ./scripts/tps-generator.sh                # forever, default rate
#   ./scripts/tps-generator.sh 60             # run for 60 seconds
#   ./scripts/tps-generator.sh forever 8      # forever with 8 concurrent workers
#
# Rate cap: faucet allows ~100 claims/min/node × 8 nodes ≈ 13 TPS sustained.
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

DURATION="${1:-forever}"
WORKERS="${2:-4}"

NODES=(
    "NYC:149.28.32.76:9090"
    "LAX:140.82.16.112:9090"
    "AMS:136.244.109.1:9090"
    "LHR:104.238.171.11:9090"
    "NRT:202.182.107.41:9090"
    "SGP:149.28.153.31:9090"
    "SAO:216.238.120.27:9090"
    "JNB:139.84.237.49:9090"
)
NUM_NODES=${#NODES[@]}

BOLD=$'\033[1m' GREEN=$'\033[32m' CYAN=$'\033[36m' YELLOW=$'\033[33m' RED=$'\033[31m' RESET=$'\033[0m'

printf "%s%sARC Chain — TPS Generator%s\n" "$CYAN" "$BOLD" "$RESET"
printf "  Pumping signed faucet transfers across %s%d%s nodes with %s%d%s workers\n" "$BOLD" "$NUM_NODES" "$RESET" "$BOLD" "$WORKERS" "$RESET"
printf "  Duration: %s%s%s\n\n" "$BOLD" "$DURATION" "$RESET"

START=$(date +%s)
TOTAL_OK=0
TOTAL_FAIL=0
TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT

# Worker function: continuously fires requests, round-robin across all nodes.
# Each successful tx appends a single character to the count file (atomic O_APPEND).
# We count file size at report time — this avoids read/modify/write races.
worker() {
    local id=$1
    local count_file="$TMPDIR/worker-$id.count"
    local fail_file="$TMPDIR/worker-$id.fail"
    : > "$count_file"
    : > "$fail_file"
    local n=0
    while [ -f "$TMPDIR/run" ]; do
        # Round-robin: each worker offsets by its ID so distribution is even
        local idx=$(( (n + id - 1) % NUM_NODES ))
        local entry="${NODES[$idx]}"
        local host="${entry#*:}"
        local addr
        addr=$(openssl rand -hex 32)
        if curl -sf -m 5 -X POST "http://${host}/faucet/claim" \
            -H 'Content-Type: application/json' \
            -d "{\"address\":\"${addr}\"}" >/dev/null 2>&1; then
            printf '.' >> "$count_file"
        else
            printf '.' >> "$fail_file"
        fi
        n=$(( n + 1 ))
        sleep 0.02
    done
}

# Status reporter — uses file size as the counter (atomic, no races)
report() {
    local total=0
    local fail=0
    local f
    for f in "$TMPDIR"/worker-*.count; do
        [ -f "$f" ] && total=$(( total + $(wc -c < "$f" 2>/dev/null || echo 0) ))
    done
    for f in "$TMPDIR"/worker-*.fail; do
        [ -f "$f" ] && fail=$(( fail + $(wc -c < "$f" 2>/dev/null || echo 0) ))
    done
    local now elapsed tps
    now=$(date +%s)
    elapsed=$(( now - START ))
    [ "$elapsed" = "0" ] && elapsed=1
    tps=$(( total / elapsed ))
    printf "  %s[%4ds]%s sent=%s%d%s tps=%s%d%s fail=%s%d%s\n" \
        "$CYAN" "$elapsed" "$RESET" \
        "$GREEN" "$total" "$RESET" \
        "$BOLD" "$tps" "$RESET" \
        "$YELLOW" "$fail" "$RESET"
}

# Spawn workers
touch "$TMPDIR/run"
WPIDS=()
for i in $(seq 1 $WORKERS); do
    worker $i &
    WPIDS+=($!)
done

# Termination handler
cleanup() {
    rm -f "$TMPDIR/run"
    sleep 1
    local pid
    for pid in "${WPIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    report
    printf "\n%s%sTPS generator stopped.%s\n" "$BOLD" "$GREEN" "$RESET"
    exit 0
}
trap cleanup INT TERM

# Reporter loop
while true; do
    if [ "$DURATION" != "forever" ]; then
        NOW=$(date +%s)
        ELAPSED=$(( NOW - START ))
        if [ "$ELAPSED" -ge "$DURATION" ]; then
            cleanup
        fi
    fi
    sleep 5
    report
done
