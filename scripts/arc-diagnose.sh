#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Community Node Diagnostic
#
# Run this if your node is "running" but not syncing. It checks the 4 things
# that break community installs most often:
#
#   1. Can you reach the 8 testnet seeds via UDP 9091 (QUIC)?
#   2. Is your local arc-node process up and what port is it on?
#   3. How many peers is your node actually connected to?
#   4. Is your dag_round advancing or stuck proposing blocks alone?
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-diagnose.sh | bash
#   # or:
#   bash scripts/arc-diagnose.sh [--port 9944]
#
# After running, paste the output in Discord / GitHub issue if you need help.
# ─────────────────────────────────────────────────────────────────────────────
set -u

RPC_PORT="${ARC_RPC_PORT:-9944}"
while [ $# -gt 0 ]; do
    case "$1" in
        --port) RPC_PORT="$2"; shift 2 ;;
        *) shift ;;
    esac
done

RED=$'\033[0;31m'
GREEN=$'\033[0;32m'
YELLOW=$'\033[0;33m'
BOLD=$'\033[1m'
RESET=$'\033[0m'

ok()    { printf "  %s✓%s %s\n"     "$GREEN"  "$RESET" "$1"; }
fail()  { printf "  %s✗%s %s\n"     "$RED"    "$RESET" "$1"; }
warn()  { printf "  %s⚠%s %s\n"     "$YELLOW" "$RESET" "$1"; }
info()  { printf "  %s•%s %s\n"     "$BOLD"   "$RESET" "$1"; }
header(){ printf "\n%s%s═══ %s ═══%s\n" "$BOLD" "$GREEN" "$1" "$RESET"; }

SEEDS="149.28.32.76:NYC 140.82.16.112:LAX 136.244.109.1:AMS 104.238.171.11:LHR 202.182.107.41:NRT 149.28.153.31:SGP 216.238.120.27:SAO 139.84.237.49:JNB"

header "1/4 — Seed node UDP reachability (QUIC on port 9091)"
REACHABLE=0
UNREACHABLE=0
for entry in $SEEDS; do
    ip="${entry%:*}"
    name="${entry#*:}"
    if command -v nc >/dev/null 2>&1; then
        if nc -zu -w 3 "$ip" 9091 >/dev/null 2>&1; then
            ok "$name ($ip:9091) — UDP reachable"
            REACHABLE=$((REACHABLE + 1))
        else
            fail "$name ($ip:9091) — UDP unreachable (firewall/ISP?)"
            UNREACHABLE=$((UNREACHABLE + 1))
        fi
    else
        warn "nc not installed — skipping UDP reachability check"
        break
    fi
done
if [ "$UNREACHABLE" -gt 0 ]; then
    warn "$UNREACHABLE of 8 seeds are unreachable via UDP 9091"
    echo "    → Your firewall or ISP is blocking outbound QUIC (UDP 9091)."
    echo "    → Common fixes: allow UDP 9091 outbound, disable VPN, try residential net."
fi

header "2/4 — Local arc-node process"
if pgrep -f "arc-node --rpc" >/dev/null 2>&1; then
    PID=$(pgrep -f "arc-node --rpc" | head -1)
    ok "arc-node running (PID $PID)"
    CMDLINE=$(ps -p "$PID" -o command= 2>/dev/null | head -c 200)
    info "command: ${CMDLINE:-unknown}"
else
    fail "arc-node is NOT running"
    echo "    → Start it with: launchctl load ~/Library/LaunchAgents/com.arc.inference.plist"
    echo "    → Or check logs: tail -50 ~/.arc/node.log"
fi

header "3/4 — Local RPC + peer count"
HEALTH=$(curl -sf -m 3 "http://localhost:$RPC_PORT/health" 2>/dev/null || echo "")
if [ -z "$HEALTH" ]; then
    fail "http://localhost:$RPC_PORT/health not responding"
    echo "    → Node is either still loading the model (wait 30-60s) or crashed."
    echo "    → Check logs: tail -50 ~/.arc/node.log"
else
    ok "RPC responding on :$RPC_PORT"
    PEERS=$(echo "$HEALTH" | sed -n 's/.*"peers":\([0-9][0-9]*\).*/\1/p')
    ROUND=$(echo "$HEALTH" | sed -n 's/.*"dag_round":\([0-9][0-9]*\).*/\1/p')
    UPTIME=$(echo "$HEALTH" | sed -n 's/.*"uptime_secs":\([0-9][0-9]*\).*/\1/p')
    info "uptime: ${UPTIME:-?} s"
    info "dag_round: ${ROUND:-?}"
    if [ -z "$PEERS" ] || [ "$PEERS" -eq 0 ] 2>/dev/null; then
        fail "peers: 0 — NODE IS ISOLATED"
        echo "    → This is the core bug. The node is running but can't reach any seed."
        echo "    → If UDP test above failed: fix firewall and restart the node."
        echo "    → If UDP test passed: look for Handshake errors in node.log:"
        echo "         tail -100 ~/.arc/node.log | grep -E 'Handshake|Failed|Timeout'"
    else
        ok "peers: $PEERS"
    fi
fi

header "4/4 — Chain sync status"
if [ -n "${ROUND:-}" ] && [ "$ROUND" -gt 0 ] 2>/dev/null; then
    # Poll each seed and take the MAX dag_round so a single outage doesn't
    # skew the gap calculation. Seeds with 0 peers / DOWN are skipped.
    REMOTE_ROUND=0
    REMOTE_NAME=""
    for entry in $SEEDS; do
        rip="${entry%%:*}"
        rname="${entry##*:}"
        rr=$(curl -sf -m 4 "http://$rip:9090/health" 2>/dev/null \
            | sed -n 's/.*"dag_round":\([0-9][0-9]*\).*/\1/p')
        if [ -n "$rr" ] && [ "$rr" -gt "$REMOTE_ROUND" ] 2>/dev/null; then
            REMOTE_ROUND=$rr
            REMOTE_NAME=$rname
        fi
    done
    if [ "$REMOTE_ROUND" -gt 0 ]; then
        GAP=$((REMOTE_ROUND - ROUND))
        if [ "$GAP" -lt 100 ]; then
            ok "Local round $ROUND vs $REMOTE_NAME $REMOTE_ROUND (gap: $GAP — synced)"
        elif [ "$GAP" -lt 10000 ]; then
            warn "Local round $ROUND vs $REMOTE_NAME $REMOTE_ROUND (gap: $GAP — catching up)"
        else
            fail "Local round $ROUND vs $REMOTE_NAME $REMOTE_ROUND (gap: $GAP — NOT SYNCED, isolated)"
            echo "    → Your node is proposing its own DAG blocks in isolation."
            echo "    → It has never successfully synced with the real testnet chain."
            echo "    → Fix the peer connectivity issue above, then restart the node."
        fi
    else
        warn "Could not reach any seed's /health — skipping sync-gap check."
    fi
else
    info "Local round not yet set — node may still be booting."
fi

echo ""
printf "%s%sPaste this output in Discord or GitHub if you still need help.%s\n" "$BOLD" "$GREEN" "$RESET"
echo ""
