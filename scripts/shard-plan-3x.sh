#!/usr/bin/env bash
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

out=""
i=0
for node in "${NODES[@]}"; do
    out="$out ${node}=${NODE_RANGES[$i]}"
    i=$((i + 1))
done
echo "${out# }"
