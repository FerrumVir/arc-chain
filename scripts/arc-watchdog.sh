#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Testnet Watchdog
#
# Polls each of the 8 testnet seed nodes every 30 seconds. Detects two
# failure modes and auto-restarts the affected node:
#
#   1. STUCK: dag_round hasn't advanced in 120 seconds (consensus frozen)
#   2. ISOLATED: 0 peers after 240 seconds of uptime (network partition)
#
# When restarting, the watchdog reads the current cmdline via ps -ef and
# preserves --shard-start, --shard-end, and --model flags. This is critical
# for sharded inference: a restart that drops the shard flags would break
# the pipeline.
#
# Bash 3.2 compatible (no associative arrays — uses parallel arrays).
#
# Usage:
#   # Run in background:
#   nohup bash scripts/arc-watchdog.sh > /tmp/arc-watchdog.log 2>&1 &
#
#   # Tail the log:
#   tail -f /tmp/arc-watchdog.log
#
#   # Stop:
#   pkill -f arc-watchdog.sh
#
# Requires SSH access to all 8 nodes via $HOME/.ssh/id_ed25519.
# ─────────────────────────────────────────────────────────────────────────────

SSH_OPTS="-i $HOME/.ssh/id_ed25519 -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o BatchMode=yes"
NAMES=(NYC LAX AMS LHR NRT SGP SAO JNB)
IPS=(149.28.32.76 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31 216.238.120.27 139.84.237.49)

# Parallel arrays for state
LAST_ROUND=(0 0 0 0 0 0 0 0)
STUCK_SINCE=(0 0 0 0 0 0 0 0)

log() { echo "[$(date '+%H:%M:%S')] $*"; }

# Restart a stuck/dead node, preserving its existing shard + model flags
# (discovered via ps -ef on the remote machine).
restart_node() {
    local idx=$1
    local node="${NAMES[$idx]}"
    local ip="${IPS[$idx]}"
    # Read the live cmdline so we don't lose --shard-start/--shard-end
    local cmdline=$(ssh -o ConnectTimeout=10 -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no -o BatchMode=yes "root@${ip}" \
        "ps -ef | grep 'arc-node --rpc' | grep -v grep | head -1 | awk '{for(i=8;i<=NF;i++) printf \"%s \", \$i}'" 2>/dev/null || echo "")
    local model_flag="--model model.gguf"
    local shard_flag=""
    if echo "$cmdline" | grep -q -- '--model'; then
        local mp=$(echo "$cmdline" | grep -oE -- '--model [^ ]+' | awk '{print $2}')
        [ -n "$mp" ] && model_flag="--model $mp"
    fi
    if echo "$cmdline" | grep -q -- '--shard-start'; then
        local ss=$(echo "$cmdline" | grep -oE -- '--shard-start [0-9]+' | awk '{print $2}')
        local se=$(echo "$cmdline" | grep -oE -- '--shard-end [0-9]+' | awk '{print $2}')
        [ -n "$ss" ] && [ -n "$se" ] && shard_flag="--shard-start $ss --shard-end $se"
    fi
    log "Restarting stuck $node ($ip) [$model_flag $shard_flag]..."
    # Step 1: kill old (separate ssh, longer timeout)
    ssh -o ConnectTimeout=15 -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no -o BatchMode=yes "root@${ip}" "screen -S arc -X quit 2>/dev/null || true; pkill -9 -f arc-node 2>/dev/null || true; sleep 2; ps aux | grep arc-node | grep -v grep | wc -l" >/dev/null 2>&1 || true
    sleep 3
    # Step 2: start new node — preserve any shard flags we discovered
    ssh -o ConnectTimeout=15 -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no -o BatchMode=yes "root@${ip}" "cd /root/arc-chain && ARC_PUBLIC_SOCKET=${ip}:9090 screen -dmS arc -L -Logfile screenlog.0 ./target/release/arc-node --rpc 0.0.0.0:9090 --p2p-port 9091 --validator-seed ${node} --seeds-file testnet-seeds.txt --genesis genesis.toml --stake 5000000 --eth-rpc-port 0 ${model_flag} ${shard_flag}" >/dev/null 2>&1 || true
    sleep 5
    # Step 3: verify new process is running
    NEW_COUNT=$(ssh -o ConnectTimeout=15 -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no -o BatchMode=yes "root@${ip}" "ps aux | grep arc-node | grep -v grep | wc -l" 2>/dev/null || echo "?")
    if [ "$NEW_COUNT" = "0" ] || [ "$NEW_COUNT" = "?" ]; then
        log "  WARNING: $node restart did not produce a running process (count=$NEW_COUNT)"
    else
        log "  $node restart confirmed ($NEW_COUNT processes)"
    fi
}

log "Watchdog starting, checking every 30s, restart threshold 120s"

while true; do
    NOW=$(date +%s)
    for i in 0 1 2 3 4 5 6 7; do
        NODE="${NAMES[$i]}"
        IP="${IPS[$i]}"
        HEALTH=$(ssh $SSH_OPTS "root@${IP}" "curl -sf http://localhost:9090/health 2>/dev/null" 2>/dev/null || echo "")
        if [ -z "$HEALTH" ]; then
            # Node is unreachable — check if process is running at all
            PROC_COUNT=$(ssh $SSH_OPTS "root@${IP}" "ps aux | grep arc-node | grep -v grep | wc -l" 2>/dev/null || echo "?")
            if [ "$PROC_COUNT" = "0" ]; then
                log "$NODE has 0 processes — restarting"
                restart_node $i
                sleep 5
            fi
            continue
        fi
        ROUND=$(echo "$HEALTH" | grep -o '"dag_round":[0-9]*' | grep -o '[0-9]*' || echo "0")
        PEERS=$(echo "$HEALTH" | grep -o '"peers":[0-9]*' | grep -o '[0-9]*' || echo "0")
        UPTIME=$(echo "$HEALTH" | grep -o '"uptime_secs":[0-9]*' | grep -o '[0-9]*' || echo "0")

        # Detect isolated nodes (0 peers) — allow 240s grace period after startup
        # for cross-continent peer dial + handshake to complete.
        if [ "$PEERS" = "0" ] && [ "$UPTIME" -gt "240" ]; then
            log "$NODE isolated (0 peers, uptime ${UPTIME}s, round $ROUND) — restarting"
            restart_node $i
            LAST_ROUND[$i]=0
            STUCK_SINCE[$i]=0
            sleep 5
            continue
        fi

        PREV="${LAST_ROUND[$i]}"
        if [ "$PREV" = "$ROUND" ]; then
            STUCK_START="${STUCK_SINCE[$i]}"
            if [ "$STUCK_START" = "0" ]; then
                STUCK_SINCE[$i]=$NOW
                STUCK_START=$NOW
            fi
            STUCK_DUR=$((NOW - STUCK_START))
            if [ $STUCK_DUR -gt 120 ]; then
                log "$NODE STUCK at round $ROUND for ${STUCK_DUR}s — restarting"
                restart_node $i
                STUCK_SINCE[$i]=0
                LAST_ROUND[$i]=0
            fi
        else
            LAST_ROUND[$i]=$ROUND
            STUCK_SINCE[$i]=0
        fi
    done
    sleep 30
done
