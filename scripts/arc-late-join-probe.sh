#!/usr/bin/env bash
# Can a node join a chain that is already running? (checklist C7/C8, defect D5)
#
# Start N-1 validators, let the chain advance well past the point where a peer
# block would be refused as "too far ahead", THEN start the last one and watch
# whether it reaches the others' height. Before authenticated history transfer
# the answer was no, permanently: the late node rejected every live block and
# never produced another.
#
# usage: arc-late-join-probe.sh [--nodes N] [--lead SECONDS] [--observe SECONDS]
set -uo pipefail

BINARY=${BINARY:-target/debug/arc-node}
NODES=${NODES:-4}
LEAD=${LEAD:-25}
OBSERVE=${OBSERVE:-60}
STAKE=6666667
BASE_RPC=9960; BASE_P2P=9160
WORK="/tmp/arc-latejoin-$(date +%Y%m%d-%H%M%S)"
PIDS=()

while [[ $# -gt 0 ]]; do
  case $1 in
    --nodes) NODES=$2; shift 2 ;;
    --lead) LEAD=$2; shift 2 ;;
    --observe) OBSERVE=$2; shift 2 ;;
    --binary) BINARY=$2; shift 2 ;;
    *) echo "unknown arg $1"; exit 2 ;;
  esac
done

cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
  sleep 1
  for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
}
trap cleanup EXIT INT TERM

health() { curl -s --max-time 4 "http://127.0.0.1:$1/health" 2>/dev/null; }
field()  { printf '%s' "$1" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get(sys.argv[1],""))' "$2" 2>/dev/null; }

derive() { # seed rpc p2p -> address
  local d="$WORK/derive-$1"; mkdir -p "$d"
  "$BINARY" --rpc "127.0.0.1:$2" --p2p-port "$3" --data-dir "$d" \
            --insecure-dev-validator-seed --validator-seed "$1" --stake "$STAKE" \
            > "$WORK/derive-$1.log" 2>&1 &
  local pid=$! a=""
  for _ in $(seq 1 30); do
    a=$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/derive-$1.log" | grep -m1 "Validator  *:" | sed -E 's/.*: *0x?([0-9a-f]{64}).*/\1/')
    [[ -n "$a" ]] && break; sleep 1
  done
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  printf '%s' "$a"
}

start() { # index
  # Separate declarations on purpose: `local i=$1 d="$WORK/node-$i"` expands
  # $i from the CALLER's scope, which silently pointed one node at another
  # node's data directory and made it fail on the lock.
  local i="$1"
  local d="$WORK/node-$i"
  local peers=""
  mkdir -p "$d"
  # Dial only LOWER-numbered peers. Simultaneous mutual dialling between two
  # arc-node processes deadlocks until timeout (defect D1), so every fixture
  # here uses a one-directional peer list; the late node therefore dials the
  # running ones, which is exactly the case under test.
  for j in $(seq 0 $((i-1))); do
    peers="${peers}127.0.0.1:$((BASE_P2P+j)),"
  done
  "$BINARY" --rpc "127.0.0.1:$((BASE_RPC+i))" --p2p-port "$((BASE_P2P+i))" --data-dir "$d" \
            --genesis "$WORK/genesis.toml" --peers "${peers%,}" \
            --insecure-dev-validator-seed --validator-seed "latejoin-$i" \
            --stake "$STAKE" > "$WORK/node-$i.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 60); do health $((BASE_RPC+i)) >/dev/null 2>&1 && break; sleep 1; done
}

mkdir -p "$WORK"
echo "=== late-join probe ==="
echo "work dir: $WORK   nodes: $NODES   lead: ${LEAD}s   observe: ${OBSERVE}s"

ADDRS=()
for i in $(seq 0 $((NODES-1))); do
  ADDRS+=("$(derive "latejoin-$i" $((BASE_RPC+i)) $((BASE_P2P+i)))")
  [[ -n "${ADDRS[$i]}" ]] || { echo "could not derive address for node $i"; exit 1; }
done
{
  echo '[chain]'; echo 'name = "arc-latejoin"'; echo 'chain_id = "0x415243"'
  echo 'validator_set_complete = false'
  echo "instance_id = \"late-join-$$-$(date -u +%s)\""; echo ''
  for a in "${ADDRS[@]}"; do echo '[[accounts]]'; echo "address = \"$a\""; echo 'balance = 1_000_000_000_000'; echo ''; done
  for a in "${ADDRS[@]}"; do echo '[[validators]]'; echo "address = \"$a\""; echo "stake = $STAKE"; echo ''; done
} > "$WORK/genesis.toml"

LATE=$((NODES-1))
echo ""
echo "starting nodes 0..$((LATE-1)), holding node $LATE back"
for i in $(seq 0 $((LATE-1))); do start "$i"; echo "  node $i up"; done

echo "letting the chain run for ${LEAD}s before the late node joins..."
sleep "$LEAD"
LEAD_ROUND=$(field "$(health $BASE_RPC)" dag_round)
LEAD_HEIGHT=$(field "$(health $BASE_RPC)" height)
echo "  chain is at dag_round=${LEAD_ROUND:-?} height=${LEAD_HEIGHT:-?} - far past the one-round gossip window"

echo ""
echo "starting the late node $LATE"
start "$LATE"

best=0
for _ in $(seq 1 "$OBSERVE"); do
  h=$(field "$(health $((BASE_RPC+LATE)))" height)
  [[ -n "$h" && "$h" -gt "$best" ]] && best=$h
  [[ "${h:-0}" -gt 0 ]] && break
  sleep 1
done
sleep 5

echo ""
echo "=== RESULT ==="
FINAL=()
for i in $(seq 0 $((NODES-1))); do
  b=$(health $((BASE_RPC+i)))
  FINAL+=("node $i: height=$(field "$b" height) dag_round=$(field "$b" dag_round) peers=$(field "$b" peers)")
done
printf '  %s\n' "${FINAL[@]}"
LATE_HEIGHT=$(field "$(health $((BASE_RPC+LATE)))" height)
echo ""
echo "  late node requested history: $(grep -c 'Requesting bounded DAG history' "$WORK/node-$LATE.log")"
echo "  late node imported history:  $(grep -c 'Joined a running chain' "$WORK/node-$LATE.log")"
echo "  peers served history:        $(cat "$WORK"/node-*.log | grep -c 'Serving bounded DAG history')"
echo "  late node rejected history:  $(grep -c 'Rejected a history response' "$WORK/node-$LATE.log")"
if [[ "${LATE_HEIGHT:-0}" -gt 0 ]]; then
  echo ""
  echo "  VERDICT: the late node JOINED a running chain (height ${LATE_HEIGHT})"
  exit 0
else
  echo ""
  echo "  VERDICT: the late node did NOT join (height ${LATE_HEIGHT:-unreachable})"
  echo "  last refusals:"
  sed 's/\x1b\[[0-9;]*m//g' "$WORK/node-$LATE.log" | grep -oE "Rejected DAG block.{0,60}" | tail -3 | sed 's/^/    /'
  exit 1
fi
