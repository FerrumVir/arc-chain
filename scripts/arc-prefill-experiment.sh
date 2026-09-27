#!/bin/bash
# Watchdogged launcher for the E1(b) batched-prefill experiment.
#
# Why this exists: the first 500-token attempt on this 16 GB host produced no
# output at all and was followed by a WindowServer watchdog panic. The machine
# was already ~21 GB into swap when it launched, because product test suites
# were running at the same time. This script therefore (a) refuses to start a
# second model-loading experiment, (b) samples memory every 2 s, (c) kills the
# run if pressure crosses a bound instead of letting the host wedge, and
# (d) records the command, samples, exit status and outcome durably so an
# interruption cannot erase the evidence again.
#
# usage: arc-prefill-experiment.sh TAG PROMPT_LEN CHUNK REPEATS
set -u

TAG="${1:?tag}"; LEN="${2:?prompt_len}"; CHUNK="${3:-64}"; REPS="${4:-2}"; MAXWALL="${5:-1800}"
OUT=/Users/excaulibur/work/outputs/arc-chain-readiness-20260919
BIN=/Users/excaulibur/work/arc-chain-readiness-20260919/target/release/examples/batched_prefill_experiment
GGUF=/Users/excaulibur/.arc/models/standard.gguf
LOCK=/tmp/arc-model-experiment.lock

# Bounds. Baseline swap is captured at launch; the run is killed if swap grows
# past baseline+DELTA or past ABS, or if the process RSS exceeds RSS_CAP.
# Bounds are absolute, not baseline-relative. The first attempt used a
# baseline+4 GB bound and tripped 14 s into model loading: on a host with
# ~60 MB free, macOS evicts the loader's own just-written pages, so swap grows
# at roughly the allocation rate while process RSS stays low. That is expected
# during a 7.8 GB load and is not by itself the wedge condition. The wedge
# condition observed on 2026-09-19 was ~21 GB of swap with the UI starved, so
# the cap sits well below it and a wall-clock bound catches a run that thrashes
# without tripping any single threshold.
SWAP_ABS_MB=14000
RSS_CAP_MB=11500
SAMPLE_SEC=2

swap_used_mb() { sysctl -n vm.swapusage | sed -E 's/.*used = ([0-9.]+)M.*/\1/'; }
free_mb() { vm_stat | awk '/Pages free/{gsub(/\./,"",$3); printf "%.0f", $3*16384/1048576}'; }

# Single-experiment lock. macOS ships neither flock nor a usable shlock in all
# images, so this is a plain PID file: stale entries from the reboot are
# reclaimed, a live one refuses.
if [ -f "$LOCK" ] && kill -0 "$(cat "$LOCK" 2>/dev/null)" 2>/dev/null; then
  echo "REFUSED: model experiment PID $(cat "$LOCK") is still running" >&2; exit 3
fi
if pgrep -f "examples/batched_prefill_experiment" >/dev/null 2>&1; then
  echo "REFUSED: a batched_prefill_experiment process is already running" >&2; exit 3
fi
echo $$ > "$LOCK"
trap 'rm -f "$LOCK"' EXIT

S="$OUT/stageb2-$TAG"
CMD="$BIN $GGUF $LEN $CHUNK $REPS"
BASE_SWAP=$(swap_used_mb)
{
  echo "command: $CMD"
  echo "started: $(date '+%Y-%m-%d %H:%M:%S %Z')"
  echo "host: $(sysctl -n hw.model) $(sysctl -n hw.memsize | awk '{printf "%.0f GB", $1/1073741824}') cores=$(sysctl -n hw.ncpu)"
  echo "baseline_swap_used_mb: $BASE_SWAP"
  echo "baseline_free_mb: $(free_mb)"
  echo "bounds: swap_abs=${SWAP_ABS_MB}MB rss_cap=${RSS_CAP_MB}MB max_wall=${MAXWALL}s"
  echo "uptime: $(uptime)"
} > "$S.env"

echo "t_s,rss_mb,swap_used_mb,free_mb,pageins_per_s,load1" > "$S.mem.csv"
echo "RUNNING" > "$S.status"

# nice: keep the window server ahead of the benchmark so a slow run degrades
# into a slow run rather than an unresponsive desktop.
nice -n 5 "$BIN" "$GGUF" "$LEN" "$CHUNK" "$REPS" > "$S.jsonl" 2> "$S.log" &
PID=$!
T0=$(date +%s); PEAK=0; VERDICT=""

pageins() { vm_stat | awk '/Pageins/{gsub(/\./,"",$2); print $2}'; }
PI_PREV=$(pageins); PI_T=$(date +%s)

while kill -0 "$PID" 2>/dev/null; do
  RSS=$(ps -o rss= -p "$PID" 2>/dev/null | awk '{printf "%.0f", $1/1024}'); RSS=${RSS:-0}
  SW=$(swap_used_mb); FR=$(free_mb); L1=$(uptime | sed -E 's/.*averages?: ([0-9.]+).*/\1/')
  NOW=$(date +%s); PI_NOW=$(pageins)
  DT=$(( NOW - PI_T )); [ "$DT" -lt 1 ] && DT=1
  PIPS=$(( (PI_NOW - PI_PREV) / DT )); PI_PREV=$PI_NOW; PI_T=$NOW
  [ "$RSS" -gt "$PEAK" ] 2>/dev/null && PEAK=$RSS
  ELAPSED=$(( NOW - T0 ))
  echo "$ELAPSED,$RSS,$SW,$FR,$PIPS,$L1" >> "$S.mem.csv"
  if awk -v s="$SW" -v a="$SWAP_ABS_MB" 'BEGIN{exit !(s > a)}'; then
    VERDICT="ABORTED_SWAP_CAP swap_used=${SW}MB cap=${SWAP_ABS_MB}MB"; break
  fi
  if [ "$RSS" -gt "$RSS_CAP_MB" ] 2>/dev/null; then
    VERDICT="ABORTED_RSS_CAP rss=${RSS}MB cap=${RSS_CAP_MB}MB"; break
  fi
  if [ "$ELAPSED" -gt "$MAXWALL" ]; then
    VERDICT="ABORTED_WALL_CLOCK elapsed=${ELAPSED}s max=${MAXWALL}s"; break
  fi
  sleep "$SAMPLE_SEC"
done

if [ -n "$VERDICT" ]; then
  kill -TERM "$PID" 2>/dev/null; sleep 3; kill -KILL "$PID" 2>/dev/null
  wait "$PID" 2>/dev/null; RC=124
else
  wait "$PID"; RC=$?
  VERDICT=$([ "$RC" -eq 0 ] && echo "COMPLETED" || echo "FAILED exit=$RC")
fi

{
  echo "$VERDICT"
  echo "exit_code: $RC"
  echo "peak_rss_mb: $PEAK"
  echo "wall_s: $(( $(date +%s) - T0 ))"
  echo "final_swap_used_mb: $(swap_used_mb)"
  echo "peak_pageins_per_s: $(awk -F, 'NR>1 && $5+0>m {m=$5} END{print m+0}' "$S.mem.csv")"
  echo "ended: $(date '+%Y-%m-%d %H:%M:%S %Z')"
} > "$S.status"
echo "$RC" > "$S.exitcode.txt"
cat "$S.status"
exit "$RC"
