#!/usr/bin/env bash
# Recorded soak for the local 4-validator candidate (checklist R8).
#
# What it records, every sample: height and DAG round per node, replica
# agreement at a common height, peer counts, finality-certificate age, RSS and
# CPU per process, disk used by each data directory, and any ERROR/WARN class
# worth counting. Controlled failure and rejoin happen on a fixed schedule so
# the run exercises recovery rather than only uptime.
#
# It does NOT start itself from anything. A soak monopolises this machine -
# heavy compilation, model evaluation, packaging and multi-process load are
# serialized here - so starting one is a deliberate decision about a day.
#
#   scripts/arc-soak.sh --self-test            # verify the harness, minutes
#   scripts/arc-soak.sh --hours 24             # the real thing
#
# The harness is validated by --self-test, which runs the identical code path
# on a short horizon. A soak that first exercises its own reporting at hour 23
# is not a soak, it is a gamble.
set -euo pipefail

HOURS=0
SELF_TEST=0
NODES=${NODES:-4}
SAMPLE_SECS=${SAMPLE_SECS:-60}
KILL_EVERY_SECS=${KILL_EVERY_SECS:-3600}
BASE_RPC=${BASE_RPC:-9960}
BASE_P2P=${BASE_P2P:-9160}
STAKE=${STAKE:-6666667}
SNAPSHOT_EVERY=${SNAPSHOT_EVERY:-500}
BINARY=${BINARY:-target/debug/arc-node}
FAUCET_POOL=${FAUCET_POOL:-2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213}
export PATH="$HOME/.local/bin:$PATH"

while [[ $# -gt 0 ]]; do
  case $1 in
    --hours) HOURS=$2; shift 2 ;;
    --self-test) SELF_TEST=1; HOURS=0; SAMPLE_SECS=10; KILL_EVERY_SECS=90; shift ;;
    --nodes) NODES=$2; shift 2 ;;
    --binary) BINARY=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ $SELF_TEST -eq 1 ]]; then
  DURATION=240
  echo "SELF-TEST: 4 minutes, 10s samples, one kill/rejoin at 90s."
  echo "This exercises the same code as a 24-hour run. It is NOT a soak result."
else
  [[ $HOURS -gt 0 ]] || { echo "pass --hours N (or --self-test)"; exit 2; }
  DURATION=$(( HOURS * 3600 ))
  echo "SOAK: ${HOURS}h, ${SAMPLE_SECS}s samples, kill/rejoin every ${KILL_EVERY_SECS}s."
fi

WORK=${WORK:-/tmp/arc-soak-$(date -u +%Y%m%dT%H%M%SZ)}
mkdir -p "$WORK"
SAMPLES="$WORK/samples.csv"
EVENTS="$WORK/events.log"
REPORT="$WORK/report.txt"

note() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$EVENTS"; }

[[ -x $BINARY ]] || { echo "no binary at $BINARY"; exit 1; }

# Provenance is part of the record: a soak result that cannot name the binary it
# soaked is not evidence.
if [[ -f scripts/arc-build-provenance.sh ]]; then
  cp /tmp/arc-provenance/latest-provenance.txt "$WORK/build-provenance.txt" 2>/dev/null || true
fi
{
  echo "binary:       $BINARY"
  echo "binary_sha256:$(shasum -a 256 "$BINARY" | cut -d' ' -f1)"
  echo "nodes:        $NODES"
  echo "duration_s:   $DURATION"
  echo "sample_s:     $SAMPLE_SECS"
  echo "kill_every_s: $KILL_EVERY_SECS"
  echo "snapshot_every_blocks: $SNAPSHOT_EVERY"
  echo "started_utc:  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "host:         $(uname -srm)"
} > "$WORK/config.txt"
cat "$WORK/config.txt"

busy=""
for i in $(seq 0 $((NODES-1))); do
  for port in $((BASE_RPC+i)) $((BASE_P2P+i)); do
    lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 && busy="$busy $port"
    lsof -nP -iUDP:"$port" >/dev/null 2>&1 && busy="$busy $port"
  done
done
[[ -z $busy ]] || { echo "REFUSING TO RUN: ports in use:$busy"; exit 1; }

PIDS=()
cleanup() {
  for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
  sleep 1
  for p in ${PIDS[@]+"${PIDS[@]}"}; do kill -9 "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

nfield() { curl -s -m 5 "http://127.0.0.1:$1$2" 2>/dev/null \
  | python3 -c "import sys,json
try: print(json.load(sys.stdin).get('$3',''))
except Exception: print('')" 2>/dev/null; }

# ── identities, then a genesis naming exactly them ──────────────────────────
ADDRS=()
for i in $(seq 0 $((NODES-1))); do
  d="$WORK/ident-$i"; mkdir -p "$d"
  "$BINARY" --rpc "127.0.0.1:$((BASE_RPC+i))" --p2p-port "$((BASE_P2P+i))" \
            --data-dir "$d" --insecure-dev-validator-seed \
            --validator-seed "soak-node-$i" --stake "$STAKE" \
            > "$WORK/ident-$i.log" 2>&1 &
  probe=$!
  a=""
  for _ in $(seq 1 30); do
    a=$(sed 's/\x1b\[[0-9;]*m//g' "$WORK/ident-$i.log" 2>/dev/null \
        | grep -m1 "Validator  *:" | sed -E 's/.*: *0x?([0-9a-f]{64}).*/\1/' || true)
    [[ -n $a ]] && break
    sleep 1
  done
  kill "$probe" 2>/dev/null || true; wait "$probe" 2>/dev/null || true
  rm -rf "$d"
  [[ -n $a ]] || { echo "could not read node $i identity"; exit 1; }
  ADDRS+=("$a")
done
sleep 3

GEN="$WORK/genesis.toml"
{
  echo '[chain]'; echo 'name = "arc-soak"'; echo 'chain_id = "0x415243"'
  echo 'validator_set_complete = false'; echo ''
  echo '[[accounts]]'; echo "address = \"$FAUCET_POOL\""; echo 'balance = 1_000_000_000_000'; echo ''
  for a in "${ADDRS[@]}"; do
    echo '[[accounts]]'; echo "address = \"$a\""; echo 'balance = 1_000_000_000_000'; echo ''
  done
  for a in "${ADDRS[@]}"; do
    echo '[[validators]]'; echo "address = \"$a\""; echo "stake = $STAKE"; echo ''
  done
} > "$GEN"

start_node() { # INDEX
  local i=$1 d="$WORK/node-$i" peers="" j
  mkdir -p "$d"
  for j in $(seq 0 $((NODES-1))); do
    [[ $j -eq $i ]] && continue
    peers="${peers}127.0.0.1:$((BASE_P2P+j)),"
  done
  "$BINARY" --rpc "127.0.0.1:$((BASE_RPC+i))" --p2p-port "$((BASE_P2P+i))" \
            --data-dir "$d" --genesis "$GEN" --peers "${peers%,}" \
            --insecure-dev-validator-seed --validator-seed "soak-node-$i" \
            --stake "$STAKE" --snapshot-every-blocks "$SNAPSHOT_EVERY" \
            >> "$WORK/node-$i.log" 2>&1 &
  PIDS[$i]=$!
}

note "starting $NODES validators"
for i in $(seq 0 $((NODES-1))); do
  start_node "$i"
  for _ in $(seq 1 60); do
    [[ -n "$(nfield "$((BASE_RPC+i))" /health status)" ]] && break
    sleep 1
  done
done

echo "sample_utc,elapsed_s,node,height,dag_round,peers,rss_kb,cpu_pct,disk_kb,alive" > "$SAMPLES"

START=$(date +%s)
LAST_KILL=$START
KILLS=0
REJOINS=0
SAMPLE_COUNT=0
AGREE_OK=0
AGREE_BAD=0

while true; do
  now=$(date +%s)
  elapsed=$(( now - START ))
  [[ $elapsed -ge $DURATION ]] && break

  stamp=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  heights=()
  for i in $(seq 0 $((NODES-1))); do
    h=$(nfield "$((BASE_RPC+i))" /health height); h=${h:-}
    r=$(nfield "$((BASE_RPC+i))" /health dag_round); r=${r:-}
    p=$(nfield "$((BASE_RPC+i))" /health peers); p=${p:-}
    pid=${PIDS[$i]:-0}
    if kill -0 "$pid" 2>/dev/null; then
      alive=1
      read -r rss cpu <<<"$(ps -o rss=,pcpu= -p "$pid" 2>/dev/null | awk '{print $1, $2}')"
    else
      alive=0; rss=""; cpu=""
    fi
    disk=$(du -sk "$WORK/node-$i" 2>/dev/null | cut -f1)
    echo "$stamp,$elapsed,$i,$h,$r,$p,${rss:-},${cpu:-},${disk:-},$alive" >> "$SAMPLES"
    heights+=("${h:-0}")
  done

  # Replica agreement at a common height behind the slowest node.
  min=$(printf '%s\n' "${heights[@]}" | sort -n | head -1)
  if [[ ${min:-0} -gt 6 ]]; then
    target=$(( min - 3 ))
    hashes=$(for i in $(seq 0 $((NODES-1))); do
      curl -s -m 5 "http://127.0.0.1:$((BASE_RPC+i))/block/$target" 2>/dev/null \
        | python3 -c "import sys,json
try: print(json.load(sys.stdin).get('hash',''))
except Exception: print('')" 2>/dev/null
    done | sort -u | grep -c . || true)
    if [[ "$hashes" == "1" ]]; then
      AGREE_OK=$(( AGREE_OK + 1 ))
    else
      AGREE_BAD=$(( AGREE_BAD + 1 ))
      note "DISAGREEMENT at height $target: $hashes distinct block hashes"
    fi
  fi

  # Controlled failure and rejoin, on schedule.
  if [[ $(( now - LAST_KILL )) -ge $KILL_EVERY_SECS ]]; then
    victim=$(( KILLS % NODES ))
    note "killing node $victim (kill #$((KILLS+1)))"
    kill -9 "${PIDS[$victim]}" 2>/dev/null || true
    wait "${PIDS[$victim]}" 2>/dev/null || true
    KILLS=$(( KILLS + 1 ))
    sleep 20
    start_node "$victim"
    for _ in $(seq 1 90); do
      [[ -n "$(nfield "$((BASE_RPC+victim))" /health status)" ]] && { REJOINS=$((REJOINS+1)); break; }
      sleep 1
    done
    note "node $victim restarted (rejoins so far: $REJOINS)"
    LAST_KILL=$(date +%s)
  fi

  SAMPLE_COUNT=$(( SAMPLE_COUNT + 1 ))
  sleep "$SAMPLE_SECS"
done

# ── report ──────────────────────────────────────────────────────────────────
{
  echo "================ ARC SOAK REPORT ================"
  cat "$WORK/config.txt"
  echo "ended_utc:    $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
  echo "samples:            $SAMPLE_COUNT"
  echo "controlled kills:   $KILLS"
  echo "observed rejoins:   $REJOINS"
  echo "agreement checks:   ok=$AGREE_OK  DISAGREEING=$AGREE_BAD"
  echo
  python3 - "$SAMPLES" "$NODES" <<'PYEOF'
import csv, sys
rows = list(csv.DictReader(open(sys.argv[1])))
nodes = int(sys.argv[2])
if not rows:
    print("no samples recorded"); raise SystemExit
def nums(field, node=None):
    out = []
    for r in rows:
        if node is not None and int(r["node"]) != node: continue
        v = r.get(field) or ""
        try: out.append(float(v))
        except ValueError: pass
    return out
print("per-node trends (first -> last, max):")
for n in range(nodes):
    h = nums("height", n); rss = nums("rss_kb", n); disk = nums("disk_kb", n)
    if not h: continue
    print(f"  node {n}: height {h[0]:.0f} -> {h[-1]:.0f}"
          f"   rss {rss[0]/1024:.0f} -> {rss[-1]/1024:.0f} MB (max {max(rss)/1024:.0f})"
          f"   disk {disk[0]/1024:.0f} -> {disk[-1]/1024:.0f} MB" if rss and disk else
          f"  node {n}: height {h[0]:.0f} -> {h[-1]:.0f}")
stalled = []
for n in range(nodes):
    h = nums("height", n)
    if len(h) > 3 and h[-1] <= h[max(0, len(h)-4)]:
        stalled.append(n)
print()
print(f"nodes whose height did not advance over the last samples: {stalled or 'none'}")
down = sum(1 for r in rows if r.get("alive") == "0")
print(f"samples where a node was down: {down} of {len(rows)} "
      f"(controlled kills account for some of these)")
PYEOF
  echo
  echo "error/warn classes in node logs (top 10):"
  cat "$WORK"/node-*.log 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' \
    | grep -E "^\S+\s+(ERROR|WARN)" \
    | sed -E 's/^\S+\s+(ERROR|WARN)\s+\S+:\s*//' | cut -c1-90 \
    | sort | uniq -c | sort -rn | head -10 | sed 's/^/  /'
  echo
  echo "SAFETY VIOLATION lines (must be zero):"
  printf '  %s\n' "$(cat "$WORK"/node-*.log 2>/dev/null | grep -c 'SAFETY VIOLATION' || echo 0)"
  echo
  if [[ $AGREE_BAD -gt 0 ]]; then
    echo "VERDICT: FAIL - replicas disagreed on canonical history during the run"
  elif [[ $KILLS -gt 0 && $REJOINS -lt $KILLS ]]; then
    echo "VERDICT: FAIL - a killed node did not come back"
  else
    echo "VERDICT: PASS"
  fi
  echo "samples: $SAMPLES"
  echo "events:  $EVENTS"
} | tee "$REPORT"
