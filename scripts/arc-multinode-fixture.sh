#!/usr/bin/env bash
# Separate-process, separate-store multi-node fixture.
#
# Why this exists: every "multi-node" test in this repo runs all nodes inside
# ONE OS process (crates/arc-node/tests/multi_node.rs uses real localhost TCP
# but a single test binary), and every native-inference "restart" is a
# drop-and-reopen of a Rust struct against the same directory. Neither
# establishes replica agreement or survives a real process death.
#
# It also exists because the soak harness could not peer its nodes: --benchmark
# forbids --genesis (main.rs:5566), so each node seeded its own
# single-validator set and the transport correctly refused the mismatch. The
# fix is the one the review named - a SHARED disposable genesis plus an
# external workload - not weakening that isolation guard.
#
# Phases:
#   1. derive each node's validator address from its deterministic dev seed
#   2. write a shared disposable genesis naming all of them
#   3. start N separate processes, each with its OWN data dir, peered
#   4. require a real quorum: every node healthy, peers > 0, validators == N
#   5. observe chain progress and compare state roots AT A COMMON HEIGHT
#   6. kill one node, restart it, require it to rejoin and catch up
#
# usage: arc-multinode-fixture.sh [--nodes N] [--binary PATH] [--settle SECONDS]
set -uo pipefail

NODES=${NODES:-3}
BINARY=${BINARY:-target/debug/arc-node}
SETTLE=${SETTLE:-45}
WORKLOAD=${WORKLOAD:-12}
RECOVER=${RECOVER:-120}
BASE_RPC=9980
BASE_P2P=9180
WORK="/tmp/arc-multinode-$(date +%Y%m%d-%H%M%S)"
STAKE=6666667
# Well-known faucet pool address, taken from the repo genesis.toml.
FAUCET_POOL="2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"

EXIT_STATUS=0; FAILURES=(); PIDS=(); ADDRS=(); CLEANED=0
fail() { FAILURES+=("$1"); EXIT_STATUS=1; echo "  FAIL: $1"; }
ok()   { echo "  ok: $1"; }

while [[ $# -gt 0 ]]; do
  case $1 in
    --nodes) NODES=$2; shift 2 ;;
    --binary) BINARY=$2; shift 2 ;;
    --settle) SETTLE=$2; shift 2 ;;
    --workload) WORKLOAD=$2; shift 2 ;;
    --recover) RECOVER=$2; shift 2 ;;
    *) echo "unknown arg $1"; exit 2 ;;
  esac
done

cleanup() {
  [[ "$CLEANED" = 1 ]] && return; CLEANED=1; trap - EXIT INT TERM
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  sleep 1
  for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null || true; done
  {
    echo ""
    echo "================ MULTI-NODE FIXTURE REPORT ================"
    echo "  nodes: $NODES   work dir: $WORK   binary: $BINARY"
    if [[ ${#FAILURES[@]} -eq 0 ]]; then echo "  Verdict: PASS"; else
      echo "  Verdict: FAIL"; for f in "${FAILURES[@]}"; do echo "    - $f"; done
    fi
    echo "  Exit status: $EXIT_STATUS"
  } | tee -a "$WORK/report.txt"
  echo "$EXIT_STATUS" > "$WORK/exitcode.txt"
  exit "$EXIT_STATUS"
}
trap cleanup EXIT INT TERM

json_field() { # port path field
  local raw code body
  raw=$(curl -s -w '\n%{http_code}' --max-time 4 "http://127.0.0.1:$1$2" 2>/dev/null) || return 1
  code=${raw##*$'\n'}; body=${raw%$'\n'*}
  [[ "$code" == "200" ]] || return 1
  printf '%s' "$body" | python3 -c '
import sys,json
try: d=json.load(sys.stdin)
except Exception: sys.exit(1)
k=sys.argv[1]
if not isinstance(d,dict) or k not in d or d[k] is None: sys.exit(1)
print(d[k])' "$3" 2>/dev/null || return 1
}

wait_healthy() { # port timeout
  local deadline=$(( $(date +%s) + $2 ))
  while [[ $(date +%s) -lt $deadline ]]; do
    json_field "$1" /health status >/dev/null && return 0
    sleep 1
  done
  return 1
}

mkdir -p "$WORK"
echo "=== multi-node fixture: $NODES separate processes, separate stores ==="
echo "work dir: $WORK"

# ── Phase 1: derive validator addresses from deterministic dev seeds ─────────
echo ""
echo "[1/6] deriving validator addresses from dev seeds"
for i in $(seq 0 $((NODES-1))); do
  d="$WORK/probe-$i"; mkdir -p "$d"
  "$BINARY" --rpc "127.0.0.1:$((BASE_RPC+i))" --p2p-port "$((BASE_P2P+i))" \
            --data-dir "$d" --insecure-dev-validator-seed \
            --validator-seed "fixture-node-$i" --stake "$STAKE" \
            > "$WORK/probe-$i.log" 2>&1 &
  probe=$!
  for _ in $(seq 1 30); do
    a=$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/probe-$i.log" 2>/dev/null | grep -m1 "Validator  *:" | sed -E 's/.*: *0x?([0-9a-f]{64}).*/\1/')
    [[ -n "${a:-}" ]] && break
    sleep 1
  done
  kill "$probe" 2>/dev/null; wait "$probe" 2>/dev/null
  if [[ -z "${a:-}" ]]; then fail "could not derive address for node $i"; exit 1; fi
  ADDRS+=("$a"); echo "  node $i -> $a"
done

# ── Phase 2: shared disposable genesis ──────────────────────────────────────
echo ""
echo "[2/6] writing shared disposable genesis"
GEN="$WORK/genesis.toml"
{
  echo '[chain]'
  echo 'name = "arc-multinode-fixture"'
  echo 'chain_id = "0x415243"'
  # false, honestly: this is a disposable fixture genesis, not an approved
  # production validator set. main.rs:6667 refuses seed-derived identities on a
  # genesis that CLAIMS completeness, and that guard is right - the fix is to
  # not make a false claim, rather than to bypass the check.
  echo 'validator_set_complete = false'
  echo ''
  # The faucet pool is a well-known address (the first account in the repo's own
  # genesis.toml). Without it funded, /faucet/claim returns HTTP 500 "Faucet pool
  # account not funded", which is exactly what the first workload attempt hit.
  echo '[[accounts]]'
  echo "address = \"$FAUCET_POOL\""
  echo 'balance = 1_000_000_000_000'
  echo ''
  for a in "${ADDRS[@]}"; do
    echo '[[accounts]]'; echo "address = \"$a\""; echo 'balance = 1_000_000_000_000'; echo ''
  done
  for a in "${ADDRS[@]}"; do
    echo '[[validators]]'; echo "address = \"$a\""; echo "stake = $STAKE"; echo ''
  done
} > "$GEN"
echo "  $GEN ($(grep -c '\[\[validators\]\]' "$GEN") validators)"

# ── Phase 3: start N separate processes with separate stores ────────────────
echo ""
echo "[3/6] starting $NODES separate processes (fresh stores, peered)"
# Topology: STAGGERED startup with an incremental peer list. Node i is started
# only after nodes 0..i-1 are healthy, and dials exactly those. This produces a
# full mesh while ensuring no two nodes ever dial each other simultaneously.
#
# Both earlier attempts failed on that distinction. A full mesh started all at
# once (every node dialing every node, including itself) timed out on every
# link. A pure star (all followers dialing only the seed) connected and
# authenticated, but nodes 1 and 2 never saw each other, the DAG never
# committed a round, and consensus reported "Round stalled, but no
# authenticated quorum view-change certificate is available". The repo's own
# passing transport test (tests/multi_node.rs::test_two_nodes_connect) starts
# its two nodes sequentially, which is the property being reproduced here.
for i in $(seq 0 $((NODES-1))); do
  d="$WORK/node-$i"; rm -rf "$d"; mkdir -p "$d"
  PEER_ARGS=()
  if [[ $i -gt 0 ]]; then
    plist=""
    for j in $(seq 0 $((i-1))); do plist="${plist}127.0.0.1:$((BASE_P2P+j)),"; done
    PEER_ARGS=(--peers "${plist%,}")
  fi
  "$BINARY" --rpc "127.0.0.1:$((BASE_RPC+i))" --p2p-port "$((BASE_P2P+i))" \
            --data-dir "$d" --genesis "$GEN" \
            ${PEER_ARGS[@]+"${PEER_ARGS[@]}"} \
            --insecure-dev-validator-seed --validator-seed "fixture-node-$i" \
            --stake "$STAKE" \
            > "$WORK/node-$i.log" 2>&1 &
  PIDS+=($!)
  echo "  node $i pid=${PIDS[$i]} rpc=$((BASE_RPC+i)) p2p=$((BASE_P2P+i)) $([ $i -eq 0 ] && echo '(seed)' || echo "-> ${PEER_ARGS[1]}")"
  if wait_healthy "$((BASE_RPC+i))" 60; then echo "    node $i healthy, proceeding"; else
    fail "node $i never became healthy during staggered startup"
    tail -12 "$WORK/node-$i.log" | sed 's/^/      /'; fi
  sleep 4
done

# ── Phase 4: require a real quorum ──────────────────────────────────────────
echo ""
echo "[4/6] requiring every node healthy, peered, and agreeing on the set"
echo "  settling ${SETTLE}s for peering and block production..."
sleep "$SETTLE"
for i in $(seq 0 $((NODES-1))); do
  p=$(json_field "$((BASE_RPC+i))" /health peers 2>/dev/null || echo "")
  v=$(json_field "$((BASE_RPC+i))" /health validators 2>/dev/null || echo "")
  echo "  node $i: peers=${p:-?} validators=${v:-?}"
  [[ "${p:-0}" -gt 0 ]] || fail "node $i has no peers (separate processes did not form a network)"
  [[ "${v:-0}" -eq "$NODES" ]] || fail "node $i sees ${v:-?} validators, expected $NODES"
done

# ── Phase 5a: bounded EXTERNAL workload ─────────────────────────────────────
#
# Without transactions the nodes commit empty blocks, the state root never
# changes, and "all nodes agree on the root" is agreement on a constant - which
# proves almost nothing. This drives real state changes from OUTSIDE the nodes,
# which is also what lets the fixture avoid --benchmark entirely (that flag
# forbids --genesis, and a shared genesis is what makes peering possible).
#
# /faucet/claim takes only a recipient address, so no client-side transaction
# signing is needed. Each claim goes to a distinct random address because the
# faucet is rate-limited per address.
echo ""
echo "[5a/6] driving a bounded external workload (${WORKLOAD} faucet claims)"
ROOT_BEFORE=$(json_field "$BASE_RPC" /sync/snapshot/info state_root 2>/dev/null || echo "")
accepted=0
for _ in $(seq 1 "$WORKLOAD"); do
  addr=$(python3 -c "import secrets;print(secrets.token_hex(32))")
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
         -X POST -H 'Content-Type: application/json' \
         -d "{\"address\":\"$addr\"}" \
         "http://127.0.0.1:$BASE_RPC/faucet/claim" 2>/dev/null)
  [[ "$code" == "200" ]] && accepted=$(( accepted + 1 ))
done
echo "  $accepted/$WORKLOAD claims accepted (HTTP 200)"
[[ "$accepted" -gt 0 ]] || fail "no external transaction was accepted, so state never changed"
sleep 20
ROOT_AFTER=$(json_field "$BASE_RPC" /sync/snapshot/info state_root 2>/dev/null || echo "")
echo "  state root before=${ROOT_BEFORE:0:18}... after=${ROOT_AFTER:0:18}..."
if [[ -n "$ROOT_BEFORE" && -n "$ROOT_AFTER" && "$ROOT_BEFORE" != "$ROOT_AFTER" ]]; then
  ok "external workload changed the state root, so agreement below is over EVOLVING state"
else
  fail "state root did not change under the external workload (agreement would be over a constant)"
fi

# ── Phase 5: chain progress and root agreement at a COMMON height ───────────
echo ""
echo "[5/6] sampling height and state root at a common height"
echo "height,node,root" > "$WORK/roots.csv"
for _ in $(seq 1 12); do
  for i in $(seq 0 $((NODES-1))); do
    h=$(json_field "$((BASE_RPC+i))" /sync/snapshot/info height 2>/dev/null || echo "")
    r=$(json_field "$((BASE_RPC+i))" /sync/snapshot/info state_root 2>/dev/null || echo "")
    [[ -n "$h" && -n "$r" ]] && echo "$h,$i,$r" >> "$WORK/roots.csv"
  done
  sleep 5
done
MAXH=$(awk -F, 'NR>1 {if ($1+0>m) m=$1+0} END{print m+0}' "$WORK/roots.csv")
echo "  max height observed: $MAXH"
[[ "$MAXH" -gt 0 ]] || fail "chain never advanced past height 0"
COMMON=$(awk -F, 'NR>1 {n[$1]++} END{best=0; for (h in n) if (n[h]>=2 && h+0>best) best=h+0; print best}' "$WORK/roots.csv")
if [[ "$COMMON" -gt 0 ]]; then
  DISTINCT=$(awk -F, -v H="$COMMON" 'NR>1 && $1==H {seen[$3]=1} END{n=0; for (k in seen) n++; print n}' "$WORK/roots.csv")
  echo "  common height $COMMON observed by >=2 nodes; distinct roots there: $DISTINCT"
  [[ "$DISTINCT" -eq 1 ]] || fail "peered nodes disagree on the state root at height $COMMON"
  [[ "$DISTINCT" -eq 1 ]] && ok "state roots agree at a common height (real replica agreement)"
else
  fail "no height was ever observed by two nodes, so agreement was never testable"
fi

# ── Phase 6: real process death and restart ─────────────────────────────────
echo ""
echo "[6/6] killing node $((NODES-1)) and restarting it as a NEW process"
LAST=$((NODES-1))
BEFORE=$(json_field "$((BASE_RPC+LAST))" /sync/snapshot/info height 2>/dev/null || echo 0)
kill -9 "${PIDS[$LAST]}" 2>/dev/null
# Verify the PROCESS is gone, not that the port stopped answering. A socket can
# linger briefly after SIGKILL, which made the first version report a false
# "still answering" for a node that was already dead.
gone=0
for _ in $(seq 1 15); do
  if ! kill -0 "${PIDS[$LAST]}" 2>/dev/null; then gone=1; break; fi
  sleep 1
done
if [[ $gone -eq 1 ]]; then ok "node $LAST process is gone (SIGKILL, not a graceful stop)"; else
  fail "node $LAST survived SIGKILL"; fi
"$BINARY" --rpc "127.0.0.1:$((BASE_RPC+LAST))" --p2p-port "$((BASE_P2P+LAST))" \
          --data-dir "$WORK/node-$LAST" --genesis "$GEN" \
          --peers "$(for j in $(seq 0 $((LAST-1))); do printf '127.0.0.1:%s,' $((BASE_P2P+j)); done | sed 's/,$//')" \
          --insecure-dev-validator-seed --validator-seed "fixture-node-$LAST" \
          --stake "$STAKE" > "$WORK/node-$LAST.restart.log" 2>&1 &
PIDS[$LAST]=$!
if wait_healthy "$((BASE_RPC+LAST))" 60; then ok "node $LAST restarted from its persisted store"; else
  fail "node $LAST did not come back after restart"
  tail -15 "$WORK/node-$LAST.restart.log" | sed 's/^/      /'; fi
# Observe recovery progressively rather than asserting a fixed deadline: the
# node restores its DAG WAL round cursor immediately but keeps its COMMIT
# cursor fail-closed until it has a quorum certificate, so a single 20 s check
# reports height 0 for a node that is recovering correctly.
echo "  observing recovery for up to ${RECOVER}s (before kill: height=$BEFORE)"
AFTER=0; recovered=0
deadline=$(( $(date +%s) + RECOVER ))
while [[ $(date +%s) -lt $deadline ]]; do
  AFTER=$(json_field "$((BASE_RPC+LAST))" /sync/snapshot/info height 2>/dev/null || echo 0)
  printf "\r    height=%-8s peers=%-3s" "$AFTER" "$(json_field "$((BASE_RPC+LAST))" /health peers 2>/dev/null || echo '?')"
  if [[ "${AFTER:-0}" -ge "${BEFORE:-0}" ]]; then recovered=1; break; fi
  sleep 5
done
echo ""
echo "  node $LAST height before kill=$BEFORE after restart=$AFTER"
if [[ $recovered -eq 1 ]]; then
  ok "node $LAST caught back up to its pre-kill height across a real process restart"
else
  fail "node $LAST did not regain its pre-kill height within ${RECOVER}s ($BEFORE -> $AFTER). \
Its DAG WAL round cursor IS restored; the commit cursor stays fail-closed pending a quorum certificate."
fi
