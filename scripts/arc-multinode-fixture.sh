#!/usr/bin/env bash
set -uo pipefail

NODES=${NODES:-4}
BINARY=${BINARY:-target/debug/arc-node}
SNAPSHOT_EVERY=${SNAPSHOT_EVERY:-100}
SETTLE=${SETTLE:-45}
WORKLOAD=${WORKLOAD:-12}
RECOVER=${RECOVER:-120}
BASE_RPC=9980
BASE_P2P=9180
WORK="/tmp/arc-multinode-$(date +%Y%m%d-%H%M%S)"
STAKE=6666667
FAUCET_POOL="2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"

EXIT_STATUS=0; FAILURES=(); PIDS=(); ADDRS=(); CLEANED=0
PHASES_COMPLETE=0
fail() { FAILURES+=("$1"); EXIT_STATUS=1; echo "  FAIL: $1"; }
ok()   { echo "  ok: $1"; }


agreement_verdict() {
    awk -F, -v want="$2" -v target="$3" '
        function bad(msg) { printf "BAD %s\n", msg; aborted = 1; exit }
        NR == 1 { next }
        {
            rows++
            node = $2; height = $3; hash = $4; txroot = $5; stateroot = $6
            if (node == "" || node !~ /^[0-9]+$/) bad("row " rows " has no usable node identity")
            if (node + 0 >= want)                 bad("row " rows " reports identity " node " which is not in the intended set 0.." want - 1)
            if (node in seen)                     bad("identity " node " reported more than once")
            seen[node] = 1; nodes++
            if (height == "" || height + 0 != target + 0) bad("identity " node " returned height " (height == "" ? "MISSING" : height) " for target " target)
            if (hash !~ /^[0-9a-f]{64}$/)      bad("identity " node " returned a malformed or empty block hash")
            if (txroot !~ /^[0-9a-f]{64}$/)    bad("identity " node " returned a malformed or empty tx_root")
            if (stateroot !~ /^[0-9a-f]{64}$/) bad("identity " node " returned a malformed or empty state_root")
            hashes[hash] = 1; txroots[txroot] = 1; stateroots[stateroot] = 1
        }
        END {
            if (aborted) exit
            if (rows == 0) bad("no evidence rows at all")
            if (nodes < want) bad("only " nodes " of " want " intended replicas reported")
            nh = 0; for (h in hashes) nh++
            nt = 0; for (t in txroots) nt++
            ns = 0; for (r in stateroots) ns++
            if (nh != 1) { printf "BAD %d distinct block hashes at height %s\n", nh, target; exit }
            if (nt != 1) { printf "BAD %d distinct tx_roots at height %s\n", nt, target; exit }
            if (ns != 1) { printf "BAD %d distinct state_roots at height %s\n", ns, target; exit }
            for (h in hashes) { printf "OK %s\n", h }
        }' "$1"
}

full_history_agreement() {
    local target="$1" file="$2" i from to
    for i in $(seq 0 $((NODES-1))); do
        : > "$file.node-$i.ndjson"
        from=1
        while [[ $from -le $target ]]; do
            to=$(( from + 99 )); [[ $to -gt $target ]] && to=$target
            curl -s --max-time 20 \
                "http://127.0.0.1:$((BASE_RPC+i))/blocks?from=$from&to=$to&limit=100" \
                >> "$file.node-$i.ndjson" 2>/dev/null || true
            printf '\n' >> "$file.node-$i.ndjson"
            from=$(( to + 1 ))
        done
    done
    python3 - "$target" "$NODES" "$file" <<'PYEOF'
import json, sys
target, nodes, base = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]

chains = {}
for n in range(nodes):
    chain = {}
    try:
        with open(f"{base}.node-{n}.ndjson") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                page = json.loads(line)
                for b in page.get("blocks", []):
                    height = b.get("height")
                    if height is None:
                        continue
                    strip = lambda v: str(v or "").replace("0x", "")
                    chain[int(height)] = (
                        strip(b.get("hash")),
                        strip(b.get("parent_hash")),
                        strip(b.get("tx_root")),
                    )
    except Exception as exc:
        print(f"UNAVAILABLE node {n}: {exc}")
        sys.exit(3)
    if not chain:
        print(f"UNAVAILABLE node {n}: the block listing returned nothing")
        sys.exit(3)
    chains[n] = chain

covered = set(chains[0])
for n in range(1, nodes):
    covered &= set(chains[n])
covered = {h for h in covered if 1 <= h <= target}
if not covered:
    print("UNAVAILABLE: no height is present on every replica")
    sys.exit(3)

missing = []
for n in range(nodes):
    gap = sorted(h for h in range(1, target + 1) if h not in chains[n])
    if gap:
        missing.append((n, len(gap), gap[:5]))

first_bad = None
for height in sorted(covered):
    if len({chains[n][height] for n in range(nodes)}) > 1:
        first_bad = height
        break

if first_bad is None and not missing:
    print(f"OK {len(covered)} heights compared on {nodes} replicas, "
          f"{min(covered)}..{max(covered)}, identical hash/parent/tx_root at every one")
    sys.exit(0)

if first_bad is not None:
    print(f"BAD first divergence at height {first_bad} of {len(covered)} compared")
    for n in range(nodes):
        h, parent, tx = chains[n][first_bad]
        print(f"  node {n}: hash={h[:18]}... parent={parent[:18]}... tx_root={tx[:18]}...")
else:
    print(f"BAD {len(covered)} heights agree, but a replica is missing heights")
for n, count, sample in missing:
    print(f"  node {n} is missing {count} heights in 1..{target}, first: {sample}")
sys.exit(1)
PYEOF
}

collect_agreement() {
    local file="$1" target="$2" i body
    echo "target,node,height,hash,tx_root,state_root" > "$file"
    for i in $(seq 0 $((NODES-1))); do
        if ! body=$(nget "$((BASE_RPC+i))" "/block/$target"); then
            echo "$target,$i,,,," >> "$file"
            echo "    node $i: no usable /block/$target response"
            continue
        fi
        printf '%s' "$body" | python3 -c '
import sys, json
target = sys.argv[1]; node = sys.argv[2]
try:
    d = json.load(sys.stdin)
    h = d.get("header") or {}
    strip = lambda v: str(v or "").replace("0x", "")
    print("%s,%s,%s,%s,%s,%s" % (target, node,
        h.get("height", ""), strip(d.get("hash")),
        strip(h.get("tx_root")), strip(h.get("state_root"))))
except Exception:
    print("%s,%s,,,," % (target, node))
' "$target" "$i" >> "$file"
        tail -1 "$file" | awk -F, '{printf "    node %s: height=%s hash=%.18s... tx_root=%.18s... state_root=%.18s...\n", $2, ($3==""?"MISSING":$3), $4, $5, $6}'
    done
}

if [[ "${1:-}" == "--self-test" ]]; then
    echo "self-test: agreement selector"
    t=$(mktemp)
    H="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    T="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    R="cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
    hdr="target,node,height,hash,tx_root,state_root"

    printf '%s\n900,0,900,%s,%s,%s\n900,1,900,%s,%s,%s\n900,2,900,%s,%s,%s\n' \
        "$hdr" "$H" "$T" "$R" "$H" "$T" "$R" "$H" "$T" "$R" > "$t"
    v=$(agreement_verdict "$t" 3 900)
    [[ "$v" == OK* ]] && echo "  PASS A: three intended replicas, one hash/tx_root/state_root -> $v" \
        || { echo "  FAIL A: $v"; rm -f "$t"; exit 1; }

    printf '%s\n801,0,801,%s,,\n801,1,801,%s,,\n801,2,801,%s,,\n' "$hdr" "$H" "$H" "$H" > "$t"
    v=$(agreement_verdict "$t" 3 801)
    [[ "$v" == BAD*tx_root* ]] && echo "  PASS B: blank roots rejected -> $v" \
        || { echo "  FAIL B: expected a blank-root rejection, got: $v"; rm -f "$t"; exit 1; }

    printf '%s\n900,0,900,%s,%s,%s\n900,1,901,%s,%s,%s\n900,2,902,%s,%s,%s\n' \
        "$hdr" "$H" "$T" "$R" "$H" "$T" "$R" "$H" "$T" "$R" > "$t"
    v=$(agreement_verdict "$t" 3 900)
    [[ "$v" == BAD*height* ]] && echo "  PASS C: wrong reported height rejected -> $v" \
        || { echo "  FAIL C: $v"; rm -f "$t"; exit 1; }

    printf '%s\n900,8,900,%s,%s,%s\n900,9,900,%s,%s,%s\n900,10,900,%s,%s,%s\n' \
        "$hdr" "$H" "$T" "$R" "$H" "$T" "$R" "$H" "$T" "$R" > "$t"
    v=$(agreement_verdict "$t" 3 900)
    [[ "$v" == BAD*intended\ set* ]] && echo "  PASS D: identities outside the intended set rejected -> $v" \
        || { echo "  FAIL D: $v"; rm -f "$t"; exit 1; }

    printf '%s\n900,1,900,%s,%s,%s\n900,1,900,%s,%s,%s\n900,1,900,%s,%s,%s\n' \
        "$hdr" "$H" "$T" "$R" "$H" "$T" "$R" "$H" "$T" "$R" > "$t"
    v=$(agreement_verdict "$t" 3 900)
    [[ "$v" == BAD*more\ than\ once* ]] && echo "  PASS E: duplicate identity rejected -> $v" \
        || { echo "  FAIL E: $v"; rm -f "$t"; exit 1; }

    printf '%s\n900,0,900,%s,%s,%s\n900,1,900,%s,%s,%s\n' "$hdr" "$H" "$T" "$R" "$H" "$T" "$R" > "$t"
    v=$(agreement_verdict "$t" 3 900)
    [[ "$v" == BAD*intended\ replicas* ]] && echo "  PASS F: missing replica rejected -> $v" \
        || { echo "  FAIL F: $v"; rm -f "$t"; exit 1; }

    printf '%s\n900,0,900,not-a-hash,%s,%s\n900,1,900,%s,%s,%s\n900,2,900,%s,%s,%s\n' \
        "$hdr" "$T" "$R" "$H" "$T" "$R" "$H" "$T" "$R" > "$t"
    v=$(agreement_verdict "$t" 3 900)
    [[ "$v" == BAD*block\ hash* ]] && echo "  PASS G: malformed block hash rejected -> $v" \
        || { echo "  FAIL G: $v"; rm -f "$t"; exit 1; }

    R2="dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
    printf '%s\n900,0,900,%s,%s,%s\n900,1,900,%s,%s,%s\n900,2,900,%s,%s,%s\n' \
        "$hdr" "$H" "$T" "$R" "$H" "$T" "$R2" "$H" "$T" "$R" > "$t"
    v=$(agreement_verdict "$t" 3 900)
    [[ "$v" == BAD*state_roots* ]] && echo "  PASS H: disagreeing state_roots rejected -> $v" \
        || { echo "  FAIL H: $v"; rm -f "$t"; exit 1; }
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

json_field() {
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

nget() {
  local raw code body
  raw=$(curl -s -w '\n%{http_code}' --max-time 6 "http://127.0.0.1:$1$2" 2>/dev/null) || return 1
  code=${raw##*$'\n'}; body=${raw%$'\n'*}
  [[ "$code" == "200" ]] || return 1
  printf '%s' "$body" | python3 -c 'import sys,json; json.load(sys.stdin)' 2>/dev/null || return 1
  printf '%s' "$body"
}

nfield() { json_field "$@"; }

height_or_fail() {
  local h; h=$(nfield "$1" /health height)
  if [[ -z "$h" || ! "$h" =~ ^[0-9]+$ ]]; then
    fail "node on port $1 ($2) did not report a height; it is unreachable, not at height 0"
    printf '0'; return 1
  fi
  printf '%s' "$h"
}

wait_healthy() {
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

echo ""
if [[ -f "$BINARY" ]]; then
  newest_source=$(find crates -name '*.rs' -newer "$BINARY" -print -quit 2>/dev/null)
  if [[ -n "$newest_source" ]]; then
    echo "REFUSING TO RUN: $BINARY is older than $newest_source"
    echo "  build it first:  cargo build -p arc-node"
    exit 2
  fi
else
  echo "REFUSING TO RUN: $BINARY does not exist; build it first"
  exit 2
fi

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

echo ""
echo "[2/6] writing shared disposable genesis"
GEN="$WORK/genesis.toml"
{
  echo '[chain]'
  echo 'name = "arc-multinode-fixture"'
  echo 'chain_id = "0x415243"'
  echo 'validator_set_complete = false'
  echo "instance_id = \"fixture-$$-$(date -u +%s)\""
  echo ''
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
            --stake "$STAKE" --snapshot-every-blocks "$SNAPSHOT_EVERY" \
            > "$WORK/node-$i.log" 2>&1 &
  PIDS+=($!)
  echo "  node $i pid=${PIDS[$i]} rpc=$((BASE_RPC+i)) p2p=$((BASE_P2P+i)) $([ $i -eq 0 ] && echo '(seed)' || echo "-> ${PEER_ARGS[1]}")"
  if wait_healthy "$((BASE_RPC+i))" 60; then echo "    node $i healthy, proceeding"; else
    fail "node $i never became healthy during staggered startup"
    tail -12 "$WORK/node-$i.log" | sed 's/^/      /'; fi
  sleep 4
done

echo ""
echo "[4/6] requiring every node healthy, peered, and agreeing on the set"
echo "  settling ${SETTLE}s for peering and block production..."
sleep "$SETTLE"
for i in $(seq 0 $((NODES-1))); do
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

collect_agreement "$WORK/agreement.csv" "$TARGET"
VERDICT=$(agreement_verdict "$WORK/agreement.csv" "$NODES" "$TARGET")
if [[ "$VERDICT" == OK* ]]; then
  ok "all $NODES replicas report the SAME committed block at height $TARGET (${VERDICT#OK })"
else
  fail "agreement at height $TARGET: ${VERDICT#BAD }"
fi

echo ""
echo "[5b/6] live RPC-backed user journeys against a real node"
jget() {
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

echo ""
echo
echo "[5c/6] a quorum finality certificate a client can verify without trusting the server"
FH=$(nfield "$BASE_RPC" /health height); FH=${FH:-0}
FTARGET=$(( FH > 8 ? FH - 4 : 0 ))
if [[ $FTARGET -gt 0 ]]; then
  curl -s -m 6 "http://127.0.0.1:$BASE_RPC/finality/$FTARGET" > "$WORK/finality.json" || true
  FVERDICT=$(python3 - "$FTARGET" "$WORK/finality.json" <<'PYEOF'
import json, sys
target = int(sys.argv[1])
try:
    with open(sys.argv[2]) as fh:
        d = json.load(fh)
except Exception as exc:
    print(f"BAD response was not JSON ({exc})"); sys.exit(0)
if "certificate_bincode_hex" not in d:
    print(f"BAD no certificate served at height {target}: {str(d)[:160]}"); sys.exit(0)
if d.get("height") != target:
    print(f"BAD served height {d.get('height')}, asked for {target}"); sys.exit(0)
if not d.get("verified_by_server"):
    print("BAD the server could not verify the certificate it served"); sys.exit(0)
signing, quorum = d.get("signing_stake"), d.get("quorum")
if signing is None or quorum is None or signing < quorum:
    print(f"BAD signing stake {signing} below quorum {quorum}"); sys.exit(0)
raw = d.get("certificate_bincode_hex") or ""
if len(raw) < 64 or any(c not in "0123456789abcdef" for c in raw):
    print("BAD the encoded certificate is missing or not hex"); sys.exit(0)
print(f"OK height {target}, {len(d.get('voters', []))} voters, signing {signing} of quorum "
      f"{quorum}/{d.get('total_stake')}, {len(raw)//2} verifiable bytes")
PYEOF
)
  case "$FVERDICT" in
    OK*) ok "finality certificate served and re-verified: ${FVERDICT#OK }" ;;
    *)   fail "finality endpoint: ${FVERDICT#BAD }" ;;
  esac
  FCODE=$(curl -s -o /dev/null -w '%{http_code}' -m 6 "http://127.0.0.1:$BASE_RPC/finality/99999999")
  [[ "$FCODE" == 404 ]] \
    && ok "a height with no certificate is refused (HTTP 404), not fabricated" \
    || fail "finality endpoint answered HTTP $FCODE for a height it cannot have certified"
else
  echo "  SKIPPED: the chain is too short to have a certified height behind the tip"
fi

echo "[6/6] killing node $((NODES-1)), restarting it, and requiring recovery"
LAST=$((NODES-1))

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

BEFORE=$(height_or_fail "$((BASE_RPC+LAST))" "node $LAST before SIGKILL")
kill -9 "${PIDS[$LAST]}" 2>/dev/null
gone=0
for _ in $(seq 1 15); do
  kill -0 "${PIDS[$LAST]}" 2>/dev/null || { gone=1; break; }
  sleep 1
done
[[ $gone -eq 1 ]] && ok "node $LAST process is gone (SIGKILL, verified by PID)" \
  || fail "node $LAST survived SIGKILL"

sleep 20
SURV=$(height_or_fail "$BASE_RPC" "surviving node 0")
[[ "$SURV" -gt "$BEFORE" ]] && ok "surviving quorum kept advancing while node $LAST was down ($BEFORE -> $SURV)" \
  || fail "chain stalled while one of $NODES nodes was down ($BEFORE -> $SURV)"

plist=""
for j in $(seq 0 $((LAST-1))); do plist="${plist}127.0.0.1:$((BASE_P2P+j)),"; done
"$BINARY" --rpc "127.0.0.1:$((BASE_RPC+LAST))" --p2p-port "$((BASE_P2P+LAST))" \
          --data-dir "$WORK/node-$LAST" --genesis "$GEN" --peers "${plist%,}" \
          --insecure-dev-validator-seed --validator-seed "fixture-node-$LAST" \
          --stake "$STAKE" --snapshot-every-blocks "$SNAPSHOT_EVERY" \
          > "$WORK/node-$LAST.restart.log" 2>&1 &
PIDS[$LAST]=$!
RESTART_PID=${PIDS[$LAST]}
if wait_healthy "$((BASE_RPC+LAST))" 90 && kill -0 "$RESTART_PID" 2>/dev/null; then
  ok "node $LAST restarted from its persisted store (pid $RESTART_PID alive)"
else
  if ! kill -0 "$RESTART_PID" 2>/dev/null; then
    fail "node $LAST exited during restart; anything answering on $((BASE_RPC+LAST)) is NOT this node"
    echo "      last lines of its own log:"
    sed 's/\x1b\[[0-9;]*m//g' "$WORK/node-$LAST.restart.log" | tail -6 | sed 's/^/        /'
    holder=$(lsof -nP -iUDP:"$((BASE_P2P+LAST))" 2>/dev/null | awk 'NR==2{print $2}')
    [[ -n "$holder" ]] && echo "        UDP $((BASE_P2P+LAST)) is held by pid $holder: $(ps -o args= -p "$holder" 2>/dev/null | grep -oE 'data-dir [^ ]+')"
  else
    fail "node $LAST did not come back after restart"
    tail -15 "$WORK/node-$LAST.restart.log" | sed 's/^/      /'
  fi
fi
RID=$(nfield "$((BASE_RPC+LAST))" /node/info validator)
[[ "$RID" == "${ADDRS[$LAST]}" ]] && ok "restarted node still presents the expected validator identity" \
  || fail "restarted node identity is ${RID:0:16}..., expected ${ADDRS[$LAST]:0:16}..."

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
  collect_agreement "$WORK/agreement-after-restart.csv" "$TARGET2"
  V2=$(agreement_verdict "$WORK/agreement-after-restart.csv" "$NODES" "$TARGET2")
  [[ "$V2" == OK* ]] && ok "all $NODES replicas agree at height $TARGET2 AFTER the restart (${V2#OK })" \
    || fail "post-restart agreement at $TARGET2: ${V2#BAD }"

  echo "  comparing the FULL canonical history on all $NODES replicas (1..$TARGET2)"
  FULL=$(full_history_agreement "$TARGET2" "$WORK/full-history" 2>&1) || true
  case "$FULL" in
    OK*) ok "full canonical history agrees: ${FULL#OK }" ;;
    UNAVAILABLE*)
      echo "  SKIPPED: ${FULL#UNAVAILABLE}"
      echo "    a bulk range endpoint is needed to compare every height; the"
      echo "    single-height check above is NOT a substitute and this run does"
      echo "    not establish full-history agreement."
      ;;
    *) printf '%s\n' "$FULL" | sed 's/^/    /'
       fail "full canonical history DIVERGES across replicas" ;;
  esac

  RLOG="$WORK/node-$LAST.restart.log"
  if grep -q "Installed a verified state snapshot" "$RLOG" 2>/dev/null; then
    ok "the restarted node recovered FROM a verified snapshot, replaying only the WAL tail"
    sed 's/\x1b\[[0-9;]*m//g' "$RLOG" | grep -m1 -E "Installed a verified state snapshot|WAL recovery complete" | sed 's/^/    /'
    sed 's/\x1b\[[0-9;]*m//g' "$RLOG" | grep -m1 "WAL recovery complete" | sed 's/^/    /'
  elif grep -q "Published a durable state snapshot" "$WORK/node-$LAST.log" 2>/dev/null; then
    fail "node $LAST published a snapshot before the kill but did not recover from one"
  else
    echo "  NOTE: no snapshot was published before the kill (chain shorter than"
    echo "    the $SNAPSHOT_EVERY-block interval), so snapshot-backed recovery is untested here."
  fi
else
  fail "not every replica reached $TARGET2 after the restart, so post-recovery agreement is untested"
fi

PHASES_COMPLETE=1
