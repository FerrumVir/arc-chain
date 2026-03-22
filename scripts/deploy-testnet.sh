#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Deploy 8-Node Global Testnet on Vultr
#
# Prerequisites:
#   brew install vultr/vultr-cli/vultr-cli
#   export VULTR_API_KEY="your-key"
#
# Usage:
#   ./scripts/deploy-testnet.sh          # Deploy all 8 nodes
#   ./scripts/deploy-testnet.sh status   # Check all node health
#   ./scripts/deploy-testnet.sh ips      # Print all IPs
#   ./scripts/deploy-testnet.sh seeds    # Print seeds file for Mac Studio
#   ./scripts/deploy-testnet.sh ssh 0    # SSH into node 0 (NYC)
#   ./scripts/deploy-testnet.sh logs 0   # Tail logs on node 0
#   ./scripts/deploy-testnet.sh teardown # Destroy all nodes
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
STATE_DIR="$HOME/.arc-testnet"
IP_FILE="$STATE_DIR/node-ips.txt"
SEEDS_FILE="$STATE_DIR/seeds.txt"
SSH_KEY="$HOME/.ssh/id_ed25519"

# ── 8 Vultr regions across 6 continents ──────────────────────────────────────
# Vultr region slugs: https://api.vultr.com/v2/regions
REGIONS=(
    "ewr"    # New York / Newark     — North America
    "lax"    # Los Angeles           — North America
    "ams"    # Amsterdam             — Europe
    "lhr"    # London                — Europe
    "nrt"    # Tokyo                 — Asia
    "sgp"    # Singapore             — Asia
    "sao"    # São Paulo             — South America
    "jnb"    # Johannesburg          — Africa
)
LABELS=(
    "arc-node-nyc"
    "arc-node-lax"
    "arc-node-ams"
    "arc-node-lhr"
    "arc-node-nrt"
    "arc-node-sgp"
    "arc-node-sao"
    "arc-node-jnb"
)

# vc2-1c-2gb = $14/mo (1 vCPU, 2GB, 55GB SSD)
# vc2-2c-4gb = $24/mo (2 vCPU, 4GB, 80GB SSD)
PLAN="vc2-2c-4gb"
OS_ID="2284"  # Ubuntu 24.04 LTS x64

# ── Colors ────────────────────────────────────────────────────────────────────
BOLD="\033[1m" GREEN="\033[32m" CYAN="\033[36m" YELLOW="\033[33m" RED="\033[31m" RESET="\033[0m"
info()  { printf "${CYAN}[INFO]${RESET}  %s\n" "$*"; }
ok()    { printf "${GREEN}[  OK]${RESET}  %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${RESET}  %s\n" "$*"; }
fail()  { printf "${RED}[FAIL]${RESET}  %s\n" "$*" >&2; exit 1; }

# ── Preflight checks ─────────────────────────────────────────────────────────
preflight() {
    command -v vultr-cli >/dev/null 2>&1 || fail "vultr-cli not found. Run: brew install vultr/vultr-cli/vultr-cli"
    [ -n "${VULTR_API_KEY:-}" ] || fail "VULTR_API_KEY not set. Run: export VULTR_API_KEY='your-key'"
    [ -f "$SSH_KEY.pub" ] || fail "SSH key not found at $SSH_KEY.pub. Run: ssh-keygen -t ed25519"
    mkdir -p "$STATE_DIR"
}

# ── Upload SSH key to Vultr (idempotent) ──────────────────────────────────────
ensure_ssh_key() {
    local pubkey
    pubkey="$(cat "$SSH_KEY.pub")"
    local existing
    existing="$(vultr-cli ssh-key list 2>/dev/null | grep "arc-deploy" | awk '{print $1}' || true)"

    if [ -n "$existing" ]; then
        info "SSH key 'arc-deploy' already registered: $existing"
        SSH_KEY_ID="$existing"
    else
        info "Uploading SSH key to Vultr..."
        SSH_KEY_ID="$(vultr-cli ssh-key create --name "arc-deploy" --key "$pubkey" 2>/dev/null | grep "^ID" | awk '{print $2}')"
        ok "SSH key uploaded: $SSH_KEY_ID"
    fi
}

# ── Create a single node ─────────────────────────────────────────────────────
create_node() {
    local idx="$1"
    local region="${REGIONS[$idx]}"
    local label="${LABELS[$idx]}"

    info "Creating $label in $region ($PLAN)..."
    local result
    result="$(vultr-cli instance create \
        --region "$region" \
        --plan "$PLAN" \
        --os "$OS_ID" \
        --label "$label" \
        --host "$label" \
        --ssh-keys "$SSH_KEY_ID" \
        --tag "arc-testnet" \
        2>/dev/null)"

    local instance_id
    instance_id="$(echo "$result" | grep "^ID" | awk '{print $2}')"
    echo "$instance_id" > "$STATE_DIR/node-${idx}-id.txt"
    ok "$label created: $instance_id"
}

# ── Wait for all nodes to get IPs ─────────────────────────────────────────────
wait_for_ips() {
    info "Waiting for all nodes to get public IPs (this takes 1-2 minutes)..."
    > "$IP_FILE"

    for idx in "${!REGIONS[@]}"; do
        local instance_id
        instance_id="$(cat "$STATE_DIR/node-${idx}-id.txt")"
        local ip=""
        local attempts=0

        while [ -z "$ip" ] || [ "$ip" = "0.0.0.0" ] || [ "$ip" = "0" ]; do
            sleep 5
            ip="$(vultr-cli instance get "$instance_id" 2>/dev/null | grep "^MAIN IP" | awk '{print $3}' || true)"
            attempts=$((attempts + 1))
            if [ $attempts -gt 30 ]; then
                fail "Timed out waiting for IP on ${LABELS[$idx]}"
            fi
        done

        echo "${LABELS[$idx]} $ip" >> "$IP_FILE"
        ok "${LABELS[$idx]}: $ip"
    done
}

# ── Generate seeds file ──────────────────────────────────────────────────────
generate_seeds() {
    info "Generating seeds file..."
    > "$SEEDS_FILE"
    echo "# ARC Chain Testnet — Auto-generated $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$SEEDS_FILE"
    echo "# 8 nodes across 6 continents" >> "$SEEDS_FILE"

    while IFS=' ' read -r label ip; do
        echo "$ip:9091  # $label" >> "$SEEDS_FILE"
    done < "$IP_FILE"

    ok "Seeds file: $SEEDS_FILE"
    cat "$SEEDS_FILE"
}

# ── Deploy arc-node to a single VPS ──────────────────────────────────────────
deploy_node() {
    local idx="$1"
    local label ip
    label="$(sed -n "$((idx+1))p" "$IP_FILE" | awk '{print $1}')"
    ip="$(sed -n "$((idx+1))p" "$IP_FILE" | awk '{print $2}')"

    info "Deploying to $label ($ip)..."

    # Wait for SSH to be ready
    local attempts=0
    while ! ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 -i "$SSH_KEY" "root@$ip" "echo ready" >/dev/null 2>&1; do
        sleep 5
        attempts=$((attempts + 1))
        if [ $attempts -gt 24 ]; then
            fail "SSH not ready on $label ($ip) after 2 minutes"
        fi
    done

    # Copy seeds file
    scp -o StrictHostKeyChecking=no -i "$SSH_KEY" "$SEEDS_FILE" "root@$ip:/tmp/seeds.txt"

    # Run install + configure
    ssh -o StrictHostKeyChecking=no -i "$SSH_KEY" "root@$ip" bash <<REMOTE
set -euo pipefail

# Install node (clones repo, builds, creates systemd service)
curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-node.sh | bash

# Stop default service, reconfigure with seeds + unique validator seed
systemctl stop arc-node || true

# Copy seeds file
cp /tmp/seeds.txt /root/.arc-chain/seeds.txt

# Update config with seeds file path
cat > /root/.arc-chain/config.toml <<'TOML'
[rpc]
listen = "0.0.0.0:9090"
eth_port = 8545

[p2p]
port = 9091
peers = []

[validator]
seed = "${label}"
stake = 5000000
min_stake = 500000

[storage]
data_dir = "/root/.arc-chain/data"
TOML

# Open firewall ports
ufw allow 9090/tcp >/dev/null 2>&1 || true   # RPC
ufw allow 9091/udp >/dev/null 2>&1 || true   # P2P QUIC
ufw allow 8545/tcp >/dev/null 2>&1 || true   # ETH RPC
ufw allow 22/tcp   >/dev/null 2>&1 || true   # SSH

# Update systemd to use seeds file + unique seed + archive mode
ARC_BIN="/root/.arc-chain/arc-chain/target/release/arc-node"
cat > /etc/systemd/system/arc-node.service <<EOF
[Unit]
Description=ARC Chain Node (${label})
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
ExecStart=\${ARC_BIN} \\
    --validator-seed "${label}" \\
    --seeds-file /root/.arc-chain/seeds.txt \\
    --archive \\
    --stake 5000000
Restart=always
RestartSec=5
LimitNOFILE=65536
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable arc-node
systemctl start arc-node

echo "NODE_DEPLOYED: ${label}"
REMOTE

    ok "$label deployed and running"
}

# ── Check health of all nodes ─────────────────────────────────────────────────
check_status() {
    [ -f "$IP_FILE" ] || fail "No nodes deployed. Run: $0"
    echo ""
    printf "${BOLD}%-20s %-16s %-8s %-10s %s${RESET}\n" "NODE" "IP" "STATUS" "HEIGHT" "PEERS"
    echo "─────────────────────────────────────────────────────────────────────"

    while IFS=' ' read -r label ip; do
        local health height peers status
        health="$(curl -s --connect-timeout 3 "http://$ip:9090/health" 2>/dev/null || echo "unreachable")"
        if echo "$health" | grep -q "ok\|healthy\|true" 2>/dev/null; then
            status="${GREEN}UP${RESET}"
            # Try to get block height and peer count
            local info_json
            info_json="$(curl -s --connect-timeout 3 "http://$ip:9090/info" 2>/dev/null || echo "{}")"
            height="$(echo "$info_json" | python3 -c "import sys,json; print(json.load(sys.stdin).get('block_height', '?'))" 2>/dev/null || echo "?")"
            peers="$(echo "$info_json" | python3 -c "import sys,json; print(json.load(sys.stdin).get('peer_count', '?'))" 2>/dev/null || echo "?")"
        else
            status="${RED}DOWN${RESET}"
            height="-"
            peers="-"
        fi
        printf "%-20s %-16s ${status}%-2s %-10s %s\n" "$label" "$ip" "" "$height" "$peers"
    done < "$IP_FILE"
    echo ""
}

# ── Print Mac Studio connection command ───────────────────────────────────────
mac_studio_cmd() {
    [ -f "$SEEDS_FILE" ] || fail "No seeds file. Deploy nodes first."
    echo ""
    echo "${BOLD}To connect your Mac Studio (your IP stays private):${RESET}"
    echo ""
    echo "  cd /tmp/arc-chain-gh"
    echo "  cargo build --release -p arc-node"
    echo "  ./target/release/arc-node \\"
    echo "      --validator-seed \"arc-mac-studio\" \\"
    echo "      --seeds-file $SEEDS_FILE \\"
    echo "      --archive"
    echo ""
    echo "Your Mac Studio connects OUTBOUND to all 8 Vultr nodes."
    echo "No port forwarding needed. Your home IP is never in the seeds file."
    echo ""
}

# ── Teardown all nodes ────────────────────────────────────────────────────────
teardown() {
    warn "This will DESTROY all 8 Vultr nodes."
    read -p "Are you sure? (yes/no): " confirm
    [ "$confirm" = "yes" ] || { info "Cancelled."; exit 0; }

    for idx in "${!REGIONS[@]}"; do
        local id_file="$STATE_DIR/node-${idx}-id.txt"
        if [ -f "$id_file" ]; then
            local instance_id
            instance_id="$(cat "$id_file")"
            info "Destroying ${LABELS[$idx]} ($instance_id)..."
            vultr-cli instance delete "$instance_id" 2>/dev/null || warn "Failed to delete $instance_id"
            rm -f "$id_file"
        fi
    done
    rm -f "$IP_FILE" "$SEEDS_FILE"
    ok "All nodes destroyed."
}

# ── Main ──────────────────────────────────────────────────────────────────────
case "${1:-deploy}" in
    deploy)
        preflight
        ensure_ssh_key

        info "Deploying 8 ARC Chain nodes across 6 continents..."
        info "Plan: $PLAN ($24/mo each = $192/mo total)"
        echo ""

        # Create all 8 instances
        for idx in "${!REGIONS[@]}"; do
            create_node "$idx"
        done

        # Wait for IPs
        wait_for_ips

        # Generate seeds file from IPs
        generate_seeds

        # Deploy to all 8 nodes (sequential — each takes ~5 min to build)
        for idx in "${!REGIONS[@]}"; do
            deploy_node "$idx"
        done

        echo ""
        ok "All 8 nodes deployed and peered!"
        echo ""
        check_status
        mac_studio_cmd
        ;;

    status)
        check_status
        ;;

    ips)
        [ -f "$IP_FILE" ] || fail "No nodes deployed."
        cat "$IP_FILE"
        ;;

    seeds)
        [ -f "$SEEDS_FILE" ] || fail "No seeds file."
        cat "$SEEDS_FILE"
        mac_studio_cmd
        ;;

    ssh)
        [ -n "${2:-}" ] || fail "Usage: $0 ssh <node-index 0-7>"
        idx="$2"
        ip="$(sed -n "$((idx+1))p" "$IP_FILE" | awk '{print $2}')"
        info "SSH into ${LABELS[$idx]} ($ip)..."
        exec ssh -i "$SSH_KEY" "root@$ip"
        ;;

    logs)
        [ -n "${2:-}" ] || fail "Usage: $0 logs <node-index 0-7>"
        idx="$2"
        ip="$(sed -n "$((idx+1))p" "$IP_FILE" | awk '{print $2}')"
        info "Tailing logs on ${LABELS[$idx]} ($ip)..."
        exec ssh -i "$SSH_KEY" "root@$ip" "journalctl -u arc-node -f"
        ;;

    teardown)
        teardown
        ;;

    *)
        echo "Usage: $0 {deploy|status|ips|seeds|ssh N|logs N|teardown}"
        exit 1
        ;;
esac
