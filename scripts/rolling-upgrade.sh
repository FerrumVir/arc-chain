#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Rolling Upgrade (zero-downtime)
#
# Builds on NYC, pipes binary to each node, restarts one at a time.
# Waits for health check before proceeding to next node.
#
# Usage:
#   ./scripts/rolling-upgrade.sh                       # Upgrade all 8 nodes (halt on first fail)
#   ./scripts/rolling-upgrade.sh --build-only          # Just build, don't deploy
#   ./scripts/rolling-upgrade.sh --skip-build          # Deploy existing binary
#   ./scripts/rolling-upgrade.sh --build-ip=<IP>       # Force a specific build host
#   ./scripts/rolling-upgrade.sh --continue-on-fail    # Keep going past a bad node (default: halt)
#   ./scripts/rolling-upgrade.sh --reset-state         # Clear dag-wal/state.wal on each node
#   ./scripts/rolling-upgrade.sh --shard-map "NYC:0:5 LAX:5:10 AMS:10:14 ..."
#                                                      # Override shard assignments during restart.
#                                                      # Format: "NAME:start:end NAME:start:end ...".
#                                                      # Any node not in the map keeps its existing
#                                                      # --shard-start/--shard-end flags (from ps).
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SSH_KEY="$HOME/.ssh/id_ed25519"
SSH_OPTS="-i $SSH_KEY -o ConnectTimeout=10 -o StrictHostKeyChecking=no -o BatchMode=yes"

# Node names, IPs, and seeds — parallel arrays (bash 3.2 compatible)
NODE_NAMES=(NYC     LAX            AMS            LHR             NRT             SGP            SAO             JNB)
NODE_IPS=(149.28.32.76 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31 216.238.120.27 139.84.237.49)

# Ports — match what's currently running on seeds
RPC_PORT=9090
P2P_PORT=9091

# Colors
BOLD="\033[1m" GREEN="\033[32m" CYAN="\033[36m" YELLOW="\033[33m" RED="\033[31m" RESET="\033[0m"
info()  { printf "${CYAN}[INFO]${RESET}  %s\n" "$*"; }
ok()    { printf "${GREEN}[  OK]${RESET}  %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${RESET}  %s\n" "$*"; }
fail()  { printf "${RED}[FAIL]${RESET}  %s\n" "$*" >&2; exit 1; }

BUILD_ONLY=false
SKIP_BUILD=false
RESET_STATE=false
HALT_ON_FAIL=true
BUILD_IP_OVERRIDE=""
SHARD_MAP=""
ONLY_NODES=""
MODEL_FILE_OVERRIDE=""
for arg in "$@"; do
    case "$arg" in
        --build-only)       BUILD_ONLY=true ;;
        --skip-build)       SKIP_BUILD=true ;;
        --reset-state)      RESET_STATE=true ;;
        --continue-on-fail) HALT_ON_FAIL=false ;;
        --build-ip=*)       BUILD_IP_OVERRIDE="${arg#--build-ip=}" ;;
        --shard-map=*)      SHARD_MAP="${arg#--shard-map=}" ;;
        --only=*)           ONLY_NODES="${arg#--only=}" ;;
        --model-file=*)     MODEL_FILE_OVERRIDE="${arg#--model-file=}" ;;
    esac
done
export RESET_STATE

# Parser for SHARD_MAP. Given NAME, returns the concatenated
# --shard-range flags this node should run with, or empty.
#
# Two supported formats per entry (whitespace-separated):
#
#   Single range (legacy):
#       NAME:START:END
#
#   Multi range (new — 3× replication):
#       NAME=A:B,C:D,E:F
#       Every comma-separated piece is emitted as its own --shard-range flag.
#
# Example multi-range input:
#   --shard-map "NYC=0:6,21:26 LAX=0:6,6:11,26:32 AMS=6:11,11:16 ..."
# Example legacy input:
#   --shard-map "NYC:0:5 LAX:5:10 AMS:10:14 ..."
shard_flags_for_node() {
    local name="$1"
    [ -z "$SHARD_MAP" ] && return 0
    for entry in $SHARD_MAP; do
        local n_eq="${entry%%=*}"
        if [ "$n_eq" != "$entry" ] && [ "$n_eq" = "$name" ]; then
            # Multi-range format: NAME=A:B,C:D
            local ranges="${entry#*=}"
            local out=""
            local IFS=","
            for r in $ranges; do
                [ -z "$r" ] && continue
                out="$out --shard-range $r"
            done
            unset IFS
            echo "$out"
            return 0
        fi
        local n="${entry%%:*}"
        if [ "$n" = "$name" ]; then
            # Legacy single-range format: NAME:START:END
            local rest="${entry#*:}"
            local s="${rest%%:*}"
            local e="${rest##*:}"
            echo "--shard-range ${s}:${e}"
            return 0
        fi
    done
}

# Auto-select a live build host. Historically BUILD_IP=NYC (index 0), but
# NYC (or any single node) can be down during an upgrade cycle — we must not
# assume the first node is alive. Skip any IP whose /health doesn't respond.
# Override with --build-ip=<IP> if you want to force a specific builder.
if [ -n "$BUILD_IP_OVERRIDE" ]; then
    BUILD_IP="$BUILD_IP_OVERRIDE"
    info "Using build host override: $BUILD_IP"
else
    BUILD_IP=""
    for candidate in "${NODE_IPS[@]}"; do
        if curl -sf -m 4 "http://${candidate}:${RPC_PORT}/health" >/dev/null 2>&1; then
            BUILD_IP="$candidate"
            info "Auto-selected live build host: $BUILD_IP"
            break
        fi
    done
    if [ -z "$BUILD_IP" ]; then
        fail "No node responded to /health — cannot auto-select build host. Use --build-ip=<IP> or --skip-build."
    fi
fi

# ── 1. Build on NYC ──────────────────────────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
    info "Pushing latest code to NYC ($BUILD_IP)..."

    # Sync local repo to the build host. IMPORTANT: excludes below protect
    # runtime artifacts on the remote — `--delete` removes anything in the
    # destination that isn't in the source, so any path that must survive
    # must be listed here. Chain state (arc-data/), model weights (*.gguf),
    # logs, and host-local generated files all live on the seed and must
    # NEVER be wiped by a code sync.
    rsync -az --delete \
        --exclude 'target/' \
        --exclude '.git/' \
        --exclude 'arc-data/' \
        --exclude 'arc-data-*/' \
        --exclude '*.gguf' \
        --exclude '*.arc-int8' \
        --exclude 'model.gguf' \
        --exclude 'known_peers.json' \
        --exclude 'node.log' \
        --exclude '*.log' \
        --exclude 'benchmark-*.json' \
        --exclude 'config.toml' \
        -e "ssh $SSH_OPTS" \
        "$HOME/arc-chain/" "root@${BUILD_IP}:/root/arc-chain/"

    ok "Code synced to NYC"

    info "Building on NYC (this takes 2-5 minutes)..."
    ssh $SSH_OPTS "root@${BUILD_IP}" \
        "source /root/.cargo/env && cd /root/arc-chain && cargo build --release -p arc-node --features candle 2>&1 | tail -5"

    ok "Build complete on NYC"
fi

[ "$BUILD_ONLY" = true ] && { ok "Build-only mode — done."; exit 0; }

# ── 2. Rolling deploy ────────────────────────────────────────────────────────
TOTAL=${#NODE_NAMES[@]}
DEPLOYED=0

for idx in $(seq 0 $((TOTAL - 1))); do
    NODE="${NODE_NAMES[$idx]}"
    IP="${NODE_IPS[$idx]}"
    # Validator seed MUST match genesis.toml [[validators]] seed (e.g. "NYC", "LAX")
    # or the derived address won't match the genesis validator set.
    VALIDATOR_SEED="${NODE}"

    # --only=A,B,C skips every node not in the comma-separated list. Makes
    # a rollout of a single node safe and explicit.
    if [ -n "$ONLY_NODES" ]; then
        case ",${ONLY_NODES}," in
            *,"${NODE}",*) : ;;
            *) continue ;;
        esac
    fi

    DEPLOYED=$((DEPLOYED + 1))
    echo ""
    printf "${BOLD}── [$DEPLOYED/$TOTAL] Upgrading $NODE ($IP) ──${RESET}\n"

    # a0. Snapshot the live --model flag BEFORE stopping the old process.
    # Doing this after step (b) would read from an empty ps and silently
    # swap the node onto the default TinyLlama model. --model-file=NAME
    # overrides the snapshot for this run.
    if [ -n "$MODEL_FILE_OVERRIDE" ]; then
        MODEL_FILE="$MODEL_FILE_OVERRIDE"
        info "Model file override: $MODEL_FILE"
    else
        MODEL_FILE=$(ssh $SSH_OPTS "root@${IP}" \
            "ps -eo args | grep 'arc-node' | grep -v grep | head -1 | grep -oE -- '--model [^ ]+' | awk '{print \$2}'" 2>/dev/null | head -1)
        MODEL_FILE="${MODEL_FILE:-model.gguf}"
        info "Snapshotted model file: $MODEL_FILE"
    fi

    # a. Copy binary from build node (pipe through localhost to avoid text-file-busy)
    if [ "$IP" != "$BUILD_IP" ]; then
        info "Copying binary from NYC to $NODE..."
        ssh $SSH_OPTS "root@${BUILD_IP}" "cat /root/arc-chain/target/release/arc-node" \
            | ssh $SSH_OPTS "root@${IP}" "cat > /tmp/arc-node-new && chmod +x /tmp/arc-node-new"
        ok "Binary copied"
    else
        ssh $SSH_OPTS "root@${IP}" "cp /root/arc-chain/target/release/arc-node /tmp/arc-node-new && chmod +x /tmp/arc-node-new"
    fi

    # b0. Determine shard flags. Priority:
    #   1. --shard-map override (operator explicitly assigns ranges — recommended)
    #   2. Snapshot from the currently-running process (preserve existing assignment,
    #      converting legacy --shard-start/--shard-end into a single --shard-range)
    #   3. None (validator/observer only)
    #
    # Preserving flags matters because without them nodes come back with no
    # shard assignment and the inference pipeline fragments — exactly the
    # failure mode arc-watchdog.sh guards against.
    SHARD_FLAGS=$(shard_flags_for_node "$NODE")
    if [ -n "$SHARD_FLAGS" ]; then
        ok "Using shard-map override for $NODE:$SHARD_FLAGS"
    else
        info "Snapshotting shard flags from running process..."
        # Try new --shard-range first (may occur multiple times), then legacy
        # --shard-start/--shard-end pair. Either way, emit as --shard-range
        # flags for the new binary.
        SHARD_FLAGS=$(ssh $SSH_OPTS "root@${IP}" "bash -s" <<'REMOTE'
PS_LINE=$(ps -eo args | grep 'arc-node' | grep -v grep | head -1)
echo "$PS_LINE" | grep -oE -- '--shard-range [0-9]+:[0-9]+' | tr '\n' ' '
LEGACY=$(echo "$PS_LINE" | grep -oE -- '--shard-start [0-9]+[[:space:]]+--shard-end [0-9]+')
if [ -n "$LEGACY" ]; then
    S=$(echo "$LEGACY" | awk '{print $2}')
    E=$(echo "$LEGACY" | awk '{print $4}')
    echo "--shard-range ${S}:${E}"
fi
REMOTE
        )
        SHARD_FLAGS=$(echo "$SHARD_FLAGS" | tr -s ' ' | sed 's/^ *//;s/ *$//')
        if [ -n "$SHARD_FLAGS" ]; then
            ok "Preserving existing shard assignment: $SHARD_FLAGS"
        else
            info "No shard flags on $NODE (validator/observer only)"
        fi
    fi

    # b. Stop old process (screen session named 'arc')
    info "Stopping old node..."
    ssh $SSH_OPTS "root@${IP}" "screen -S arc -X quit 2>/dev/null || true; sleep 1; pkill -f 'arc-node.*validator-seed' 2>/dev/null || true; sleep 1" || true

    # c. Move new binary into place
    ssh $SSH_OPTS "root@${IP}" "mv /tmp/arc-node-new /root/arc-chain/target/release/arc-node"

    # c2. Optional: clear old state if --reset-state flag is passed.
    # This is needed when switching validator identity (seed), since the
    # persisted validator set won't match the new address.
    if [ "${RESET_STATE:-false}" = "true" ]; then
        info "Resetting state (dag-wal, state.wal, known_peers)..."
        ssh $SSH_OPTS "root@${IP}" "rm -rf /root/arc-chain/arc-data/dag-wal /root/arc-chain/arc-data/state.wal /root/arc-chain/arc-data/known_peers.json"
    fi

    # d. Sync seeds file and genesis
    rsync -az -e "ssh $SSH_OPTS" \
        "$HOME/arc-chain/testnet-seeds.txt" "$HOME/arc-chain/genesis.toml" \
        "root@${IP}:/root/arc-chain/"

    # e. Start new node in screen. MODEL_FILE was snapshotted in step (a0)
    # — before the old process was killed — and must not be re-computed here.
    MODEL_FLAG="--model ${MODEL_FILE}"
    info "Starting new node${MODEL_FLAG:+ with inference (${MODEL_FILE})}${SHARD_FLAGS:+ + shard flags}..."
    ssh $SSH_OPTS "root@${IP}" "cd /root/arc-chain && screen -dmS arc ./target/release/arc-node \
        --rpc 0.0.0.0:${RPC_PORT} \
        --p2p-port ${P2P_PORT} \
        --validator-seed ${VALIDATOR_SEED} \
        --seeds-file testnet-seeds.txt \
        --genesis genesis.toml \
        --stake 5000000 \
        --eth-rpc-port 0 \
        ${MODEL_FLAG} \
        ${SHARD_FLAGS}"

    # f. Wait for health (up to 360 seconds — multi-range loads re-open
    # the GGUF once per range, which pushes a 3-range node to ~3 min on
    # cold boot. 60 s was too tight and caused false failures.)
    info "Waiting for health check..."
    HEALTHY=false
    for i in $(seq 1 36); do
        sleep 10
        HEALTH=$(ssh $SSH_OPTS "root@${IP}" "curl -sf http://localhost:${RPC_PORT}/health 2>/dev/null" || echo "")
        if [ -n "$HEALTH" ]; then
            PEERS=$(echo "$HEALTH" | grep -o '"peers":[0-9]*' | grep -o '[0-9]*' || echo "0")
            ROUND=$(echo "$HEALTH" | grep -o '"dag_round":[0-9]*' | grep -o '[0-9]*' || echo "?")
            ok "$NODE healthy — peers: $PEERS, round: $ROUND"
            HEALTHY=true
            break
        fi
        printf "  ... attempt $i/36\n"
    done

    if [ "$HEALTHY" = false ]; then
        warn "$NODE failed health check after 60s — check manually: ssh root@${IP}"
        if [ "$HALT_ON_FAIL" = "true" ]; then
            fail "Halting rollout so a bad binary doesn't propagate to the rest of the chain. \
Re-run with --continue-on-fail to override, or investigate \`ssh root@${IP} 'tail /root/arc-chain/node.log'\` first."
        else
            warn "Continuing anyway (--continue-on-fail set)..."
        fi
    fi

    # Post-restart sanity: wait 10s and verify dag_round advances. A node that
    # comes up with status=ok but frozen round is still broken — exactly the
    # failure mode the watchdog restarts for. Confirm *before* moving on.
    info "Verifying round advance on $NODE..."
    R1=$(ssh $SSH_OPTS "root@${IP}" "curl -sf http://localhost:${RPC_PORT}/health" 2>/dev/null \
        | grep -o '"dag_round":[0-9]*' | grep -o '[0-9]*' || echo 0)
    sleep 10
    R2=$(ssh $SSH_OPTS "root@${IP}" "curl -sf http://localhost:${RPC_PORT}/health" 2>/dev/null \
        | grep -o '"dag_round":[0-9]*' | grep -o '[0-9]*' || echo 0)
    if [ "$R2" -gt "$R1" ] 2>/dev/null; then
        ok "Round advanced $R1 -> $R2 (consensus healthy)"
    else
        warn "Round did not advance ($R1 -> $R2). Node may be isolated or stuck."
        if [ "$HALT_ON_FAIL" = "true" ]; then
            fail "Halting rollout — consensus progress check failed on $NODE."
        fi
    fi
done

# ── 3. Final status ──────────────────────────────────────────────────────────
echo ""
printf "${BOLD}${GREEN}════════════════════════════════════════════════════════════════${RESET}\n"
printf "${BOLD}${GREEN}  Rolling Upgrade Complete — All $TOTAL Nodes Deployed${RESET}\n"
printf "${BOLD}${GREEN}════════════════════════════════════════════════════════════════${RESET}\n"
echo ""

for idx in $(seq 0 $((TOTAL - 1))); do
    NODE="${NODE_NAMES[$idx]}"
    IP="${NODE_IPS[$idx]}"
    HEALTH=$(ssh $SSH_OPTS "root@${IP}" "curl -sf http://localhost:${RPC_PORT}/health 2>/dev/null" || echo "UNREACHABLE")
    if [ "$HEALTH" = "UNREACHABLE" ]; then
        printf "  ${RED}%-4s${RESET} (%s): UNREACHABLE\n" "$NODE" "$IP"
    else
        PEERS=$(echo "$HEALTH" | grep -o '"peers":[0-9]*' | grep -o '[0-9]*' || echo "?")
        ROUND=$(echo "$HEALTH" | grep -o '"dag_round":[0-9]*' | grep -o '[0-9]*' || echo "?")
        printf "  ${GREEN}%-4s${RESET} (%s): peers=%s round=%s\n" "$NODE" "$IP" "$PEERS" "$ROUND"
    fi
done

echo ""
info "Dashboard: http://140.82.16.112:3200"
info "Wallet:    http://140.82.16.112:3100"
echo ""
