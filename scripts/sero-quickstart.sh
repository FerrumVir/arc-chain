#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Sero One-Command Quickstart
#
# Downloads pre-built binary + config files, starts node as observer.
# Zero compile, zero configuration. Just bring a GGUF model.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/sero-quickstart.sh | bash -s -- /path/to/model.gguf
#
# Or download and run:
#   curl -sSL -o quickstart.sh https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/sero-quickstart.sh
#   bash quickstart.sh /path/to/model.gguf
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="FerrumVir/arc-chain"
ARC_DIR="${HOME}/.arc-quickstart"

# Help flag
if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    cat <<HELP
ARC Chain Sero Quickstart

USAGE:
  sero-quickstart.sh [MODEL_PATH]

ARGUMENTS:
  MODEL_PATH    Path to a GGUF model file (Llama, Mistral, Phi, Gemma, Qwen).
                If omitted, downloads TinyLlama 1.1B (638 MB) automatically.

EXAMPLES:
  # Use a model you already have:
  ./sero-quickstart.sh ~/models/llama-3-8b.Q4_K_M.gguf

  # Auto-download default model:
  ./sero-quickstart.sh

  # Run via curl pipe:
  curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/sero-quickstart.sh \\
    | bash -s -- ~/models/your-model.gguf

OUTPUT:
  Node listens on:
    http://localhost:9944        — RPC + inference endpoint
    http://localhost:9944/health — health status

  Test inference:
    curl -X POST http://localhost:9944/inference/run \\
      -H 'Content-Type: application/json' \\
      -d '{"input":"[INST] Hello [/INST]","max_tokens":32}'

  Live testnet dashboard:
    http://140.82.16.112:3200

HELP
    exit 0
fi

MODEL_PATH="${1:-}"

# Colors
BOLD='\033[1m' GREEN='\033[32m' CYAN='\033[36m' YELLOW='\033[33m' RED='\033[31m' RESET='\033[0m'
info()  { printf "${CYAN}[INFO]${RESET}  %s\n" "$*"; }
ok()    { printf "${GREEN}[  OK]${RESET}  %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${RESET}  %s\n" "$*"; }
fail()  { printf "${RED}[FAIL]${RESET}  %s\n" "$*" >&2; exit 1; }

cat <<'BANNER'
  ╔══════════════════════════════════════════╗
  ║     ARC Chain — Sero Quickstart          ║
  ║     Verifiable AI inference on chain     ║
  ╚══════════════════════════════════════════╝
BANNER
echo ""

# ── 1. Detect platform ──────────────────────────────────────────────────────
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
    *)
        fail "Unsupported OS: $OS"
        ;;
esac
ok "Platform: $OS $ARCH → $ASSET"

# ── 2. Validate model ───────────────────────────────────────────────────────
if [ -z "$MODEL_PATH" ]; then
    warn "No model path provided. Will download TinyLlama 1.1B (638 MB)"
    MODEL_PATH="${ARC_DIR}/tinyllama-1.1b-chat.gguf"
elif [ ! -f "$MODEL_PATH" ]; then
    fail "Model file not found: $MODEL_PATH"
fi

# ── 3. Get latest version from GitHub ───────────────────────────────────────
info "Checking latest version..."
VERSION=$(curl -sf "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep -m1 '"tag_name"' \
    | sed -E 's/.*"v?([0-9]+\.[0-9]+\.[0-9]+)".*/\1/')
[ -z "$VERSION" ] && fail "Could not fetch latest version"
ok "Latest version: v$VERSION"

# ── 4. Download files ───────────────────────────────────────────────────────
mkdir -p "$ARC_DIR"
cd "$ARC_DIR"

if [ ! -f arc-node ] || ! ./arc-node --version 2>/dev/null | grep -q "$VERSION"; then
    info "Downloading $ASSET ($VERSION)..."
    curl -fL --progress-bar -o arc-node.tmp \
        "https://github.com/${REPO}/releases/download/v${VERSION}/${ASSET}"
    chmod +x arc-node.tmp
    mv arc-node.tmp arc-node
    ok "Binary installed: $ARC_DIR/arc-node"
else
    ok "Binary already at v$VERSION"
fi

info "Downloading testnet-seeds.txt..."
curl -fsL -o testnet-seeds.txt \
    "https://raw.githubusercontent.com/${REPO}/main/testnet-seeds.txt"
ok "Seeds: 8 testnet nodes"

info "Downloading genesis.toml..."
curl -fsL -o genesis.toml \
    "https://raw.githubusercontent.com/${REPO}/main/genesis.toml"
ok "Genesis: arc-testnet"

# Download default model if needed
if [ ! -f "$MODEL_PATH" ]; then
    info "Downloading TinyLlama 1.1B (638 MB, takes ~30s)..."
    curl -fL --progress-bar -o "$MODEL_PATH" \
        "https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf"
    ok "Model downloaded: $MODEL_PATH"
else
    ok "Using model: $MODEL_PATH ($(du -h "$MODEL_PATH" | cut -f1))"
fi

# ── 5. Generate unique validator seed ───────────────────────────────────────
SEED_FILE="$ARC_DIR/validator-seed.txt"
if [ ! -f "$SEED_FILE" ]; then
    echo "sero-$(openssl rand -hex 4)" > "$SEED_FILE"
fi
VALIDATOR_SEED="$(cat "$SEED_FILE")"
ok "Validator seed: $VALIDATOR_SEED"

# ── 6. Start the node ───────────────────────────────────────────────────────
echo ""
printf "${BOLD}${GREEN}════════════════════════════════════════════════════════════════${RESET}\n"
printf "${BOLD}${GREEN}  Starting ARC node as inference observer${RESET}\n"
printf "${BOLD}${GREEN}════════════════════════════════════════════════════════════════${RESET}\n"
echo ""
echo "  ${BOLD}Local RPC:${RESET}        http://localhost:9944"
echo "  ${BOLD}Health:${RESET}           curl http://localhost:9944/health"
echo "  ${BOLD}Run inference:${RESET}    curl -X POST http://localhost:9944/inference/run \\"
echo "                       -H 'Content-Type: application/json' \\"
echo "                       -d '{\"input\":\"[INST] Hello [/INST]\",\"max_tokens\":32}'"
echo "  ${BOLD}Live dashboard:${RESET}   http://140.82.16.112:3200"
echo ""
echo "  ${BOLD}Press Ctrl+C to stop the node${RESET}"
echo ""

exec ./arc-node \
    --rpc 0.0.0.0:9944 \
    --p2p-port 9945 \
    --seeds-file testnet-seeds.txt \
    --genesis genesis.toml \
    --validator-seed "$VALIDATOR_SEED" \
    --stake 0 \
    --min-stake 0 \
    --eth-rpc-port 0 \
    --data-dir "${ARC_DIR}/data" \
    --model "$MODEL_PATH"
