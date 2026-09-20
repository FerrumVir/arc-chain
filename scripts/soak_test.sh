#!/usr/bin/env bash
# ARC Chain - Soak Test
#
# Runs N validator nodes on this machine for a sustained period and records
# liveness, height progression, state-root agreement, memory and errors.
#
# Usage: ./scripts/soak_test.sh [--nodes N] [--duration SECONDS] [--batch-size N]
#                              [--binary PATH]
#
# ---------------------------------------------------------------------------
# 2026-09-20: this script was rewritten because the previous version could not
# fail. Three defects, each of which alone made a green report meaningless:
#
#   1. It passed --validator-id, --rpc-port and --batch-size. None of those
#      flags exist on arc-node; the real ones are --rpc <addr>, --p2p-port and
#      --bench-batch. Every node therefore died instantly on a clap parse
#      error. The script never checked startup, so it slept for the full
#      duration and printed a completed report over five dead processes.
#   2. Every node was given the same implicit --data-dir, so even with correct
#      flags they would have fought over one WAL.
#   3. `generate_report` was defined AFTER the monitor loop that its EXIT trap
#      could fire from, so an early Ctrl+C called an undefined function; and
#      `grep -c ... node-*.log` over several files emits "file:count" per line,
#      so the error count written to the CSV was malformed.
#
#   The header also claimed to monitor "state root consistency" and never
#   looked at a state root. It does now.
#
#   4. Even with the right flag names, --benchmark is behind
#      #[cfg(feature = "benchmark-tools")], and the script's own build step was
#      `cargo build --release --bin arc-node` with no features. The binary it
#      built could never accept the workload flag it then passed. The build step
#      now requests the feature, and the script probes the binary for the flag
#      and refuses to run a workload-free "soak" silently.
#
# The script is fail-closed: if a node does not answer /health during startup,
# the soak aborts and exits non-zero rather than producing a vacuous pass.
# ---------------------------------------------------------------------------

set -euo pipefail

NODES=${NODES:-5}
DURATION=${DURATION:-86400}          # 24 hours
BATCH_SIZE=${BATCH_SIZE:-10000}
BINARY=${BINARY:-target/release/arc-node}
STARTUP_TIMEOUT=${STARTUP_TIMEOUT:-60}
LOG_DIR="logs/soak-$(date +%Y%m%d-%H%M%S)"

while [[ $# -gt 0 ]]; do
    case $1 in
        --nodes) NODES=$2; shift 2 ;;
        --duration) DURATION=$2; shift 2 ;;
        --batch-size) BATCH_SIZE=$2; shift 2 ;;
        --binary) BINARY=$2; shift 2 ;;
        *) echo "Unknown arg: $1"; exit 2 ;;
    esac
done

HOURS=$((DURATION / 3600)); MINS=$(( (DURATION % 3600) / 60 ))
PIDS=(); RPC_PORTS=(); START_TIME=0; STARTUP_OK=0
# Declared up front: generate_report runs from an EXIT trap that can fire before
# the capability probe below assigns it, and `set -u` would abort there.
BENCH_ARGS=()

rss_mb() { ps -o rss= -p "$1" 2>/dev/null | awk '{printf "%.0f", $1/1024}'; }
node_json() { curl -s --max-time 3 "http://127.0.0.1:$1$2" 2>/dev/null; }

generate_report() {
    local end_time total_time rc=0
    end_time=$(date +%s); total_time=$(( end_time - ${START_TIME:-end_time} ))
    {
      echo ""
      echo "================================================================"
      echo " SOAK TEST REPORT"
      echo "================================================================"
      echo "  Binary:     $BINARY"
      echo "  Nodes:      $NODES"
      echo "  Duration:   $((total_time / 3600))h $(( (total_time % 3600) / 60 ))m ${total_time}s"
      echo "  Ended:      $(date)"
      echo "  Startup:    $([ "$STARTUP_OK" = 1 ] && echo "all nodes answered /health" || echo "DID NOT COMPLETE")"
      echo "  Workload:   $([ ${#BENCH_ARGS[@]:-0} -gt 0 ] && echo "--benchmark --bench-batch $BATCH_SIZE" || echo "NONE (idle soak)")"
      echo ""
      echo "  Per-node outcome:"
      for i in $(seq 0 $((NODES - 1))); do
          local log="$LOG_DIR/node-${i}.log" errs lines alive
          errs=0; lines=0
          [[ -f "$log" ]] && { errs=$(grep -c -i -E "error|panic|fatal" "$log" 2>/dev/null || true); lines=$(wc -l < "$log" | tr -d ' '); }
          alive=no; [[ ${#PIDS[@]} -gt $i ]] && kill -0 "${PIDS[$i]}" 2>/dev/null && alive=yes
          printf "    node %d: alive=%-3s errors=%-5s log_lines=%-7s peak_rss=%s MB\n" \
                 "$i" "$alive" "${errs:-0}" "$lines" "$(cat "$LOG_DIR/node-${i}.peakrss" 2>/dev/null || echo '?')"
          [[ "$alive" = no ]] && rc=1
          [[ "${errs:-0}" -gt 0 ]] && rc=1
      done
      echo ""
      echo "  Height / state-root agreement (last sample):"
      if [[ -s "$LOG_DIR/roots.csv" ]]; then
          tail -1 "$LOG_DIR/roots.csv"
          local distinct
          distinct=$(tail -1 "$LOG_DIR/roots.csv" | cut -d, -f3- | tr ',' '\n' | grep -v '^$' | sort -u | wc -l | tr -d ' ')
          echo "    distinct state roots across nodes: $distinct"
          [[ "$distinct" != "1" ]] && echo "    NOTE: nodes do not agree on a single state root."
      else
          echo "    no samples recorded"
          rc=1
      fi
      echo ""
      echo "  Logs: $LOG_DIR/   (aggregate.csv, roots.csv, node-*.log)"
      echo "  Verdict: $([ $rc -eq 0 ] && echo PASS || echo FAIL)"
    } | tee -a "$LOG_DIR/report.txt"
    echo "$rc" > "$LOG_DIR/exitcode.txt"
    return $rc
}

cleanup() {
    echo ""; echo "Shutting down nodes..."
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    sleep 2
    for pid in "${PIDS[@]:-}"; do kill -9 "$pid" 2>/dev/null || true; done
    generate_report || true
}
trap cleanup EXIT INT TERM

echo "================================================================"
echo " ARC Chain - Soak Test"
echo "================================================================"
echo "  Nodes: $NODES   Duration: ${HOURS}h ${MINS}m (${DURATION}s)   Batch: $BATCH_SIZE"
echo "  Binary: $BINARY"
echo "  Logs:   $LOG_DIR"
echo "  Started: $(date)"
echo ""
mkdir -p "$LOG_DIR"

if [[ ! -x "$BINARY" ]]; then
    echo "[1/3] $BINARY not present - building release binary with benchmark-tools..."
    cargo build --release --bin arc-node --features benchmark-tools 2>&1 | tail -3
    [[ -x "$BINARY" ]] || { echo "ERROR: $BINARY still not present after build."; exit 1; }
else
    echo "[1/3] Using existing binary $BINARY ($("$BINARY" --version 2>/dev/null || echo 'version unknown'))"
fi

# Capability probe. A soak with no transaction generation exercises restart and
# memory but not throughput, so refuse rather than quietly downgrade.
if ! "$BINARY" --help 2>&1 | grep -q -- "--benchmark"; then
    echo ""
    echo "ERROR: $BINARY was built without the 'benchmark-tools' feature, so it"
    echo "       does not accept --benchmark and would generate no load."
    echo "       Rebuild with:  cargo build --release --bin arc-node --features benchmark-tools"
    echo "       (Set ALLOW_IDLE_SOAK=1 to run an explicitly workload-free soak.)"
    if [[ "${ALLOW_IDLE_SOAK:-0}" != "1" ]]; then exit 1; fi
    echo "       ALLOW_IDLE_SOAK=1 set - continuing WITHOUT a workload."
    BENCH_ARGS=()
else
    BENCH_ARGS=(--benchmark --bench-batch "$BATCH_SIZE")
fi

echo ""
echo "[2/3] Starting $NODES nodes..."
for i in $(seq 0 $((NODES - 1))); do
    PORT_RPC=$((9944 + i)); PORT_P2P=$((9100 + i))
    DATA="$LOG_DIR/data-$i"; mkdir -p "$DATA"
    echo "  node $i: rpc=127.0.0.1:$PORT_RPC p2p=$PORT_P2P data=$DATA"
    "$BINARY" \
        --rpc "127.0.0.1:$PORT_RPC" \
        --p2p-port "$PORT_P2P" \
        --data-dir "$DATA" \
        --stake 0 \
        ${BENCH_ARGS[@]+"${BENCH_ARGS[@]}"} \
        > "$LOG_DIR/node-${i}.log" 2>&1 &
    PIDS+=($!); RPC_PORTS+=("$PORT_RPC")
done
echo "  PIDs: ${PIDS[*]}"

# Fail-closed startup gate. A node that never answers is not a soak.
echo ""
echo "  Waiting up to ${STARTUP_TIMEOUT}s for every node to answer /health..."
deadline=$(( $(date +%s) + STARTUP_TIMEOUT ))
for i in $(seq 0 $((NODES - 1))); do
    ok=0
    while [[ $(date +%s) -lt $deadline ]]; do
        if ! kill -0 "${PIDS[$i]}" 2>/dev/null; then
            echo "  ERROR: node $i exited during startup. Last log lines:"
            tail -15 "$LOG_DIR/node-${i}.log" | sed 's/^/      /'
            exit 1
        fi
        [[ -n "$(node_json "${RPC_PORTS[$i]}" /health)" ]] && { ok=1; break; }
        sleep 1
    done
    [[ $ok = 1 ]] || { echo "  ERROR: node $i never answered /health within ${STARTUP_TIMEOUT}s."; tail -15 "$LOG_DIR/node-${i}.log" | sed 's/^/      /'; exit 1; }
    echo "    node $i healthy"
done
STARTUP_OK=1

echo ""
echo "[3/3] Monitoring for ${HOURS}h ${MINS}m (Ctrl+C stops early and still reports)..."
START_TIME=$(date +%s)
echo "timestamp,elapsed_s,alive_nodes,total_errors,total_rss_mb" > "$LOG_DIR/aggregate.csv"
echo "elapsed_s,heights,state_roots" > "$LOG_DIR/roots.csv"

while true; do
    now=$(date +%s); elapsed=$(( now - START_TIME ))
    [[ $elapsed -ge $DURATION ]] && { echo ""; echo "Duration reached."; break; }

    alive=0; total_rss=0
    for i in $(seq 0 $((NODES - 1))); do
        if kill -0 "${PIDS[$i]}" 2>/dev/null; then
            alive=$(( alive + 1 ))
            r=$(rss_mb "${PIDS[$i]}"); r=${r:-0}; total_rss=$(( total_rss + r ))
            prev=$(cat "$LOG_DIR/node-${i}.peakrss" 2>/dev/null || echo 0)
            [[ $r -gt $prev ]] && echo "$r" > "$LOG_DIR/node-${i}.peakrss"
        fi
    done
    # One number, not one per file: cat then count.
    errors=$(cat "$LOG_DIR"/node-*.log 2>/dev/null | grep -c -i -E "error|panic|fatal" || true)
    echo "$now,$elapsed,$alive,${errors:-0},$total_rss" >> "$LOG_DIR/aggregate.csv"

    heights=""; roots=""
    for p in "${RPC_PORTS[@]}"; do
        j=$(node_json "$p" /sync/snapshot/info)
        heights="$heights$(echo "$j" | sed -nE 's/.*"height":([0-9]+).*/\1/p'),"
        roots="$roots$(echo "$j" | sed -nE 's/.*"state_root":"([^"]*)".*/\1/p'),"
    done
    echo "$elapsed,${heights%,},${roots%,}" >> "$LOG_DIR/roots.csv"

    if [[ $alive -lt $NODES ]]; then
        echo ""; echo "ERROR: only $alive/$NODES nodes alive at ${elapsed}s - stopping early."; break
    fi
    printf "\r  [%5ds] alive %d/%d | errors %s | rss %d MB      " "$elapsed" "$alive" "$NODES" "${errors:-0}" "$total_rss"
    sleep 10
done
