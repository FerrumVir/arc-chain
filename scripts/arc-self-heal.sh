#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — on-host self-heal daemon (closes GH #30)
#
# Runs on EACH seed host as a systemd service. Polls localhost:9090/health
# every 30 s and restarts the local arc-node process on either of:
#
#   1. SILENT: /health gives no response for ≥60 s (RPC hang, deadlock).
#   2. DRIFT:  dag_round unchanged for ≥300 s AND a remote peer (read from
#              testnet-seeds.txt) is ≥100 rounds ahead.
#
# Replaces the manual kick-loop TJ ran during the autopilot window. The
# restart reuses the exact argv + environment of the live process by reading
# /proc/PID/cmdline and /proc/PID/environ — so every --shard-range and the
# critical ARC_PUBLIC_SOCKET variable survive the restart.
#
# Safety:
#   - 5-minute debounce between restarts of the same node (no flapping).
#   - For DRIFT restarts (but not SILENT) we require ≥4 healthy remote peers
#     so a cascade of drifts can't take the chain below consensus quorum.
#   - Preserves the last known-good cmdline to a state file so we can still
#     relaunch if arc-node crashed in the window between last poll and now.
#
# Logs go to /root/arc-chain/self-heal.log AND systemd journal.
#
# Env overrides (all optional):
#   ARC_DIR=/root/arc-chain
#   ARC_RPC_PORT=9090
#   ARC_POLL_INTERVAL=30
#   ARC_SILENT_THRESHOLD=60
#   ARC_DRIFT_THRESHOLD=300
#   ARC_PEER_ADVANCE_MIN=100
#   ARC_RESTART_DEBOUNCE=300
#   ARC_MIN_HEALTHY_PEERS=4
# ─────────────────────────────────────────────────────────────────────────────

# Never use `set -e` — one transient curl failure must not kill the daemon.
set -u

ARC_DIR="${ARC_DIR:-/root/arc-chain}"
RPC_PORT="${ARC_RPC_PORT:-9090}"
POLL_INTERVAL="${ARC_POLL_INTERVAL:-30}"
# 180s silent threshold — longer than the issue's initial 60s proposal. The
# multi-range model reload on seed hosts takes ~3 min (3× GGUF open), during
# which /health is silent. 60s tripped during normal boot and risked flapping.
SILENT_THRESHOLD="${ARC_SILENT_THRESHOLD:-180}"
DRIFT_THRESHOLD="${ARC_DRIFT_THRESHOLD:-300}"
PEER_ADVANCE_MIN="${ARC_PEER_ADVANCE_MIN:-100}"
# 600s debounce — with a ~3 min cold-boot, 300s could re-restart a node still
# loading its shards. 10 minutes gives a restarted node generous room to fully
# rejoin before the daemon is allowed to try again.
RESTART_DEBOUNCE="${ARC_RESTART_DEBOUNCE:-600}"
MIN_HEALTHY_PEERS="${ARC_MIN_HEALTHY_PEERS:-4}"

SEEDS_FILE="${ARC_DIR}/testnet-seeds.txt"
LOG="${ARC_DIR}/self-heal.log"
STATE_FILE="${ARC_DIR}/.self-heal-last-good.sh"

# 2026-04-27: arc-inference-traffic.service was crowding the chain's
# block-production slot with stale benchmark Transfer txs (one tx/block,
# 100% rejected at execute_tx because the synthetic sender has no
# state) — drowning real submissions including Milestone B's escrow
# release flow. Unconditionally stop + disable the service every poll
# so a manual `systemctl start` or a future package install can't
# silently re-enable the hog.
#
# Override with ALLOW_INFERENCE_TRAFFIC=1 (e.g. dedicated benchmark
# nodes) — but the production seed config never wants this on.
ALLOW_INFERENCE_TRAFFIC="${ALLOW_INFERENCE_TRAFFIC:-0}"

mkdir -p "$ARC_DIR"
touch "$LOG"

log() {
    printf '[%s] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" | tee -a "$LOG"
}

# Resolve the process's public socket so we exclude ourselves from peer polls.
# Use pgrep -x so we match only comm="arc-node" (the binary), not SCREEN/bash
# processes whose argv happens to contain the string "arc-node".
find_pid()        { pgrep -x arc-node | head -1; }
read_env_var()    { tr '\0' '\n' < "/proc/$1/environ" 2>/dev/null | grep -E "^$2=" | head -1 | cut -d= -f2-; }
read_cmdline()    { tr '\0' ' '  < "/proc/$1/cmdline" 2>/dev/null; }
read_cwd()        { readlink "/proc/$1/cwd" 2>/dev/null; }

SELF_SOCKET=""
refresh_self_socket() {
    local pid
    pid=$(find_pid)
    if [ -n "$pid" ]; then
        local sock
        sock=$(read_env_var "$pid" ARC_PUBLIC_SOCKET)
        [ -n "$sock" ] && SELF_SOCKET="$sock"
    fi
}

# Curl localhost /health. Empty output means no response.
get_self_health() {
    curl -sf -m 10 "http://127.0.0.1:${RPC_PORT}/health" 2>/dev/null
}

parse_round() {
    echo "$1" | grep -oE '"dag_round":[0-9]+' | grep -oE '[0-9]+' | head -1
}

# Iterate testnet-seeds.txt (one socket per line) and curl each peer's
# /health. Returns both the max peer round and the count of healthy peers.
# Prints: "BEST_ROUND HEALTHY_COUNT".
peer_snapshot() {
    local best=0 healthy=0
    # De-dup on IP (seeds file lists each node on both 9091 and 443).
    local seen_ips=""
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        case "$line" in '#'*) continue ;; esac
        local socket ip
        socket=$(echo "$line" | grep -oE '([0-9]{1,3}\.){3}[0-9]{1,3}:[0-9]+' | head -1)
        [ -z "$socket" ] && continue
        ip="${socket%%:*}"
        case " $seen_ips " in *" $ip "*) continue ;; esac
        seen_ips="$seen_ips $ip"
        # Skip ourselves.
        if [ -n "$SELF_SOCKET" ]; then
            local self_ip="${SELF_SOCKET%%:*}"
            [ "$ip" = "$self_ip" ] && continue
        fi
        local h round
        h=$(curl -sf -m 5 "http://${ip}:${RPC_PORT}/health" 2>/dev/null)
        [ -z "$h" ] && continue
        healthy=$((healthy + 1))
        round=$(parse_round "$h")
        [ -z "$round" ] && continue
        [ "$round" -gt "$best" ] && best=$round
    done < "$SEEDS_FILE"
    echo "$best $healthy"
}

# Snapshot the live process so we can replay it exactly. Writes to STATE_FILE
# as a sourceable bash script.
persist_last_good() {
    local pid
    pid=$(find_pid)
    [ -z "$pid" ] && return 1
    local cmdline cwd env_lines
    cmdline=$(read_cmdline "$pid")
    cwd=$(read_cwd "$pid")
    [ -z "$cmdline" ] && return 1
    [ -z "$cwd" ] && cwd="$ARC_DIR"
    # Preserve ARC_* plus PATH (needed for relative binary paths).
    env_lines=$(tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null \
        | grep -E '^(ARC_|PATH=|HOME=|USER=|LANG=)')
    {
        printf '# arc-self-heal last-good snapshot — %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        printf 'LAST_CWD=%q\n' "$cwd"
        printf 'LAST_CMDLINE=%q\n' "$cmdline"
        printf 'LAST_ENV=(\n'
        while IFS= read -r kv; do
            [ -z "$kv" ] && continue
            printf '    %q\n' "$kv"
        done <<< "$env_lines"
        printf ')\n'
    } > "$STATE_FILE.tmp" && mv "$STATE_FILE.tmp" "$STATE_FILE"
}

# Relaunch arc-node. Prefers live /proc snapshot, falls back to STATE_FILE.
restart_arc_node() {
    local reason="$1"
    log "RESTART reason=\"$reason\""

    local cwd cmdline env_lines source="live"
    local pid
    pid=$(find_pid)
    if [ -n "$pid" ]; then
        cwd=$(read_cwd "$pid")
        cmdline=$(read_cmdline "$pid")
        env_lines=$(tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null \
            | grep -E '^(ARC_|PATH=|HOME=|USER=|LANG=)')
    fi

    if [ -z "${cmdline:-}" ] && [ -f "$STATE_FILE" ]; then
        log "  arc-node not running; restoring from last-good snapshot"
        # shellcheck disable=SC1090
        source "$STATE_FILE"
        cwd="${LAST_CWD:-$ARC_DIR}"
        cmdline="${LAST_CMDLINE:-}"
        env_lines=$(printf '%s\n' "${LAST_ENV[@]:-}")
        source="snapshot"
    fi

    if [ -z "${cmdline:-}" ]; then
        log "  ABORT: no cmdline available (live dead + no snapshot). Operator must intervene."
        return 1
    fi
    [ -z "${cwd:-}" ] && cwd="$ARC_DIR"

    # Hard kill any surviving arc-node. -9 because we've already decided it's
    # broken — a clean shutdown would hang on the same path that hung /health.
    # -x matches the exact comm="arc-node" (not argv substrings) so we can't
    # accidentally SIGKILL ourselves or any other process whose cmdline has
    # "arc-node" in it.
    pkill -9 -x arc-node 2>/dev/null || true
    sleep 3

    # Re-export the captured environment in a subshell so our own daemon's
    # env stays clean.
    local boot_log="${ARC_DIR}/self-heal-boot.log"
    (
        while IFS= read -r kv; do
            [ -z "$kv" ] && continue
            export "$kv"
        done <<< "$env_lines"
        cd "$cwd" || exit 1
        # setsid + nohup + disown per autopilot learnings — screen-based
        # launches dropped silently from non-tty ssh.
        setsid nohup bash -c "exec $cmdline" </dev/null >>"$boot_log" 2>&1 &
        disown
    )
    log "  launched from ${source} snapshot (cwd=$cwd)"
}

# ── Main loop ────────────────────────────────────────────────────────────────
log "arc-self-heal starting"
log "  poll=${POLL_INTERVAL}s silent=${SILENT_THRESHOLD}s drift=${DRIFT_THRESHOLD}s peer_advance_min=${PEER_ADVANCE_MIN} debounce=${RESTART_DEBOUNCE}s min_healthy_peers=${MIN_HEALTHY_PEERS}"

refresh_self_socket
log "  self_socket=${SELF_SOCKET:-unknown}"

SILENT_SINCE=0
DRIFT_ROUND=0
DRIFT_SINCE=0
LAST_RESTART=0

while true; do
    NOW=$(date +%s)

    # ── PERMANENT RULE: kill stale benchmark traffic source ─────────────
    # arc-inference-traffic submits null-sig Transfer txs that always
    # fail at execute_tx but consume 100% of the per-block tx slot,
    # blocking real submissions (e.g. Milestone B's escrow release).
    # Stop + disable on every poll unless explicitly allowed.
    if [ "$ALLOW_INFERENCE_TRAFFIC" != "1" ]; then
        if systemctl is-active arc-inference-traffic.service 2>/dev/null | grep -qx active; then
            log "stale benchmark traffic detected — stopping arc-inference-traffic.service"
            systemctl stop arc-inference-traffic.service 2>/dev/null || true
            systemctl disable arc-inference-traffic.service 2>/dev/null || true
        fi
    fi

    # Keep SELF_SOCKET fresh in case arc-node restarted.
    [ -z "$SELF_SOCKET" ] && refresh_self_socket

    HEALTH=$(get_self_health)

    if [ -z "$HEALTH" ]; then
        [ "$SILENT_SINCE" = 0 ] && { SILENT_SINCE=$NOW; log "silent: first miss"; }
        SILENT_DUR=$((NOW - SILENT_SINCE))
        if [ "$SILENT_DUR" -ge "$SILENT_THRESHOLD" ]; then
            if [ $((NOW - LAST_RESTART)) -ge "$RESTART_DEBOUNCE" ]; then
                restart_arc_node "/health silent for ${SILENT_DUR}s"
                LAST_RESTART=$NOW
                SILENT_SINCE=0
                DRIFT_SINCE=0
                DRIFT_ROUND=0
            else
                log "silent ${SILENT_DUR}s but debounce active ($((NOW - LAST_RESTART))s since last restart)"
            fi
        fi
        sleep "$POLL_INTERVAL"
        continue
    fi

    # Healthy response. Reset silence counter and persist cmdline for later fallback.
    if [ "$SILENT_SINCE" != 0 ]; then
        log "silent cleared"
        SILENT_SINCE=0
    fi
    persist_last_good || true

    MY_ROUND=$(parse_round "$HEALTH")
    [ -z "$MY_ROUND" ] && MY_ROUND=0

    if [ "$MY_ROUND" = "$DRIFT_ROUND" ] && [ "$MY_ROUND" != 0 ]; then
        [ "$DRIFT_SINCE" = 0 ] && DRIFT_SINCE=$NOW
        DRIFT_DUR=$((NOW - DRIFT_SINCE))
        if [ "$DRIFT_DUR" -ge "$DRIFT_THRESHOLD" ]; then
            SNAP=$(peer_snapshot)
            BEST_PEER=${SNAP%% *}
            HEALTHY_COUNT=${SNAP##* }
            DELTA=$((BEST_PEER - MY_ROUND))
            if [ "$BEST_PEER" -gt "$MY_ROUND" ] && [ "$DELTA" -ge "$PEER_ADVANCE_MIN" ]; then
                if [ "$HEALTHY_COUNT" -lt "$MIN_HEALTHY_PEERS" ]; then
                    log "drift detected (my=$MY_ROUND peer=$BEST_PEER Δ=$DELTA) but only $HEALTHY_COUNT healthy peers (<$MIN_HEALTHY_PEERS); refusing restart — chain already fragile"
                elif [ $((NOW - LAST_RESTART)) -lt "$RESTART_DEBOUNCE" ]; then
                    log "drift detected but debounce active ($((NOW - LAST_RESTART))s since last restart)"
                else
                    restart_arc_node "drift: round $MY_ROUND unchanged ${DRIFT_DUR}s; peer at $BEST_PEER (Δ=$DELTA); healthy_peers=$HEALTHY_COUNT"
                    LAST_RESTART=$NOW
                    DRIFT_SINCE=0
                    DRIFT_ROUND=0
                fi
            else
                log "round frozen ${DRIFT_DUR}s at $MY_ROUND but no peer ≥$PEER_ADVANCE_MIN ahead (best=$BEST_PEER Δ=$DELTA healthy=$HEALTHY_COUNT); holding"
            fi
        fi
    else
        DRIFT_ROUND=$MY_ROUND
        DRIFT_SINCE=0
    fi

    sleep "$POLL_INTERVAL"
done
