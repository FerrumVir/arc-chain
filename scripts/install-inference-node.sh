#!/bin/bash
set -e

# ─── ARC Inference Node: Install as System Service ────────────────────────────
# Installs the inference node as a persistent systemd service.
# Survives reboots. Auto-restarts on crash.
#
# Usage: sudo ./scripts/install-inference-node.sh [--model PATH]
# ─────────────────────────────────────────────────────────────────────────────

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MODEL_PATH="${1:-$REPO_ROOT/model.gguf}"
SEED_FILE="$HOME/.arc-inference-seed"

# Generate persistent identity (survives restarts)
if [ ! -f "$SEED_FILE" ]; then
    SEED="arc-inference-$(openssl rand -hex 8)"
    echo "$SEED" > "$SEED_FILE"
    echo "Generated persistent identity: $SEED"
else
    SEED=$(cat "$SEED_FILE")
    echo "Using existing identity: $SEED"
fi

echo "Installing ARC inference node as system service..."

cat > /etc/systemd/system/arc-inference.service << EOF
[Unit]
Description=ARC Chain Inference Node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$USER
WorkingDirectory=$REPO_ROOT
ExecStart=$REPO_ROOT/target/release/arc-node \\
    --rpc 0.0.0.0:9090 \\
    --seeds-file $REPO_ROOT/testnet-seeds.txt \\
    --genesis $REPO_ROOT/genesis.toml \\
    --validator-seed "$SEED" \\
    --model "$MODEL_PATH" \\
    --stake 5000000
Restart=always
RestartSec=10
LimitNOFILE=65536
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable arc-inference
systemctl start arc-inference

echo ""
echo "Inference node installed and running!"
echo "  Identity: $SEED (saved to $SEED_FILE)"
echo "  Status:   sudo systemctl status arc-inference"
echo "  Logs:     sudo journalctl -u arc-inference -f"
echo "  Stop:     sudo systemctl stop arc-inference"
echo "  Restart:  sudo systemctl restart arc-inference"
