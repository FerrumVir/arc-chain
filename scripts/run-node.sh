#!/bin/bash
set -euo pipefail

# ARC Chain Node Launcher
# Requirements: Rust toolchain, 500K+ staked ARC

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

echo "╔═══════════════════════════════════════╗"
echo "║   ARC Chain — Node Setup              ║"
echo "╚═══════════════════════════════════════╝"
echo ""

# Parse args
RPC_PORT="${RPC_PORT:-9090}"
P2P_PORT="${P2P_PORT:-9091}"  
STAKE="${STAKE:-5000000}"
DATA_DIR="${DATA_DIR:-$HOME/.arc-chain}"
PEERS="${PEERS:-}"
BUILD_MODE="${BUILD_MODE:-release}"

echo "Configuration:"
echo "  RPC Port:    $RPC_PORT"
echo "  P2P Port:    $P2P_PORT"
echo "  Stake:       $STAKE ARC"
echo "  Data Dir:    $DATA_DIR"
echo "  Build Mode:  $BUILD_MODE"
echo ""

# Minimum stake check
MIN_STAKE=500000
if [ "$STAKE" -lt "$MIN_STAKE" ]; then
    echo "ERROR: Minimum stake is ${MIN_STAKE} ARC (Spark tier)"
    echo "Staking tiers:"
    echo "  Spark:  500,000 ARC  (vote only)"
    echo "  Arc:  5,000,000 ARC  (produce blocks)"
    echo "  Core: 50,000,000 ARC (governance)"
    exit 1
fi

# Determine tier
if [ "$STAKE" -ge 50000000 ]; then
    TIER="Core"
elif [ "$STAKE" -ge 5000000 ]; then
    TIER="Arc"
else
    TIER="Spark"
fi
echo "  Tier:        $TIER"
echo ""

# Create data directory
mkdir -p "$DATA_DIR"

# Build
echo "Building arc-node ($BUILD_MODE)..."
cd "$PROJECT_DIR"
if [ "$BUILD_MODE" = "release" ]; then
    cargo build --release -p arc-node 2>&1 | tail -3
    BIN="$PROJECT_DIR/target/release/arc-node"
else
    cargo build -p arc-node 2>&1 | tail -3
    BIN="$PROJECT_DIR/target/debug/arc-node"
fi

echo ""
echo "Starting ARC Chain node..."
echo ""

# Build args
ARGS="--rpc 0.0.0.0:$RPC_PORT --p2p-port $P2P_PORT --stake $STAKE --data-dir $DATA_DIR"
if [ -n "$PEERS" ]; then
    ARGS="$ARGS --peers $PEERS"
fi

# Run
RUST_LOG=arc=info exec "$BIN" $ARGS
