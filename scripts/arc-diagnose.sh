#!/usr/bin/env bash
# ARC community-node and public-fleet diagnostic.
#
# This script never prints process arguments, environment variables, key paths,
# seeds, or validator material. It treats HTTP reachability, DAG motion, block
# production, and canonical-chain agreement as separate facts.
set -Eeuo pipefail

RPC_PORT="${ARC_RPC_PORT:-9944}"
LOCAL_RPC=""
TIMEOUT="${ARC_DIAG_TIMEOUT:-5}"
PUBLIC_SEEDS="${ARC_PUBLIC_SEEDS:-nyc=http://149.28.32.76:9090 lax=http://140.82.16.112:9090 ams=http://136.244.109.1:9090 lhr=http://104.238.171.11:9090 nrt=http://202.182.107.41:9090 sgp=http://149.28.153.31:9090}"

usage() {
    cat <<'EOF'
ARC node diagnostic

Usage:
  bash scripts/arc-diagnose.sh [--port PORT | --rpc URL] [--timeout SECONDS]

Options:
  --port PORT       Local RPC port (default: 9944).
  --rpc URL         Full local RPC origin (default: http://127.0.0.1:PORT).
  --timeout SEC     Per-request timeout (default: 5).
  -h, --help        Show this help.

ARC_PUBLIC_SEEDS may override the public comparison set as whitespace-separated
NAME=URL entries. A support-safe report contains public hashes and counters but
never the node command line, seed phrase, environment, or validator key path.
EOF
}

need_value() {
    [ "$#" -ge 2 ] && [ -n "$2" ] || {
        printf 'error: %s requires a value\n' "$1" >&2
        exit 2
    }
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --port) need_value "$@"; RPC_PORT="$2"; shift 2 ;;
        --rpc) need_value "$@"; LOCAL_RPC="${2%/}"; shift 2 ;;
        --timeout) need_value "$@"; TIMEOUT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'error: unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

case "$RPC_PORT" in ''|*[!0-9]*) printf 'error: invalid RPC port: %s\n' "$RPC_PORT" >&2; exit 2 ;; esac
[ "$RPC_PORT" -ge 1 ] && [ "$RPC_PORT" -le 65535 ] || {
    printf 'error: RPC port must be between 1 and 65535\n' >&2
    exit 2
}
case "$TIMEOUT" in ''|*[!0-9]*) printf 'error: timeout must be a positive integer\n' >&2; exit 2 ;; esac
[ "$TIMEOUT" -ge 1 ] || { printf 'error: timeout must be positive\n' >&2; exit 2; }
[ -n "$LOCAL_RPC" ] || LOCAL_RPC="http://127.0.0.1:$RPC_PORT"

for command_name in curl python3 mktemp sort awk sed pgrep; do
    command -v "$command_name" >/dev/null 2>&1 || {
        printf 'error: required command is missing: %s\n' "$command_name" >&2
        exit 2
    }
done

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    GREEN=$'\033[32m'; RED=$'\033[31m'; YELLOW=$'\033[33m'; BOLD=$'\033[1m'; RESET=$'\033[0m'
else
    GREEN=''; RED=''; YELLOW=''; BOLD=''; RESET=''
fi

pass() { printf '  %sPASS%s  %s\n' "$GREEN" "$RESET" "$*"; }
fail() { printf '  %sFAIL%s  %s\n' "$RED" "$RESET" "$*"; }
warn() { printf '  %sWARN%s  %s\n' "$YELLOW" "$RESET" "$*"; }
section() { printf '\n%s%s%s\n' "$BOLD" "$*" "$RESET"; }

WORK_DIR="$(mktemp -d)"
trap 'rm -rf -- "$WORK_DIR"' EXIT
OVERALL=0

json_fields() {
    python3 - "$1" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        body = json.load(handle)
except Exception:
    raise SystemExit(1)

values = [
    body.get("status", "unknown"),
    body.get("version", "unknown"),
    body.get("height", 0),
    body.get("peers", 0),
    body.get("dag_round", 0),
    body.get("dag_committed", 0),
]
print("\t".join(str(value) for value in values))
PY
}

block_fields() {
    python3 - "$1" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        body = json.load(handle)
    header = body["header"]
    values = [
        body["hash"],
        header["state_root"],
        header.get("proof_hash", ""),
        header.get("timestamp", 0),
    ]
except Exception:
    raise SystemExit(1)
print("\t".join(str(value) for value in values))
PY
}

section "1/4  Local process and RPC"
LOCAL_PIDS="$(pgrep -x arc-node 2>/dev/null || true)"
if [ -n "$LOCAL_PIDS" ]; then
    PID_COUNT="$(printf '%s\n' "$LOCAL_PIDS" | awk 'NF {count += 1} END {print count + 0}')"
    pass "$PID_COUNT arc-node process(es) detected; arguments intentionally redacted"
else
    warn "no process named exactly arc-node was detected"
fi

LOCAL_HEALTH="$WORK_DIR/local-health.json"
if curl --fail --silent --show-error --max-time "$TIMEOUT" \
    "$LOCAL_RPC/health" -o "$LOCAL_HEALTH" 2>/dev/null; then
    if LOCAL_ROW="$(json_fields "$LOCAL_HEALTH")"; then
        IFS=$'\t' read -r LOCAL_STATUS LOCAL_VERSION LOCAL_HEIGHT LOCAL_PEERS LOCAL_ROUND LOCAL_COMMITTED <<<"$LOCAL_ROW"
        pass "RPC $LOCAL_RPC responds: status=$LOCAL_STATUS version=$LOCAL_VERSION height=$LOCAL_HEIGHT peers=$LOCAL_PEERS"
        printf '        DAG round=%s process-commit-counter=%s (neither proves chain agreement)\n' \
            "$LOCAL_ROUND" "$LOCAL_COMMITTED"
    else
        fail "RPC returned malformed health JSON"
        OVERALL=1
        LOCAL_HEIGHT=0
    fi
else
    fail "RPC $LOCAL_RPC is unreachable"
    OVERALL=1
    LOCAL_HEIGHT=0
fi

section "2/4  Public validator reachability and block liveness"
REACHABLE=0
COMMON_HEIGHT=""
SEED_NAMES=()
for entry in $PUBLIC_SEEDS; do
    name="${entry%%=*}"
    url="${entry#*=}"
    if [ -z "$name" ] || [ "$url" = "$entry" ]; then
        fail "invalid ARC_PUBLIC_SEEDS entry: $entry"
        OVERALL=1
        continue
    fi
    case "$url" in http://*|https://*) ;; *) fail "$name has a non-HTTP RPC URL"; OVERALL=1; continue ;; esac

    health_file="$WORK_DIR/$name-health.json"
    if ! curl --fail --silent --show-error --max-time "$TIMEOUT" "$url/health" -o "$health_file" 2>/dev/null; then
        fail "$name RPC unreachable"
        OVERALL=1
        continue
    fi
    if ! row="$(json_fields "$health_file")"; then
        fail "$name returned malformed health JSON"
        OVERALL=1
        continue
    fi
    IFS=$'\t' read -r status version height peers round committed <<<"$row"
    if [ "$height" -le 0 ] 2>/dev/null; then
        fail "$name reports no retained block height"
        OVERALL=1
        continue
    fi

    tip_file="$WORK_DIR/$name-tip.json"
    if ! curl --fail --silent --show-error --max-time "$TIMEOUT" "$url/block/$height" -o "$tip_file" 2>/dev/null \
        || ! tip_row="$(block_fields "$tip_file")"; then
        fail "$name health is reachable but its reported tip block is not"
        OVERALL=1
        continue
    fi
    IFS=$'\t' read -r tip_hash tip_root _tip_proof tip_timestamp <<<"$tip_row"
    now_ms="$(( $(date +%s) * 1000 ))"
    age_seconds="$(( (now_ms - tip_timestamp) / 1000 ))"
    [ "$age_seconds" -ge 0 ] || age_seconds=0
    if [ "$age_seconds" -le 600 ]; then
        pass "$name v$version rpc=$status height=$height age=${age_seconds}s peers=$peers"
    else
        fail "$name v$version height=$height is stale by ${age_seconds}s"
        OVERALL=1
    fi
    printf '        DAG round=%s process-commit-counter=%s\n' "$round" "$committed"
    printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$url" "$height" "$tip_hash" "$tip_root" \
        >> "$WORK_DIR/reachable.tsv"
    SEED_NAMES+=("$name")
    REACHABLE=$((REACHABLE + 1))
    if [ -z "$COMMON_HEIGHT" ] || [ "$height" -lt "$COMMON_HEIGHT" ]; then
        COMMON_HEIGHT="$height"
    fi
done

if [ "$REACHABLE" -lt 5 ]; then
    fail "only $REACHABLE/6 public validators expose a comparable retained chain; strict quorum needs 5"
    OVERALL=1
else
    pass "$REACHABLE/6 public validators expose health and their reported tip block"
fi

section "3/4  Same-height canonical-chain proof"
if [ -z "$COMMON_HEIGHT" ]; then
    fail "no common retained height can be tested"
    OVERALL=1
else
    : > "$WORK_DIR/common-signatures.tsv"
    while IFS=$'\t' read -r name url _height _tip_hash _tip_root; do
        block_file="$WORK_DIR/$name-common.json"
        if ! curl --fail --silent --show-error --max-time "$TIMEOUT" \
            "$url/block/$COMMON_HEIGHT" -o "$block_file" 2>/dev/null \
            || ! common_row="$(block_fields "$block_file")"; then
            fail "$name cannot serve block $COMMON_HEIGHT"
            OVERALL=1
            continue
        fi
        IFS=$'\t' read -r block_hash state_root proof_hash _timestamp <<<"$common_row"
        printf '%s\t%s\t%s\t%s\n' "$name" "$block_hash" "$state_root" "$proof_hash" \
            >> "$WORK_DIR/common-signatures.tsv"
        printf '        %-4s block=%s state=%s proof=%s\n' \
            "$name" "${block_hash:0:12}" "${state_root:0:12}" "${proof_hash:0:12}"
    done < "$WORK_DIR/reachable.tsv"

    SIGNATURE_COUNT="$(cut -f2-3 "$WORK_DIR/common-signatures.tsv" | sort -u | awk 'NF {n += 1} END {print n + 0}')"
    PROOF_ZERO_COUNT="$(awk -F '\t' '$4 ~ /^0+$/ {n += 1} END {print n + 0}' "$WORK_DIR/common-signatures.tsv")"
    COMPARED_COUNT="$(awk 'NF {n += 1} END {print n + 0}' "$WORK_DIR/common-signatures.tsv")"
    if [ "$COMPARED_COUNT" -ge 5 ] && [ "$SIGNATURE_COUNT" -eq 1 ]; then
        pass "$COMPARED_COUNT validators agree at height $COMMON_HEIGHT on block hash and state root"
    else
        fail "$COMPARED_COUNT validators produced $SIGNATURE_COUNT distinct block/state pairs at height $COMMON_HEIGHT"
        OVERALL=1
    fi
    if [ "$PROOF_ZERO_COUNT" -gt 0 ]; then
        fail "$PROOF_ZERO_COUNT/$COMPARED_COUNT sampled blocks carry an all-zero proof hash"
        OVERALL=1
    else
        pass "every sampled block carries a non-zero proof hash"
    fi

    if [ "$LOCAL_HEIGHT" -ge "$COMMON_HEIGHT" ] 2>/dev/null; then
        LOCAL_BLOCK="$WORK_DIR/local-common.json"
        if curl --fail --silent --show-error --max-time "$TIMEOUT" \
            "$LOCAL_RPC/block/$COMMON_HEIGHT" -o "$LOCAL_BLOCK" 2>/dev/null \
            && local_common_row="$(block_fields "$LOCAL_BLOCK")"; then
            IFS=$'\t' read -r local_hash local_root _local_proof _local_timestamp <<<"$local_common_row"
            if awk -F '\t' -v hash="$local_hash" -v root="$local_root" \
                '$2 == hash && $3 == root {found=1} END {exit !found}' "$WORK_DIR/common-signatures.tsv"; then
                pass "local node matches a public block/state pair at height $COMMON_HEIGHT"
            else
                fail "local node disagrees with every public block/state pair at height $COMMON_HEIGHT"
                OVERALL=1
            fi
        else
            fail "local node reports sufficient height but cannot serve block $COMMON_HEIGHT"
            OVERALL=1
        fi
    else
        warn "local height $LOCAL_HEIGHT has not reached public comparison height $COMMON_HEIGHT"
    fi
fi

section "4/4  Result"
if [ "$OVERALL" -eq 0 ]; then
    pass "RPC liveness, recent block production, quorum reachability, and same-height agreement all passed"
    printf '        This proves the checks above only; use the reward receipt and restart harness for full readiness.\n'
else
    fail "one or more independent readiness checks failed"
    printf '        Save this output for support. It contains no process arguments or validator secrets.\n'
fi

exit "$OVERALL"
