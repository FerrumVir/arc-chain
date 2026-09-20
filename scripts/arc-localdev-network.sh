#!/usr/bin/env bash
# Start (or stop) a disposable local network for local-development UI work.
#
# The multi-node fixture tears its network down when it finishes, which is right
# for a gate but useless for driving a browser against it. This starts the same
# shape of network and leaves it running, then prints the RPC URL the
# local-development Playwright project expects in ARC_LOCALDEV_RPC.
#
# It is disposable and loopback-only, and it is NOT a production or recovered
# network. Start order is staggered because simultaneous mutual dialling between
# arc-node processes deadlocks until timeout (packet defect D1), and node N must
# start before the chain advances or it can never join (D5).
#
# usage: arc-localdev-network.sh [--nodes N] [--binary PATH]
#        arc-localdev-network.sh --stop
set -uo pipefail

NODES=${NODES:-3}
BINARY=${BINARY:-target/debug/arc-node}
BASE_RPC=9990; BASE_P2P=9190
STAKE=6666667
FAUCET_POOL="2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"
STATE_DIR="/tmp/arc-localdev-network"

if [[ "${1:-}" == "--stop" ]]; then
  if [[ -f "$STATE_DIR/pids" ]]; then
    while read -r pid; do kill "$pid" 2>/dev/null; done < "$STATE_DIR/pids"
    sleep 2
    while read -r pid; do kill -9 "$pid" 2>/dev/null; done < "$STATE_DIR/pids"
    rm -f "$STATE_DIR/pids"
    echo "stopped"
  else
    echo "no recorded network to stop"
  fi
  exit 0
fi

while [[ $# -gt 0 ]]; do
  case $1 in
    --nodes) NODES=$2; shift 2 ;;
    --binary) BINARY=$2; shift 2 ;;
    *) echo "unknown arg $1"; exit 2 ;;
  esac
done

rm -rf "$STATE_DIR"; mkdir -p "$STATE_DIR"
for i in $(seq 0 $((NODES-1))); do
  for port in $((BASE_RPC+i)) $((BASE_P2P+i)); do
    if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 || lsof -nP -iUDP:"$port" >/dev/null 2>&1; then
      echo "ERROR: port $port is in use; refusing to start"; exit 1
    fi
  done
done

echo "deriving validator addresses..."
ADDRS=()
for i in $(seq 0 $((NODES-1))); do
  d="$STATE_DIR/probe-$i"; mkdir -p "$d"
  "$BINARY" --rpc "127.0.0.1:$((BASE_RPC+i))" --p2p-port "$((BASE_P2P+i))" \
            --data-dir "$d" --insecure-dev-validator-seed \
            --validator-seed "localdev-$i" --stake "$STAKE" \
            > "$STATE_DIR/probe-$i.log" 2>&1 &
  probe=$!
  a=""
  for _ in $(seq 1 30); do
    a=$(sed 's/\x1b\[[0-9;]*m//g' "$STATE_DIR/probe-$i.log" | grep -m1 "Validator  *:" | sed -E 's/.*: *0x?([0-9a-f]{64}).*/\1/')
    [[ -n "$a" ]] && break; sleep 1
  done
  kill "$probe" 2>/dev/null; wait "$probe" 2>/dev/null
  [[ -n "$a" ]] || { echo "could not derive address for node $i"; exit 1; }
  ADDRS+=("$a")
done

GEN="$STATE_DIR/genesis.toml"
{
  echo '[chain]'; echo 'name = "arc-localdev"'; echo 'chain_id = "0x415243"'
  echo 'validator_set_complete = false'; echo ''
  echo '[[accounts]]'; echo "address = \"$FAUCET_POOL\""; echo 'balance = 1_000_000_000_000'; echo ''
  for a in "${ADDRS[@]}"; do
    echo '[[accounts]]'; echo "address = \"$a\""; echo 'balance = 1_000_000_000_000'; echo ''
  done
  for a in "${ADDRS[@]}"; do
    echo '[[validators]]'; echo "address = \"$a\""; echo "stake = $STAKE"; echo ''
  done
} > "$GEN"

: > "$STATE_DIR/pids"
for i in $(seq 0 $((NODES-1))); do
  d="$STATE_DIR/node-$i"; rm -rf "$d"; mkdir -p "$d"
  PEER_ARGS=()
  if [[ $i -gt 0 ]]; then
    plist=""
    for j in $(seq 0 $((i-1))); do plist="${plist}127.0.0.1:$((BASE_P2P+j)),"; done
    PEER_ARGS=(--peers "${plist%,}")
  fi
  "$BINARY" --rpc "127.0.0.1:$((BASE_RPC+i))" --p2p-port "$((BASE_P2P+i))" \
            --data-dir "$d" --genesis "$GEN" \
            ${PEER_ARGS[@]+"${PEER_ARGS[@]}"} \
            --insecure-dev-validator-seed --validator-seed "localdev-$i" \
            --stake "$STAKE" > "$STATE_DIR/node-$i.log" 2>&1 &
  echo $! >> "$STATE_DIR/pids"
  echo "  node $i on 127.0.0.1:$((BASE_RPC+i))"
  for _ in $(seq 1 60); do
    curl -s --max-time 3 "http://127.0.0.1:$((BASE_RPC+i))/health" >/dev/null 2>&1 && break
    sleep 1
  done
done

echo ""
echo "waiting for the chain to produce blocks..."
for _ in $(seq 1 40); do
  h=$(curl -s --max-time 3 "http://127.0.0.1:$BASE_RPC/health" 2>/dev/null \
      | python3 -c 'import sys,json;print(json.load(sys.stdin).get("height",0))' 2>/dev/null || echo 0)
  [[ "${h:-0}" -gt 0 ]] && break
  sleep 3
done
echo "  height=${h:-0}"
# Put at least one real transaction on the chain. An all-empty chain is still a
# working chain, but "transaction details" is part of what the local-development
# explorer view is supposed to demonstrate, and the faucet is the only signed
# transaction this script can produce without holding a user key.
DEV_RECIPIENT="1111111111111111111111111111111111111111111111111111111111111111"
echo "submitting one faucet transaction so a block carries transaction detail..."
claim=$(curl -s --max-time 10 -X POST "http://127.0.0.1:$BASE_RPC/faucet/claim" \
        -H 'content-type: application/json' \
        -d "{\"address\":\"$DEV_RECIPIENT\"}" 2>/dev/null || true)
tx=$(printf '%s' "$claim" | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("tx_hash",""))
except Exception: print("")' 2>/dev/null || echo "")
if [[ -n "$tx" ]]; then
  for _ in $(seq 1 30); do
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 \
           "http://127.0.0.1:$BASE_RPC/tx/$tx" 2>/dev/null || echo 000)
    [[ "$code" == "200" ]] && break
    sleep 2
  done
  echo "  faucet tx $tx -> /tx lookup HTTP $code"
else
  echo "  WARNING: no faucet tx was created; response: ${claim:0:200}"
fi

echo ""
echo "ARC_LOCALDEV_RPC=http://127.0.0.1:$BASE_RPC"
echo "stop with: $0 --stop"
