#!/bin/bash
set -e

# ─── ARC Inference Node: One-Command Join ─────────────────────────────────────
#
# Run AI inference for the ARC network and earn tokens.
# Your GPU computes inference, results are attested on-chain.
#
# Usage:
#   ./scripts/join-inference.sh              # Join with TinyLlama 1.1B
#   ./scripts/join-inference.sh --model PATH # Join with custom GGUF model
#
# What happens:
#   1. Builds the node (first time: 5-15 minutes)
#   2. Downloads TinyLlama 1.1B (638 MB) if no model specified
#   3. Connects to the live testnet (your IP stays private)
#   4. Starts serving inference requests
#   5. Submits attestations on-chain (earns ARC)
#
# Requirements: Rust nightly, ~2GB disk, ~4GB RAM, GPU recommended
# ─────────────────────────────────────────────────────────────────────────────

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${CYAN}"
echo "  ╔══════════════════════════════════════════╗"
echo "  ║     ARC Chain — Inference Node           ║"
echo "  ║     Earn ARC by running AI inference     ║"
echo "  ╚══════════════════════════════════════════╝"
echo -e "${NC}"

# ─── Parse args ────────────────────────────────────────────────────────────
MODEL_PATH=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --model) MODEL_PATH="$2"; shift 2 ;;
        *) shift ;;
    esac
done

# ─── Build ─────────────────────────────────────────────────────────────────
echo -e "${YELLOW}[1/5] Building arc-node...${NC}"
echo "  First build takes 5-15 minutes. You'll see compiler output below."
echo ""
if [ ! -f target/release/arc-node ]; then
    cargo build --release -p arc-node 2>&1
    if [ $? -ne 0 ]; then
        echo -e "${YELLOW}Build failed. Make sure you have Rust nightly: rustup default nightly${NC}"
        exit 1
    fi
    echo -e "${GREEN}  Build complete!${NC}"
else
    echo "  Binary exists. Rebuild with: cargo build --release -p arc-node"
fi

# ─── Model ─────────────────────────────────────────────────────────────────
if [ -z "$MODEL_PATH" ]; then
    MODEL_PATH="$REPO_ROOT/model.gguf"
    if [ ! -f "$MODEL_PATH" ]; then
        echo -e "${YELLOW}[2/5] Downloading TinyLlama 1.1B (638 MB)...${NC}"
        curl -L -o "$MODEL_PATH" \
            "https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf"
    else
        echo -e "${YELLOW}[2/5] Model already downloaded.${NC}"
    fi
else
    echo -e "${YELLOW}[2/5] Using custom model: $MODEL_PATH${NC}"
fi

# ─── Identity ──────────────────────────────────────────────────────────────
SEED="arc-inference-$(openssl rand -hex 4)"
echo -e "${YELLOW}[3/5] Your inference node ID: ${SEED}${NC}"

# ─── Show info ─────────────────────────────────────────────────────────────
echo -e "${YELLOW}[4/5] Connecting to ARC testnet...${NC}"
echo ""
echo -e "${GREEN}  Your node:${NC}"
echo -e "${GREEN}    RPC:        http://localhost:9090${NC}"
echo -e "${GREEN}    Health:     http://localhost:9090/health${NC}"
echo -e "${GREEN}    Inference:  http://localhost:9090/inference/run${NC}"
echo ""
echo -e "${GREEN}  Try inference:${NC}"
echo -e "${GREEN}    curl -X POST http://localhost:9090/inference/run \\${NC}"
echo -e "${GREEN}      -H 'Content-Type: application/json' \\${NC}"
echo -e "${GREEN}      -d '{\"input\":\"[INST] What is 2+2? [/INST]\",\"max_tokens\":32}'${NC}"
echo ""
echo -e "${GREEN}  Dashboard:    http://140.82.16.112:3200${NC}"
echo -e "${GREEN}  Wallet:       http://140.82.16.112:3100${NC}"
echo ""

# ─── Start ─────────────────────────────────────────────────────────────────
echo -e "${YELLOW}[5/5] Starting inference node...${NC}"
echo -e "${CYAN}Your IP is private — it will not be shared with other nodes.${NC}"
echo "Press Ctrl+C to stop."
echo ""

./target/release/arc-node \
    --rpc 0.0.0.0:9090 \
    --seeds-file testnet-seeds.txt \
    --genesis genesis.toml \
    --validator-seed "$SEED" \
    --model "$MODEL_PATH" \
    --stake 5000000

echo ""
echo -e "${YELLOW}Node stopped. To restart: ./scripts/join-inference.sh${NC}"
