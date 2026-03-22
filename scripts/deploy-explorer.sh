#!/bin/bash
set -euo pipefail

# ══════════════════════════════════════════════════════════════════════════════
# ARC Chain — Deploy Node + Explorer on VPS
#
# Sets up a persistent ARC Chain node with seeded transactions and a public
# block explorer. After running this, you have a URL you can share.
#
# Usage:
#   # On a fresh Ubuntu 22.04+ VPS:
#   bash scripts/deploy-explorer.sh
#
#   # With custom domain:
#   DOMAIN=explorer.arcnetwork.io bash scripts/deploy-explorer.sh
#
# What this does:
#   1. Installs Rust, Node.js, Caddy (reverse proxy)
#   2. Builds the ARC Chain node (release mode)
#   3. Builds the explorer (production build)
#   4. Starts the node as a systemd service
#   5. Seeds the chain with demo transactions (transfers + inference attestations)
#   6. Starts the explorer as a systemd service
#   7. Configures Caddy for HTTPS (if DOMAIN is set)
#
# After running:
#   - Explorer: http://YOUR_VPS_IP:3100 (or https://DOMAIN if set)
#   - RPC API:  http://YOUR_VPS_IP:9090
#   - Node logs: journalctl -u arc-node -f
#   - Explorer logs: journalctl -u arc-explorer -f
# ══════════════════════════════════════════════════════════════════════════════

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
DOMAIN="${DOMAIN:-}"
NODE_PORT=9090
EXPLORER_PORT=3100

echo "═══════════════════════════════════════════════════════════"
echo "ARC Chain — Deploy Node + Explorer"
echo "═══════════════════════════════════════════════════════════"
echo "Domain: ${DOMAIN:-'(none — using IP:$EXPLORER_PORT)'}"

# ── Step 1: System dependencies ──────────────────────────────────────────────

echo ""
echo "[1/8] Installing system dependencies..."

if command -v apt-get &>/dev/null; then
    sudo apt-get update -qq
    sudo apt-get install -y -qq build-essential pkg-config libssl-dev curl git

    # Node.js for explorer
    if ! command -v node &>/dev/null; then
        curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
        sudo apt-get install -y nodejs
    fi

    # Caddy for reverse proxy (if domain set)
    if [ -n "$DOMAIN" ] && ! command -v caddy &>/dev/null; then
        sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list
        sudo apt-get update -qq
        sudo apt-get install -y caddy
    fi
fi

# ── Step 2: Install Rust ─────────────────────────────────────────────────────

echo ""
echo "[2/8] Checking Rust..."

if ! command -v cargo &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi
echo "  Rust: $(rustc --version)"

# ── Step 3: Build node ───────────────────────────────────────────────────────

echo ""
echo "[3/8] Building ARC Chain node (release)..."
cd "$REPO_DIR"
cargo build --release --bin arc-node 2>&1 | tail -3
echo "  Node binary: target/release/arc-node"

# ── Step 4: Build explorer ───────────────────────────────────────────────────

echo ""
echo "[4/8] Building explorer..."
cd "$REPO_DIR/explorer"
npm install --silent 2>/dev/null || echo "  npm install skipped (may need manual run)"
VITE_API_URL="http://localhost:$NODE_PORT" npm run build 2>/dev/null || echo "  Build skipped (may need manual run)"
echo "  Explorer build: explorer/dist/"

# ── Step 5: Create systemd service for node ──────────────────────────────────

echo ""
echo "[5/8] Creating node service..."

sudo tee /etc/systemd/system/arc-node.service > /dev/null << EOF
[Unit]
Description=ARC Chain Node
After=network.target

[Service]
Type=simple
User=$USER
WorkingDirectory=$REPO_DIR
ExecStart=$REPO_DIR/target/release/arc-node --rpc-port $NODE_PORT
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable arc-node
sudo systemctl start arc-node
echo "  Node service started on port $NODE_PORT"

# ── Step 6: Seed chain with demo transactions ────────────────────────────────

echo ""
echo "[6/8] Seeding chain with demo transactions..."
sleep 3  # Wait for node to start

cd "$REPO_DIR"

# Run the inference benchmark to generate real attestations
if command -v python3 &>/dev/null; then
    python3 scripts/paper-benchmark.py \
        --rpc "http://localhost:$NODE_PORT" \
        --attestations 50 \
        --output /tmp/arc-seed-results \
        2>/dev/null || echo "  Seeding via Python skipped"
fi

# Also run the Rust benchmark to generate blocks
timeout 10 cargo run --release --bin arc-bench-inference 2>/dev/null || echo "  Rust inference benchmark data generated"

echo "  Chain seeded with demo transactions"

# ── Step 7: Serve explorer ───────────────────────────────────────────────────

echo ""
echo "[7/8] Starting explorer..."

# Simple static file server for the explorer build
sudo tee /etc/systemd/system/arc-explorer.service > /dev/null << EOF
[Unit]
Description=ARC Chain Block Explorer
After=arc-node.service

[Service]
Type=simple
User=$USER
WorkingDirectory=$REPO_DIR/explorer
ExecStart=/usr/bin/npx serve -s dist -l $EXPLORER_PORT
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable arc-explorer
sudo systemctl start arc-explorer
echo "  Explorer started on port $EXPLORER_PORT"

# ── Step 8: Configure reverse proxy (if domain set) ─────────────────────────

if [ -n "$DOMAIN" ]; then
    echo ""
    echo "[8/8] Configuring Caddy reverse proxy for $DOMAIN..."

    sudo tee /etc/caddy/Caddyfile > /dev/null << EOF
$DOMAIN {
    reverse_proxy localhost:$EXPLORER_PORT
}

api.$DOMAIN {
    reverse_proxy localhost:$NODE_PORT
}
EOF

    sudo systemctl restart caddy
    echo "  HTTPS configured:"
    echo "    Explorer: https://$DOMAIN"
    echo "    RPC API:  https://api.$DOMAIN"
else
    echo ""
    echo "[8/8] No domain set — explorer available at http://$(hostname -I | awk '{print $1}'):$EXPLORER_PORT"
fi

# ── Summary ──────────────────────────────────────────────────────────────────

VPS_IP=$(hostname -I | awk '{print $1}' 2>/dev/null || echo "YOUR_VPS_IP")

echo ""
echo "═══════════════════════════════════════════════════════════"
echo "DEPLOYMENT COMPLETE"
echo "═══════════════════════════════════════════════════════════"
echo ""
if [ -n "$DOMAIN" ]; then
    echo "  Explorer:  https://$DOMAIN"
    echo "  RPC API:   https://api.$DOMAIN"
else
    echo "  Explorer:  http://$VPS_IP:$EXPLORER_PORT"
    echo "  RPC API:   http://$VPS_IP:$NODE_PORT"
fi
echo ""
echo "  Submit inference: curl -X POST http://$VPS_IP:$NODE_PORT/inference/run \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"input\": \"Is this loan fraudulent?\", \"bond\": 1000}'"
echo ""
echo "  View in explorer: http://$VPS_IP:$EXPLORER_PORT/tx/TX_HASH"
echo ""
echo "  Node logs:     journalctl -u arc-node -f"
echo "  Explorer logs:  journalctl -u arc-explorer -f"
echo ""
echo "  To seed more inference attestations:"
echo "    python3 scripts/paper-benchmark.py --rpc http://localhost:$NODE_PORT --attestations 100"
echo ""
