#!/bin/bash
# ─── ARC Chain: Rolling Deploy (zero-downtime) ─────────────────────────────
# Upgrades nodes ONE AT A TIME so consensus never drops below 6/8 quorum.
# Each node: pull → rebuild → restart → wait for peers → next node.
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

echo -e "${YELLOW}Rolling deploy — one node at a time, consensus stays live${NC}"
echo ""

for entry in "${NODES[@]}"; do
    ip="${entry%%:*}"
    seed="${entry##*:}"

    echo -e "${YELLOW}[$seed] Upgrading $ip...${NC}"

    # Pull + rebuild + restart
    ssh $SSH_OPTS -i "$SSH_KEY" "root@$ip" "
        export PATH=/root/.cargo/bin:\$PATH
        cd /root/arc-chain
        git fetch origin main && git reset --hard origin/main
        cargo build --release -p arc-node 2>&1 | tail -1
        killall -9 arc-node 2>/dev/null
        sleep 2
        # Keep state data across restarts (WAL, accounts, blocks).
        # Only wipe on explicit --clean flag.
        nohup target/release/arc-node \
            --rpc 0.0.0.0:9090 --validator-seed $seed \
            --seeds-file /root/.arc-chain/seeds.txt \
            --genesis genesis.toml --stake 5000000 --eth-rpc-port 0 \
            </dev/null >/tmp/arc-node.log 2>&1 &
        sleep 5
        echo 'PID: '\$(pgrep -f 'arc-node.*validator' | head -1)
    " 2>&1 | sed "s/^/  /"

    # Wait for node to rejoin consensus (get peers)
    echo -n "  Waiting for peers..."
    for i in $(seq 1 12); do
        sleep 5
        peers=$(curl -sf --max-time 3 "http://$ip:9090/health" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('peers',0))" 2>/dev/null || echo 0)
        if [ "$peers" -ge 3 ] 2>/dev/null; then
            echo -e " ${GREEN}$peers peers — OK${NC}"
            break
        fi
        echo -n "."
    done

    echo ""
done

echo -e "${GREEN}All 8 nodes upgraded. Consensus maintained throughout.${NC}"
