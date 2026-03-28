#!/bin/bash
set -e
# ─── ARC Chain Load Test ─────────────────────────────────────────────
# Sends N signed transfers through the live testnet and measures TPS.
# Usage: ./scripts/load-test.sh [count] [node_ip]
#   count: number of transactions (default: 1000)
#   node_ip: target RPC node (default: 140.82.16.112)
# ─────────────────────────────────────────────────────────────────────

COUNT="${1:-1000}"
NODE="${2:-140.82.16.112}"
RPC="http://$NODE:9090"

GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${CYAN}"
echo "  ╔══════════════════════════════════════╗"
echo "  ║      ARC Chain — Load Test           ║"
echo "  ╚══════════════════════════════════════╝"
echo -e "${NC}"

# Check node is alive
echo -e "${YELLOW}[1/4] Checking node health...${NC}"
HEALTH=$(curl -sf "$RPC/health" 2>/dev/null) || { echo "Node $RPC unreachable"; exit 1; }
ROUND=$(echo "$HEALTH" | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_round'])" 2>/dev/null)
PEERS=$(echo "$HEALTH" | python3 -c "import sys,json; print(json.load(sys.stdin)['peers'])" 2>/dev/null)
echo "  Node: $RPC"
echo "  DAG Round: $ROUND"
echo "  Peers: $PEERS"
echo ""

# Fund the test sender via faucet
echo -e "${YELLOW}[2/4] Funding test account...${NC}"
SENDER="1111111111111111111111111111111111111111111111111111111111111111"
FAUCET=$(curl -sf -X POST "$RPC/faucet/claim" \
  -H 'Content-Type: application/json' \
  -d "{\"address\":\"$SENDER\"}" 2>/dev/null)
echo "  Sender: $SENDER"
echo "  Faucet: $(echo "$FAUCET" | python3 -c "import sys,json; d=json.load(sys.stdin); print(f'{d.get(\"amount\",0)} ARC')" 2>/dev/null || echo 'rate limited (already funded)')"
echo ""

# Generate recipient addresses
echo -e "${YELLOW}[3/4] Sending $COUNT transfers...${NC}"
START_TIME=$(python3 -c "import time; print(time.time())")
SUCCESS=0
FAIL=0

# Send in parallel batches of 50
BATCH=50
for ((i=0; i<COUNT; i+=BATCH)); do
  REMAINING=$((COUNT - i))
  THIS_BATCH=$((REMAINING < BATCH ? REMAINING : BATCH))

  for ((j=0; j<THIS_BATCH; j++)); do
    IDX=$((i + j))
    # Generate unique recipient from index
    RECIP=$(printf '%064x' $((IDX + 10000)))
    curl -sf -X POST "$RPC/tx/submit" \
      -H 'Content-Type: application/json' \
      -d "{\"from\":\"$SENDER\",\"to\":\"$RECIP\",\"amount\":1,\"nonce\":$IDX}" \
      >/dev/null 2>&1 && SUCCESS=$((SUCCESS+1)) || FAIL=$((FAIL+1)) &
  done
  wait

  # Progress
  DONE=$((i + THIS_BATCH))
  PCT=$((DONE * 100 / COUNT))
  echo -ne "  [$PCT%] $DONE/$COUNT sent ($SUCCESS ok, $FAIL fail)\r"
done
echo ""

END_TIME=$(python3 -c "import time; print(time.time())")
ELAPSED=$(python3 -c "print(f'{$END_TIME - $START_TIME:.2f}')")
TPS=$(python3 -c "print(f'{$SUCCESS / ($END_TIME - $START_TIME):.0f}')")

echo ""
echo -e "${YELLOW}[4/4] Results${NC}"
echo ""

# Get new round
HEALTH2=$(curl -sf "$RPC/health" 2>/dev/null)
ROUND2=$(echo "$HEALTH2" | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_round'])" 2>/dev/null)
COMMITTED2=$(echo "$HEALTH2" | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_committed'])" 2>/dev/null)

echo -e "${GREEN}  ┌─────────────────────────────────────┐"
echo -e "  │  Transactions: $SUCCESS / $COUNT"
echo -e "  │  Failed:       $FAIL"
echo -e "  │  Time:         ${ELAPSED}s"
echo -e "  │  TPS:          $TPS tx/sec"
echo -e "  │  DAG Round:    $ROUND → $ROUND2"
echo -e "  │  Committed:    $COMMITTED2 blocks"
echo -e "  └─────────────────────────────────────┘${NC}"
echo ""
echo "  Run again: ./scripts/load-test.sh $COUNT $NODE"
