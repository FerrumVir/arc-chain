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

# Default 4, not 3, and the reason is arithmetic rather than taste.
#
# The consensus quorum is stake-weighted at just over two thirds. With 3 equal
# validators a node logs
#   total_stake=20000001 quorum=13333335
# and two survivors hold 13333334 - exactly ONE short. Killing a member of a
# 3-node committee therefore cannot make progress, and the "restart recovery"
# test would be measuring a committee that never had quorum rather than a
# recovery defect. At 4 nodes the three survivors hold 20000001 against a
# ~17777779 quorum, so progress while one is down is genuinely expected.
#
# The threshold is NOT weakened to make the test pass; the committee is sized so
# the premise holds.
NODES=${NODES:-4}
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
# Set only by the last line of the script. Any early exit - an error, `set -u`
# firing, an interrupt - leaves it 0, and the report fails. A run that did not
# finish is not a run that passed.
PHASES_COMPLETE=0
fail() { FAILURES+=("$1"); EXIT_STATUS=1; echo "  FAIL: $1"; }
ok()   { echo "  ok: $1"; }


# ── agreement selector (shared by the live run and the self-test) ───────────
#
# Counted ROWS per height before, not distinct node identities, so two samples
# from ONE node satisfied "a common height". It also correlated a height from
# one snapshot request with a root from another. Both are false-pass paths.
#
# Now: one explicit target height, one coherent /block/{target} response per
# node, and every intended replica must appear by DISTINCT node index.
# agreement_verdict FILE EXPECTED_NODES -> "OK <hash>" | "BAD <reason>"
agreement_verdict() {
    awk -F, -v want="$2" '
        NR > 1 && $2 != "" && $3 != "" {
            if (!($2 in seen)) { seen[$2] = 1; nodes++ }
            hashes[$3] = 1; roots[$4] = 1; rows++
        }
        END {
            nh = 0; for (h in hashes) nh++
            nr = 0; for (r in roots) nr++
            if (nodes < want) { printf "BAD only %d distinct node(s) of %d reported (rows=%d)\n", nodes, want, rows; exit }
            if (nh != 1)      { printf "BAD %d distinct block hashes at the target height\n", nh; exit }
            if (nr != 1)      { printf "BAD %d distinct tx roots at the target height\n", nr; exit }
            for (h in hashes) { printf "OK %s\n", h }
        }' "$1"
}

if [[ "${1:-}" == "--self-test" ]]; then
    echo "self-test: agreement selector cannot be satisfied by one node"
    t=$(mktemp)
    printf 'target,node,hash,tx_root\n900,0,0xaaa,0xttt\n900,1,0xaaa,0xttt\n900,2,0xaaa,0xttt\n' > "$t"
    v=$(agreement_verdict "$t" 3)
    [[ "$v" == OK* ]] && echo "  PASS A: three distinct nodes, one hash -> $v" \
        || { echo "  FAIL A: $v"; rm -f "$t"; exit 1; }

    # The exact defect Codex reproduced: repeated rows from a single node.
    printf 'target,node,hash,tx_root\n900,1,0xaaa,0xttt\n900,1,0xaaa,0xttt\n900,1,0xaaa,0xttt\n' > "$t"
    v=$(agreement_verdict "$t" 3)
    [[ "$v" == BAD*distinct\ node* ]] && echo "  PASS B: repeated samples from one node rejected -> $v" \
        || { echo "  FAIL B: expected a distinct-node rejection, got: $v"; rm -f "$t"; exit 1; }

    printf 'target,node,hash,tx_root\n900,0,0xaaa,0xttt\n900,1,0xbbb,0xttt\n900,2,0xaaa,0xttt\n' > "$t"
    v=$(agreement_verdict "$t" 3)
    [[ "$v" == BAD*block\ hashes* ]] && echo "  PASS C: disagreeing block hashes rejected -> $v" \
        || { echo "  FAIL C: $v"; rm -f "$t"; exit 1; }

    printf 'target,node,hash,tx_root\n900,0,0xaaa,0xttt\n900,1,0xaaa,0xuuu\n900,2,0xaaa,0xttt\n' > "$t"
    v=$(agreement_verdict "$t" 3)
    [[ "$v" == BAD*tx\ roots* ]] && echo "  PASS D: disagreeing tx roots rejected -> $v" \
        || { echo "  FAIL D: $v"; rm -f "$t"; exit 1; }

    printf 'target,node,hash,tx_root\n900,0,0xaaa,0xttt\n900,1,0xaaa,0xttt\n' > "$t"
    v=$(agreement_verdict "$t" 3)
    [[ "$v" == BAD*distinct\ node* ]] && echo "  PASS E: a missing replica rejected -> $v" \
        || { echo "  FAIL E: $v"; rm -f "$t"; exit 1; }
    rm -f "$t"
    echo "SELF-TEST OK"; exit 0
fi

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
    if [[ "${PHASES_COMPLETE:-0}" != "1" ]]; then
      FAILURES+=("the run did not reach the end of the script; some phases never executed")
      EXIT_STATUS=1
    fi
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

nget() { # port path -> body; fails unless HTTP 200 AND valid JSON
  local raw code body
  raw=$(curl -s -w '\n%{http_code}' --max-time 6 "http://127.0.0.1:$1$2" 2>/dev/null) || return 1
  code=${raw##*$'\n'}; body=${raw%$'\n'*}
  [[ "$code" == "200" ]] || return 1
  printf '%s' "$body" | python3 -c 'import sys,json; json.load(sys.stdin)' 2>/dev/null || return 1
  printf '%s' "$body"
}

nfield() { json_field "$@"; }

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
# Reserve the ports before starting anything. Without this an unrelated local
# listener on one of them can be mistaken for a test child, and every
# subsequent assertion is then made against someone else's process.
echo ""
echo "[3/6] reserving ports and starting $NODES separate processes"
for i in $(seq 0 $((NODES-1))); do
  for port in $((BASE_RPC+i)) $((BASE_P2P+i)); do
    if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 || lsof -nP -iUDP:"$port" >/dev/null 2>&1; then
      fail "port $port is already in use; refusing to start so an unrelated listener cannot be mistaken for a test node"
      exit 1
    fi
  done
done
ok "ports $BASE_RPC-$((BASE_RPC+NODES-1)) and $BASE_P2P-$((BASE_P2P+NODES-1)) are free"
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
  # Identity check: this port must be serving the validator this fixture
  # derived for index i, not some other node that happens to be listening.
  id=$(nfield "$((BASE_RPC+i))" /node/info validator 2>/dev/null || echo "")
  if [[ "$id" == "${ADDRS[$i]}" ]]; then
    ok "node $i presents the expected validator identity"
  else
    fail "node $i identity is ${id:0:16}..., expected ${ADDRS[$i]:0:16}... - refusing to trust this process"
  fi
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

# ── Phase 5: agreement at ONE explicit target height, across ALL replicas ───
#
# The previous version sampled each node's CURRENT head every few seconds and
# then looked for a height that appeared twice. That had two false-pass paths:
# it counted rows rather than distinct node identities (two samples from one
# node satisfied it), and it correlated a height from one snapshot request with
# a root from another.
#
# Now: choose a target above every head, wait for every node to reach it, then
# ask each node for that HISTORICAL block in a single coherent response.
echo ""
echo "[5/6] agreement at one explicit target height across all $NODES replicas"
TARGET=0
for i in $(seq 0 $((NODES-1))); do
  h=$(nfield "$((BASE_RPC+i))" /health height); h=${h:-0}
  [[ "$h" -gt "$TARGET" ]] && TARGET=$h
done
TARGET=$(( TARGET + 5 ))
echo "  target height: $TARGET"
deadline=$(( $(date +%s) + 180 ))
allthere=0
while [[ $(date +%s) -lt $deadline ]]; do
  ready=0
  for i in $(seq 0 $((NODES-1))); do
    h=$(nfield "$((BASE_RPC+i))" /health height); h=${h:-0}
    [[ "$h" -ge "$TARGET" ]] && ready=$(( ready + 1 ))
  done
  [[ $ready -eq $NODES ]] && { allthere=1; break; }
  sleep 3
done
[[ $allthere -eq 1 ]] && ok "every replica reached height $TARGET" \
  || fail "not every replica reached the target height $TARGET within 180s"

echo "target,node,hash,tx_root" > "$WORK/agreement.csv"
for i in $(seq 0 $((NODES-1))); do
  B=$(nget "$((BASE_RPC+i))" "/block/$TARGET") || { fail "node $i could not serve block $TARGET"; continue; }
  BH=$(printf '%s' "$B" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("hash",""))' 2>/dev/null)
  BT=$(printf '%s' "$B" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("tx_root",""))' 2>/dev/null)
  echo "$TARGET,$i,$BH,$BT" >> "$WORK/agreement.csv"
  echo "    node $i: block $TARGET hash=${BH:0:18}... tx_root=${BT:0:18}..."
done
VERDICT=$(agreement_verdict "$WORK/agreement.csv" "$NODES")
if [[ "$VERDICT" == OK* ]]; then
  ok "all $NODES replicas report the SAME committed block at height $TARGET (${VERDICT#OK })"
else
  fail "agreement at height $TARGET: ${VERDICT#BAD }"
fi

# ── Phase 5b: live RPC-backed user journeys ─────────────────────────────────
#
# Gate 5 was being claimed on 214 Playwright tests that all run against
# `mockInvoke` (desktop/src/lib/tauri.ts), with the four specs that touch a real
# node unconditionally skipped. That is mock coverage and establishes no live
# journey.
#
# The explorer's own live gate (explorer/test-live.mjs) cannot supply one here:
# it requires an approved recovery checkpoint and all SIX production maintenance
# interlocks, so it is bound to the production fleet, not to anything
# reproducible locally. These journeys run against the REAL nodes above instead.
echo ""
echo "[5b/6] live RPC-backed user journeys against a real node"
jget() { # path -> body; fails unless HTTP 200 AND valid JSON
  local raw code body
  raw=$(curl -s -w '\n%{http_code}' --max-time 6 "http://127.0.0.1:$BASE_RPC$1" 2>/dev/null) || return 1
  code=${raw##*$'\n'}; body=${raw%$'\n'*}
  [[ "$code" == "200" ]] || return 1
  printf '%s' "$body" | python3 -c 'import sys,json; json.load(sys.stdin)' 2>/dev/null || return 1
  printf '%s' "$body"
}
jfield() { printf '%s' "$1" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get(sys.argv[1],""))' "$2" 2>/dev/null; }

H=$(jget /health) && ok "health journey: /health is HTTP 200 and valid JSON" || fail "live /health failed"
for f in status height validators peers dag_round; do
  [[ -n "$(jfield "$H" "$f")" ]] || fail "/health is missing $f"
done
echo "    status=$(jfield "$H" status) height=$(jfield "$H" height) validators=$(jfield "$H" validators) peers=$(jfield "$H" peers)"

JH=$(jfield "$H" height)
if [[ "${JH:-0}" -gt 0 ]] && jget "/block/$JH" >/dev/null; then
  ok "browsing journey: /block/{height} returns a real mined block"
else
  fail "live /block/{height} did not return a mined block at height ${JH:-0}"
fi

ACC=$(jget "/account/${ADDRS[0]}") && ok "account journey: /account/{address} is HTTP 200" \
  || fail "live /account failed"
echo "    validator balance=$(jfield "$ACC" balance) nonce=$(jfield "$ACC" nonce)"

# The payment journey that gate 5 actually needs: claim -> mined -> readable.
RECIP=$(python3 -c "import secrets;print(secrets.token_hex(32))")
BEFORE=$(jfield "$(jget "/account/$RECIP" || echo '{}')" balance); BEFORE=${BEFORE:-0}
CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 -X POST \
       -H 'Content-Type: application/json' -d "{\"address\":\"$RECIP\"}" \
       "http://127.0.0.1:$BASE_RPC/faucet/claim")
if [[ "$CODE" == "200" ]]; then
  credited=0; AFTER=$BEFORE
  for _ in $(seq 1 20); do
    AFTER=$(jfield "$(jget "/account/$RECIP" || echo '{}')" balance); AFTER=${AFTER:-0}
    [[ "${AFTER:-0}" -gt "${BEFORE:-0}" ]] && { credited=1; break; }
    sleep 5
  done
  echo "    payment: claim HTTP 200, recipient balance $BEFORE -> $AFTER"
  [[ $credited -eq 1 ]] && ok "payment journey: funds were MINED and read back over live RPC" \
    || fail "payment journey: claim accepted but the balance never changed"
else
  fail "payment journey: faucet claim rejected (HTTP $CODE)"
fi

BAD=$(curl -s -o /dev/null -w '%{http_code}' --max-time 6 "http://127.0.0.1:$BASE_RPC/account/not-a-valid-address")
[[ "$BAD" != "200" ]] && ok "error journey: an invalid address is refused (HTTP $BAD), not answered with a fake account" \
  || fail "error journey: an invalid address returned HTTP 200"
MISSING=$(curl -s -o /dev/null -w '%{http_code}' --max-time 6 "http://127.0.0.1:$BASE_RPC/block/99999999")
[[ "$MISSING" != "200" ]] && ok "error journey: a nonexistent block is refused (HTTP $MISSING), not fabricated" \
  || fail "error journey: a nonexistent block returned HTTP 200"

# ── Phase 6: real process death, restart, and CONTINUED consensus ───────────
#
# The previous gate accepted `AFTER >= BEFORE`, and the saved run was
# 1558 -> 1558. That shows a reopen at the persisted height, not recovery: it
# passes for a node that comes back and then does nothing. This requires the
# restarted node to rejoin, accept NEW state-changing work, advance BEYOND its
# pre-kill height, and agree with the surviving replicas on the result.
#
# With 3 validators, killing one leaves 2 - which retains quorum, so the chain
# is expected to keep moving while the node is down. No threshold is weakened
# to make this pass.
echo ""
echo "[6/6] killing node $((NODES-1)), restarting it, and requiring recovery"
LAST=$((NODES-1))

# Verify the premise before relying on it: the survivors must actually retain
# quorum, or a stall proves nothing about recovery.
QUORUM_OK=$(python3 -c "
s=$STAKE; n=$NODES
total=n*s; surv=(n-1)*s; q=total*2//3+1
print('yes' if surv>=q else f'no survivors={surv} quorum~{q}')
")
if [[ "$QUORUM_OK" == yes ]]; then
  ok "surviving $((NODES-1)) of $NODES validators retain quorum, so progress while one is down is expected"
else
  fail "this committee does not retain quorum after losing one node ($QUORUM_OK); a stall here would \
say nothing about recovery. Use more validators rather than lowering the threshold."
fi

BEFORE=$(nfield "$((BASE_RPC+LAST))" /health height); BEFORE=${BEFORE:-0}
kill -9 "${PIDS[$LAST]}" 2>/dev/null
gone=0
for _ in $(seq 1 15); do
  kill -0 "${PIDS[$LAST]}" 2>/dev/null || { gone=1; break; }
  sleep 1
done
[[ $gone -eq 1 ]] && ok "node $LAST process is gone (SIGKILL, verified by PID)" \
  || fail "node $LAST survived SIGKILL"

# The survivors must keep making progress without it.
sleep 20
SURV=$(nfield "$BASE_RPC" /health height); SURV=${SURV:-0}
[[ "$SURV" -gt "$BEFORE" ]] && ok "surviving quorum kept advancing while node $LAST was down ($BEFORE -> $SURV)" \
  || fail "chain stalled while one of $NODES nodes was down ($BEFORE -> $SURV)"

plist=""
for j in $(seq 0 $((LAST-1))); do plist="${plist}127.0.0.1:$((BASE_P2P+j)),"; done
"$BINARY" --rpc "127.0.0.1:$((BASE_RPC+LAST))" --p2p-port "$((BASE_P2P+LAST))" \
          --data-dir "$WORK/node-$LAST" --genesis "$GEN" --peers "${plist%,}" \
          --insecure-dev-validator-seed --validator-seed "fixture-node-$LAST" \
          --stake "$STAKE" > "$WORK/node-$LAST.restart.log" 2>&1 &
PIDS[$LAST]=$!
if wait_healthy "$((BASE_RPC+LAST))" 90; then ok "node $LAST restarted from its persisted store"; else
  fail "node $LAST did not come back after restart"
  tail -15 "$WORK/node-$LAST.restart.log" | sed 's/^/      /'
fi
# Identity must still be the fixture's node, not some other local listener.
RID=$(nfield "$((BASE_RPC+LAST))" /node/info validator)
[[ "$RID" == "${ADDRS[$LAST]}" ]] && ok "restarted node still presents the expected validator identity" \
  || fail "restarted node identity is ${RID:0:16}..., expected ${ADDRS[$LAST]:0:16}..."

# Rejoin: peers back, then NEW state-changing work.
sleep 25
RP=$(nfield "$((BASE_RPC+LAST))" /health peers); RP=${RP:-0}
[[ "$RP" -ge 1 ]] && ok "node $LAST rejoined the mesh (peers=$RP)" \
  || fail "node $LAST came back but never rejoined (peers=$RP)"

RECIP2=$(python3 -c "import secrets;print(secrets.token_hex(32))")
CODE2=$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 -X POST \
        -H 'Content-Type: application/json' -d "{\"address\":\"$RECIP2\"}" \
        "http://127.0.0.1:$BASE_RPC/faucet/claim")
if [[ "$CODE2" != "200" ]]; then
  fail "post-restart workload was not accepted (HTTP $CODE2), so recovery could not be tested"
else
  ok "post-restart state-changing transaction accepted"
  seen=0
  for _ in $(seq 1 24); do
    b=$(nfield "$((BASE_RPC+LAST))" "/account/$RECIP2" balance); b=${b:-0}
    [[ "$b" -gt 0 ]] && { seen=1; break; }
    sleep 5
  done
  [[ $seen -eq 1 ]] && ok "the restarted node observes the NEW transaction's effect" \
    || fail "the restarted node never observed work committed after it came back"
fi

AFTER=$(nfield "$((BASE_RPC+LAST))" /health height); AFTER=${AFTER:-0}
echo "  node $LAST height: before kill=$BEFORE after recovery=$AFTER"
[[ "$AFTER" -gt "$BEFORE" ]] && ok "node $LAST advanced BEYOND its pre-kill height (not merely reopened)" \
  || fail "node $LAST only reopened at $AFTER; it never advanced past its pre-kill height $BEFORE"

# Finally: all replicas must agree again, on a block minted after the restart.
TARGET2=$(( AFTER + 3 ))
echo "  re-verifying agreement at height $TARGET2 after recovery"
deadline=$(( $(date +%s) + 180 )); allthere=0
while [[ $(date +%s) -lt $deadline ]]; do
  ready=0
  for i in $(seq 0 $((NODES-1))); do
    h=$(nfield "$((BASE_RPC+i))" /health height); h=${h:-0}
    [[ "$h" -ge "$TARGET2" ]] && ready=$(( ready + 1 ))
  done
  [[ $ready -eq $NODES ]] && { allthere=1; break; }
  sleep 3
done
if [[ $allthere -eq 1 ]]; then
  echo "target,node,hash,tx_root" > "$WORK/agreement-after-restart.csv"
  for i in $(seq 0 $((NODES-1))); do
    B=$(nget "$((BASE_RPC+i))" "/block/$TARGET2") || continue
    BH=$(printf '%s' "$B" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("hash",""))' 2>/dev/null)
    BT=$(printf '%s' "$B" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("tx_root",""))' 2>/dev/null)
    echo "$TARGET2,$i,$BH,$BT" >> "$WORK/agreement-after-restart.csv"
  done
  V2=$(agreement_verdict "$WORK/agreement-after-restart.csv" "$NODES")
  [[ "$V2" == OK* ]] && ok "all $NODES replicas agree at height $TARGET2 AFTER the restart (${V2#OK })" \
    || fail "post-restart agreement at $TARGET2: ${V2#BAD }"
else
  fail "not every replica reached $TARGET2 after the restart, so post-recovery agreement is untested"
fi

PHASES_COMPLETE=1
