#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Emits a 3×-replication --shard-map string for rolling-upgrade.sh.
#
# Every layer range is held by REPLICATION (default 3) distinct nodes, so no
# single node failure ever leaves a range uncovered. The canonical 32-layer,
# six-node, 3x plan is a balanced block design: every node holds exactly 16
# layers. Custom dimensions use the deterministic round-robin fallback below.
#
# Usage:
#   scripts/shard-plan-3x.sh [N_LAYERS] [RANGE_COUNT] [REPLICATION] [NODE1 NODE2 ...]
#   Defaults: N_LAYERS=32 RANGE_COUNT=6 REPLICATION=3 NODES=NYC LAX AMS LHR NRT SGP
#
# Example:
#   scripts/shard-plan-3x.sh 32 6 3 NYC LAX AMS LHR NRT SGP
#   → NYC=0:6,12:17,17:22 LAX=0:6,12:17,22:27 ...
#
# Drop SAO and JNB from the node list while their RPC is broken. Bring them
# back later by re-running the plan with 8 nodes and redeploying.
#
# Feed directly to rolling-upgrade.sh:
#   ./scripts/rolling-upgrade.sh --shard-map="$(./scripts/shard-plan-3x.sh)"
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

N_LAYERS="${1:-32}"
RANGES="${2:-6}"
REPL="${3:-3}"
shift 3 2>/dev/null || shift $# 2>/dev/null || true
if [ "$#" -gt 0 ]; then
    NODES=("$@")
else
    NODES=(NYC LAX AMS LHR NRT SGP)
fi

N="${#NODES[@]}"
if [ "$N" -lt "$REPL" ]; then
    echo "ERROR: need at least REPLICATION=$REPL nodes, got $N" >&2
    exit 1
fi

# Compute layer range boundaries so total layers is evenly split.
BASE=$(( N_LAYERS / RANGES ))
REM=$(( N_LAYERS - BASE * RANGES ))

STARTS=(); ENDS=()
cursor=0
for r in $(seq 0 $((RANGES - 1))); do
    STARTS+=("$cursor")
    width=$BASE
    if [ "$r" -lt "$REM" ]; then
        width=$((width + 1))
    fi
    cursor=$((cursor + width))
    ENDS+=("$cursor")
done
if [ "$cursor" -ne "$N_LAYERS" ]; then
    echo "ERROR: range layout mismatch ($cursor != $N_LAYERS)" >&2
    exit 1
fi

# Assignment grid: ranges[r] → replica_nodes. Use round-robin with a per-range
# offset so no node holds two copies of the same range, and load balances.
# bash 3.2 compatible: parallel arrays keyed by node index, not associative.
NODE_RANGES=()
for _ in "${NODES[@]}"; do
    NODE_RANGES+=("")
done
node_idx_of() {
    local name="$1"
    local i=0
    for n in "${NODES[@]}"; do
        if [ "$n" = "$name" ]; then echo "$i"; return; fi
        i=$((i + 1))
    done
    echo "-1"
}
CANONICAL_ROWS=("0 1 2" "3 4 5" "0 1 3" "0 2 4" "1 4 5" "2 3 5")
for r in $(seq 0 $((RANGES - 1))); do
    if [ "$N_LAYERS" -eq 32 ] && [ "$RANGES" -eq 6 ] && [ "$REPL" -eq 3 ] && [ "$N" -eq 6 ]; then
        replica_indices="${CANONICAL_ROWS[$r]}"
    else
        replica_indices=""
        for k in $(seq 0 $((REPL - 1))); do
            replica_indices="$replica_indices $(( (r + k) % N ))"
        done
    fi
    for idx in $replica_indices; do
        piece="${STARTS[$r]}:${ENDS[$r]}"
        if [ -z "${NODE_RANGES[$idx]}" ]; then
            NODE_RANGES[$idx]="$piece"
        else
            NODE_RANGES[$idx]="${NODE_RANGES[$idx]},${piece}"
        fi
    done
done

# Emit in the format shard_flags_for_node's multi-range parser expects:
#   NODE=RANGE[,RANGE]... NODE=RANGE[,RANGE]...
out=""
i=0
for node in "${NODES[@]}"; do
    out="$out ${node}=${NODE_RANGES[$i]}"
    i=$((i + 1))
done
echo "${out# }"
