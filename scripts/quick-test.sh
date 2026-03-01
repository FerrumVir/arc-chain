#!/bin/bash
set -euo pipefail

# ARC Chain Quick Test — build, start, submit tx, verify block, check receipt
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
RPC="http://127.0.0.1:9090"
NODE_PID=""

cleanup() {
    if [ -n "$NODE_PID" ] && kill -0 "$NODE_PID" 2>/dev/null; then
        kill "$NODE_PID" 2>/dev/null || true
        wait "$NODE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "=== ARC Chain Quick Test ==="
echo ""

# 1. Build in debug mode
echo "[1/7] Building arc-node (debug)..."
cd "$PROJECT_DIR"
cargo build -p arc-node 2>&1 | tail -5
BIN="$PROJECT_DIR/target/debug/arc-node"

if [ ! -f "$BIN" ]; then
    echo "FAIL: Binary not found at $BIN"
    exit 1
fi
echo "  OK: Binary built"

# 2. Start the node in background
echo "[2/7] Starting node..."
RUST_LOG=arc=info "$BIN" &
NODE_PID=$!
echo "  PID: $NODE_PID"

# 3. Wait for node to start
echo "[3/7] Waiting for node to come up..."
sleep 3

# 4. Check health endpoint
echo "[4/7] Checking /health..."
HEALTH=$(curl -sf "$RPC/health" 2>/dev/null || echo "FAIL")
if echo "$HEALTH" | grep -q '"status":"ok"'; then
    echo "  OK: Node is healthy"
else
    echo "FAIL: Health check failed — $HEALTH"
    exit 1
fi

# Get initial block height
INITIAL_HEIGHT=$(curl -sf "$RPC/info" | python3 -c "import sys,json; print(json.load(sys.stdin)['block_height'])" 2>/dev/null || echo "0")
echo "  Initial height: $INITIAL_HEIGHT"

# 5. Submit a test transfer
echo "[5/7] Submitting test transfer..."
# Genesis account 0 = hash of [0x00] — prefunded with 1T
FROM="af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
TO="2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"

TX_RESULT=$(curl -sf -X POST "$RPC/tx/submit" \
    -H "Content-Type: application/json" \
    -d "{\"from\":\"$FROM\",\"to\":\"$TO\",\"amount\":1000,\"nonce\":0}" 2>/dev/null || echo "FAIL")

if echo "$TX_RESULT" | grep -q '"status":"pending"'; then
    TX_HASH=$(echo "$TX_RESULT" | python3 -c "import sys,json; print(json.load(sys.stdin)['tx_hash'])" 2>/dev/null)
    echo "  OK: TX submitted — $TX_HASH"
else
    echo "FAIL: TX submission failed — $TX_RESULT"
    exit 1
fi

# 6. Wait for block production
echo "[6/7] Waiting for block production..."
sleep 2

NEW_HEIGHT=$(curl -sf "$RPC/info" | python3 -c "import sys,json; print(json.load(sys.stdin)['block_height'])" 2>/dev/null || echo "0")
echo "  New height: $NEW_HEIGHT"

if [ "$NEW_HEIGHT" -gt "$INITIAL_HEIGHT" ]; then
    echo "  OK: Block produced (height $INITIAL_HEIGHT -> $NEW_HEIGHT)"
else
    echo "FAIL: No new block produced"
    exit 1
fi

# 7. Check receipt
echo "[7/7] Checking transaction receipt..."
RECEIPT=$(curl -sf "$RPC/tx/$TX_HASH" 2>/dev/null || echo "FAIL")
if echo "$RECEIPT" | grep -q '"success":true'; then
    echo "  OK: Transaction confirmed and successful"
else
    echo "FAIL: Receipt check failed — $RECEIPT"
    exit 1
fi

echo ""
echo "==============================="
echo "  PASS — All checks passed"
echo "==============================="
