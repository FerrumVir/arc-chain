#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Rolling Upgrade (zero-downtime)
#
# Builds on NYC, pipes binary to each node, restarts one at a time.
# Waits for health check before proceeding to next node.
#
# Usage:
#   ./scripts/rolling-upgrade.sh              # Upgrade all 8 nodes
#   ./scripts/rolling-upgrade.sh --build-only # Just build, don't deploy
#   ./scripts/rolling-upgrade.sh --skip-build # Deploy existing binary
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
for arg in "$@"; do
    case "$arg" in
        --build-only)  BUILD_ONLY=true ;;
        --skip-build)  SKIP_BUILD=true ;;
        --reset-state) RESET_STATE=true ;;
    esac
done
export RESET_STATE

BUILD_IP="${NODE_IPS[0]}"  # NYC

# ── 1. Build on NYC ──────────────────────────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
    info "Pushing latest code to NYC ($BUILD_IP)..."

    # Sync local repo to NYC (exclude target dir)
    rsync -az --delete \
        --exclude 'target/' --exclude '.git/' --exclude 'model.gguf' \
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

    DEPLOYED=$((DEPLOYED + 1))
    echo ""
    printf "${BOLD}── [$DEPLOYED/$TOTAL] Upgrading $NODE ($IP) ──${RESET}\n"

    # a. Copy binary from build node (pipe through localhost to avoid text-file-busy)
    if [ "$IP" != "$BUILD_IP" ]; then
        info "Copying binary from NYC to $NODE..."
        ssh $SSH_OPTS "root@${BUILD_IP}" "cat /root/arc-chain/target/release/arc-node" \
            | ssh $SSH_OPTS "root@${IP}" "cat > /tmp/arc-node-new && chmod +x /tmp/arc-node-new"
        ok "Binary copied"
    else
        ssh $SSH_OPTS "root@${IP}" "cp /root/arc-chain/target/release/arc-node /tmp/arc-node-new && chmod +x /tmp/arc-node-new"
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

    # e. Start new node in screen. Load model on LAX for inference demo.
    MODEL_FLAG=""
    if [ "$NODE" = "LAX" ]; then
        MODEL_FLAG="--model model.gguf"
    fi
    info "Starting new node${MODEL_FLAG:+ with inference}..."
    ssh $SSH_OPTS "root@${IP}" "cd /root/arc-chain && screen -dmS arc ./target/release/arc-node \
        --rpc 0.0.0.0:${RPC_PORT} \
        --p2p-port ${P2P_PORT} \
        --validator-seed ${VALIDATOR_SEED} \
        --seeds-file testnet-seeds.txt \
        --genesis genesis.toml \
        --stake 5000000 \
        --eth-rpc-port 0 \
        ${MODEL_FLAG}"

    # f. Wait for health (up to 60 seconds)
    info "Waiting for health check..."
    HEALTHY=false
    for i in $(seq 1 12); do
        sleep 5
        HEALTH=$(ssh $SSH_OPTS "root@${IP}" "curl -sf http://localhost:${RPC_PORT}/health 2>/dev/null" || echo "")
        if [ -n "$HEALTH" ]; then
            PEERS=$(echo "$HEALTH" | grep -o '"peers":[0-9]*' | grep -o '[0-9]*' || echo "0")
            ROUND=$(echo "$HEALTH" | grep -o '"dag_round":[0-9]*' | grep -o '[0-9]*' || echo "?")
            ok "$NODE healthy — peers: $PEERS, round: $ROUND"
            HEALTHY=true
            break
        fi
        printf "  ... attempt $i/12\n"
    done

    if [ "$HEALTHY" = false ]; then
        warn "$NODE failed health check after 60s — check manually: ssh root@${IP}"
        warn "Continuing anyway (node may need more time to connect peers)..."
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
