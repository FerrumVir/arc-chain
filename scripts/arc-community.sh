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
        --help|-h)
            echo "ARC Community Node Installer"
            echo ""
            echo "Usage: bash arc-community.sh [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --tiny          Use TinyLlama 1.1B (638 MB) instead of Llama 2 7B (4.1 GB)"
            echo "  --skip-model    Skip model download (relay-only mode, no local inference)"
            echo "  --model PATH    Use your own GGUF model file"
            echo "  --cpu-limit N   Max CPU percentage (default: 15)"
            echo "  --help, -h      Show this help"
            echo ""
            echo "Hardware auto-detection:"
            echo "  RAM < 4 GB   -> relay-only mode (no model downloaded)"
            echo "  RAM 4-7 GB   -> auto-selects TinyLlama 1.1B (638 MB)"
            echo "  RAM >= 8 GB  -> default Llama 2 7B Chat (4.1 GB)"
            echo "  Use --tiny or --model to override auto-detection."
            echo ""
            echo "Examples:"
            echo "  bash arc-community.sh                  # Auto-detect hardware, pick best model"
            echo "  bash arc-community.sh --tiny            # Force smaller model"
            echo "  bash arc-community.sh --cpu-limit 25    # Allow 25% CPU"
            echo ""
            echo "After install, your dashboard opens at http://localhost:9090/worker/dashboard"
            exit 0
            ;;
        *) echo "Unknown option: $1 (run with --help to see usage)"; exit 1 ;;
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

# ── Pre-flight checks ──────────────────────────────────
echo -e "  ${D}Running pre-flight checks...${N}"

# Check curl is installed (needed for model download + API calls)
if ! command -v curl &>/dev/null; then
    fail "curl is not installed. Install it first:\n    macOS: xcode-select --install\n    Ubuntu/Debian: sudo apt install curl\n    Fedora/RHEL: sudo dnf install curl"
fi

# Check git is installed (needed to clone the repo for building)
if [[ -z "${SKIP_MODEL}" ]] && ! command -v git &>/dev/null; then
    if [[ ! -x "${HOME}/.arc/bin/arc-node" ]]; then
        fail "git is not installed (needed to build from source). Install it first:\n    macOS: xcode-select --install\n    Ubuntu/Debian: sudo apt install git\n    Fedora/RHEL: sudo dnf install git"
    fi
fi

# Check disk space (need ~5 GB for model + build artifacts)
AVAILABLE_MB=0
if command -v df &>/dev/null; then
    if [[ "$(uname -s)" == "Darwin" ]]; then
        AVAILABLE_MB=$(df -m "${HOME}" 2>/dev/null | awk 'NR==2{print $4}' || echo 0)
    else
        AVAILABLE_MB=$(df -m "${HOME}" 2>/dev/null | awk 'NR==2{print $4}' || echo 0)
    fi
fi
if [[ "${AVAILABLE_MB}" -gt 0 ]] 2>/dev/null; then
    NEED_MB=5000
    if [[ -n "${SKIP_MODEL}" ]]; then NEED_MB=2000; fi
    if [[ "${AVAILABLE_MB}" -lt "${NEED_MB}" ]]; then
        fail "Not enough disk space. Need ~$((NEED_MB / 1000)) GB, have $((AVAILABLE_MB / 1000)) GB free.\n    Free up space or use --tiny (638 MB model) or --skip-model."
    fi
    ok "Disk space: $((AVAILABLE_MB / 1000)) GB free"
else
    warn "Could not check disk space — continuing anyway"
fi

# Check if port 9090 is already in use by something else
if command -v lsof &>/dev/null; then
    PORT_PID=$(lsof -ti :9090 2>/dev/null | head -1 || true)
    if [[ -n "${PORT_PID}" ]]; then
        PORT_CMD=$(ps -p "${PORT_PID}" -o comm= 2>/dev/null || echo "unknown")
        if [[ "${PORT_CMD}" == *"arc-node"* || "${PORT_CMD}" == *"arc_node"* ]]; then
            warn "ARC node already running on port 9090 (PID ${PORT_PID}). Will restart it."
        else
            fail "Port 9090 is already in use by '${PORT_CMD}' (PID ${PORT_PID}).\n    Stop it first: kill ${PORT_PID}\n    Or check: lsof -i :9090"
        fi
    fi
elif command -v ss &>/dev/null; then
    if ss -tlnp 2>/dev/null | grep -q ':9090 '; then
        warn "Port 9090 may be in use. If the node fails to start, check: ss -tlnp | grep 9090"
    fi
fi

# Auto-open firewall for UDP 9091 (P2P port) — don't ask, just do it
if [[ "$(uname -s)" == "Linux" ]]; then
    # ufw
    if command -v ufw &>/dev/null; then
        if ufw status 2>/dev/null | grep -q "Status: active"; then
            if ! ufw status 2>/dev/null | grep -q "9091/udp"; then
                if [[ "$(id -u)" -eq 0 ]]; then
                    ufw allow 9091/udp >/dev/null 2>&1 && ok "Firewall: opened UDP 9091 (ufw)"
                else
                    sudo ufw allow 9091/udp >/dev/null 2>&1 && ok "Firewall: opened UDP 9091 (ufw)" || warn "Could not open UDP 9091 — try: sudo ufw allow 9091/udp"
                fi
            fi
        fi
    fi
    # firewalld
    if command -v firewall-cmd &>/dev/null; then
        if firewall-cmd --state 2>/dev/null | grep -q "running"; then
            if [[ "$(id -u)" -eq 0 ]]; then
                firewall-cmd --permanent --add-port=9091/udp >/dev/null 2>&1 && firewall-cmd --reload >/dev/null 2>&1 && ok "Firewall: opened UDP 9091 (firewalld)"
            else
                sudo firewall-cmd --permanent --add-port=9091/udp >/dev/null 2>&1 && sudo firewall-cmd --reload >/dev/null 2>&1 && ok "Firewall: opened UDP 9091 (firewalld)" || true
            fi
        fi
    fi
    # iptables (fallback — only if neither ufw nor firewalld)
    if ! command -v ufw &>/dev/null && ! command -v firewall-cmd &>/dev/null; then
        if command -v iptables &>/dev/null; then
            if [[ "$(id -u)" -eq 0 ]]; then
                iptables -C INPUT -p udp --dport 9091 -j ACCEPT 2>/dev/null || iptables -I INPUT -p udp --dport 9091 -j ACCEPT 2>/dev/null && ok "Firewall: opened UDP 9091 (iptables)"
            else
                sudo iptables -C INPUT -p udp --dport 9091 -j ACCEPT 2>/dev/null || sudo iptables -I INPUT -p udp --dport 9091 -j ACCEPT 2>/dev/null && ok "Firewall: opened UDP 9091 (iptables)" || true
            fi
        fi
    fi
fi
# macOS doesn't typically block outbound UDP, and the application firewall
# doesn't filter by port. No action needed.

ok "Pre-flight checks passed"

# ── Hardware-based model recommendation ────────────────
# Auto-detect RAM and recommend optimal model if user didn't specify --tiny or --model
if [[ -z "${MODEL_PATH}" && -z "${SKIP_MODEL}" ]]; then
    TOTAL_RAM_MB=0
    if [[ "$(uname -s)" == "Darwin" ]]; then
        RAM_BYTES=$(sysctl -n hw.memsize 2>/dev/null || echo 0)
        TOTAL_RAM_MB=$((RAM_BYTES / 1024 / 1024))
    elif [[ -f /proc/meminfo ]]; then
        RAM_KB=$(grep MemTotal /proc/meminfo 2>/dev/null | awk '{print $2}' || echo 0)
        TOTAL_RAM_MB=$((RAM_KB / 1024))
    fi

    if [[ "${TOTAL_RAM_MB}" -gt 0 ]]; then
        TOTAL_RAM_GB=$((TOTAL_RAM_MB / 1024))
        if [[ "${TOTAL_RAM_MB}" -lt 4096 ]]; then
            warn "Low RAM detected (${TOTAL_RAM_GB} GB). Switching to relay-only mode (--skip-model)."
            warn "Your node will relay inference requests to other workers."
            SKIP_MODEL=1
        elif [[ "${TOTAL_RAM_MB}" -lt 8192 ]]; then
            # Check if user already explicitly chose --tiny (MODEL_NAME would be TinyLlama)
            if [[ "${MODEL_NAME}" != "TinyLlama 1.1B" ]]; then
                warn "Detected ${TOTAL_RAM_GB} GB RAM — auto-selecting TinyLlama 1.1B (638 MB) for best performance."
                warn "To override: re-run without auto-detection by passing --model /path/to/model.gguf"
                MODEL_URL="https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf"
                MODEL_NAME="TinyLlama 1.1B"
                MODEL_SIZE="638 MB"
            fi
        else
            ok "Hardware: ${TOTAL_RAM_GB} GB RAM — Llama 2 7B Chat recommended"
        fi
    fi
fi
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
        git pull origin main --quiet 2>/dev/null || warn "Could not update source (offline?). Building with existing code."
    else
        echo -e "  ${D}Cloning repository...${N}"
        if ! git clone --depth 1 "${REPO_URL}" "${ARC_DIR}/src/arc-chain" 2>&1 | tail -3; then
            fail "Failed to clone repository. Check your internet connection and that git is installed.\n    Repo: ${REPO_URL}"
        fi
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
    if ! curl -L --progress-bar --fail "${MODEL_URL}" -o "${MODEL_PATH}.tmp" 2>&1; then
        rm -f "${MODEL_PATH}.tmp"
        fail "Model download failed. Check your internet connection and try again.\n    You can also use --tiny for a smaller model, or --skip-model to skip."
    fi
    # Sanity check: model file should be at least 100 MB
    FILE_SIZE=$(wc -c < "${MODEL_PATH}.tmp" 2>/dev/null || echo 0)
    if [[ "${FILE_SIZE}" -lt 100000000 ]]; then
        rm -f "${MODEL_PATH}.tmp"
        fail "Downloaded model is too small (${FILE_SIZE} bytes) — likely a partial download.\n    Check disk space and internet connection, then try again."
    fi
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

CONNECTED=0
for i in $(seq 1 30); do
    if curl -sf http://localhost:9090/health >/dev/null 2>&1; then
        PEERS="0"
        HEALTH_JSON=$(curl -sf http://localhost:9090/health 2>/dev/null || echo "{}")
        if command -v python3 &>/dev/null; then
            PEERS=$(echo "${HEALTH_JSON}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('peers', json.load(open('/dev/stdin') if False else sys.stdin).get('peer_count',0)))" 2>/dev/null || echo "0")
        else
            # Fallback: extract peers with grep (no python needed)
            PEERS=$(echo "${HEALTH_JSON}" | grep -o '"peer[s_]*count*":[0-9]*' | head -1 | grep -o '[0-9]*$' || echo "0")
        fi
        if [[ "${PEERS}" -lt 3 ]] 2>/dev/null; then
            warn "Connected to only ${PEERS} peers (expected 8). Possible firewall issue."
            echo -e "  ${D}ARC uses UDP port 9091 for P2P. If you have a firewall:${N}"
            echo -e "  ${D}  Linux:   sudo ufw allow 9091/udp${N}"
            echo -e "  ${D}  Router:  forward UDP port 9091 to this machine${N}"
            echo -e "  ${D}  Cloud:   allow inbound UDP 9091 in your security group${N}"
            echo -e "  ${D}Your node still works with fewer peers, just slower to receive jobs.${N}"
        else
            ok "Connected to ${PEERS} peers"
        fi
        CONNECTED=1
        break
    fi
    sleep 1
done
if [[ "${CONNECTED}" -eq 0 ]]; then
    warn "Node didn't respond after 30s. Check logs: tail -f ${ARC_DIR}/node.log"
fi

# Faucet claim
ADDR=""
EARNINGS_JSON=$(curl -sf http://localhost:9090/worker/earnings 2>/dev/null || echo "{}")
if command -v python3 &>/dev/null; then
    ADDR=$(echo "${EARNINGS_JSON}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('address',''))" 2>/dev/null || echo "")
else
    ADDR=$(echo "${EARNINGS_JSON}" | grep -o '"address":"[^"]*"' | head -1 | sed 's/"address":"//;s/"//' || echo "")
fi
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
if [[ "${OS}" == "Darwin" ]]; then
echo -e "    Stop:     launchctl bootout gui/\$(id -u)/com.arc.inference"
echo -e "    Restart:  launchctl kickstart -k gui/\$(id -u)/com.arc.inference"
elif [[ "${OS}" == "Linux" ]] && command -v systemctl &>/dev/null; then
echo -e "    Stop:     systemctl --user stop arc-inference"
echo -e "    Restart:  systemctl --user restart arc-inference"
else
echo -e "    Stop:     kill \$(cat ${ARC_DIR}/node.pid)"
echo -e "    Restart:  bash $0"
fi
echo ""
echo -e "  ${C}Share with friends${N}:"
echo -e "    curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-community.sh | bash"
echo ""
