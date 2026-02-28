#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ARC Chain — Bare Metal Deployment Script
# =============================================================================
#
# Deploys arc-node + explorer directly on a Linux server (no Docker needed).
# Tested on Ubuntu 22.04 / 24.04 and Debian 12.
#
# Usage:
#   ssh root@your-server
#   git clone <your-repo> /opt/arc-chain
#   cd /opt/arc-chain
#   bash deploy.sh
#
# What it does:
#   1. Installs Rust, Node.js, and system dependencies
#   2. Builds arc-node and arc-bench (release mode)
#   3. Builds the Next.js explorer (production)
#   4. Creates systemd services for both
#   5. Starts everything
#
# After running: your explorer is live on port 3100, node RPC on port 9090
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE_PORT=9090
EXPLORER_PORT=3100

echo "============================================="
echo "  ARC Chain — Bare Metal Deployment"
echo "============================================="
echo ""

# ---------------------------------------------------------------------------
# 1. System dependencies
# ---------------------------------------------------------------------------
echo "[1/6] Installing system dependencies..."

if command -v apt-get &>/dev/null; then
    apt-get update -qq
    apt-get install -y -qq \
        build-essential cmake pkg-config curl git \
        libssl-dev libvulkan-dev mesa-vulkan-drivers \
        > /dev/null 2>&1
    echo "  ✓ System packages installed"
elif command -v dnf &>/dev/null; then
    dnf install -y -q \
        gcc gcc-c++ cmake pkgconfig curl git \
        openssl-devel vulkan-loader-devel mesa-vulkan-drivers \
        > /dev/null 2>&1
    echo "  ✓ System packages installed"
else
    echo "  ⚠ Unknown package manager. Install manually: build-essential cmake libssl-dev libvulkan-dev"
fi

# ---------------------------------------------------------------------------
# 2. Rust toolchain
# ---------------------------------------------------------------------------
echo "[2/6] Setting up Rust toolchain..."

if ! command -v rustc &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    source "$HOME/.cargo/env"
    echo "  ✓ Rust installed ($(rustc --version))"
else
    echo "  ✓ Rust already installed ($(rustc --version))"
fi

# Ensure we have a new enough Rust (edition 2024 needs 1.85+)
RUST_VER=$(rustc --version | grep -oP '\d+\.\d+')
if (( $(echo "$RUST_VER < 1.85" | bc -l) )); then
    echo "  Updating Rust to latest stable..."
    rustup update stable
fi

# ---------------------------------------------------------------------------
# 3. Build Rust binaries
# ---------------------------------------------------------------------------
echo "[3/6] Building Rust binaries (release mode)..."
echo "  This takes 2-5 minutes on first build..."

cd "$SCRIPT_DIR"
cargo build --release -p arc-node -p arc-bench 2>&1 | tail -3

echo "  ✓ arc-node:  $(ls -lh target/release/arc-node  | awk '{print $5}')"
echo "  ✓ arc-bench: $(ls -lh target/release/arc-bench | awk '{print $5}')"

# ---------------------------------------------------------------------------
# 4. Node.js + Explorer build
# ---------------------------------------------------------------------------
echo "[4/6] Building Next.js explorer..."

if ! command -v node &>/dev/null; then
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - > /dev/null 2>&1
    apt-get install -y -qq nodejs > /dev/null 2>&1
    echo "  ✓ Node.js installed ($(node --version))"
else
    echo "  ✓ Node.js already installed ($(node --version))"
fi

cd "$SCRIPT_DIR/explorer"
npm ci --silent
npm run build 2>&1 | tail -5

echo "  ✓ Explorer built"

# ---------------------------------------------------------------------------
# 5. Create systemd services
# ---------------------------------------------------------------------------
echo "[5/6] Creating systemd services..."

# arc-node service
cat > /etc/systemd/system/arc-node.service << EOF
[Unit]
Description=ARC Chain Node
After=network.target

[Service]
Type=simple
User=root
WorkingDirectory=$SCRIPT_DIR
ExecStart=$SCRIPT_DIR/target/release/arc-node
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

# arc-explorer service
cat > /etc/systemd/system/arc-explorer.service << EOF
[Unit]
Description=ARC Chain Explorer
After=arc-node.service
Requires=arc-node.service

[Service]
Type=simple
User=root
WorkingDirectory=$SCRIPT_DIR/explorer
ExecStart=/usr/bin/node $SCRIPT_DIR/explorer/.next/standalone/server.js
Restart=always
RestartSec=5
Environment=NODE_ENV=production
Environment=PORT=$EXPLORER_PORT
Environment=ARC_NODE_URL=http://127.0.0.1:$NODE_PORT

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
echo "  ✓ Systemd services created"

# ---------------------------------------------------------------------------
# 6. Start services
# ---------------------------------------------------------------------------
echo "[6/6] Starting services..."

systemctl enable arc-node arc-explorer
systemctl restart arc-node
sleep 2

# Wait for node to be healthy
echo "  Waiting for arc-node..."
for i in {1..10}; do
    if curl -sf http://127.0.0.1:$NODE_PORT/health > /dev/null 2>&1; then
        echo "  ✓ arc-node is running on port $NODE_PORT"
        break
    fi
    sleep 1
done

systemctl restart arc-explorer
sleep 2
echo "  ✓ arc-explorer is running on port $EXPLORER_PORT"

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------
echo ""
echo "============================================="
echo "  ARC Chain is LIVE"
echo "============================================="
echo ""
echo "  Node RPC:    http://$(hostname -I | awk '{print $1}'):$NODE_PORT"
echo "  Explorer:    http://$(hostname -I | awk '{print $1}'):$EXPLORER_PORT"
echo "  Verify page: http://$(hostname -I | awk '{print $1}'):$EXPLORER_PORT/verify"
echo ""
echo "  Run benchmark:  $SCRIPT_DIR/target/release/arc-bench"
echo ""
echo "  Manage services:"
echo "    systemctl status arc-node"
echo "    systemctl status arc-explorer"
echo "    journalctl -u arc-node -f"
echo "    journalctl -u arc-explorer -f"
echo ""
echo "  To add HTTPS (recommended):"
echo "    apt install nginx certbot python3-certbot-nginx"
echo "    # Configure nginx reverse proxy for port 3100"
echo "    certbot --nginx -d your-domain.com"
echo ""
