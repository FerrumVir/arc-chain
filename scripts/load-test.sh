#!/bin/bash
set -e
# ─── ARC Chain Load Test ─────────────────────────────────────────────
# Sends N transfers through the live testnet and measures TPS.
# Uses multiple sender addresses to avoid per-sender rate limits.
# Usage: ./scripts/load-test.sh [count] [node_ip]
# ─────────────────────────────────────────────────────────────────────

COUNT="${1:-1000}"
NODE="${2:-140.82.16.112}"
RPC="http://$NODE:9090"
SENDERS=20  # Spread across 20 senders to avoid rate limit

GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${CYAN}"
echo "  ╔══════════════════════════════════════╗"
echo "  ║      ARC Chain — Load Test           ║"
echo "  ╚══════════════════════════════════════╝"
echo -e "${NC}"

# Check node
echo -e "${YELLOW}[1/4] Checking node...${NC}"
HEALTH=$(curl -sf "$RPC/health" 2>/dev/null) || { echo "Node unreachable"; exit 1; }
echo "$HEALTH" | python3 -c "import sys,json; d=json.load(sys.stdin); print(f'  Round: {d[\"dag_round\"]}  Peers: {d[\"peers\"]}  Validators: {d[\"validators\"]}')"

# Fund senders
echo -e "${YELLOW}[2/4] Funding $SENDERS test senders...${NC}"
for ((s=0; s<SENDERS; s++)); do
  ADDR=$(printf 'aa%062x' $s)
  curl -sf -X POST "$RPC/faucet/claim" \
    -H 'Content-Type: application/json' \
    -d "{\"address\":\"$ADDR\"}" >/dev/null 2>&1 &
done
wait
echo "  $SENDERS senders funded"

# Send transfers
echo -e "${YELLOW}[3/4] Sending $COUNT transfers ($SENDERS senders × $((COUNT/SENDERS)) each)...${NC}"
TMPDIR=$(mktemp -d)
START=$(python3 -c "import time; print(time.time())")

BATCH=50
for ((i=0; i<COUNT; i+=BATCH)); do
  REMAINING=$((COUNT - i))
  THIS=$((REMAINING < BATCH ? REMAINING : BATCH))

  for ((j=0; j<THIS; j++)); do
    IDX=$((i + j))
    S=$((IDX % SENDERS))
    SENDER=$(printf 'aa%062x' $S)
    RECIP=$(printf 'bb%062x' $IDX)
    NONCE=$((IDX / SENDERS))
    (curl -sf -X POST "$RPC/tx/submit" \
      -H 'Content-Type: application/json' \
      -d "{\"from\":\"$SENDER\",\"to\":\"$RECIP\",\"amount\":1,\"nonce\":$NONCE}" \
      >/dev/null 2>&1 && echo 1 >> "$TMPDIR/ok" || echo 1 >> "$TMPDIR/fail") &
  done
  wait

  DONE=$((i + THIS))
  OK=$(wc -l < "$TMPDIR/ok" 2>/dev/null || echo 0)
  FAIL=$(wc -l < "$TMPDIR/fail" 2>/dev/null || echo 0)
  echo -ne "  [$((DONE*100/COUNT))%] $DONE/$COUNT (${OK} ok, ${FAIL} fail)\r"
done
echo ""

END=$(python3 -c "import time; print(time.time())")
OK=$(wc -l < "$TMPDIR/ok" 2>/dev/null | tr -d ' ' || echo 0)
FAIL=$(wc -l < "$TMPDIR/fail" 2>/dev/null | tr -d ' ' || echo 0)
ELAPSED=$(python3 -c "print(f'{$END - $START:.2f}')")
TPS=$(python3 -c "ok=$OK; t=$END-$START; print(f'{ok/t:.0f}' if t>0 else '0')")
rm -rf "$TMPDIR"

# Results
echo -e "\n${YELLOW}[4/4] Results${NC}\n"
HEALTH2=$(curl -sf "$RPC/health" 2>/dev/null)
ROUND2=$(echo "$HEALTH2" | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_round'])" 2>/dev/null)
COMMITTED=$(echo "$HEALTH2" | python3 -c "import sys,json; print(json.load(sys.stdin)['dag_committed'])" 2>/dev/null)

echo -e "${GREEN}  ┌─────────────────────────────────────┐"
echo -e "  │  Sent:         $OK / $COUNT"
echo -e "  │  Failed:       $FAIL"
echo -e "  │  Time:         ${ELAPSED}s"
echo -e "  │  Submission:   $TPS tx/sec"
echo -e "  │  DAG Round:    → $ROUND2"
echo -e "  │  Committed:    $COMMITTED blocks"
echo -e "  └─────────────────────────────────────┘${NC}"
