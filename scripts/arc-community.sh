#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────
# ARC Community Inference Node — One-Command Setup
#
# Join the ARC inference network in under 60 seconds.
# Run AI inference, earn ARC tokens, auto-upgrade.
#
# Usage:
#   curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-community.sh | bash
#   # OR
#   bash scripts/arc-community.sh [--model /path/to/model.gguf] [--cpu-limit 15]
# ────────────────────────────────────────────────────────────────
set -euo pipefail

ARC_DIR="${HOME}/.arc"
IDENTITY_FILE="${ARC_DIR}/identity.seed"
LOG_FILE="${ARC_DIR}/node.log"
PID_FILE="${ARC_DIR}/node.pid"
SEEDS_FILE="${ARC_DIR}/seeds.txt"
MODEL_URL="https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf"
CPU_LIMIT="${ARC_CPU_LIMIT:-15}"
MODEL_PATH=""
BINARY=""

# ── Parse args ────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case $1 in
        --model) MODEL_PATH="$2"; shift 2 ;;
        --cpu-limit) CPU_LIMIT="$2"; shift 2 ;;
        --binary) BINARY="$2"; shift 2 ;;
        *) echo "Unknown flag: $1"; exit 1 ;;
    esac
done

# ── Colors ────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'

info()  { echo -e "${GREEN}[ARC]${NC} $*"; }
warn()  { echo -e "${YELLOW}[ARC]${NC} $*"; }
error() { echo -e "${RED}[ARC]${NC} $*"; exit 1; }

echo -e "${BOLD}"
echo "  ╔═══════════════════════════════════════════╗"
echo "  ║   ARC Community Inference Node            ║"
echo "  ║   Serve AI. Earn ARC. Decentralize AI.    ║"
echo "  ╚═══════════════════════════════════════════╝"
echo -e "${NC}"

# ── Step 1: Create data directory ─────────────────────────────
mkdir -p "${ARC_DIR}/bin"
info "Data directory: ${ARC_DIR}"

# ── Step 2: Persistent identity ───────────────────────────────
if [[ -f "${IDENTITY_FILE}" ]]; then
    SEED=$(cat "${IDENTITY_FILE}")
    info "Identity loaded: ${SEED}"
else
    SEED="arc-worker-$(openssl rand -hex 8)"
    echo "${SEED}" > "${IDENTITY_FILE}"
    chmod 600 "${IDENTITY_FILE}"
    info "New identity created: ${SEED}"
fi

# ── Step 3: Find or build binary ─────────────────────────────
if [[ -n "${BINARY}" && -x "${BINARY}" ]]; then
    info "Using binary: ${BINARY}"
elif [[ -x "${ARC_DIR}/bin/arc-node" ]]; then
    BINARY="${ARC_DIR}/bin/arc-node"
    info "Using cached binary: ${BINARY}"
elif [[ -x "./target/release/arc-node" ]]; then
    BINARY="./target/release/arc-node"
    info "Using local build: ${BINARY}"
elif command -v cargo &>/dev/null; then
    info "Building from source (first time only, ~5 minutes)..."
    if [[ -d ".git" && -f "Cargo.toml" ]]; then
        cargo build --release -p arc-node 2>&1 | tail -3
        BINARY="./target/release/arc-node"
    elif [[ -d "${ARC_DIR}/src/arc-chain" ]]; then
        cd "${ARC_DIR}/src/arc-chain"
        git pull origin main
        cargo build --release -p arc-node 2>&1 | tail -3
        BINARY="./target/release/arc-node"
    else
        info "Cloning ARC Chain..."
        git clone https://github.com/FerrumVir/arc-chain.git "${ARC_DIR}/src/arc-chain"
        cd "${ARC_DIR}/src/arc-chain"
        cargo build --release -p arc-node 2>&1 | tail -3
        BINARY="./target/release/arc-node"
    fi
    # Cache for future runs
    cp "${BINARY}" "${ARC_DIR}/bin/arc-node"
    info "Binary built and cached at ${ARC_DIR}/bin/arc-node"
else
    error "No binary found and Rust not installed. Install Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

# ── Step 4: Download model if needed ─────────────────────────
if [[ -z "${MODEL_PATH}" ]]; then
    MODEL_PATH="${ARC_DIR}/model.gguf"
fi

if [[ ! -f "${MODEL_PATH}" ]]; then
    info "Downloading TinyLlama 1.1B model (638 MB)..."
    curl -L --progress-bar "${MODEL_URL}" -o "${MODEL_PATH}.tmp"
    mv "${MODEL_PATH}.tmp" "${MODEL_PATH}"
    info "Model downloaded: ${MODEL_PATH}"
else
    info "Model found: ${MODEL_PATH}"
fi

# ── Step 5: Write seeds file ─────────────────────────────────
cat > "${SEEDS_FILE}" <<'SEEDS'
# ARC Testnet Seed Nodes — 8 validators across 6 continents
149.28.32.76:9091
140.82.16.112:9091
136.244.109.1:9091
104.238.171.11:9091
202.182.107.41:9091
149.28.153.31:9091
216.238.120.27:9091
139.84.237.49:9091
SEEDS

# ── Step 6: Stop existing node if running ─────────────────────
if [[ -f "${PID_FILE}" ]] && kill -0 "$(cat "${PID_FILE}")" 2>/dev/null; then
    warn "Stopping existing node (PID $(cat "${PID_FILE}"))..."
    kill "$(cat "${PID_FILE}")" 2>/dev/null || true
    sleep 2
fi

# ── Step 7: Start the node ────────────────────────────────────
info "Starting node in worker mode (CPU limit: ${CPU_LIMIT}%)..."

nohup "${BINARY}" \
    --rpc 0.0.0.0:9090 \
    --seeds-file "${SEEDS_FILE}" \
    --genesis genesis.toml \
    --validator-seed "${SEED}" \
    --stake 5000000 \
    --model "${MODEL_PATH}" \
    --mode worker \
    --cpu-limit "${CPU_LIMIT}" \
    --eth-rpc-port 0 \
    > "${LOG_FILE}" 2>&1 &

NODE_PID=$!
echo "${NODE_PID}" > "${PID_FILE}"
info "Node started (PID: ${NODE_PID})"

# ── Step 8: Wait for health ──────────────────────────────────
info "Waiting for node to connect..."
for i in $(seq 1 30); do
    if curl -sf http://localhost:9090/health >/dev/null 2>&1; then
        PEERS=$(curl -sf http://localhost:9090/health 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('peer_count',0))" 2>/dev/null || echo "0")
        info "Node is up! Connected to ${PEERS} peers."
        break
    fi
    sleep 1
done

# ── Step 9: Auto-claim from faucet ───────────────────────────
ADDR=$(curl -sf http://localhost:9090/node/info 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('address',''))" 2>/dev/null || echo "")
if [[ -n "${ADDR}" ]]; then
    # Claim from first reachable seed
    for seed in 140.82.16.112 149.28.32.76 136.244.109.1; do
        if curl -sf -X POST "http://${seed}:9090/faucet/claim" \
            -H "Content-Type: application/json" \
            -d "{\"address\": \"${ADDR}\"}" >/dev/null 2>&1; then
            info "Faucet claim submitted for attestation bonds"
            break
        fi
    done
fi

# ── Step 10: Print summary ────────────────────────────────────
echo ""
echo -e "${BOLD}═══════════════════════════════════════════${NC}"
echo -e "${GREEN}  Your ARC inference node is running!${NC}"
echo -e "${BOLD}═══════════════════════════════════════════${NC}"
echo ""
echo -e "  ${BLUE}Address:${NC}  ${ADDR:-<starting...>}"
echo -e "  ${BLUE}Mode:${NC}     Worker (inference only)"
echo -e "  ${BLUE}CPU:${NC}      ${CPU_LIMIT}% limit"
echo -e "  ${BLUE}Model:${NC}    $(basename "${MODEL_PATH}")"
echo -e "  ${BLUE}Earnings:${NC} http://localhost:9090/worker/earnings"
echo -e "  ${BLUE}Logs:${NC}     tail -f ${LOG_FILE}"
echo ""
echo -e "  ${YELLOW}Commands:${NC}"
echo -e "    Status:   curl -s localhost:9090/worker/earnings | python3 -m json.tool"
echo -e "    Logs:     tail -f ${LOG_FILE}"
echo -e "    Stop:     kill \$(cat ${PID_FILE})"
echo -e "    Restart:  bash scripts/arc-community.sh"
echo ""

# ── Step 11: Set up launchd/systemd for auto-start ───────────
OS="$(uname -s)"
if [[ "${OS}" == "Darwin" ]]; then
    PLIST_DIR="${HOME}/Library/LaunchAgents"
    PLIST_FILE="${PLIST_DIR}/com.arc.inference.plist"
    mkdir -p "${PLIST_DIR}"

    BINARY_ABS="$(cd "$(dirname "${BINARY}")" && pwd)/$(basename "${BINARY}")"
    MODEL_ABS="$(cd "$(dirname "${MODEL_PATH}")" && pwd)/$(basename "${MODEL_PATH}")"

    cat > "${PLIST_FILE}" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.arc.inference</string>
    <key>ProgramArguments</key>
    <array>
        <string>${BINARY_ABS}</string>
        <string>--rpc</string><string>0.0.0.0:9090</string>
        <string>--seeds-file</string><string>${SEEDS_FILE}</string>
        <string>--validator-seed</string><string>${SEED}</string>
        <string>--stake</string><string>5000000</string>
        <string>--model</string><string>${MODEL_ABS}</string>
        <string>--mode</string><string>worker</string>
        <string>--cpu-limit</string><string>${CPU_LIMIT}</string>
        <string>--eth-rpc-port</string><string>0</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>Nice</key>
    <integer>15</integer>
    <key>LowPriorityBackgroundIO</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${LOG_FILE}</string>
    <key>StandardErrorPath</key>
    <string>${LOG_FILE}</string>
    <key>WorkingDirectory</key>
    <string>${ARC_DIR}</string>
</dict>
</plist>
PLIST

    info "launchd plist written: ${PLIST_FILE}"
    info "To enable auto-start on login: launchctl load ${PLIST_FILE}"

elif [[ "${OS}" == "Linux" ]]; then
    SERVICE_DIR="${HOME}/.config/systemd/user"
    SERVICE_FILE="${SERVICE_DIR}/arc-inference.service"
    mkdir -p "${SERVICE_DIR}"

    BINARY_ABS="$(readlink -f "${BINARY}")"
    MODEL_ABS="$(readlink -f "${MODEL_PATH}")"

    cat > "${SERVICE_FILE}" <<SERVICE
[Unit]
Description=ARC Community Inference Node
After=network-online.target

[Service]
Type=simple
ExecStart=${BINARY_ABS} --rpc 0.0.0.0:9090 --seeds-file ${SEEDS_FILE} --validator-seed ${SEED} --stake 5000000 --model ${MODEL_ABS} --mode worker --cpu-limit ${CPU_LIMIT} --eth-rpc-port 0
Restart=always
RestartSec=5
CPUQuota=${CPU_LIMIT}%
WorkingDirectory=${ARC_DIR}
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
SERVICE

    info "systemd service written: ${SERVICE_FILE}"
    info "To enable auto-start: systemctl --user enable arc-inference && systemctl --user start arc-inference"
fi

info "Done. Your node is earning ARC!"
