#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Community Worker Registration (legacy compat)
#
# v0.7.0 NOTE: this script is no longer required. arc-node v0.7.0+
# auto-registers as a community worker on every seed it peers with —
# the inference worker loop runs in-process. Keep this script only
# for older arc-node binaries (v0.3 → v0.6) that don't have native
# auto-registration. Once your install is on v0.7.0+, you can stop
# this script and uninstall it from any system service unit.
#
# What it does (for legacy binaries):
#   1. Detects your arc-node's validator address and platform
#   2. POSTs /community/register to each seed every 60s. Tries port
#      9090 first (v0.7.0+ native handler), falls back to 3001 only
#      for the rapidly-shrinking set of seeds still running the old
#      Python sidecar.
#   3. POSTs /community/heartbeat every 15s to stay alive (TTL 90s)
#
# Usage:
#   nohup bash scripts/arc-community-register.sh &
# ─────────────────────────────────────────────────────────────────────────────

set -u

ARC_RPC="${ARC_RPC:-http://localhost:9944}"
# Legacy port for the v0.6.x Python gateway sidecar — kept for
# transitional compat only. v0.7.0+ folds the gateway into arc-node
# itself on port 9090.
LEGACY_GATEWAY_PORT=3001

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

echo "ARC Community Worker Registration (legacy script)"
echo "  worker_id: $WORKER_ID"
echo "  platform:  $PLATFORM"
echo "  arc-node:  $ARC_RPC"
echo "  seeds:     8 (will try arc-node :9090 first, legacy gateway :$LEGACY_GATEWAY_PORT as fallback)"
echo "  note:      arc-node v0.7.0+ auto-registers natively — this script is unnecessary on new installs"
echo ""

REGISTER_JSON="{\"worker_id\":\"$WORKER_ID\",\"name\":\"$HOSTNAME\",\"platform\":\"$PLATFORM\",\"capabilities\":[\"inference\"]}"
HEARTBEAT_JSON="{\"worker_id\":\"$WORKER_ID\"}"

TICK=0
REGISTERED=0
while true; do
    for seed in $SEEDS; do
        # Prefer arc-node native (9090); fall back to legacy gateway (3001)
        # only for seeds that haven't upgraded to v0.7.0+ yet.
        for port in 9090 $LEGACY_GATEWAY_PORT; do
            EP="http://${seed}:${port}"
            if [ $((TICK % 4)) -eq 0 ]; then
                # Full register every 60s
                if curl -sf -m 5 -X POST "$EP/community/register" \
                    -H "Content-Type: application/json" \
                    -d "$REGISTER_JSON" >/dev/null 2>&1; then
                    if [ $REGISTERED -eq 0 ]; then
                        echo "  ✓ Registered with $EP"
                        REGISTERED=1
                    fi
                fi
            else
                curl -sf -m 5 -X POST "$EP/community/heartbeat" \
                    -H "Content-Type: application/json" \
                    -d "$HEARTBEAT_JSON" >/dev/null 2>&1
            fi
        done
    done
    TICK=$((TICK + 1))
    sleep 15
done
