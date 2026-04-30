#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Network Health Check
#
# Pings the /health endpoint of all 8 testnet seed nodes via SSH and prints
# peer count + dag_round for each. Reports STATUS: ALL HEALTHY or DOWN.
#
# Usage:
#   bash scripts/arc-health-check.sh
#
# Requires SSH access to all 8 nodes (used by the operator running the
# testnet). For external/public health, use curl directly:
#   curl http://149.28.32.76:9090/health
# ─────────────────────────────────────────────────────────────────────────────
SSH_KEY="$HOME/.ssh/id_ed25519"
SSH_OPTS="-i $SSH_KEY -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o BatchMode=yes"
NAMES=(NYC LAX AMS LHR NRT SGP SAO JNB)
IPS=(149.28.32.76 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31 216.238.120.27 139.84.237.49)
echo "=== ARC Health Check $(date) ==="
ALL_OK=true
for i in 0 1 2 3 4 5 6 7; do
    HEALTH=$(ssh $SSH_OPTS "root@${IPS[$i]}" "curl -sf http://localhost:9090/health 2>/dev/null" 2>/dev/null || echo "")
    if [ -n "$HEALTH" ]; then
        PEERS=$(echo "$HEALTH" | grep -o '"peers":[0-9]*' | grep -o '[0-9]*')
        ROUND=$(echo "$HEALTH" | grep -o '"dag_round":[0-9]*' | grep -o '[0-9]*')
        printf "  %-4s peers=%-2s round=%s\n" "${NAMES[$i]}" "$PEERS" "$ROUND"
    else
        printf "  %-4s DOWN\n" "${NAMES[$i]}"
        ALL_OK=false
    fi
done
if [ "$ALL_OK" = true ]; then
    echo "STATUS: ALL HEALTHY"
else
    echo "STATUS: SOME NODES DOWN"
fi
