#!/usr/bin/env bash
# Controlled comparison: what does this build need in order to COMMIT a block?
#
# Two observations were being explained by one guess. The four-validator stall
# and the single-validator non-commit were both attributed to "explicit
# --genesis", but the comparison behind that claim changed several variables at
# once (explicit genesis AND activation AND no --benchmark, against self-genesis
# AND --benchmark AND no activation).
#
# RESULT (2026-09-20, recorded in outputs/.../round3/d7-block-production-probe.txt):
# A=NO_COMMIT, B=NO_COMMIT, C=COMMITS. The genesis source makes no difference.
# Arms A and B carry no workload, and consensus.rs:1640-1643 states the rule
# directly - multi-validator mode proposes every round so the DAG advances,
# single-validator mode proposes only when it has transactions - so an idle
# single-validator chain not committing is designed behaviour, not a defect.
#
# The staged lifecycle diagnostic that originally claimed a single validator
# "never leaves height 0 even with work pending" was wrong, and its own node log
# disproved it: the node produced height 1, executed the paid request, and then
# shut itself down on a preimage it had deleted. See the commit "Stop the first
# transaction from killing a single-validator node".
#
# This holds everything else constant and varies ONE thing at a time:
#
#   A  1 validator, explicit --genesis
#   B  1 validator, self-seeded genesis        (varies: genesis source)
#   C  3 validators, explicit --genesis        (varies: committee size)
#
# No activation, no runtime, no --benchmark in any arm, so those cannot explain
# a difference. Each arm is bounded and reports only whether committed height
# left 0, separately from whether DAG rounds advanced.
set -uo pipefail

BINARY=${BINARY:-target/debug/arc-node}
OBSERVE=${OBSERVE:-45}
STAKE=6666667
WORK="/tmp/arc-blockprobe-$(date +%Y%m%d-%H%M%S)"
PIDS=()

cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
  sleep 1
  for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
}
trap cleanup EXIT INT TERM

mkdir -p "$WORK"
echo "=== block-production probe ==="
echo "work dir: $WORK   observe: ${OBSERVE}s per arm"

derive() { # seed rpc p2p -> address on stdout
  local d="$WORK/probe-$1"; mkdir -p "$d"
  "$BINARY" --rpc "127.0.0.1:$2" --p2p-port "$3" --data-dir "$d" \
            --insecure-dev-validator-seed --validator-seed "$1" --stake "$STAKE" \
            > "$WORK/probe-$1.log" 2>&1 &
  local pid=$! a=""
  for _ in $(seq 1 30); do
    a=$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/probe-$1.log" | grep -m1 "Validator  *:" | sed -E 's/.*: *0x?([0-9a-f]{64}).*/\1/')
    [[ -n "$a" ]] && break; sleep 1
  done
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  printf '%s' "$a"
}

health() { curl -s --max-time 4 "http://127.0.0.1:$1/health" 2>/dev/null; }
field() { printf '%s' "$1" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get(sys.argv[1],""))' "$2" 2>/dev/null; }

# Report committed height AND dag round separately: live rounds are not commits.
observe() { # label port
  local h0 r0 h1 r1 body
  body=$(health "$2"); h0=$(field "$body" height); r0=$(field "$body" dag_round)
  sleep "$OBSERVE"
  body=$(health "$2"); h1=$(field "$body" height); r1=$(field "$body" dag_round)
  printf "  %-46s height %s -> %s   dag_round %s -> %s   %s\n" \
    "$2" "${h0:-?}" "${h1:-?}" "${r0:-?}" "${r1:-?}" \
    "$([[ "${h1:-0}" -gt 0 ]] && echo COMMITS || echo 'NO COMMIT')"
  [[ "${h1:-0}" -gt 0 ]] && return 0 || return 1
}

start_node() { # seed rpc p2p datadir [genesis] [peers]
  local seed=$1 rpc=$2 p2p=$3 dd=$4 gen=${5:-} peers=${6:-}
  mkdir -p "$dd"
  local args=(--rpc "127.0.0.1:$rpc" --p2p-port "$p2p" --data-dir "$dd"
              --insecure-dev-validator-seed --validator-seed "$seed" --stake "$STAKE")
  [[ -n "$gen"   ]] && args+=(--genesis "$gen")
  [[ -n "$peers" ]] && args+=(--peers "$peers")
  "$BINARY" "${args[@]}" > "$WORK/$seed.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 60); do health "$rpc" >/dev/null 2>&1 && break; sleep 1; done
}

write_genesis() { # file addr...
  local f=$1; shift
  { echo '[chain]'; echo 'name = "arc-blockprobe"'; echo 'chain_id = "0x415243"'
    echo 'validator_set_complete = false'
    echo "instance_id = \"block-probe-$$-$(date -u +%s)\""; echo ''
    for a in "$@"; do echo '[[accounts]]'; echo "address = \"$a\""; echo 'balance = 1_000_000_000_000'; echo ''; done
    for a in "$@"; do echo '[[validators]]'; echo "address = \"$a\""; echo "stake = $STAKE"; echo ''; done
  } > "$f"
}

RESULTS=()

echo ""
echo "[A] 1 validator, EXPLICIT genesis"
A0=$(derive probe-a0 9940 9140)
write_genesis "$WORK/gen-a.toml" "$A0"
start_node probe-a0 9940 9140 "$WORK/a0" "$WORK/gen-a.toml"
observe "A" 9940 && RESULTS+=("A=COMMITS") || RESULTS+=("A=NO_COMMIT")
cleanup; PIDS=(); sleep 2

echo ""
echo "[B] 1 validator, SELF-SEEDED genesis (only the genesis source differs)"
start_node probe-b0 9941 9141 "$WORK/b0"
observe "B" 9941 && RESULTS+=("B=COMMITS") || RESULTS+=("B=NO_COMMIT")
cleanup; PIDS=(); sleep 2

echo ""
echo "[C] 3 validators, EXPLICIT genesis (only the committee size differs from A)"
C0=$(derive probe-c0 9942 9142); C1=$(derive probe-c1 9943 9143); C2=$(derive probe-c2 9944 9144)
write_genesis "$WORK/gen-c.toml" "$C0" "$C1" "$C2"
start_node probe-c0 9942 9142 "$WORK/c0" "$WORK/gen-c.toml"
start_node probe-c1 9943 9143 "$WORK/c1" "$WORK/gen-c.toml" "127.0.0.1:9142"
start_node probe-c2 9944 9144 "$WORK/c2" "$WORK/gen-c.toml" "127.0.0.1:9142,127.0.0.1:9143"
observe "C" 9942 && RESULTS+=("C=COMMITS") || RESULTS+=("C=NO_COMMIT")
cleanup; PIDS=()

echo ""
echo "=== RESULT ==="
for r in "${RESULTS[@]}"; do echo "  $r"; done
echo ""
echo "  A vs B isolates the genesis source with committee size held at 1."
echo "  A vs C isolates committee size with the genesis source held explicit."
echo "  logs: $WORK"
