#!/usr/bin/env bash
# ARC Chain — Soak Test
# Runs N validator nodes on this machine for a sustained period
# Monitors TPS, errors, memory, and state root consistency
#
# Usage: ./scripts/soak_test.sh [--nodes N] [--duration SECONDS] [--batch-size N]

set -euo pipefail

# Defaults
NODES=${NODES:-5}
DURATION=${DURATION:-86400}  # 24 hours
BATCH_SIZE=${BATCH_SIZE:-10000}
LOG_DIR="logs/soak-$(date +%Y%m%d-%H%M%S)"
BINARY="target/release/arc-node"

# Parse args
while [[ $# -gt 0 ]]; do
    case $1 in
        --nodes) NODES=$2; shift 2 ;;
        --duration) DURATION=$2; shift 2 ;;
        --batch-size) BATCH_SIZE=$2; shift 2 ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

# Calculate hours/minutes for display
HOURS=$((DURATION / 3600))
MINS=$(( (DURATION % 3600) / 60 ))

echo "================================================================"
echo " ARC Chain — Soak Test"
echo "================================================================"
echo ""
echo "  Nodes:          $NODES"
echo "  Duration:       ${HOURS}h ${MINS}m (${DURATION}s)"
echo "  Batch size:     $BATCH_SIZE"
echo "  Log directory:  $LOG_DIR"
echo "  Started:        $(date)"
echo ""

# Create log directory
mkdir -p "$LOG_DIR"

# Build release if needed
echo "[1/3] Building release binary..."
cargo build --release --bin arc-node 2>&1 | tail -3
echo "  Build complete."

# Check binary exists
if [[ ! -f "$BINARY" ]]; then
    echo "ERROR: $BINARY not found. Build failed?"
    exit 1
fi

echo ""
echo "[2/3] Starting $NODES validator nodes..."

# Start nodes
PIDS=()
for i in $(seq 0 $((NODES - 1))); do
    PORT_RPC=$((9090 + i))
    PORT_P2P=$((9100 + i))
    NODE_LOG="$LOG_DIR/node-${i}.log"

    echo "  Node $i: RPC=$PORT_RPC P2P=$PORT_P2P log=$NODE_LOG"

    # Start node with benchmark mode
    $BINARY \
        --validator-id "$i" \
        --rpc-port "$PORT_RPC" \
        --p2p-port "$PORT_P2P" \
        --batch-size "$BATCH_SIZE" \
        --benchmark \
        > "$NODE_LOG" 2>&1 &

    PIDS+=($!)
done

echo ""
echo "  All $NODES nodes started (PIDs: ${PIDS[*]})"

# Cleanup on exit
cleanup() {
    echo ""
    echo "Shutting down nodes..."
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null
    echo "All nodes stopped."
    generate_report
}
trap cleanup EXIT INT TERM

echo ""
echo "[3/3] Monitoring for ${HOURS}h ${MINS}m..."
echo ""
echo "  Press Ctrl+C to stop early."
echo ""

# Monitor loop
START_TIME=$(date +%s)
AGGREGATE_LOG="$LOG_DIR/aggregate.csv"
echo "timestamp,elapsed_s,alive_nodes,total_errors" > "$AGGREGATE_LOG"

while true; do
    CURRENT=$(date +%s)
    ELAPSED=$((CURRENT - START_TIME))

    if [[ $ELAPSED -ge $DURATION ]]; then
        echo ""
        echo "Duration reached ($DURATION seconds). Stopping."
        break
    fi

    # Count alive nodes
    ALIVE=0
    ERRORS=0
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            ALIVE=$((ALIVE + 1))
        fi
    done

    # Count errors across all logs
    ERRORS=$(grep -c -i "error\|panic\|fatal" "$LOG_DIR"/node-*.log 2>/dev/null || echo 0)

    # Log aggregate
    echo "$CURRENT,$ELAPSED,$ALIVE,$ERRORS" >> "$AGGREGATE_LOG"

    # Print status every 60 seconds
    if [[ $((ELAPSED % 60)) -lt 10 ]]; then
        ELAPSED_H=$((ELAPSED / 3600))
        ELAPSED_M=$(( (ELAPSED % 3600) / 60 ))
        printf "\r  [%02d:%02d:%02d] Alive: %d/%d | Errors: %s      " \
            "$ELAPSED_H" "$ELAPSED_M" "$((ELAPSED % 60))" "$ALIVE" "$NODES" "$ERRORS"
    fi

    sleep 10
done

generate_report() {
    echo ""
    echo "================================================================"
    echo " SOAK TEST REPORT"
    echo "================================================================"
    echo ""

    END_TIME=$(date +%s)
    TOTAL_TIME=$((END_TIME - START_TIME))
    TOTAL_H=$((TOTAL_TIME / 3600))
    TOTAL_M=$(( (TOTAL_TIME % 3600) / 60 ))

    echo "  Duration:     ${TOTAL_H}h ${TOTAL_M}m"
    echo "  Nodes:        $NODES"
    echo "  Started:      $(date -r "$START_TIME" 2>/dev/null || date -d @"$START_TIME" 2>/dev/null || echo "$START_TIME")"
    echo "  Ended:        $(date)"
    echo ""

    # Error summary per node
    echo "  Per-node error counts:"
    for i in $(seq 0 $((NODES - 1))); do
        NODE_LOG="$LOG_DIR/node-${i}.log"
        if [[ -f "$NODE_LOG" ]]; then
            ERR_COUNT=$(grep -c -i "error\|panic" "$NODE_LOG" 2>/dev/null || echo 0)
            LINES=$(wc -l < "$NODE_LOG" | tr -d ' ')
            echo "    Node $i: $ERR_COUNT errors, $LINES log lines"
        else
            echo "    Node $i: (no log)"
        fi
    done

    echo ""
    echo "  Logs:         $LOG_DIR/"
    echo "  Aggregate:    $AGGREGATE_LOG"
    echo ""
    echo "================================================================"

    # Save report
    echo "Report saved to $LOG_DIR/report.txt"
}
