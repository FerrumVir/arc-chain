#!/bin/bash
# ─── ARC Chain: Rolling Deploy (zero-downtime) ─────────────────────────────
# Upgrades nodes ONE AT A TIME so consensus never drops below 6/8 quorum.
# Build ONCE on first node, distribute binary to all others.
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

SSH_KEY="$HOME/.ssh/id_ed25519"
SSH_OPTS="-o StrictHostKeyChecking=no -o ConnectTimeout=10"

NODES=(
    "149.28.32.76:NYC"
    "140.82.16.112:LAX"
    "136.244.109.1:AMS"
    "104.238.171.11:LHR"
    "202.182.107.41:NRT"
    "149.28.153.31:SGP"
    "216.238.120.27:SAO"
    "139.84.237.49:JNB"
)

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

BUILD_NODE="${NODES[0]%%:*}"  # Build on first node (NYC)
BUILD_NAME="${NODES[0]##*:}"

echo -e "${YELLOW}Rolling deploy — build once, distribute to all${NC}"
echo ""

# ── Step 1: Build on first node ──────────────────────────────────────────
echo -e "${YELLOW}[${BUILD_NAME}] Building on ${BUILD_NODE}...${NC}"
ssh $SSH_OPTS -i "$SSH_KEY" "root@${BUILD_NODE}" "
    export PATH=/root/.cargo/bin:\$PATH
    cd /root/arc-chain
    git fetch origin main && git reset --hard origin/main
    cargo build --release -p arc-node 2>&1 | tail -3
    ls -la target/release/arc-node
" 2>&1 | sed "s/^/  /"

BINARY_HASH=$(ssh $SSH_OPTS -i "$SSH_KEY" "root@${BUILD_NODE}" "sha256sum /root/arc-chain/target/release/arc-node | cut -d' ' -f1")
echo -e "  Binary hash: ${BINARY_HASH}"

# ── Step 2: Distribute binary to all other nodes ────────────────────────
echo ""
echo -e "${YELLOW}Distributing binary to all nodes...${NC}"
for entry in "${NODES[@]}"; do
    ip="${entry%%:*}"
    seed="${entry##*:}"
    if [ "$ip" = "$BUILD_NODE" ]; then
        continue  # Skip build node
    fi
    echo -n "  ${seed} (${ip})... "
    # Pull code (for genesis.toml, seeds, scripts) + copy binary via pipe
    ssh $SSH_OPTS -i "$SSH_KEY" "root@$ip" "
        cd /root/arc-chain
        git fetch origin main && git reset --hard origin/main
    " 2>/dev/null
    # Pipe binary through localhost to avoid remote-to-remote scp issues.
    # Write to .new first (avoids "text file busy" if old binary is running).
    ssh $SSH_OPTS -i "$SSH_KEY" "root@${BUILD_NODE}" "cat /root/arc-chain/target/release/arc-node" \
        | ssh $SSH_OPTS -i "$SSH_KEY" "root@${ip}" "cat > /root/arc-chain/target/release/arc-node.new && chmod +x /root/arc-chain/target/release/arc-node.new && mv -f /root/arc-chain/target/release/arc-node.new /root/arc-chain/target/release/arc-node" 2>/dev/null
    echo -e "${GREEN}OK${NC}"
done

# ── Step 3: Rolling restart — one at a time ─────────────────────────────
echo ""
echo -e "${YELLOW}Rolling restart — one node at a time${NC}"
echo ""

for entry in "${NODES[@]}"; do
    ip="${entry%%:*}"
    seed="${entry##*:}"

    echo -e "${YELLOW}[$seed] Restarting $ip...${NC}"

    ssh $SSH_OPTS -i "$SSH_KEY" "root@$ip" "
        cd /root/arc-chain
        killall -9 arc-node 2>/dev/null
        sleep 2
        nohup target/release/arc-node \
            --rpc 0.0.0.0:9090 --validator-seed $seed \
            --seeds-file /root/.arc-chain/seeds.txt \
            --genesis genesis.toml --stake 5000000 --eth-rpc-port 0 \
            </dev/null >/tmp/arc-node.log 2>&1 &
        sleep 3
        echo 'PID: '\$(pgrep -f 'arc-node.*validator' | head -1)
    " 2>&1 | sed "s/^/  /"

    # Wait for node to rejoin consensus
    echo -n "  Waiting for peers..."
    for i in $(seq 1 12); do
        sleep 5
        peers=$(curl -sf --max-time 3 "http://$ip:9090/health" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('peer_count',0))" 2>/dev/null || echo 0)
        if [ "$peers" -ge 3 ] 2>/dev/null; then
            echo -e " ${GREEN}$peers peers — OK${NC}"
            break
        fi
        echo -n "."
    done

    echo ""
done

echo -e "${GREEN}All 8 nodes upgraded. Binary hash: ${BINARY_HASH}${NC}"
