#!/usr/bin/env bash
# Launch 5 ARC Chain validator nodes on different cores for consensus benchmarking.
# Each node gets its own RPC port, P2P port, validator seed, and sender partition.
#
# Usage:
#   ./scripts/run_cluster.sh          # Launch 5 nodes
#   ./scripts/run_cluster.sh stop     # Kill all arc-node processes

set -euo pipefail

BINARY="./target/release/arc-node"
NUM_NODES=5
SENDERS_PER_NODE=10  # 50 senders / 5 nodes

if [[ "${1:-}" == "stop" ]]; then
    echo "Stopping all arc-node processes..."
    pkill -f "arc-node.*--benchmark" || true
    echo "Done."
    exit 0
fi

if [[ ! -f "$BINARY" ]]; then
    echo "Binary not found at $BINARY"
    echo "Run: cargo build --release -p arc-node"
    exit 1
fi

echo "╔═══════════════════════════════════════════╗"
echo "║  ARC Chain — 5-Node Consensus Benchmark   ║"
echo "╚═══════════════════════════════════════════╝"
echo ""

PIDS=()

for i in $(seq 0 $((NUM_NODES - 1))); do
    RPC_PORT=$((9090 + i))
    P2P_PORT=$((9100 + i))
    SEED="arc-benchmark-validator-$i"
    SENDER_START=$((i * SENDERS_PER_NODE))

    # Build --peers list (all other nodes)
    PEERS=""
    for j in $(seq 0 $((NUM_NODES - 1))); do
        if [[ $j -ne $i ]]; then
            [[ -n "$PEERS" ]] && PEERS="$PEERS,"
            PEERS="${PEERS}127.0.0.1:$((9100 + j))"
        fi
    done

    echo "Node $i: RPC=127.0.0.1:$RPC_PORT P2P=:$P2P_PORT seed=$SEED senders=$SENDER_START-$((SENDER_START + SENDERS_PER_NODE - 1))"

    $BINARY \
        --rpc "127.0.0.1:$RPC_PORT" \
        --p2p-port $P2P_PORT \
        --validator-seed "$SEED" \
        --benchmark \
        --bench-sender-start $SENDER_START \
        --bench-sender-count $SENDERS_PER_NODE \
        --bench-sign-threads 2 \
        --bench-rayon-threads 2 \
        --peers "$PEERS" \
        --stake 50000000 \
        2>&1 | sed "s/^/[node-$i] /" &

    PIDS+=($!)
done

echo ""
echo "$NUM_NODES nodes launched."
echo "RPC endpoints: http://127.0.0.1:9090 .. http://127.0.0.1:$((9090 + NUM_NODES - 1))"
echo ""
echo "Press Ctrl+C to stop all nodes, or run: $0 stop"
echo ""

# Wait for all background processes; forward SIGINT to children
trap 'echo "Stopping..."; kill "${PIDS[@]}" 2>/dev/null; wait' INT TERM
wait
