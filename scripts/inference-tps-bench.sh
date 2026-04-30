#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - inference TPS + minimum-throughput benchmark
#
# Fires N inference requests against the testnet and reports:
#   - successful inferences
#   - tokens per second (chain-side, via output_tokens / total_ms)
#   - end-to-end requests per second (wall-clock through the script)
#   - per-request latency (min/median/max)
#   - per-token latency
#
# Use this to prove the chain meets a minimum TPS / N-inferences target -
# e.g. "we run 50 inferences in under 5 minutes" → 1 inference per 6 s
# average, which is well within today's 6-seed pipeline.
#
# Usage:
#   ./scripts/inference-tps-bench.sh
#   ./scripts/inference-tps-bench.sh --requests=20 --max-tokens=3 --concurrency=4
#   ./scripts/inference-tps-bench.sh --coordinator=http://149.28.32.76:9090
#
# Defaults are sized for the live 6-seed testnet (~50–60 s per token):
#   requests=10, max_tokens=3, concurrency=2 → ~3–4 min total wall time.
# ─────────────────────────────────────────────────────────────────────────────

set -u

REQUESTS=10
MAX_TOKENS=3
CONCURRENCY=2
COORDINATOR=""
ENDPOINT="run_consensus"  # or run_sharded
PROMPT_PREFIX="Benchmark prompt"

for arg in "$@"; do
    case "$arg" in
        --requests=*)     REQUESTS="${arg#--requests=}" ;;
        --max-tokens=*)   MAX_TOKENS="${arg#--max-tokens=}" ;;
        --concurrency=*)  CONCURRENCY="${arg#--concurrency=}" ;;
        --coordinator=*)  COORDINATOR="${arg#--coordinator=}" ;;
        --endpoint=*)     ENDPOINT="${arg#--endpoint=}" ;;
        --prompt=*)       PROMPT_PREFIX="${arg#--prompt=}" ;;
        -h|--help)
            grep -E "^# " "$0" | sed -E 's/^# ?//'
            exit 0
            ;;
    esac
done

# Auto-pick a coordinator if not given. AMS first since its shard
# registry is consistently clean (no stub announces).
SEEDS=(
    "http://136.244.109.1:9090"   # AMS
    "http://149.28.32.76:9090"    # NYC
    "http://104.238.171.11:9090"  # LHR
    "http://202.182.107.41:9090"  # NRT
    "http://149.28.153.31:9090"   # SGP
    "http://140.82.16.112:9090"   # LAX
)
if [ -z "$COORDINATOR" ]; then
    for s in "${SEEDS[@]}"; do
        if curl -sf -m 5 -o /dev/null "$s/health"; then
            COORDINATOR="$s"
            break
        fi
    done
    if [ -z "$COORDINATOR" ]; then
        echo "no coordinator reachable; pass --coordinator=URL" >&2
        exit 1
    fi
fi

OUT_DIR="$(mktemp -d -t arc-tps-bench.XXXXXX)"
trap 'rm -rf "$OUT_DIR"' EXIT

echo "════════════════════════════════════════════════════════════════════════"
echo " ARC Chain - inference TPS benchmark"
echo "════════════════════════════════════════════════════════════════════════"
echo " coordinator: $COORDINATOR"
echo " endpoint:    /inference/$ENDPOINT"
echo " requests:    $REQUESTS"
echo " max_tokens:  $MAX_TOKENS  (per request)"
echo " concurrency: $CONCURRENCY"
echo

START_NS=$(python3 -c "import time;print(int(time.time()*1e9))")

fire_one() {
    local idx=$1
    local prompt="${PROMPT_PREFIX} ${idx}"
    local out_file="$OUT_DIR/req-${idx}.json"
    local t0=$(python3 -c "import time;print(time.time())")
    local body="{\"input\":\"${prompt}\",\"max_tokens\":${MAX_TOKENS},\"k\":3}"
    if curl -sf -m 600 -X POST "${COORDINATOR}/inference/${ENDPOINT}" \
        -H 'Content-Type: application/json' -d "$body" > "$out_file" 2>/dev/null
    then
        local t1=$(python3 -c "import time;print(time.time())")
        printf '%s %.3f\n' "ok" "$(python3 -c "print($t1 - $t0)")" > "$OUT_DIR/timing-${idx}"
    else
        printf '%s 0\n' "fail" > "$OUT_DIR/timing-${idx}"
    fi
}

# Concurrent firing. Use background jobs + wait.
for i in $(seq 1 "$REQUESTS"); do
    fire_one "$i" &
    # Throttle to CONCURRENCY at a time
    if [ $((i % CONCURRENCY)) -eq 0 ]; then
        wait
    fi
done
wait

END_NS=$(python3 -c "import time;print(int(time.time()*1e9))")
WALL_MS=$(python3 -c "print(($END_NS - $START_NS) / 1e6)")

# Aggregate.
python3 <<PYEOF
import json, glob, os, statistics

out_dir = "$OUT_DIR"
wall_ms = $WALL_MS
n_req   = $REQUESTS
max_tok = $MAX_TOKENS
endpoint = "$ENDPOINT"

ok = 0
fail = 0
latencies = []  # per-request seconds
total_tokens = 0
chain_total_ms = 0   # sum of total_ms reported by the chain (compute time)
hashes = []
sample_outputs = []

for f in sorted(glob.glob(os.path.join(out_dir, "req-*.json"))):
    idx = os.path.basename(f).split('-')[1].split('.')[0]
    timing_file = os.path.join(out_dir, f"timing-{idx}")
    if os.path.exists(timing_file):
        with open(timing_file) as t:
            line = t.read().strip().split()
            if line[0] == "ok":
                latencies.append(float(line[1]))
            else:
                fail += 1
                continue
    else:
        fail += 1
        continue

    try:
        with open(f) as fh:
            d = json.load(fh)
        ok += 1
        total_tokens += d.get("tokens_generated", 0)
        chain_total_ms += d.get("total_ms", 0)
        hashes.append(d.get("output_hash", "?"))
        if len(sample_outputs) < 3:
            sample_outputs.append((d.get("output", "")[:60], d.get("output_hash", "?")[:14]))
    except Exception:
        fail += 1

print()
print("─" * 72)
print(" Results")
print("─" * 72)
print(f"  successful:      {ok}/{n_req}  ({100.0 * ok / n_req if n_req else 0:.1f}%)")
print(f"  failed:          {fail}/{n_req}")
print(f"  wall time:       {wall_ms / 1000:.2f} s")
print(f"  requests/sec:    {(ok / (wall_ms / 1000)) if wall_ms > 0 else 0:.3f}  (end-to-end through this script)")
if total_tokens > 0 and chain_total_ms > 0:
    chain_tps = total_tokens / (chain_total_ms / 1000)
    print(f"  chain tokens/s:  {chain_tps:.2f}  (sum_tokens / sum_chain_total_ms - model compute only)")
    print(f"  ms / token:      {chain_total_ms / total_tokens:.0f} ms")
if latencies:
    latencies.sort()
    print(f"  per-request min: {min(latencies):.2f} s")
    print(f"  per-request p50: {statistics.median(latencies):.2f} s")
    print(f"  per-request max: {max(latencies):.2f} s")

if hashes:
    unique = len(set(hashes))
    print(f"  unique hashes:   {unique}/{ok}  ({'PROMPT ISOLATION ✓' if unique == ok else 'COLLISIONS DETECTED ✗ - see known model issue'})")

print()
print("─" * 72)
print(" Sample outputs")
print("─" * 72)
for out, h in sample_outputs:
    print(f"  hash={h}..  output={out!r}")
print()

# Minimum-throughput evaluation
target_tps = float(os.environ.get("MIN_TPS", "0.05"))   # 1 req per 20s default
target_count = int(os.environ.get("MIN_INFERENCES", "1"))
actual_tps = (ok / (wall_ms / 1000)) if wall_ms > 0 else 0
print("─" * 72)
print(" Minimum-throughput check")
print("─" * 72)
print(f"  required: ≥ {target_count} successful inferences AND ≥ {target_tps} req/s")
print(f"  actual:   {ok} successful, {actual_tps:.3f} req/s")
if ok >= target_count and actual_tps >= target_tps:
    print("  ✓ PASS")
    rc = 0
else:
    print("  ✗ FAIL")
    rc = 1
import sys; sys.exit(rc)
PYEOF
