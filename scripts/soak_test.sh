#!/usr/bin/env bash
# ARC Chain - Soak Test
#
# Runs N validator nodes locally and records liveness, height progression,
# state-root agreement, memory and errors.
#
# usage: ./scripts/soak_test.sh [--nodes N] [--duration SECONDS] [--batch-size N]
#                              [--binary PATH] [--max-rss-mb N]
#        ./scripts/soak_test.sh --self-test     prove status propagation
#
# ---------------------------------------------------------------------------
# This file has been rewritten twice, and the second rewrite was needed because
# the first one still could not be trusted:
#
#   v0 could not fail at all. It passed --validator-id/--rpc-port/--batch-size
#   (none of which exist on arc-node), passed --benchmark to a binary its own
#   build step built WITHOUT the benchmark-tools feature that defines it, gave
#   every node the same implicit --data-dir, and never checked that a node
#   started. A 24-hour run would have slept over five dead processes and
#   printed a completed report.
#
#   v1 fixed the flags and added a startup gate, then reported a verdict its own
#   exit status contradicted: `rc` was mutated inside a brace group piped to
#   `tee`, which runs in a SUBSHELL, so every `rc=1` was discarded. Both saved
#   reports said FAIL while their exitcode.txt said 0. It also left the liveness
#   snapshot stale when the monitor loop broke on a dead node, and accepted any
#   HTTP body - including an error page - as a health sample.
#
# Rules now enforced:
#   - status is accumulated in plain variables, never inside a pipeline; the
#     report is written to a file and then printed, so nothing runs in a subshell
#   - the first failure reason is preserved and reported even if later checks
#     also fail, and cleanup cannot discard it
#   - process exit status, report verdict, and exitcode.txt always agree, for
#     startup failure, node death, workload failure, interruption and success
#   - liveness is snapshotted immediately before the loop exits by ANY path
#   - health/snapshot samples must be HTTP 200 AND parse as JSON AND carry the
#     fields being read; anything else is a missing sample, not a zero
#   - state roots are compared at a COMMON HEIGHT, so a node that is merely
#     behind is reported as lag, not as divergence
#   - peak RSS is bounded and exceeding the bound fails the run
# ---------------------------------------------------------------------------

set -uo pipefail   # deliberately NOT -e: status is handled explicitly

NODES=${NODES:-5}
DURATION=${DURATION:-86400}
BATCH_SIZE=${BATCH_SIZE:-10000}
BINARY=${BINARY:-target/release/arc-node}
STARTUP_TIMEOUT=${STARTUP_TIMEOUT:-60}
MAX_RSS_MB=${MAX_RSS_MB:-4096}
LOG_DIR=""

EXIT_STATUS=0
FAILURES=()
FIRST_FAILURE=""
PIDS=(); RPC_PORTS=(); START_TIME=0; STARTUP_OK=0
BENCH_ARGS=(); NODE_ARGS=(--stake 0); NEEDS_SEED=0; PEER_LIST=""
CLEANED=0

fail() {
    FAILURES+=("$1")
    [[ -z "$FIRST_FAILURE" ]] && FIRST_FAILURE="$1"
    EXIT_STATUS=1
}

# ── validated sampling ──────────────────────────────────────────────────────
# A sample is only a sample if it is HTTP 200, parses as JSON, and contains the
# fields being read. Anything else is recorded as missing.
http_json_fields() { # port path field...
    local port="$1" path="$2"; shift 2
    local raw code body
    raw=$(curl -s -w '\n%{http_code}' --max-time 4 "http://127.0.0.1:${port}${path}" 2>/dev/null) || return 1
    code=${raw##*$'\n'}; body=${raw%$'\n'*}
    [[ "$code" == "200" ]] || return 1
    printf '%s' "$body" | python3 -c '
import sys, json
want = sys.argv[1:]
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
if not isinstance(d, dict):
    sys.exit(1)
out = []
for k in want:
    if k not in d or d[k] is None:
        sys.exit(1)
    out.append(str(d[k]))
print(" ".join(out))
' "$@" 2>/dev/null || return 1
}

snapshot_liveness() {
    [[ -z "$LOG_DIR" ]] && return 0
    local i
    for i in $(seq 0 $((NODES - 1))); do
        if [[ ${#PIDS[@]} -gt $i ]] && kill -0 "${PIDS[$i]}" 2>/dev/null; then
            echo yes > "$LOG_DIR/node-${i}.alive"
        else
            echo no > "$LOG_DIR/node-${i}.alive"
        fi
    done
}

# ── acceptance evaluation (runs BEFORE the report, mutates status directly) ──
evaluate_acceptance() {
    local i errs alive maxrss

    if [[ "$STARTUP_OK" != 1 ]]; then
        fail "startup did not complete"
    fi

    for i in $(seq 0 $((NODES - 1))); do
        alive=$(cat "$LOG_DIR/node-${i}.alive" 2>/dev/null || echo unknown)
        [[ "$alive" != yes ]] && fail "node $i not alive at end of run (state=$alive)"
        # grep -c already prints 0 when nothing matches; it just exits 1. An
        # `|| echo 0` here appended a second line and broke every [[ -gt ]].
        errs=$(grep -c -i -E "error|panic|fatal" "$LOG_DIR/node-${i}.log" 2>/dev/null)
        errs=${errs:-0}
        [[ "$errs" -gt 0 ]] && fail "node $i logged ${errs} error/panic/fatal lines"
        maxrss=$(cat "$LOG_DIR/node-${i}.peakrss" 2>/dev/null || echo 0)
        [[ "${maxrss:-0}" -gt "$MAX_RSS_MB" ]] && fail "node $i peak RSS ${maxrss} MB exceeds bound ${MAX_RSS_MB} MB"
    done

    local samples missing
    samples=$(( $(wc -l < "$LOG_DIR/roots.csv" 2>/dev/null || echo 1) - 1 ))
    [[ "$samples" -lt 1 ]] && fail "no height/state-root samples were recorded"
    missing=$(awk -F, 'NR>1 && ($3=="" || $4=="") {n++} END{print n+0}' "$LOG_DIR/roots.csv" 2>/dev/null)
    [[ "${missing:-0}" -gt 0 ]] && fail "${missing} height/state-root samples were missing or invalid"

    # Root agreement is only meaningful between nodes that share a chain.
    #
    # Comparing roots across UNPEERED nodes is meaningless in both directions,
    # and this harness got it wrong once in each: equal roots at different
    # heights were nearly reported as "agreement" (they are determinism - the
    # same transaction sequence from the same genesis), and then different roots
    # at the SAME height were reported as divergence (they are simply two
    # different chains that happen to be the same length). Neither says anything
    # about consensus. So the check runs only when peering was requested, which
    # is the only configuration where a common chain identity exists.
    local diverged=0
    if [[ -n "$PEER_LIST" ]]; then
        diverged=$(awk -F, 'NR>1 && $3!="" && $4!="" {
            key=$3; if (!(key in seen)) { seen[key]=$4 } else if (seen[key]!=$4) { bad[key]=1 }
        } END { n=0; for (k in bad) n++; print n }' "$LOG_DIR/roots.csv" 2>/dev/null)
        diverged=${diverged:-0}
        if [[ "$diverged" -gt 0 ]]; then
            fail "${diverged} height(s) have conflicting state roots among PEERED nodes (divergence, not lag)"
        fi
    fi
    echo "$diverged" > "$LOG_DIR/diverged_heights.txt"

    # Workload actually exercised the block path.
    local maxh
    maxh=$(awk -F, 'NR>1 && $3!="" {if ($3+0>m) m=$3+0} END{print m+0}' "$LOG_DIR/roots.csv" 2>/dev/null)
    echo "${maxh:-0}" > "$LOG_DIR/max_height.txt"
    [[ "${maxh:-0}" -eq 0 ]] && fail "height never left 0 - the block path was never exercised"

    if [[ -n "$PEER_LIST" ]]; then
        local peers
        peers=$(cat "$LOG_DIR/peers.observed" 2>/dev/null || echo 0)
        [[ "${peers:-0}" -eq 0 ]] && fail "peering was requested but no peer ever connected"
    fi
}

generate_report() {
    local total_time=0 i
    [[ "$START_TIME" -gt 0 ]] && total_time=$(( $(date +%s) - START_TIME ))
    {
      echo "================================================================"
      echo " SOAK TEST REPORT"
      echo "================================================================"
      echo "  Binary:     $BINARY"
      echo "  Nodes:      $NODES"
      echo "  Duration:   ${total_time}s (requested ${DURATION}s)"
      echo "  Ended:      $(date)"
      echo "  Startup:    $([ "$STARTUP_OK" = 1 ] && echo "all nodes answered /health" || echo "DID NOT COMPLETE")"
      echo "  Peering:    ${PEER_LIST:-none (independent chains)}"
      echo "  Workload:   $([ ${#BENCH_ARGS[@]} -gt 0 ] && echo "--benchmark --bench-batch $BATCH_SIZE" || echo "NONE (idle)")"
      echo "  RSS bound:  ${MAX_RSS_MB} MB per node"
      echo ""
      echo "  Per-node outcome:"
      for i in $(seq 0 $((NODES - 1))); do
          printf "    node %d: alive=%-8s errors=%-5s log_lines=%-7s peak_rss=%s MB\n" \
            "$i" \
            "$(cat "$LOG_DIR/node-${i}.alive" 2>/dev/null || echo unknown)" \
            "$(grep -c -i -E "error|panic|fatal" "$LOG_DIR/node-${i}.log" 2>/dev/null)" \
            "$(wc -l < "$LOG_DIR/node-${i}.log" 2>/dev/null | tr -d ' ')" \
            "$(cat "$LOG_DIR/node-${i}.peakrss" 2>/dev/null || echo '?')"
      done
      echo ""
      echo "  Chain state (last sample per node):"
      if [[ $(wc -l < "$LOG_DIR/roots.csv" 2>/dev/null || echo 0) -gt 1 ]]; then
          local last
          last=$(tail -n +2 "$LOG_DIR/roots.csv" | awk -F, 'END{print $1}')
          awk -F, -v L="$last" 'NR>1 && $1==L {printf "    node %s: height=%-8s root=%s\n", $2, ($3==""?"MISSING":$3), ($4==""?"MISSING":substr($4,1,18)"...")}' "$LOG_DIR/roots.csv"
          echo "    max height reached: $(cat "$LOG_DIR/max_height.txt" 2>/dev/null || echo '?')"
          if [[ -n "$PEER_LIST" ]]; then
              echo "    heights with conflicting roots: $(cat "$LOG_DIR/diverged_heights.txt" 2>/dev/null || echo '?')"
          else
              echo "    root agreement: NOT CHECKED (nodes are independent chains)"
          fi
          if [[ -z "$PEER_LIST" ]]; then
              echo "    NOTE: nodes were not peered, so they are separate chains and their"
              echo "          state roots are not comparable in either direction. Equal roots"
              echo "          would be determinism (same tx sequence, same genesis), not"
              echo "          agreement; unequal roots at equal heights are just two different"
              echo "          chains of the same length. Neither is evidence about consensus."
          else
              echo "    Peers observed: $(cat "$LOG_DIR/peers.observed" 2>/dev/null || echo 0)"
              echo "    Dial timeouts:  $(cat "$LOG_DIR"/node-*.log 2>/dev/null | grep -c 'Timeout connecting')"
          fi
      else
          echo "    no samples recorded"
      fi
      echo ""
      if [[ ${#FAILURES[@]} -eq 0 ]]; then
          echo "  Verdict: PASS"
      else
          echo "  Verdict: FAIL"
          echo "  First failure: $FIRST_FAILURE"
          local f
          for f in "${FAILURES[@]}"; do echo "    - $f"; done
      fi
      echo ""
      echo "  Logs: $LOG_DIR/   (aggregate.csv, roots.csv, node-*.log)"
      echo "  Exit status: $EXIT_STATUS"
    } > "$LOG_DIR/report.txt" 2>&1
    cat "$LOG_DIR/report.txt"
    echo "$EXIT_STATUS" > "$LOG_DIR/exitcode.txt"
}

cleanup() {
    [[ "$CLEANED" = 1 ]] && return
    CLEANED=1
    trap - EXIT INT TERM
    # Snapshot the real state BEFORE killing anything, so the report describes
    # the run rather than the shutdown.
    snapshot_liveness
    echo ""; echo "Shutting down nodes..."
    local pid
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    sleep 2
    for pid in "${PIDS[@]:-}"; do kill -9 "$pid" 2>/dev/null || true; done
    if [[ -n "$LOG_DIR" ]]; then
        evaluate_acceptance
        generate_report
    fi
    exit "$EXIT_STATUS"
}

# ── self-test ───────────────────────────────────────────────────────────────
if [[ "${1:-}" == "--self-test" ]]; then
    echo "self-test: status propagation out of the report writer"
    LOG_DIR=$(mktemp -d); NODES=1; START_TIME=$(date +%s); STARTUP_OK=1
    trap 'rm -rf "$LOG_DIR"' EXIT
    : > "$LOG_DIR/node-0.log"; echo yes > "$LOG_DIR/node-0.alive"; echo 10 > "$LOG_DIR/node-0.peakrss"
    printf 'elapsed_s,node,height,state_root\n1,0,5,0xaa\n' > "$LOG_DIR/roots.csv"
    evaluate_acceptance; generate_report >/dev/null
    [[ "$EXIT_STATUS" = 0 && "$(cat "$LOG_DIR/exitcode.txt")" = 0 ]] \
        && echo "  PASS A: healthy run -> verdict PASS, exitcode 0" \
        || { echo "  FAIL A: status=$EXIT_STATUS artifact=$(cat "$LOG_DIR/exitcode.txt")"; exit 1; }

    EXIT_STATUS=0; FAILURES=(); FIRST_FAILURE=""
    echo no > "$LOG_DIR/node-0.alive"
    evaluate_acceptance; generate_report >/dev/null
    [[ "$EXIT_STATUS" = 1 && "$(cat "$LOG_DIR/exitcode.txt")" = 1 \
       && "$(grep -c 'Verdict: FAIL' "$LOG_DIR/report.txt")" = 1 ]] \
        && echo "  PASS B: dead node -> verdict FAIL, exitcode 1, report agrees" \
        || { echo "  FAIL B: status=$EXIT_STATUS artifact=$(cat "$LOG_DIR/exitcode.txt")"; exit 1; }

    EXIT_STATUS=0; FAILURES=(); FIRST_FAILURE=""; echo yes > "$LOG_DIR/node-0.alive"
    PEER_LIST="127.0.0.1:9100"; echo 1 > "$LOG_DIR/peers.observed"
    printf 'elapsed_s,node,height,state_root\n1,0,5,0xaa\n1,1,5,0xbb\n' > "$LOG_DIR/roots.csv"
    evaluate_acceptance
    [[ "$EXIT_STATUS" = 1 ]] && echo "  PASS C: PEERED, same height, different roots -> divergence detected" \
        || { echo "  FAIL C: divergence not detected"; exit 1; }

    EXIT_STATUS=0; FAILURES=(); FIRST_FAILURE=""
    printf 'elapsed_s,node,height,state_root\n1,0,5,0xaa\n1,1,7,0xbb\n' > "$LOG_DIR/roots.csv"
    evaluate_acceptance
    [[ "$EXIT_STATUS" = 0 ]] && echo "  PASS D: PEERED, different heights -> lag, not divergence" \
        || { echo "  FAIL D: lag misreported as failure: ${FAILURES[*]}"; exit 1; }

    EXIT_STATUS=0; FAILURES=(); FIRST_FAILURE=""; PEER_LIST=""
    printf 'elapsed_s,node,height,state_root\n1,0,5,0xaa\n1,1,5,0xbb\n' > "$LOG_DIR/roots.csv"
    evaluate_acceptance
    [[ "$EXIT_STATUS" = 0 ]] && echo "  PASS D2: UNPEERED, same height different roots -> not compared" \
        || { echo "  FAIL D2: compared roots across independent chains: ${FAILURES[*]}"; exit 1; }

    EXIT_STATUS=0; FAILURES=(); FIRST_FAILURE=""
    printf 'elapsed_s,node,height,state_root\n1,0,0,0xaa\n' > "$LOG_DIR/roots.csv"
    evaluate_acceptance
    [[ "$EXIT_STATUS" = 1 ]] && echo "  PASS E: height stuck at 0 -> block path never exercised, FAIL" \
        || { echo "  FAIL E: stuck height accepted"; exit 1; }

    EXIT_STATUS=0; FAILURES=(); FIRST_FAILURE=""
    printf 'elapsed_s,node,height,state_root\n1,0,,\n' > "$LOG_DIR/roots.csv"
    evaluate_acceptance
    [[ "$EXIT_STATUS" = 1 ]] && echo "  PASS F: missing sample fields -> FAIL, not treated as zero" \
        || { echo "  FAIL F: missing sample accepted"; exit 1; }

    EXIT_STATUS=0; FAILURES=(); FIRST_FAILURE=""; echo 99999 > "$LOG_DIR/node-0.peakrss"
    printf 'elapsed_s,node,height,state_root\n1,0,5,0xaa\n' > "$LOG_DIR/roots.csv"
    evaluate_acceptance
    [[ "$EXIT_STATUS" = 1 ]] && echo "  PASS G: peak RSS over bound -> FAIL" \
        || { echo "  FAIL G: RSS bound not enforced"; exit 1; }
    echo "SELF-TEST OK"; exit 0
fi

while [[ $# -gt 0 ]]; do
    case $1 in
        --nodes) NODES=$2; shift 2 ;;
        --duration) DURATION=$2; shift 2 ;;
        --batch-size) BATCH_SIZE=$2; shift 2 ;;
        --binary) BINARY=$2; shift 2 ;;
        --max-rss-mb) MAX_RSS_MB=$2; shift 2 ;;
        *) echo "Unknown arg: $1"; exit 2 ;;
    esac
done

LOG_DIR="logs/soak-$(date +%Y%m%d-%H%M%S)"
trap cleanup EXIT INT TERM

echo "================================================================"
echo " ARC Chain - Soak Test"
echo "================================================================"
echo "  Nodes: $NODES   Duration: ${DURATION}s   Batch: $BATCH_SIZE   RSS bound: ${MAX_RSS_MB} MB"
echo "  Binary: $BINARY"
echo "  Logs:   $LOG_DIR"
echo "  Started: $(date)"
mkdir -p "$LOG_DIR"

if [[ ! -x "$BINARY" ]]; then
    echo "[1/3] $BINARY not present - building with benchmark-tools..."
    cargo build --release --bin arc-node --features benchmark-tools 2>&1 | tail -3
    [[ -x "$BINARY" ]] || { fail "binary $BINARY missing after build"; exit 1; }
else
    echo "[1/3] Using $BINARY ($("$BINARY" --version 2>/dev/null || echo 'version unknown'))"
fi

if ! "$BINARY" --help 2>&1 | grep -q -- "--benchmark"; then
    echo ""
    echo "ERROR: $BINARY was built without the 'benchmark-tools' feature, so it"
    echo "       does not accept --benchmark and would generate no load."
    echo "       Rebuild: cargo build --release --bin arc-node --features benchmark-tools"
    echo "       (ALLOW_IDLE_SOAK=1 to run an explicitly workload-free soak.)"
    if [[ "${ALLOW_IDLE_SOAK:-0}" != "1" ]]; then fail "binary lacks benchmark-tools"; exit 1; fi
    echo "       ALLOW_IDLE_SOAK=1 - continuing WITHOUT a workload."
else
    # arc-node gates --benchmark behind its own isolation contract
    # (main.rs:5566): positive stake, explicit dev seed, and NO --genesis; it
    # separately refuses any non-loopback benchmark peer. That combination IS a
    # disposable local devnet, which is why a positive stake is safe here.
    BENCH_ARGS=(--benchmark --bench-batch "$BATCH_SIZE"
                --insecure-dev-validator-seed --stake "${SOAK_STAKE:-5000000}")
    NEEDS_SEED=1
fi

if [[ -n "${ARC_COMMUNITY_RPC_URLS:-}" ]]; then
    echo "ERROR: ARC_COMMUNITY_RPC_URLS is set. Unset it; a soak runs isolated."
    fail "ARC_COMMUNITY_RPC_URLS set"; exit 1
fi

if [[ ${#BENCH_ARGS[@]} -gt 0 ]]; then NODE_ARGS=("${BENCH_ARGS[@]}"); else NODE_ARGS=(--stake 0); fi

if [[ "${SOAK_PEERED:-0}" = "1" && $NODES -gt 1 ]]; then
    for i in $(seq 0 $((NODES - 1))); do PEER_LIST="${PEER_LIST}127.0.0.1:$((9100 + i)),"; done
    PEER_LIST="${PEER_LIST%,}"
    echo "  peering: $PEER_LIST"
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
        ${NODE_ARGS[@]+"${NODE_ARGS[@]}"} \
        $([ "${NEEDS_SEED:-0}" = 1 ] && echo "--validator-seed soak-node-$i") \
        $([ -n "$PEER_LIST" ] && echo "--peers $PEER_LIST") \
        > "$LOG_DIR/node-${i}.log" 2>&1 &
    PIDS+=($!); RPC_PORTS+=("$PORT_RPC")
done
echo "  PIDs: ${PIDS[*]}"

echo ""
echo "  Waiting up to ${STARTUP_TIMEOUT}s for every node to answer a VALID /health..."
deadline=$(( $(date +%s) + STARTUP_TIMEOUT ))
for i in $(seq 0 $((NODES - 1))); do
    ok=0
    while [[ $(date +%s) -lt $deadline ]]; do
        if ! kill -0 "${PIDS[$i]}" 2>/dev/null; then
            echo "  ERROR: node $i exited during startup. Last log lines:"
            tail -15 "$LOG_DIR/node-${i}.log" | sed 's/^/      /'
            fail "node $i exited during startup"
            exit 1
        fi
        if http_json_fields "${RPC_PORTS[$i]}" /health height status >/dev/null; then ok=1; break; fi
        sleep 1
    done
    [[ $ok = 1 ]] || { echo "  ERROR: node $i never returned a valid /health within ${STARTUP_TIMEOUT}s."
                       tail -15 "$LOG_DIR/node-${i}.log" | sed 's/^/      /'
                       fail "node $i never returned a valid /health"; exit 1; }
    echo "    node $i healthy"
done
STARTUP_OK=1

echo ""
echo "[3/3] Monitoring for ${DURATION}s (Ctrl+C stops early and still reports)..."
START_TIME=$(date +%s)
echo "timestamp,elapsed_s,alive_nodes,total_errors,total_rss_mb" > "$LOG_DIR/aggregate.csv"
echo "elapsed_s,node,height,state_root" > "$LOG_DIR/roots.csv"
echo 0 > "$LOG_DIR/peers.observed"

while true; do
    now=$(date +%s); elapsed=$(( now - START_TIME ))
    [[ $elapsed -ge $DURATION ]] && { snapshot_liveness; echo ""; echo "Duration reached."; break; }

    alive=0; total_rss=0
    for i in $(seq 0 $((NODES - 1))); do
        if kill -0 "${PIDS[$i]}" 2>/dev/null; then
            alive=$(( alive + 1 ))
            r=$(ps -o rss= -p "${PIDS[$i]}" 2>/dev/null | awk '{printf "%.0f", $1/1024}'); r=${r:-0}
            total_rss=$(( total_rss + r ))
            prev=$(cat "$LOG_DIR/node-${i}.peakrss" 2>/dev/null || echo 0)
            [[ $r -gt $prev ]] && echo "$r" > "$LOG_DIR/node-${i}.peakrss"
        fi
    done
    errors=$(cat "$LOG_DIR"/node-*.log 2>/dev/null | grep -c -i -E "error|panic|fatal" || true)
    echo "$now,$elapsed,$alive,${errors:-0},$total_rss" >> "$LOG_DIR/aggregate.csv"

    if [[ -n "$PEER_LIST" ]]; then
        p=$(http_json_fields "${RPC_PORTS[0]}" /health peers 2>/dev/null) || p=""
        [[ -n "$p" ]] && echo "$p" > "$LOG_DIR/peers.observed"
    fi
    for n in $(seq 0 $((NODES - 1))); do
        if s=$(http_json_fields "${RPC_PORTS[$n]}" /sync/snapshot/info height state_root); then
            echo "$elapsed,$n,${s% *},${s#* }" >> "$LOG_DIR/roots.csv"
        else
            echo "$elapsed,$n,," >> "$LOG_DIR/roots.csv"
        fi
    done

    if [[ $alive -lt $NODES ]]; then
        snapshot_liveness
        echo ""; echo "ERROR: only $alive/$NODES nodes alive at ${elapsed}s - stopping early."
        fail "only $alive/$NODES nodes alive at ${elapsed}s"
        break
    fi
    printf "\r  [%5ds] alive %d/%d | errors %s | rss %d MB      " "$elapsed" "$alive" "$NODES" "${errors:-0}" "$total_rss"
    sleep 10
done
