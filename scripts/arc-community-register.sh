#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Community Worker Registration (works with ANY arc-node version)
#
# Registers this node as a community inference worker with all 8 testnet
# seed gateways. Runs as a sidecar alongside arc-node — no binary changes
# needed. Works with v0.3.0, v0.4.x, v0.5.x, or any future version.
#
# What it does:
#   1. Detects your arc-node's validator address and platform
#   2. POSTs /community/register to each seed gateway (port 3001) every 60s
#   3. POSTs /community/heartbeat every 15s to stay alive (TTL 90s)
#   4. Optionally polls /community/claim_work to compute inference jobs
#
# Usage:
#   # Start alongside your arc-node:
#   nohup bash scripts/arc-community-register.sh &
#
#   # Or install as a service (the main installer does this automatically):
#   curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-community-register.sh | bash
# ─────────────────────────────────────────────────────────────────────────────

set -u

ARC_RPC="${ARC_RPC:-http://localhost:9944}"
GATEWAY_PORT=3001

# Detect worker identity from local arc-node
WORKER_ID=""
for port in 9944 9090; do
    h=$(curl -sf -m 3 "http://localhost:$port/node/info" 2>/dev/null || echo "")
    if [ -n "$h" ]; then
        WORKER_ID=$(echo "$h" | python3 -c "import sys,json; print(json.load(sys.stdin).get('validator',''))" 2>/dev/null || echo "")
        ARC_RPC="http://localhost:$port"
        break
    fi
done

if [ -z "$WORKER_ID" ]; then
    WORKER_ID="community-$(hostname)-$(date +%s | tail -c 6)"
fi

PLATFORM="$(uname -s)-$(uname -m)"
HOSTNAME=$(hostname 2>/dev/null || echo "unknown")

# Seed gateway addresses
SEEDS="149.28.32.76 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31 216.238.120.27 139.84.237.49"

echo "ARC Community Worker Registration"
echo "  worker_id: $WORKER_ID"
echo "  platform:  $PLATFORM"
echo "  arc-node:  $ARC_RPC"
echo "  gateways:  8 seeds on port $GATEWAY_PORT"
echo ""

REGISTER_JSON="{\"worker_id\":\"$WORKER_ID\",\"name\":\"$HOSTNAME\",\"platform\":\"$PLATFORM\",\"capabilities\":[\"inference\"]}"
HEARTBEAT_JSON="{\"worker_id\":\"$WORKER_ID\"}"

TICK=0
while true; do
    for seed in $SEEDS; do
        GW="http://${seed}:${GATEWAY_PORT}"
        if [ $((TICK % 4)) -eq 0 ]; then
            # Full register every 60s
            curl -sf -m 5 -X POST "$GW/community/register" \
                -H "Content-Type: application/json" \
                -d "$REGISTER_JSON" >/dev/null 2>&1
        else
            # Heartbeat every 15s
            curl -sf -m 5 -X POST "$GW/community/heartbeat" \
                -H "Content-Type: application/json" \
                -d "$HEARTBEAT_JSON" >/dev/null 2>&1
        fi
    done
    TICK=$((TICK + 1))
    sleep 15
done
