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
  # `local a=$1 b="$WORK/node-$a"` does NOT work: `local` is a builtin and its
  # arguments are expanded before it runs, so `$a` there is whatever the CALLER
  # had - in the sampling loop, the last node index. The self-test caught this
  # by restarting node 0 against node 3's data directory, which then refused to
  # start on the lock and left the node down for the rest of the run.
  local i=$1
  local d="$WORK/node-$i"
  local peers=""
  local j
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
    kill_at=$(date +%s)
    [[ -z ${SOAK_FIRST_KILL_ELAPSED:-} ]] && export SOAK_FIRST_KILL_ELAPSED=$(( kill_at - START ))
    kill -9 "${PIDS[$victim]}" 2>/dev/null || true
    wait "${PIDS[$victim]}" 2>/dev/null || true
    KILLS=$(( KILLS + 1 ))
    sleep 20
    start_node "$victim"
    # Bound the wait in WALL CLOCK, not iterations. Each probe of a node that
    # is not up costs a curl timeout, so "90 attempts" was nine minutes, not
    # ninety seconds - long enough to look like the whole harness had hung.
    rejoin_deadline=$(( $(date +%s) + 120 ))
    rejoined=0
    while [[ $(date +%s) -lt $rejoin_deadline ]]; do
      if [[ -n "$(nfield "$((BASE_RPC+victim))" /health status)" ]]; then
        rejoined=1; REJOINS=$(( REJOINS + 1 )); break
      fi
      sleep 2
    done
    DOWNTIME_TOTAL=$(( ${DOWNTIME_TOTAL:-0} + $(date +%s) - kill_at ))
    if [[ $rejoined -eq 1 ]]; then
      note "node $victim restarted and answering after $(( $(date +%s) - kill_at ))s (rejoins so far: $REJOINS)"
    else
      note "NODE $victim DID NOT COME BACK within 120s; its last lines:"
      sed 's/\x1b\[[0-9;]*m//g' "$WORK/node-$victim.log" | tail -4 | tee -a "$EVENTS"
    fi
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
  echo "controlled downtime: ${DOWNTIME_TOTAL:-0}s total across $KILLS kills"
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
# Longest run of consecutive samples in which NO node's height advanced. A
# network-wide stall is the failure a soak exists to find, so it is measured
# directly instead of glanced at over the last few samples.
by_time = {}
for r in rows:
    try:
        by_time.setdefault(int(r["elapsed_s"]), []).append(float(r["height"]))
    except (ValueError, KeyError):
        pass
times = sorted(by_time)
longest = run = 0
for a, b in zip(times, times[1:]):
    if max(by_time[b], default=0) <= max(by_time[a], default=0):
        run += 1
        longest = max(longest, run)
    else:
        run = 0
print()
print(f"longest network-wide stall: {longest} consecutive samples with no height advancing")
open(sys.argv[1] + ".stall", "w").write(str(longest))

# Throughput after the first controlled fault, against the rate before it.
# "Still advancing" is not "recovered": the first post-fix self-test advanced
# the whole time and still ran about fifteen times slower after one restart
# than before it, for as long as it was observed. A soak that reports only
# advance-or-not would call that a pass.
import os
kill_at = os.environ.get("SOAK_FIRST_KILL_ELAPSED")
rate_before = rate_after = None
if kill_at and times:
    k = int(kill_at)
    pre = [t for t in times if t <= k and max(by_time[t], default=0) > 0]
    post = [t for t in times if t > k + 30]
    if len(pre) >= 2:
        a, b = pre[0], pre[-1]
        rate_before = (max(by_time[b]) - max(by_time[a])) / max(1, b - a)
    if len(post) >= 2:
        a, b = post[0], post[-1]
        rate_after = (max(by_time[b]) - max(by_time[a])) / max(1, b - a)
if rate_before and rate_after is not None:
    ratio = rate_after / rate_before if rate_before > 0 else 0.0
    print(f"throughput: {rate_before:.2f} heights/s before the first fault, "
          f"{rate_after:.2f} after it ({ratio:.0%} of baseline)")
    open(sys.argv[1] + ".ratio", "w").write(f"{ratio:.4f}")
else:
    print("throughput: not enough samples either side of the first fault to compare")
down = sum(1 for r in rows if r.get("alive") == "0")
print(f"samples that observed a node down: {down} of {len(rows)}")
print("  (sampling pauses during a controlled kill, so controlled downtime is")
print("   reported separately below rather than inferred from samples)")
PYEOF
  echo
  echo "error/warn classes in node logs (top 10, grouped by message shape):"
  # Grouped in python, not sed: macOS sed has no \S or \s, so the first
  # version never stripped the timestamps and every line counted once - a
  # "top 10" of ten singletons that told you nothing.
  python3 - "$WORK" <<'PYEOF'
import glob, re, collections, sys
ansi = re.compile(r"\x1b\[[0-9;]*m")
line_re = re.compile(r"^\S+\s+(ERROR|WARN)\s+\S+:\s*(.*)$")
counts = collections.Counter()
for path in glob.glob(sys.argv[1] + "/node-*.log"):
    for raw in open(path, errors="replace"):
        m = line_re.match(ansi.sub("", raw).strip())
        if not m:
            continue
        shape = re.sub(r"0x[0-9a-f]+|\b[0-9a-f]{16,}\b", "<h>", m.group(2))
        shape = re.sub(r"=\S+", "=N", shape)[:100]
        counts[(m.group(1), shape)] += 1
for (level, shape), n in counts.most_common(10):
    print(f"  {n:>6}  {level:<5} {shape}")
if not counts:
    print("  none")
PYEOF
  echo
  echo "SAFETY VIOLATION lines (must be zero):"
  printf '  %s\n' "$(cat "$WORK"/node-*.log 2>/dev/null | grep -c 'SAFETY VIOLATION' || echo 0)"
  echo
  STALL=$(cat "$SAMPLES.stall" 2>/dev/null || echo 0)
  if [[ $AGREE_BAD -gt 0 ]]; then
    echo "VERDICT: FAIL - replicas disagreed on canonical history during the run"
  elif [[ ${STALL:-0} -ge ${STALL_LIMIT_SAMPLES:-6} ]]; then
    echo "VERDICT: FAIL - the whole network stopped advancing for $STALL consecutive samples"
  elif python3 -c "import sys; r=float(open('$SAMPLES.ratio').read()); sys.exit(0 if r < ${MIN_RECOVERED_RATIO:-0.5} else 1)" 2>/dev/null; then
    echo "VERDICT: FAIL - throughput after a controlled fault stayed below ${MIN_RECOVERED_RATIO:-0.5}x baseline"
    echo "         (advancing is not the same as recovered)"
  elif [[ $KILLS -gt 0 && $REJOINS -lt $KILLS ]]; then
    echo "VERDICT: FAIL - a killed node did not come back"
  else
    echo "VERDICT: PASS"
  fi
  echo "samples: $SAMPLES"
  echo "events:  $EVENTS"
} | tee "$REPORT"
