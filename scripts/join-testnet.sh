#!/bin/bash
set -e

# ─── ARC Testnet: One-Command Join ───────────────────────────────────────────
#
# This script builds the node, optionally downloads a model, connects to the
# live testnet, and shows you chain stats + inference speed.
#
# Usage:
#   ./scripts/join-testnet.sh                    # Join testnet (no inference)
#   ./scripts/join-testnet.sh --with-inference   # Join + download model + run inference
#
# What you'll see:
#   - Live chain: block height, TPS, consensus rounds
#   - Peer connections to seed nodes across 6 continents
#   - (with --with-inference) Run deterministic inference and verify output hash
#
# Requirements: Rust nightly, ~2GB disk for build, ~4GB for model
# ─────────────────────────────────────────────────────────────────────────────

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${CYAN}"
echo "  ╔══════════════════════════════════════════╗"
echo "  ║         ARC Chain — Join Testnet         ║"
echo "  ╚══════════════════════════════════════════╝"
echo -e "${NC}"

# ─── Build ───────────────────────────────────────────────────────────────────
echo -e "${YELLOW}[1/4] Building arc-node (release mode)...${NC}"
if [ ! -f target/release/arc-node ]; then
    cargo build --release -p arc-node 2>&1 | tail -3
else
    echo "  Binary exists. Rebuild with: cargo build --release -p arc-node"
fi

# ─── Model (optional) ───────────────────────────────────────────────────────
MODEL_FLAG=""
if [[ "$1" == "--with-inference" ]]; then
    MODEL_PATH="$REPO_ROOT/model.gguf"
    if [ ! -f "$MODEL_PATH" ]; then
        echo -e "${YELLOW}[2/4] Downloading TinyLlama 1.1B (638 MB)...${NC}"
        curl -L -o "$MODEL_PATH" \
            "https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf"
    else
        echo -e "${YELLOW}[2/4] Model already downloaded.${NC}"
    fi
    MODEL_FLAG="--model $MODEL_PATH"
else
    echo -e "${YELLOW}[2/4] Skipping model download (use --with-inference to enable).${NC}"
fi

# ─── Generate unique validator identity ──────────────────────────────────────
SEED="arc-community-$(openssl rand -hex 4)"
echo -e "${YELLOW}[3/4] Validator identity: ${SEED}${NC}"

# ─── Start node ──────────────────────────────────────────────────────────────
echo -e "${YELLOW}[4/4] Starting node and connecting to testnet...${NC}"
echo ""
echo -e "${GREEN}  RPC:       http://localhost:9090${NC}"
echo -e "${GREEN}  Health:    http://localhost:9090/health${NC}"
echo -e "${GREEN}  Stats:     http://localhost:9090/stats${NC}"
echo -e "${GREEN}  Explorer:  cd explorer && npm run dev → http://localhost:3100${NC}"
echo ""

if [[ -n "$MODEL_FLAG" ]]; then
    echo -e "${GREEN}  Inference: curl -X POST http://localhost:9090/inference/run \\${NC}"
    echo -e "${GREEN}    -H 'Content-Type: application/json' \\${NC}"
    echo -e "${GREEN}    -d '{\"input\":\"[INST] What is 2+2? [/INST]\",\"max_tokens\":16}'${NC}"
    echo ""
fi

echo -e "${CYAN}Connecting to ARC testnet seeds across 6 continents...${NC}"
echo "Press Ctrl+C to stop."
echo ""

exec ./target/release/arc-node \
    --rpc 0.0.0.0:9090 \
    --seeds-file testnet-seeds.txt \
    --genesis genesis.toml \
    --validator-seed "$SEED" \
    $MODEL_FLAG
