#!/usr/bin/env bash
# =========================================================================
# ARC Chain — 4-Node Testnet Provisioning Script
# =========================================================================
# Provisions 4 Hetzner Cloud CAX41 (ARM64) servers with cloud-init,
# deploys node configs with correct peer addresses, and starts the network.
#
# Prerequisites:
#   - hcloud CLI installed and authenticated (hcloud context active)
#   - SSH key registered in Hetzner Cloud (name: "default" or set SSH_KEY_NAME)
#   - This script run from the deploy/ directory
#
# Usage:
#   ./setup-testnet.sh
#   SSH_KEY_NAME=my-key DATACENTER=fsn1-dc14 ./setup-testnet.sh
# =========================================================================

set -euo pipefail

# -- Configuration --------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE_COUNT=4
SERVER_TYPE="${SERVER_TYPE:-cax41}"          # ARM64, 16 vCPU, 32 GB RAM
DATACENTER="${DATACENTER:-ash-dc1}"         # Ashburn, Virginia
IMAGE="${IMAGE:-ubuntu-24.04}"
SSH_KEY_NAME="${SSH_KEY_NAME:-default}"
CLOUD_INIT="${SCRIPT_DIR}/cloud-init.yml"
CONFIG_DIR="${SCRIPT_DIR}/config"
LABEL="project=arc-testnet"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# -- Preflight Checks ----------------------------------------------------

echo -e "${BOLD}ARC Chain Testnet — Provisioning 4 Nodes${NC}"
echo "============================================"
echo ""

if ! command -v hcloud &>/dev/null; then
    echo -e "${RED}ERROR: hcloud CLI not found. Install it:${NC}"
    echo "  brew install hcloud  (macOS)"
    echo "  https://github.com/hetznercloud/cli/releases"
    exit 1
fi

if ! hcloud server-type describe "$SERVER_TYPE" &>/dev/null; then
    echo -e "${RED}ERROR: Server type '$SERVER_TYPE' not available.${NC}"
    echo "Run: hcloud server-type list"
    exit 1
fi

if [ ! -f "$CLOUD_INIT" ]; then
    echo -e "${RED}ERROR: cloud-init.yml not found at $CLOUD_INIT${NC}"
    exit 1
fi

echo -e "${CYAN}Server type:${NC}  $SERVER_TYPE"
echo -e "${CYAN}Datacenter:${NC}   $DATACENTER"
echo -e "${CYAN}Image:${NC}        $IMAGE"
echo -e "${CYAN}SSH key:${NC}      $SSH_KEY_NAME"
echo ""

# -- Step 1: Create Servers -----------------------------------------------

declare -a SERVER_IPS
declare -a SERVER_IDS

for i in $(seq 0 $((NODE_COUNT - 1))); do
    NAME="arc-node-${i}"

    # Check if server already exists
    if hcloud server describe "$NAME" &>/dev/null; then
        echo -e "${YELLOW}Server $NAME already exists, fetching IP...${NC}"
        IP=$(hcloud server ip "$NAME")
        ID=$(hcloud server describe "$NAME" -o json | jq -r '.id')
    else
        echo -e "${CYAN}Creating server ${BOLD}$NAME${NC}${CYAN}...${NC}"
        RESULT=$(hcloud server create \
            --name "$NAME" \
            --type "$SERVER_TYPE" \
            --datacenter "$DATACENTER" \
            --image "$IMAGE" \
            --ssh-key "$SSH_KEY_NAME" \
            --user-data-from-file "$CLOUD_INIT" \
            --label "$LABEL" \
            -o json)

        IP=$(echo "$RESULT" | jq -r '.server.public_net.ipv4.ip')
        ID=$(echo "$RESULT" | jq -r '.server.id')
        echo -e "${GREEN}  Created: $NAME ($IP)${NC}"
    fi

    SERVER_IPS[$i]="$IP"
    SERVER_IDS[$i]="$ID"
done

echo ""
echo -e "${GREEN}All servers created.${NC}"
echo ""

# -- Step 2: Wait for Cloud-Init ------------------------------------------

echo -e "${CYAN}Waiting for cloud-init to complete on all nodes...${NC}"
echo "(This typically takes 60-90 seconds)"
echo ""

for i in $(seq 0 $((NODE_COUNT - 1))); do
    IP="${SERVER_IPS[$i]}"
    NAME="arc-node-${i}"
    echo -n "  Waiting for $NAME ($IP)..."

    # Wait for SSH to be reachable (max 120s)
    RETRIES=0
    until ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
          root@"$IP" "test -f /var/lib/cloud/instance/boot-finished" 2>/dev/null; do
        RETRIES=$((RETRIES + 1))
        if [ "$RETRIES" -ge 24 ]; then
            echo -e " ${RED}TIMEOUT${NC}"
            echo -e "${RED}Cloud-init did not complete on $NAME within 120s.${NC}"
            echo "SSH in manually: ssh root@$IP"
            exit 1
        fi
        sleep 5
        echo -n "."
    done
    echo -e " ${GREEN}ready${NC}"
done

echo ""

# -- Step 3: Deploy Configs with Real IPs ----------------------------------

echo -e "${CYAN}Deploying node configs with peer addresses...${NC}"
echo ""

for i in $(seq 0 $((NODE_COUNT - 1))); do
    IP="${SERVER_IPS[$i]}"
    NAME="arc-node-${i}"
    CONFIG_SRC="${CONFIG_DIR}/node-${i}.toml"

    echo -e "  Deploying config to ${BOLD}$NAME${NC} ($IP)..."

    # Generate config with real peer IPs substituted
    CONFIG_CONTENT=$(cat "$CONFIG_SRC")
    for j in $(seq 0 $((NODE_COUNT - 1))); do
        CONFIG_CONTENT=$(echo "$CONFIG_CONTENT" | sed "s/__NODE_${j}_IP__/${SERVER_IPS[$j]}/g")
    done

    # Upload config and genesis
    echo "$CONFIG_CONTENT" | ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        root@"$IP" "cat > /etc/arc/config.toml"

    scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -q \
        "${CONFIG_DIR}/genesis.toml" root@"$IP":/etc/arc/genesis.toml

    # Fix ownership and start service
    ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@"$IP" \
        "chown arc:arc /etc/arc/config.toml /etc/arc/genesis.toml && \
         systemctl daemon-reload && \
         systemctl restart arc-node"

    echo -e "  ${GREEN}$NAME started${NC}"
done

echo ""

# -- Step 4: Verify --------------------------------------------------------

echo -e "${CYAN}Waiting 10 seconds for nodes to initialize...${NC}"
sleep 10

echo ""
echo -e "${BOLD}Verifying node health...${NC}"
echo ""

ALL_HEALTHY=true
for i in $(seq 0 $((NODE_COUNT - 1))); do
    IP="${SERVER_IPS[$i]}"
    NAME="arc-node-${i}"
    echo -n "  $NAME ($IP): "

    if curl -sf --connect-timeout 5 "http://${IP}:9090/health" &>/dev/null; then
        echo -e "${GREEN}HEALTHY${NC}"
    else
        echo -e "${RED}UNREACHABLE${NC}"
        ALL_HEALTHY=false
    fi
done

echo ""

# -- Summary ---------------------------------------------------------------

echo "============================================"
echo -e "${BOLD}ARC Chain Testnet — Deployment Summary${NC}"
echo "============================================"
echo ""

for i in $(seq 0 $((NODE_COUNT - 1))); do
    IP="${SERVER_IPS[$i]}"
    echo -e "  ${BOLD}arc-node-${i}${NC}"
    echo "    IP:   $IP"
    echo "    RPC:  http://${IP}:9090"
    echo "    P2P:  ${IP}:9091"
    echo "    SSH:  ssh root@${IP}"
    echo ""
done

if [ "$ALL_HEALTHY" = true ]; then
    echo -e "${GREEN}All 4 nodes are healthy and producing blocks.${NC}"
else
    echo -e "${YELLOW}Some nodes are not yet healthy. Check logs with:${NC}"
    echo "  ssh root@<IP> journalctl -u arc-node -f"
fi

echo ""
echo -e "${CYAN}Quick commands:${NC}"
echo "  Check health:   curl http://${SERVER_IPS[0]}:9090/health"
echo "  View logs:       ssh root@${SERVER_IPS[0]} journalctl -u arc-node -f"
echo "  Monitor all:     ./monitor.sh"
echo "  Tear down:       ./teardown.sh"

# -- Save IPs to file for other scripts -----------------------------------

IP_FILE="${SCRIPT_DIR}/.node-ips"
: > "$IP_FILE"
for i in $(seq 0 $((NODE_COUNT - 1))); do
    echo "${SERVER_IPS[$i]}" >> "$IP_FILE"
done
echo ""
echo -e "${CYAN}Node IPs saved to ${IP_FILE}${NC}"
