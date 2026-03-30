#!/usr/bin/env bash
# ════════════════════════════════════════════════════════════════
#  ARC Network — Join in 60 Seconds
#
#  Run AI inference on your device. Earn ARC tokens.
#
#  Usage:
#    curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-community.sh | bash
#
#  Or with options:
#    bash arc-community.sh --model /path/to/model.gguf --cpu-limit 20
# ════════════════════════════════════════════════════════════════
set -euo pipefail

ARC_DIR="${HOME}/.arc"
REPO_URL="https://github.com/FerrumVir/arc-chain.git"
MODEL_URL="https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf"
MODEL_NAME="Llama 2 7B Chat"
MODEL_SIZE="4.1 GB"
CPU_LIMIT="15"
MODEL_PATH=""
SKIP_MODEL=""

# ── Parse args ──────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case $1 in
        --model) MODEL_PATH="$2"; shift 2 ;;
        --cpu-limit) CPU_LIMIT="$2"; shift 2 ;;
        --skip-model) SKIP_MODEL=1; shift ;;
        --tiny) MODEL_URL="https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf"; MODEL_NAME="TinyLlama 1.1B"; MODEL_SIZE="638 MB"; shift ;;
        *) echo "Unknown: $1"; exit 1 ;;
    esac
done

# ── Colors + helpers ────────────────────────────────────
R='\033[0;31m'; G='\033[0;32m'; Y='\033[1;33m'; B='\033[0;34m'
C='\033[0;36m'; W='\033[1;37m'; D='\033[0;90m'; N='\033[0m'

step()  { echo -e "\n${C}[$1/6]${N} ${W}$2${N}"; }
ok()    { echo -e "  ${G}✓${N} $1"; }
warn()  { echo -e "  ${Y}!${N} $1"; }
fail()  { echo -e "  ${R}✗${N} $1"; exit 1; }
spin()  {
    local pid=$1 msg=$2 chars='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
    while kill -0 "$pid" 2>/dev/null; do
        for (( i=0; i<${#chars}; i++ )); do
            printf "\r  ${D}${chars:$i:1}${N} %s" "$msg"
            sleep 0.1
        done
    done
    printf "\r"
}

# ── Banner ──────────────────────────────────────────────
echo ""
echo -e "${W}  ╔═══════════════════════════════════════╗${N}"
echo -e "${W}  ║${N}   ${C}ARC Network${N} — Decentralized AI      ${W}║${N}"
echo -e "${W}  ║${N}   Run inference. Earn tokens.          ${W}║${N}"
echo -e "${W}  ╚═══════════════════════════════════════╝${N}"
echo ""

# ── Step 1: Create data directory + identity ────────────
step 1 "Setting up your node identity"
mkdir -p "${ARC_DIR}/bin" "${ARC_DIR}/data"

if [[ -f "${ARC_DIR}/identity.seed" ]]; then
    SEED=$(cat "${ARC_DIR}/identity.seed")
    ok "Identity loaded: ${SEED}"
else
    SEED="arc-worker-$(openssl rand -hex 8 2>/dev/null || head -c 16 /dev/urandom | xxd -p)"
    echo "${SEED}" > "${ARC_DIR}/identity.seed"
    chmod 600 "${ARC_DIR}/identity.seed"
    ok "New identity created: ${SEED}"
fi

# ── Step 2: Get the binary ──────────────────────────────
step 2 "Getting the ARC node binary"

BINARY=""
OS="$(uname -s)"
ARCH="$(uname -m)"

# Check for existing binary
if [[ -x "${ARC_DIR}/bin/arc-node" ]]; then
    BINARY="${ARC_DIR}/bin/arc-node"
    ok "Using cached binary"
elif [[ -d "${ARC_DIR}/src/arc-chain" && -x "${ARC_DIR}/src/arc-chain/target/release/arc-node" ]]; then
    BINARY="${ARC_DIR}/src/arc-chain/target/release/arc-node"
    ok "Using existing build"
else
    # Need to build from source
    if ! command -v cargo &>/dev/null; then
        warn "Rust not installed. Installing now..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain nightly 2>/dev/null
        source "${HOME}/.cargo/env" 2>/dev/null || export PATH="${HOME}/.cargo/bin:${PATH}"
        if ! command -v cargo &>/dev/null; then
            fail "Failed to install Rust. Please install manually: https://rustup.rs"
        fi
        ok "Rust installed"
    else
        ok "Rust found: $(rustc --version 2>/dev/null | head -1)"
    fi

    if [[ -d "${ARC_DIR}/src/arc-chain" ]]; then
        echo -e "  ${D}Updating source...${N}"
        cd "${ARC_DIR}/src/arc-chain"
        git pull origin main --quiet 2>/dev/null || true
    else
        echo -e "  ${D}Cloning repository...${N}"
        git clone --depth 1 "${REPO_URL}" "${ARC_DIR}/src/arc-chain" 2>/dev/null
        cd "${ARC_DIR}/src/arc-chain"
    fi

    echo -e "  ${D}Building (first time takes 3-5 minutes)...${N}"
    cargo build --release -p arc-node 2>&1 | tail -1 &
    BUILD_PID=$!
    spin $BUILD_PID "Building arc-node..."
    wait $BUILD_PID || fail "Build failed. Check ${ARC_DIR}/src/arc-chain for errors."
    BINARY="${ARC_DIR}/src/arc-chain/target/release/arc-node"
    cp "${BINARY}" "${ARC_DIR}/bin/arc-node"
    ok "Build complete"
fi

# Verify binary works
if ! "${BINARY:-${ARC_DIR}/bin/arc-node}" --help &>/dev/null; then
    fail "Binary is not working. Try rebuilding."
fi
BINARY="${BINARY:-${ARC_DIR}/bin/arc-node}"
ok "Binary verified"

# ── Step 3: Download AI model ───────────────────────────
step 3 "Downloading AI model (${MODEL_NAME}, ${MODEL_SIZE})"

if [[ -n "${MODEL_PATH}" && -f "${MODEL_PATH}" ]]; then
    ok "Using provided model: ${MODEL_PATH}"
elif [[ -n "${SKIP_MODEL}" ]]; then
    MODEL_PATH=""
    warn "Skipping model download (node will relay requests only)"
elif [[ -f "${ARC_DIR}/model.gguf" ]]; then
    MODEL_PATH="${ARC_DIR}/model.gguf"
    ok "Model already downloaded"
else
    MODEL_PATH="${ARC_DIR}/model.gguf"
    curl -L --progress-bar "${MODEL_URL}" -o "${MODEL_PATH}.tmp" 2>&1
    mv "${MODEL_PATH}.tmp" "${MODEL_PATH}"
    ok "Model downloaded: ${MODEL_NAME}"
fi

# ── Step 4: Write seeds + genesis ───────────────────────
step 4 "Configuring network connection"

cat > "${ARC_DIR}/seeds.txt" <<'SEEDS'
149.28.32.76:9091
140.82.16.112:9091
136.244.109.1:9091
104.238.171.11:9091
202.182.107.41:9091
149.28.153.31:9091
216.238.120.27:9091
139.84.237.49:9091
SEEDS

# Copy genesis if we have the source
GENESIS=""
if [[ -f "${ARC_DIR}/src/arc-chain/genesis.toml" ]]; then
    GENESIS="${ARC_DIR}/src/arc-chain/genesis.toml"
elif [[ -f "./genesis.toml" ]]; then
    GENESIS="./genesis.toml"
else
    # Download genesis from a seed
    for seed_ip in 140.82.16.112 149.28.32.76 136.244.109.1; do
        if curl -sf "http://${seed_ip}:9090/" >/dev/null 2>&1; then
            # Use the source checkout's genesis
            warn "Genesis file not found locally. Using source checkout."
            break
        fi
    done
    if [[ -z "${GENESIS}" ]]; then
        fail "Cannot find genesis.toml. Clone the repo first or run from the arc-chain directory."
    fi
fi
ok "Network configured (8 seed validators across 6 continents)"

# ── Step 5: Set up persistent service + start ───────────
step 5 "Starting your inference node (persistent)"

# Resolve absolute paths for service configs
BINARY_ABS="$(cd "$(dirname "${BINARY}")" && pwd)/$(basename "${BINARY}")"
GENESIS_ABS="$(cd "$(dirname "${GENESIS}")" && pwd)/$(basename "${GENESIS}")"
MODEL_ABS=""
if [[ -n "${MODEL_PATH}" && -f "${MODEL_PATH}" ]]; then
    MODEL_ABS="$(cd "$(dirname "${MODEL_PATH}")" && pwd)/$(basename "${MODEL_PATH}")"
fi

if [[ "${OS}" == "Darwin" ]]; then
    # ── macOS: use launchd as primary process manager ──
    PLIST_DIR="${HOME}/Library/LaunchAgents"
    PLIST="${PLIST_DIR}/com.arc.inference.plist"
    mkdir -p "${PLIST_DIR}"

    # Unload old if exists (ignore errors)
    launchctl bootout "gui/$(id -u)/com.arc.inference" 2>/dev/null || true
    # Also kill any leftover nohup processes
    if [[ -f "${ARC_DIR}/node.pid" ]]; then
        kill "$(cat "${ARC_DIR}/node.pid")" 2>/dev/null || true
        sleep 1
    fi

    cat > "${PLIST}" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.arc.inference</string>
    <key>ProgramArguments</key>
    <array>
        <string>${BINARY_ABS}</string>
        <string>--rpc</string><string>0.0.0.0:9090</string>
        <string>--seeds-file</string><string>${ARC_DIR}/seeds.txt</string>
        <string>--genesis</string><string>${GENESIS_ABS}</string>
        <string>--validator-seed</string><string>${SEED}</string>
        <string>--stake</string><string>5000000</string>
        <string>--mode</string><string>worker</string>
        <string>--cpu-limit</string><string>${CPU_LIMIT}</string>
        <string>--eth-rpc-port</string><string>0</string>
$(if [[ -n "${MODEL_ABS}" ]]; then
    echo "        <string>--model</string><string>${MODEL_ABS}</string>"
fi)
    </array>
    <key>WorkingDirectory</key><string>$(dirname "${GENESIS_ABS}")</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Background</string>
    <key>Nice</key><integer>15</integer>
    <key>LowPriorityBackgroundIO</key><true/>
    <key>StandardOutPath</key><string>${ARC_DIR}/node.log</string>
    <key>StandardErrorPath</key><string>${ARC_DIR}/node.log</string>
</dict>
</plist>
PLIST

    # Load and start — launchd manages the process from now on.
    # KeepAlive=true means it restarts on crash AND starts on boot.
    launchctl load "${PLIST}" 2>/dev/null
    ok "Persistent service installed (launchd)"
    ok "Survives: close terminal, close browser, logout, reboot"

elif [[ "${OS}" == "Linux" ]]; then
    # ── Linux: use systemd user service ──
    SERVICE_DIR="${HOME}/.config/systemd/user"
    SERVICE="${SERVICE_DIR}/arc-inference.service"
    mkdir -p "${SERVICE_DIR}"

    # Stop old
    systemctl --user stop arc-inference 2>/dev/null || true
    if [[ -f "${ARC_DIR}/node.pid" ]]; then
        kill "$(cat "${ARC_DIR}/node.pid")" 2>/dev/null || true
        sleep 1
    fi

    LINUX_BINARY="$(readlink -f "${BINARY}")"
    LINUX_GENESIS="$(readlink -f "${GENESIS}")"
    LINUX_MODEL=""
    if [[ -n "${MODEL_ABS}" ]]; then LINUX_MODEL=" --model $(readlink -f "${MODEL_PATH}")"; fi

    cat > "${SERVICE}" <<SERVICE
[Unit]
Description=ARC Community Inference Node
After=network-online.target
[Service]
Type=simple
ExecStart=${LINUX_BINARY} --rpc 0.0.0.0:9090 --seeds-file ${ARC_DIR}/seeds.txt --genesis ${LINUX_GENESIS} --validator-seed ${SEED} --stake 5000000 --mode worker --cpu-limit ${CPU_LIMIT} --eth-rpc-port 0${LINUX_MODEL}
Restart=always
RestartSec=5
CPUQuota=${CPU_LIMIT}%
WorkingDirectory=$(dirname "${LINUX_GENESIS}")
Environment=RUST_LOG=info
[Install]
WantedBy=default.target
SERVICE

    systemctl --user daemon-reload 2>/dev/null
    systemctl --user enable --now arc-inference 2>/dev/null
    # Enable lingering so the service runs even when logged out
    loginctl enable-linger "$(whoami)" 2>/dev/null || true
    ok "Persistent service installed (systemd)"
    ok "Survives: close terminal, close browser, logout, reboot"

else
    # Fallback: nohup for unsupported OS
    if [[ -f "${ARC_DIR}/node.pid" ]]; then
        kill "$(cat "${ARC_DIR}/node.pid")" 2>/dev/null || true; sleep 1
    fi
    CMD=("${BINARY_ABS}" --rpc 0.0.0.0:9090 --seeds-file "${ARC_DIR}/seeds.txt" --validator-seed "${SEED}" --stake 5000000 --mode worker --cpu-limit "${CPU_LIMIT}" --eth-rpc-port 0)
    if [[ -n "${GENESIS_ABS}" ]]; then CMD+=(--genesis "${GENESIS_ABS}"); fi
    if [[ -n "${MODEL_ABS}" ]]; then CMD+=(--model "${MODEL_ABS}"); fi
    cd "$(dirname "${GENESIS_ABS}")"
    nohup "${CMD[@]}" > "${ARC_DIR}/node.log" 2>&1 &
    echo $! > "${ARC_DIR}/node.pid"
    warn "No launchd/systemd — using nohup (won't survive reboot)"
fi

# ── Step 6: Wait for health + claim faucet ────────────────
step 6 "Connecting to network"

for i in $(seq 1 30); do
    if curl -sf http://localhost:9090/health >/dev/null 2>&1; then
        PEERS=$(curl -sf http://localhost:9090/health 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('peers',0))" 2>/dev/null || echo "0")
        ok "Connected to ${PEERS} peers"
        break
    fi
    sleep 1
done

# Faucet claim
ADDR=$(curl -sf http://localhost:9090/worker/earnings 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('address',''))" 2>/dev/null || echo "")
if [[ -n "${ADDR}" ]]; then
    for seed_ip in 140.82.16.112 149.28.32.76 136.244.109.1; do
        if curl -sf -X POST "http://${seed_ip}:9090/faucet/claim" -H "Content-Type: application/json" -d "{\"address\": \"${ADDR}\"}" >/dev/null 2>&1; then
            ok "Testnet tokens claimed"
            break
        fi
    done
fi

# ── Open dashboard ──────────────────────────────────────
DASHBOARD_URL="http://localhost:9090/worker/dashboard"

if [[ "${OS}" == "Darwin" ]]; then
    open "${DASHBOARD_URL}" 2>/dev/null &
elif command -v xdg-open &>/dev/null; then
    xdg-open "${DASHBOARD_URL}" 2>/dev/null &
fi

# ── Summary ─────────────────────────────────────────────
echo ""
echo -e "${W}  ════════════════════════════════════════${N}"
echo -e "${G}  Your ARC node is running!${N}"
echo -e "${W}  ════════════════════════════════════════${N}"
echo ""
echo -e "  ${B}Dashboard${N}:  ${DASHBOARD_URL}"
echo -e "  ${B}Address${N}:    ${ADDR:-starting...}"
echo -e "  ${B}Model${N}:      ${MODEL_NAME}"
echo -e "  ${B}CPU Limit${N}:  ${CPU_LIMIT}%"
echo -e "  ${B}Logs${N}:       tail -f ${ARC_DIR}/node.log"
echo ""
echo -e "  ${Y}Commands${N}:"
echo -e "    Status:   open ${DASHBOARD_URL}"
echo -e "    Stop:     kill \$(cat ${ARC_DIR}/node.pid)"
echo -e "    Restart:  bash $0"
echo ""
echo -e "  ${C}Share with friends${N}:"
echo -e "    curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-community.sh | bash"
echo ""
