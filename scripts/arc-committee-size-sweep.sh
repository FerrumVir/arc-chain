#!/usr/bin/env bash
# Committee-size sweep with a start barrier, for the four-validator stall (D6).
#
# scripts/arc-block-production-probe.sh established that committee size, not the
# genesis source, decides whether an IDLE chain produces blocks: one validator
# proposes only when it has work (consensus.rs:1640-1643), three validators
# propose every round and committed 530 blocks in 45 s. D6 claims four
# validators stall at height 1 while DAG rounds keep advancing. This varies only
# the committee size, from 2 to 5, and measures the same two numbers each time.
#
# START BARRIER (harness-level, protocol untouched):
#   every node must report peers == N-1 BEFORE the observation window opens.
# A node that joins after its peers have started proposing is a different
# experiment (that is D5), and mixing the two is what made D6 ambiguous.
# Nothing here lowers quorum, widens a round window, or trusts a peer snapshot;
# the barrier only decides WHEN measurement starts.
#
# Reports, per committee size:
#   quorum/total stake as the node itself reports them
#   committed height before -> after      (chain progress)
#   dag_round before -> after             (liveness, which is NOT progress)
#
# usage: arc-committee-size-sweep.sh [--sizes "2 3 4 5"] [--observe 45]
set -uo pipefail

BINARY=${BINARY:-target/debug/arc-node}
OBSERVE=${OBSERVE:-45}
BARRIER_TIMEOUT=${BARRIER_TIMEOUT:-60}
SIZES=${SIZES:-"2 3 4 5"}
STAKE=6666667
BASE_RPC=9950; BASE_P2P=9150
WORK="/tmp/arc-committee-sweep-$(date +%Y%m%d-%H%M%S)"
PIDS=()

while [[ $# -gt 0 ]]; do
  case $1 in
    --sizes) SIZES=$2; shift 2 ;;
    --observe) OBSERVE=$2; shift 2 ;;
    --binary) BINARY=$2; shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    --genesis-barrier) GENESIS_BARRIER=1; shift ;;
    *) echo "unknown arg $1"; exit 2 ;;
  esac
done

cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
  sleep 1
  for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
  PIDS=()
}
trap cleanup EXIT INT TERM

health() { curl -s --max-time 4 "http://127.0.0.1:$1/health" 2>/dev/null; }
field()  { printf '%s' "$1" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get(sys.argv[1],""))' "$2" 2>/dev/null; }

# Read the committee the node actually froze, not the one we intended.
committee() { # port -> "validators total_stake quorum"
  curl -s --max-time 4 "http://127.0.0.1:$1/validators" 2>/dev/null | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    print("? ? ?"); raise SystemExit
vs = d.get("validators", [])
total = d.get("total_stake", sum(int(v.get("stake", 0)) for v in vs))
# Same rule as ValidatorSet::new: strictly greater than two thirds.
print(len(vs), total, total * 2 // 3 + 1)
' 2>/dev/null || echo "? ? ?"
}

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

write_genesis() { # file addr...
  local f=$1; shift
  { echo '[chain]'; echo 'name = "arc-committee-sweep"'; echo 'chain_id = "0x415243"'
    echo 'validator_set_complete = false'; echo ''
    for a in "$@"; do echo '[[accounts]]'; echo "address = \"$a\""; echo 'balance = 1_000_000_000_000'; echo ''; done
    for a in "$@"; do echo '[[validators]]'; echo "address = \"$a\""; echo "stake = $STAKE"; echo ''; done
  } > "$f"
}

# ── Self-test: the barrier predicate, with no node and no build ───────────────
# barrier_met FILE N  where FILE holds one peer count per line, one per node.
barrier_met() { # peer-count-file expected-nodes
  awk -v want="$2" '
    { n++; if ($1 + 0 != want - 1) bad = 1 }
    END {
      if (n != want) { print "INCOMPLETE"; exit 1 }
      if (bad)       { print "NOT_MET";    exit 1 }
      print "MET"
    }' "$1"
}

if [[ -n "${SELF_TEST:-}" ]]; then
  t=$(mktemp -d); fail=0
  check() { # name expected file n
    local got; got=$(barrier_met "$3" "$4")
    if [[ "$got" == "$2" ]]; then echo "  ok   $1"; else echo "  FAIL $1: want $2 got $got"; fail=1; fi
  }
  printf '1\n1\n' > "$t/a";      check "2 nodes, both see 1 peer"        MET        "$t/a" 2
  printf '3\n3\n3\n3\n' > "$t/b"; check "4 nodes, all see 3 peers"        MET        "$t/b" 4
  printf '3\n3\n2\n3\n' > "$t/c"; check "4 nodes, one short"              NOT_MET    "$t/c" 4
  printf '3\n3\n3\n' > "$t/d";    check "4 expected, only 3 reported"     INCOMPLETE "$t/d" 4
  printf '0\n0\n' > "$t/e";       check "2 nodes, no peers at all"        NOT_MET    "$t/e" 2
  printf '' > "$t/f";             check "no rows at all"                  INCOMPLETE "$t/f" 4
  printf '4\n3\n3\n3\n' > "$t/g"; check "4 nodes, one sees an extra peer" NOT_MET    "$t/g" 4
  rm -rf "$t"
  [[ $fail -eq 0 ]] && { echo "self-test: PASS"; exit 0; } || { echo "self-test: FAIL"; exit 1; }
fi

mkdir -p "$WORK"
echo "=== committee-size sweep ==="
echo "work dir: $WORK   observe: ${OBSERVE}s   barrier timeout: ${BARRIER_TIMEOUT}s"
echo "no --benchmark, no native activation, no runtime, no workload in any arm"

SUMMARY=()
for N in $SIZES; do
  echo ""
  echo "[N=$N] $N validators, explicit genesis"
  cleanup; sleep 2

  ADDRS=()
  for i in $(seq 0 $((N-1))); do
    ADDRS+=("$(derive "sweep-$N-$i" $((BASE_RPC+i)) $((BASE_P2P+i)))")
  done
  for a in "${ADDRS[@]}"; do
    [[ -n "$a" ]] || { echo "  could not derive an address; skipping N=$N"; continue 2; }
  done
  GEN="$WORK/gen-$N.toml"; write_genesis "$GEN" "${ADDRS[@]}"

  for i in $(seq 0 $((N-1))); do
    d="$WORK/n$N-$i"; mkdir -p "$d"
    peers=""
    if [[ $i -gt 0 ]]; then
      for j in $(seq 0 $((i-1))); do peers="${peers}127.0.0.1:$((BASE_P2P+j)),"; done
      peers="${peers%,}"
    fi
    args=(--rpc "127.0.0.1:$((BASE_RPC+i))" --p2p-port "$((BASE_P2P+i))" --data-dir "$d"
          --genesis "$GEN" --insecure-dev-validator-seed --validator-seed "sweep-$N-$i"
          --stake "$STAKE")
    [[ -n "${GENESIS_BARRIER:-}" ]] && args+=(--require-full-committee-at-genesis)
    [[ -n "$peers" ]] && args+=(--peers "$peers")
    "$BINARY" "${args[@]}" > "$WORK/n$N-$i.log" 2>&1 &
    PIDS+=($!)
    for _ in $(seq 1 60); do health $((BASE_RPC+i)) >/dev/null 2>&1 && break; sleep 1; done
  done

  # ── start barrier ──────────────────────────────────────────────────────────
  peerfile="$WORK/peers-$N.txt"; verdict="INCOMPLETE"
  for _ in $(seq 1 "$BARRIER_TIMEOUT"); do
    : > "$peerfile"
    for i in $(seq 0 $((N-1))); do
      b=$(health $((BASE_RPC+i))); p=$(field "$b" peers)
      printf '%s\n' "${p:-x}" >> "$peerfile"
    done
    verdict=$(barrier_met "$peerfile" "$N") && break
    sleep 1
  done
  echo "  start barrier: $verdict (peers per node: $(tr '\n' ' ' < "$peerfile"))"

  read -r vcount vstake vquorum <<<"$(committee $BASE_RPC)"
  echo "  committee as reported by node 0: validators=$vcount total_stake=$vstake quorum=$vquorum"

  b=$(health $BASE_RPC); h0=$(field "$b" height); r0=$(field "$b" dag_round)
  sleep "$OBSERVE"
  b=$(health $BASE_RPC); h1=$(field "$b" height); r1=$(field "$b" dag_round)

  # Every node's committed height, so "the chain progressed" is not one node's word.
  allh=""
  for i in $(seq 0 $((N-1))); do
    allh="$allh $(field "$(health $((BASE_RPC+i)))" height)"
  done

  state="STALLED"
  [[ "${h1:-0}" -gt "${h0:-0}" ]] && state="PROGRESSING"
  [[ "${h1:-0}" -eq 0 ]] && state="NO_COMMIT"
  echo "  height $h0 -> $h1   dag_round $r0 -> $r1   -> $state"
  echo "  heights across all $N nodes:$allh"
  SUMMARY+=("N=$N barrier=$verdict quorum=$vquorum height=${h0:-?}->${h1:-?} dag_round=${r0:-?}->${r1:-?} $state")
  cleanup
done

echo ""
echo "=== RESULT ==="
for s in "${SUMMARY[@]}"; do echo "  $s"; done
echo ""
echo "  A rising dag_round with a flat height is liveness WITHOUT progress:"
echo "  the DAG advances but the two-round commit rule never certifies a leader."
echo "  logs: $WORK"
