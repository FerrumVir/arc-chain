#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Community Node Installer (one command, persistent, auto-updating)
#
# Installs an ARC inference node that:
#   1. Downloads the latest pre-built binary for your OS/arch
#   2. Downloads Llama-2-7B-Chat Q4_K_M (the network's reference model)
#   3. Pulls testnet seeds + genesis from main branch
#   4. Generates a unique validator seed for your machine
#   5. Installs as a persistent service:
#        macOS: ~/Library/LaunchAgents/com.arc.inference.plist (launchd)
#        Linux: /etc/systemd/system/arc-node.service (systemd)
#   6. Installs a daily auto-updater that checks GitHub for new releases
#   7. Joins the testnet as an observer (stake 0) — contributes inference compute
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh | bash
#
#   # Or with options:
#   bash install-community-node.sh --model ~/path/to/model.gguf
#   bash install-community-node.sh --no-service     # don't install systemd/launchd
#   bash install-community-node.sh --no-auto-update # don't install updater
#   bash install-community-node.sh --port 9944      # use a different RPC port
#
# After install, your node will be running and visible at the live dashboard:
#   http://140.82.16.112:3200
#
# Stop the service:
#   macOS: launchctl unload ~/Library/LaunchAgents/com.arc.inference.plist
#   Linux: sudo systemctl stop arc-node
#
# Uninstall:
#   bash install-community-node.sh --uninstall
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="FerrumVir/arc-chain"
ARC_DIR="${ARC_DIR:-${HOME}/.arc}"
RPC_PORT="${ARC_RPC_PORT:-9944}"
P2P_PORT="${ARC_P2P_PORT:-9945}"
INSTALL_SERVICE=true
INSTALL_UPDATER=true
USER_MODEL=""
DO_UNINSTALL=false

# Default model: Llama-2-7B-Chat Q4_K_M (~4 GB) — fits 8 GB RAM, coherent output
DEFAULT_MODEL_URL="https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf"
DEFAULT_MODEL_FILE="llama-2-7b-chat.Q4_K_M.gguf"
DEFAULT_MODEL_SIZE_GB=4

# ── Parse args ──────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
    case "$1" in
        --model)          USER_MODEL="$2"; shift 2 ;;
        --no-service)     INSTALL_SERVICE=false; shift ;;
        --no-auto-update) INSTALL_UPDATER=false; shift ;;
        --port)           RPC_PORT="$2"; P2P_PORT=$(( $2 + 1 )); shift 2 ;;
        --uninstall)      DO_UNINSTALL=true; shift ;;
        --help|-h)        sed -n '3,40p' "$0"; exit 0 ;;
        *)                echo "Unknown flag: $1" >&2; exit 1 ;;
    esac
done

# Colors
BOLD=$'\033[1m' DIM=$'\033[2m' GREEN=$'\033[32m' CYAN=$'\033[36m'
YELLOW=$'\033[33m' RED=$'\033[31m' RESET=$'\033[0m'
info() { printf "%s[INFO]%s %s\n" "$CYAN" "$RESET" "$*"; }
ok()   { printf "%s[  OK]%s %s\n" "$GREEN" "$RESET" "$*"; }
warn() { printf "%s[WARN]%s %s\n" "$YELLOW" "$RESET" "$*"; }
fail() { printf "%s[FAIL]%s %s\n" "$RED" "$RESET" "$*" >&2; exit 1; }

cat <<BANNER
${BOLD}${CYAN}
  ╔════════════════════════════════════════════════════════════╗
  ║   ARC Chain — Community Node Installer                     ║
  ║   Verifiable AI inference. Run it. Earn from the network.  ║
  ╚════════════════════════════════════════════════════════════╝${RESET}
BANNER
echo ""

# ── Detect platform ─────────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
ASSET=""
case "$OS" in
    Darwin)
        case "$ARCH" in
            arm64)  ASSET="arc-node-macos-arm64" ;;
            x86_64) ASSET="arc-node-macos-x86_64" ;;
            *)      fail "Unsupported macOS arch: $ARCH" ;;
        esac
        ;;
    Linux)
        case "$ARCH" in
            x86_64|amd64)   ASSET="arc-node-linux-x86_64" ;;
            aarch64|arm64)  ASSET="arc-node-linux-aarch64" ;;
            *)              fail "Unsupported Linux arch: $ARCH" ;;
        esac
        ;;
    *) fail "Unsupported OS: $OS" ;;
esac
ok "Platform: $OS $ARCH → $ASSET"

# ── Uninstall path ──────────────────────────────────────────────────────────
if [ "$DO_UNINSTALL" = true ]; then
    info "Uninstalling ARC community node..."
    if [ "$OS" = "Darwin" ]; then
        launchctl unload "$HOME/Library/LaunchAgents/com.arc.inference.plist" 2>/dev/null || true
        rm -f "$HOME/Library/LaunchAgents/com.arc.inference.plist"
        launchctl unload "$HOME/Library/LaunchAgents/com.arc.updater.plist" 2>/dev/null || true
        rm -f "$HOME/Library/LaunchAgents/com.arc.updater.plist"
    elif [ "$OS" = "Linux" ]; then
        if [ "$EUID" -eq 0 ] || command -v sudo >/dev/null; then
            ${SUDO:-sudo} systemctl stop arc-node 2>/dev/null || true
            ${SUDO:-sudo} systemctl disable arc-node 2>/dev/null || true
            ${SUDO:-sudo} rm -f /etc/systemd/system/arc-node.service
            ${SUDO:-sudo} rm -f /etc/systemd/system/arc-updater.timer /etc/systemd/system/arc-updater.service
            ${SUDO:-sudo} systemctl daemon-reload || true
        fi
    fi
    pkill -f "arc-node" 2>/dev/null || true
    ok "Service files removed. ARC dir at $ARC_DIR is preserved (delete manually if you want)."
    exit 0
fi

# ── Detect available RAM (informational) ────────────────────────────────────
TOTAL_RAM_GB=0
if [ "$OS" = "Darwin" ]; then
    TOTAL_RAM_BYTES=$(sysctl -n hw.memsize 2>/dev/null || echo 0)
    TOTAL_RAM_GB=$(( TOTAL_RAM_BYTES / 1024 / 1024 / 1024 ))
elif [ "$OS" = "Linux" ]; then
    TOTAL_RAM_GB=$(free -g 2>/dev/null | awk 'NR==2{print $2}' || echo 0)
fi
ok "Detected ${TOTAL_RAM_GB} GB RAM"
if [ "$TOTAL_RAM_GB" -lt 6 ]; then
    warn "Less than 6 GB RAM. Llama-7B Q4 needs ~5 GB. Falling back to TinyLlama 1.1B (638 MB)."
    DEFAULT_MODEL_URL="https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf"
    DEFAULT_MODEL_FILE="tinyllama-1.1b-chat.gguf"
    DEFAULT_MODEL_SIZE_GB=1
fi

# ── Get latest version from GitHub ──────────────────────────────────────────
info "Checking latest release..."
VERSION=$(curl -sf "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep -m1 '"tag_name"' \
    | sed -E 's/.*"v?([0-9]+\.[0-9]+\.[0-9]+)".*/\1/' || echo "")
if [ -z "$VERSION" ]; then
    warn "Could not fetch GitHub release, falling back to v0.3.1"
    VERSION="0.3.1"
fi
ok "Latest version: v$VERSION"

# ── Set up ARC directory ────────────────────────────────────────────────────
mkdir -p "$ARC_DIR" "$ARC_DIR/bin" "$ARC_DIR/data"
cd "$ARC_DIR"

# ── Download binary ─────────────────────────────────────────────────────────
if [ -x "$ARC_DIR/bin/arc-node" ] && "$ARC_DIR/bin/arc-node" --version 2>/dev/null | grep -q "$VERSION"; then
    ok "Binary already at v$VERSION"
else
    info "Downloading $ASSET v$VERSION..."
    curl -fL --progress-bar -o "$ARC_DIR/bin/arc-node.tmp" \
        "https://github.com/${REPO}/releases/download/v${VERSION}/${ASSET}"
    chmod +x "$ARC_DIR/bin/arc-node.tmp"
    mv "$ARC_DIR/bin/arc-node.tmp" "$ARC_DIR/bin/arc-node"
    ok "Binary installed: $ARC_DIR/bin/arc-node"
fi

# Save the version for the auto-updater to compare against
echo "$VERSION" > "$ARC_DIR/version.txt"

# ── Download seeds + genesis ────────────────────────────────────────────────
info "Downloading testnet config..."
curl -fsL -o "$ARC_DIR/seeds.txt" "https://raw.githubusercontent.com/${REPO}/main/testnet-seeds.txt"
curl -fsL -o "$ARC_DIR/genesis.toml" "https://raw.githubusercontent.com/${REPO}/main/genesis.toml"
ok "Seeds + genesis downloaded"

# ── Download model ──────────────────────────────────────────────────────────
if [ -n "$USER_MODEL" ]; then
    if [ ! -f "$USER_MODEL" ]; then
        fail "Model file not found: $USER_MODEL"
    fi
    MODEL_PATH="$USER_MODEL"
    ok "Using your model: $MODEL_PATH ($(du -h "$MODEL_PATH" | cut -f1))"
else
    MODEL_PATH="$ARC_DIR/$DEFAULT_MODEL_FILE"
    if [ -f "$MODEL_PATH" ]; then
        SIZE=$(stat -f%z "$MODEL_PATH" 2>/dev/null || stat -c%s "$MODEL_PATH" 2>/dev/null || echo 0)
        if [ "$SIZE" -gt 100000000 ]; then  # >100 MB suggests a real GGUF
            ok "Model already downloaded: $MODEL_PATH ($(du -h "$MODEL_PATH" | cut -f1))"
        else
            rm -f "$MODEL_PATH"
        fi
    fi
    if [ ! -f "$MODEL_PATH" ]; then
        info "Downloading $DEFAULT_MODEL_FILE (~${DEFAULT_MODEL_SIZE_GB} GB) — this is one-time, takes a few minutes"
        curl -fL --progress-bar -o "$MODEL_PATH.tmp" "$DEFAULT_MODEL_URL"
        mv "$MODEL_PATH.tmp" "$MODEL_PATH"
        ok "Model downloaded: $MODEL_PATH"
    fi
fi

# ── Generate unique validator seed ──────────────────────────────────────────
SEED_FILE="$ARC_DIR/identity.seed"
if [ ! -f "$SEED_FILE" ]; then
    HOST=$(hostname 2>/dev/null | tr -d '\n' | tr -c 'A-Za-z0-9' '-' | cut -c 1-16)
    RAND=$(openssl rand -hex 4 2>/dev/null || head -c 4 /dev/urandom | xxd -p)
    echo "community-${HOST}-${RAND}" > "$SEED_FILE"
fi
VALIDATOR_SEED="$(cat "$SEED_FILE")"
ok "Validator seed: $VALIDATOR_SEED"

# ── Build the auto-update script ────────────────────────────────────────────
UPDATER_PATH="$ARC_DIR/bin/arc-auto-update.sh"
cat > "$UPDATER_PATH" <<'UPDATER_EOF'
#!/usr/bin/env bash
# ARC Chain auto-updater. Checks GitHub for a newer release, downloads it,
# replaces the binary atomically, and restarts the service if needed.
set -euo pipefail
ARC_DIR="${ARC_DIR:-$HOME/.arc}"
REPO="FerrumVir/arc-chain"
LOG="$ARC_DIR/auto-update.log"
exec >>"$LOG" 2>&1
echo "[$(date)] auto-update check starting"
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Darwin) case "$ARCH" in arm64) ASSET="arc-node-macos-arm64" ;; x86_64) ASSET="arc-node-macos-x86_64" ;; *) exit 0 ;; esac ;;
    Linux)  case "$ARCH" in x86_64|amd64) ASSET="arc-node-linux-x86_64" ;; aarch64|arm64) ASSET="arc-node-linux-aarch64" ;; *) exit 0 ;; esac ;;
    *) exit 0 ;;
esac
LATEST=$(curl -sf "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep -m1 '"tag_name"' \
    | sed -E 's/.*"v?([0-9]+\.[0-9]+\.[0-9]+)".*/\1/' || echo "")
[ -z "$LATEST" ] && { echo "no version, exit"; exit 0; }
LOCAL=$(cat "$ARC_DIR/version.txt" 2>/dev/null || echo "")
if [ "$LATEST" = "$LOCAL" ]; then
    echo "[$(date)] up to date ($LOCAL)"
    exit 0
fi
echo "[$(date)] new version available: $LOCAL → $LATEST. Downloading."
curl -fL -o "$ARC_DIR/bin/arc-node.new" "https://github.com/${REPO}/releases/download/v${LATEST}/${ASSET}"
chmod +x "$ARC_DIR/bin/arc-node.new"
mv "$ARC_DIR/bin/arc-node.new" "$ARC_DIR/bin/arc-node"
echo "$LATEST" > "$ARC_DIR/version.txt"
echo "[$(date)] binary updated to v$LATEST"
# Restart the service so it picks up the new binary
if [ "$OS" = "Darwin" ]; then
    launchctl kickstart -k "gui/$(id -u)/com.arc.inference" 2>/dev/null || true
    echo "[$(date)] launchd kickstart sent"
elif [ "$OS" = "Linux" ]; then
    sudo systemctl restart arc-node 2>/dev/null || systemctl --user restart arc-node 2>/dev/null || true
    echo "[$(date)] systemctl restart sent"
fi
echo "[$(date)] auto-update complete"
UPDATER_EOF
chmod +x "$UPDATER_PATH"
ok "Auto-updater script: $UPDATER_PATH"

# ── Install service ─────────────────────────────────────────────────────────
if [ "$INSTALL_SERVICE" = true ]; then
    if [ "$OS" = "Darwin" ]; then
        PLIST="$HOME/Library/LaunchAgents/com.arc.inference.plist"
        mkdir -p "$HOME/Library/LaunchAgents"
        # Stop any existing instance first
        launchctl unload "$PLIST" 2>/dev/null || true
        cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.arc.inference</string>
    <key>ProgramArguments</key>
    <array>
        <string>$ARC_DIR/bin/arc-node</string>
        <string>--rpc</string><string>0.0.0.0:$RPC_PORT</string>
        <string>--p2p-port</string><string>$P2P_PORT</string>
        <string>--seeds-file</string><string>$ARC_DIR/seeds.txt</string>
        <string>--genesis</string><string>$ARC_DIR/genesis.toml</string>
        <string>--validator-seed</string><string>$VALIDATOR_SEED</string>
        <string>--stake</string><string>0</string>
        <string>--min-stake</string><string>0</string>
        <string>--eth-rpc-port</string><string>0</string>
        <string>--data-dir</string><string>$ARC_DIR/data</string>
        <string>--model</string><string>$MODEL_PATH</string>
        <string>--community-mode</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>ARC_DIR</key><string>$ARC_DIR</string>
    </dict>
    <key>WorkingDirectory</key><string>$ARC_DIR</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Background</string>
    <key>Nice</key><integer>15</integer>
    <key>LowPriorityBackgroundIO</key><true/>
    <key>StandardOutPath</key><string>$ARC_DIR/node.log</string>
    <key>StandardErrorPath</key><string>$ARC_DIR/node.log</string>
</dict>
</plist>
PLIST_EOF
        launchctl load "$PLIST"
        ok "launchd service installed: $PLIST"

        if [ "$INSTALL_UPDATER" = true ]; then
            UPDATER_PLIST="$HOME/Library/LaunchAgents/com.arc.updater.plist"
            launchctl unload "$UPDATER_PLIST" 2>/dev/null || true
            cat > "$UPDATER_PLIST" <<UPLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.arc.updater</string>
    <key>ProgramArguments</key>
    <array>
        <string>$UPDATER_PATH</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>ARC_DIR</key><string>$ARC_DIR</string>
    </dict>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key><integer>4</integer>
        <key>Minute</key><integer>17</integer>
    </dict>
    <key>StandardOutPath</key><string>$ARC_DIR/auto-update.log</string>
    <key>StandardErrorPath</key><string>$ARC_DIR/auto-update.log</string>
</dict>
</plist>
UPLIST_EOF
            launchctl load "$UPDATER_PLIST"
            ok "Auto-updater scheduled daily at 04:17 local"
        fi
    elif [ "$OS" = "Linux" ]; then
        if [ "$EUID" -ne 0 ] && ! command -v sudo >/dev/null; then
            warn "No root and no sudo — falling back to background process (not persistent)"
            nohup "$ARC_DIR/bin/arc-node" \
                --rpc "0.0.0.0:$RPC_PORT" --p2p-port "$P2P_PORT" \
                --seeds-file "$ARC_DIR/seeds.txt" --genesis "$ARC_DIR/genesis.toml" \
                --validator-seed "$VALIDATOR_SEED" --stake 0 --min-stake 0 \
                --eth-rpc-port 0 --data-dir "$ARC_DIR/data" \
                --model "$MODEL_PATH" > "$ARC_DIR/node.log" 2>&1 &
            ok "Started in background, PID $!"
        else
            SUDO=""
            [ "$EUID" -ne 0 ] && SUDO="sudo"
            SERVICE_FILE="/etc/systemd/system/arc-node.service"
            $SUDO tee "$SERVICE_FILE" > /dev/null <<SERVICE_EOF
[Unit]
Description=ARC Chain Inference Node
After=network.target

[Service]
Type=simple
User=$USER
WorkingDirectory=$ARC_DIR
Environment=ARC_DIR=$ARC_DIR
ExecStart=$ARC_DIR/bin/arc-node \\
    --rpc 0.0.0.0:$RPC_PORT \\
    --p2p-port $P2P_PORT \\
    --seeds-file $ARC_DIR/seeds.txt \\
    --genesis $ARC_DIR/genesis.toml \\
    --validator-seed $VALIDATOR_SEED \\
    --stake 0 --min-stake 0 \\
    --eth-rpc-port 0 \\
    --data-dir $ARC_DIR/data \\
    --model $MODEL_PATH \\
    --community-mode
Restart=always
RestartSec=5
StandardOutput=append:$ARC_DIR/node.log
StandardError=append:$ARC_DIR/node.log

[Install]
WantedBy=multi-user.target
SERVICE_EOF
            $SUDO systemctl daemon-reload
            $SUDO systemctl enable arc-node
            $SUDO systemctl restart arc-node
            ok "systemd service installed and started: arc-node.service"

            if [ "$INSTALL_UPDATER" = true ]; then
                UPDATER_SERVICE="/etc/systemd/system/arc-updater.service"
                UPDATER_TIMER="/etc/systemd/system/arc-updater.timer"
                $SUDO tee "$UPDATER_SERVICE" > /dev/null <<USVC_EOF
[Unit]
Description=ARC Chain auto-updater (one-shot)

[Service]
Type=oneshot
User=$USER
Environment=ARC_DIR=$ARC_DIR
ExecStart=$UPDATER_PATH
USVC_EOF
                $SUDO tee "$UPDATER_TIMER" > /dev/null <<UTIMER_EOF
[Unit]
Description=ARC Chain auto-updater daily

[Timer]
OnCalendar=*-*-* 04:17:00
Persistent=true

[Install]
WantedBy=timers.target
UTIMER_EOF
                $SUDO systemctl daemon-reload
                $SUDO systemctl enable --now arc-updater.timer
                ok "Auto-updater timer enabled (daily 04:17 local)"
            fi
        fi
    fi
else
    warn "--no-service: skipped persistent service install"
    # When --no-service is used, start the node as a detached background
    # process so the user still has a running node to test with. They can
    # kill it manually with the printed command.
    info "Starting node as a detached background process (no service)..."
    nohup "$ARC_DIR/bin/arc-node" \
        --rpc "0.0.0.0:$RPC_PORT" --p2p-port "$P2P_PORT" \
        --seeds-file "$ARC_DIR/seeds.txt" --genesis "$ARC_DIR/genesis.toml" \
        --validator-seed "$VALIDATOR_SEED" --stake 0 --min-stake 0 \
        --eth-rpc-port 0 --data-dir "$ARC_DIR/data" \
        --model "$MODEL_PATH" > "$ARC_DIR/node.log" 2>&1 &
    NODE_PID=$!
    echo "$NODE_PID" > "$ARC_DIR/node.pid"
    ok "Node started in background, PID $NODE_PID (logs: $ARC_DIR/node.log)"
    info "To stop: kill \$(cat $ARC_DIR/node.pid)"
fi

# ── Sanity check: wait for the node to come up AND connect to peers ─────────
# A node with status=ok but peers=0 is RUNNING but ISOLATED — it'll propose
# its own DAG blocks forever without ever seeing the real chain. That's the
# most common community-install failure and health-status alone doesn't catch
# it. We explicitly wait for peers >= 1 before declaring success.
info "Waiting for node to come up on http://localhost:$RPC_PORT ..."
sleep 5
NODE_UP=false
for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
    if curl -sf -m 2 "http://localhost:$RPC_PORT/health" >/dev/null 2>&1; then
        H=$(curl -sf -m 2 "http://localhost:$RPC_PORT/health")
        ok "Node is alive: $H"
        NODE_UP=true
        break
    fi
    sleep 5
    [ $i -eq 12 ] && warn "Node not responding yet — check $ARC_DIR/node.log"
done

if [ "$NODE_UP" = true ]; then
    info "Waiting for peer connections (up to 60s)..."
    PEER_COUNT=0
    for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
        # Extract peers value from /health JSON
        PEER_COUNT=$(curl -sf -m 3 "http://localhost:$RPC_PORT/health" 2>/dev/null \
            | sed -n 's/.*"peers":\([0-9][0-9]*\).*/\1/p')
        PEER_COUNT=${PEER_COUNT:-0}
        if [ "$PEER_COUNT" -ge 1 ] 2>/dev/null; then
            ok "Connected to $PEER_COUNT peer(s) — node is actually part of the network"
            break
        fi
        printf "."
        sleep 5
    done
    echo ""
    if [ "${PEER_COUNT:-0}" -lt 1 ] 2>/dev/null; then
        echo ""
        printf "%s%s⚠ NODE IS RUNNING BUT HAS ZERO PEERS%s\n" "$BOLD" "$YELLOW" "$RESET"
        echo ""
        echo "  Your node started successfully but could not connect to any of the 8"
        echo "  ARC testnet seed nodes. It is running in isolation and proposing its"
        echo "  own DAG blocks that nobody else will see."
        echo ""
        echo "  ${BOLD}Most likely cause:${RESET} your firewall/ISP is blocking outbound UDP"
        echo "  to port 9091. ARC uses QUIC (UDP) for P2P, not TCP."
        echo ""
        echo "  ${BOLD}Quick diagnosis:${RESET}"
        echo "    nc -zu -w 3 149.28.32.76 9091   # should print 'succeeded'"
        echo "    nc -zu -w 3 140.82.16.112 9091  # should print 'succeeded'"
        echo ""
        echo "  ${BOLD}If the nc tests succeed but peers stays 0:${RESET}"
        echo "    tail -f $ARC_DIR/node.log | grep -E 'Handshake|Failed|Timeout'"
        echo "  and paste the output in the Discord / GitHub issue."
        echo ""
        echo "  ${BOLD}Common fixes:${RESET}"
        echo "    • Allow outbound UDP 9091 in your firewall"
        echo "    • Disable VPN if it blocks UDP"
        echo "    • If on corporate/school network, try from a residential connection"
        echo "    • On macOS: System Settings → Privacy & Security → Firewall → allow arc-node"
        echo ""
    fi
fi

# ── Final banner ────────────────────────────────────────────────────────────
echo ""
printf "%s%s════════════════════════════════════════════════════════════════%s\n" "$BOLD" "$GREEN" "$RESET"
printf "%s%s  ARC community node installed and running!%s\n" "$BOLD" "$GREEN" "$RESET"
printf "%s%s════════════════════════════════════════════════════════════════%s\n" "$BOLD" "$GREEN" "$RESET"
echo ""
echo "  ${BOLD}Local RPC:${RESET}      http://localhost:$RPC_PORT"
echo "  ${BOLD}Health:${RESET}         curl http://localhost:$RPC_PORT/health"
echo "  ${BOLD}Run inference:${RESET}  curl -X POST http://localhost:$RPC_PORT/inference/run \\"
echo "                     -H 'Content-Type: application/json' \\"
echo "                     -d '{\"input\":\"[INST] What is 2+2? [/INST]\",\"max_tokens\":32}'"
echo ""
echo "  ${BOLD}Node logs:${RESET}      tail -f $ARC_DIR/node.log"
echo "  ${BOLD}Live network:${RESET}   http://140.82.16.112:3200"
echo ""
if [ "$INSTALL_SERVICE" = true ]; then
    if [ "$OS" = "Darwin" ]; then
        echo "  ${BOLD}Service:${RESET} launchd com.arc.inference (auto-restart on crash)"
        echo "  ${BOLD}Stop:${RESET}    launchctl unload ~/Library/LaunchAgents/com.arc.inference.plist"
    else
        echo "  ${BOLD}Service:${RESET} systemd arc-node.service (auto-restart on crash)"
        echo "  ${BOLD}Stop:${RESET}    sudo systemctl stop arc-node"
        echo "  ${BOLD}Logs:${RESET}    journalctl -u arc-node -f"
    fi
fi
if [ "$INSTALL_UPDATER" = true ]; then
    echo "  ${BOLD}Auto-update:${RESET} daily check at 04:17 local time"
    echo "  ${BOLD}Manual update:${RESET} $UPDATER_PATH"
fi
echo ""
echo "  ${BOLD}Uninstall:${RESET}    bash $0 --uninstall"
echo ""
