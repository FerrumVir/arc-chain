#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: arc-export-volatile-state.sh cannot safely operate the current validator fleet.' \
    'No action was taken. Use the approved manifest-bound recovery capture workflow.' >&2
exit 78
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — export the volatile, unreplicated node state before any restart
#
# WHY THIS EXISTS
#
# Three things on a seed live in process memory with no WAL entry and no
# snapshot, and are destroyed by a restart:
#
#   1. `inference_results`   — the recorded sharded runs. On 2026-08-17 LHR
#                              held 15, and that was the network's ENTIRE
#                              stock of genuine sharded output.
#   2. `community_workers`   — the worker scoreboard. Registrations are
#                              node-local and NOT replicated between seeds,
#                              so each seed holds a different set.
#   3. `sharded_runs_total`  — the per-node run/byte counters.
#
# Chain state is NOT in that category: blocks, height and accounts replay from
# `state.wal` on startup, and the DAG WAL recovers on boot. This script exists
# for the three things that do not come back.
#
# It is the read-only step that must happen BEFORE a rolling restart. Running
# it costs nothing and forecloses nothing.
#
# SAFETY
#
#   - GET only. This script never POSTs and never mutates a seed.
#   - It NEVER calls `/community/list`. That handler PRUNES the worker
#     registry as a side effect; calling it is what empties the scoreboard
#     you were about to export. `/workers/scoreboard` does not prune.
#   - Dry-run is the DEFAULT. Pass --run to actually write files.
#   - A non-200 is recorded as a non-200. The script never writes an empty or
#     error body to a file that looks like captured data.
#
# USAGE
#
#   bash scripts/arc-export-volatile-state.sh              # dry run: show the plan
#   bash scripts/arc-export-volatile-state.sh --run        # perform the export
#   bash scripts/arc-export-volatile-state.sh --run --out DIR
#
# EXIT CODES
#
#   0  every CRITICAL endpoint captured on every reachable seed
#   1  at least one CRITICAL endpoint failed — do NOT restart that seed yet
#   2  bad usage / missing dependency
# ─────────────────────────────────────────────────────────────────────────────

# No `set -e`: one transient curl failure must not abort a salvage run that
# still has five other seeds to capture.
set -u

RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'
BOLD=$'\033[1m'; DIM=$'\033[2m'; RESET=$'\033[0m'

# ── Seeds ────────────────────────────────────────────────────────────────────
# name:host. Kept literal rather than parsed from testnet-seeds.txt because
# that file lists P2P ports (9091/443); these are the RPC ports (9090).
SEEDS=(
    "NYC:149.28.32.76"
    "LAX:140.82.16.112"
    "AMS:136.244.109.1"
    "LHR:104.238.171.11"
    "NRT:202.182.107.41"
    "SGP:149.28.153.31"
)
RPC_PORT="${ARC_RPC_PORT:-9090}"
TIMEOUT="${ARC_EXPORT_TIMEOUT:-30}"

# ── Endpoints ────────────────────────────────────────────────────────────────
# CRITICAL = irreplaceable on restart. A failure here blocks the restart.
# CONTEXT  = reconstructible or forensically useful, but not irreplaceable.
#
# `/community/list` is deliberately absent and must stay absent — see SAFETY.
CRITICAL_ENDPOINTS=(
    "/inference/results"      # the recorded sharded runs — the whole point
    "/workers/scoreboard"     # node-local worker registry, not replicated
    "/inference/attestations" # attestation history held by this node
)
CONTEXT_ENDPOINTS=(
    "/health"
    "/block/latest"
    "/validators"             # the divergent sets — evidence for the stall
    "/stats"
    "/shards"
    "/models"
    "/inference/latency_stats" # the poisoned EWMA, before a restart clears it
    "/inference/cache_stats"
    "/economics/rewards"
    "/network/info"           # 404s on v0.7.2/v0.7.9 — recorded as such
)

DRY_RUN=1
OUT_ROOT=""

while [ $# -gt 0 ]; do
    case "$1" in
        --run)   DRY_RUN=0; shift ;;
        --out)   OUT_ROOT="${2:-}"; shift 2 ;;
        -h|--help)
            sed -n '2,46p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *)
            printf "%s[FAIL]%s unknown argument: %s\n" "$RED" "$RESET" "$1" >&2
            exit 2 ;;
    esac
done

command -v curl >/dev/null || { printf "%s[FAIL]%s curl not found\n" "$RED" "$RESET" >&2; exit 2; }

# Portable sha256 — macOS ships shasum, Linux ships sha256sum.
sha256_of() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null; then shasum -a 256 "$1" | awk '{print $1}'
    else echo "unavailable"; fi
}

json_escape() { python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$1" 2>/dev/null || printf '"%s"' "$1"; }

STAMP="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
[ -z "$OUT_ROOT" ] && OUT_ROOT="arc-state-export"
OUT_DIR="${OUT_ROOT}/${STAMP}"

printf "\n%s┌─ ARC volatile-state export ─────────────────────────────────┐%s\n" "$BOLD" "$RESET"
printf "%s│%s  captured at (UTC): %s\n" "$BOLD" "$RESET" "$STAMP"
printf "%s│%s  seeds: %d   endpoints: %d critical + %d context\n" \
    "$BOLD" "$RESET" "${#SEEDS[@]}" "${#CRITICAL_ENDPOINTS[@]}" "${#CONTEXT_ENDPOINTS[@]}"
printf "%s│%s  output: %s\n" "$BOLD" "$RESET" "$OUT_DIR"
printf "%s└─────────────────────────────────────────────────────────────┘%s\n\n" "$BOLD" "$RESET"

if [ "$DRY_RUN" -eq 1 ]; then
    printf "%sDRY RUN%s — nothing will be written. Pass %s--run%s to execute.\n\n" \
        "$YELLOW$BOLD" "$RESET" "$BOLD" "$RESET"
    printf "Would issue %sGET only%s, and would create:\n\n" "$BOLD" "$RESET"
    for entry in "${SEEDS[@]}"; do
        name="${entry%%:*}"; host="${entry##*:}"
        printf "  %s%s%s (%s)\n" "$BOLD" "$name" "$RESET" "$host"
        for ep in "${CRITICAL_ENDPOINTS[@]}"; do
            printf "    %sCRITICAL%s  GET http://%s:%s%s\n" "$YELLOW" "$RESET" "$host" "$RPC_PORT" "$ep"
        done
        for ep in "${CONTEXT_ENDPOINTS[@]}"; do
            printf "    %scontext   GET http://%s:%s%s%s\n" "$DIM" "$host" "$RPC_PORT" "$ep" "$RESET"
        done
    done
    printf "\n  plus %s/manifest.json and %s/SUMMARY.md\n" "$OUT_DIR" "$OUT_DIR"
    printf "\n%sNot requested, by design:%s /community/list (it prunes the worker registry),\n" "$BOLD" "$RESET"
    printf "and every POST route.\n\n"
    exit 0
fi

mkdir -p "$OUT_DIR" || { printf "%s[FAIL]%s cannot create %s\n" "$RED" "$RESET" "$OUT_DIR" >&2; exit 2; }

MANIFEST="${OUT_DIR}/manifest.json"
printf '{\n  "captured_at_utc": "%s",\n  "rpc_port": "%s",\n  "files": [\n' "$STAMP" "$RPC_PORT" > "$MANIFEST"
MANIFEST_FIRST=1

CRITICAL_FAILURES=0
TOTAL_OK=0
TOTAL_FAIL=0
SUMMARY_ROWS=""

# fetch <name> <host> <endpoint> <tier>
fetch() {
    local name="$1" host="$2" ep="$3" tier="$4"
    local slug; slug="$(printf '%s' "$ep" | sed 's#^/##; s#[/?=]#_#g')"
    local dest="${OUT_DIR}/${name}${ep:+_}${slug}.json"
    local url="http://${host}:${RPC_PORT}${ep}"

    local body_file; body_file="$(mktemp)"
    local code
    code="$(curl -sS -m "$TIMEOUT" -o "$body_file" -w '%{http_code}' "$url" 2>/dev/null)"
    local bytes; bytes="$(wc -c < "$body_file" | tr -d ' ')"

    if [ "$code" = "200" ] && [ "$bytes" -gt 0 ]; then
        mv "$body_file" "$dest"
        local sum; sum="$(sha256_of "$dest")"
        TOTAL_OK=$((TOTAL_OK + 1))
        [ "$MANIFEST_FIRST" -eq 0 ] && printf ',\n' >> "$MANIFEST"
        MANIFEST_FIRST=0
        printf '    {"seed": "%s", "endpoint": "%s", "tier": "%s", "http": %s, "bytes": %s, "sha256": "%s", "file": "%s"}' \
            "$name" "$ep" "$tier" "$code" "$bytes" "$sum" "$(basename "$dest")" >> "$MANIFEST"
        printf "    %s✓%s %-26s %s%s bytes%s\n" "$GREEN" "$RESET" "$ep" "$DIM" "$bytes" "$RESET"
        return 0
    fi

    # Not 200. Record the failure as a failure — never as data.
    rm -f "$body_file"
    TOTAL_FAIL=$((TOTAL_FAIL + 1))
    [ "$MANIFEST_FIRST" -eq 0 ] && printf ',\n' >> "$MANIFEST"
    MANIFEST_FIRST=0
    printf '    {"seed": "%s", "endpoint": "%s", "tier": "%s", "http": %s, "bytes": 0, "sha256": null, "file": null}' \
        "$name" "$ep" "$tier" "${code:-0}" >> "$MANIFEST"

    if [ "$tier" = "CRITICAL" ]; then
        CRITICAL_FAILURES=$((CRITICAL_FAILURES + 1))
        printf "    %s✗ CRITICAL%s %-20s HTTP %s\n" "$RED" "$RESET" "$ep" "${code:-no-response}"
    else
        printf "    %s· %-26s HTTP %s%s\n" "$DIM" "$ep" "${code:-no-response}" "$RESET"
    fi
    return 1
}

for entry in "${SEEDS[@]}"; do
    name="${entry%%:*}"; host="${entry##*:}"
    printf "  %s%s%s (%s)\n" "$BOLD" "$name" "$RESET" "$host"

    for ep in "${CRITICAL_ENDPOINTS[@]}"; do fetch "$name" "$host" "$ep" "CRITICAL"; done
    for ep in "${CONTEXT_ENDPOINTS[@]}"; do fetch "$name" "$host" "$ep" "context"; done

    # Per-seed headline figures, read back from what we just captured — never
    # from memory, and never restated from a doc.
    res_file="${OUT_DIR}/${name}_inference_results.json"
    sb_file="${OUT_DIR}/${name}_workers_scoreboard.json"
    n_runs="n/a"; n_workers="n/a"
    [ -f "$res_file" ] && n_runs="$(python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
if isinstance(d,list): print(len(d))
else:
    for k in ("results","inference_results","count","total"):
        v=d.get(k)
        if isinstance(v,list): print(len(v)); break
        if isinstance(v,int): print(v); break
    else: print(len(d) if isinstance(d,dict) else "?")
' "$res_file" 2>/dev/null || echo "?")"
    [ -f "$sb_file" ] && n_workers="$(python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
print(d.get("count_total","?"))
' "$sb_file" 2>/dev/null || echo "?")"

    printf "    %s→ recorded runs: %s · workers: %s%s\n\n" "$DIM" "$n_runs" "$n_workers" "$RESET"
    SUMMARY_ROWS="${SUMMARY_ROWS}| ${name} | ${host} | ${n_runs} | ${n_workers} |"$'\n'
done

printf '\n  ],\n  "critical_failures": %s,\n  "files_captured": %s,\n  "requests_failed": %s\n}\n' \
    "$CRITICAL_FAILURES" "$TOTAL_OK" "$TOTAL_FAIL" >> "$MANIFEST"

# ── Human-readable summary ───────────────────────────────────────────────────
{
    printf '# ARC volatile-state export — %s\n\n' "$STAMP"
    printf 'Read-only capture of the three things a restart destroys:\n'
    printf '`inference_results`, the worker scoreboard, and the per-node counters.\n\n'
    printf '| Seed | Host | Recorded runs | Workers |\n'
    printf '|---|---|---:|---:|\n'
    printf '%s' "$SUMMARY_ROWS"
    printf '\nFiles captured: %s · requests failed: %s · CRITICAL failures: %s\n\n' \
        "$TOTAL_OK" "$TOTAL_FAIL" "$CRITICAL_FAILURES"
    printf 'Every figure above was read back from the captured files in this\n'
    printf 'directory, not from any prior document. Verify with `manifest.json`.\n'
} > "${OUT_DIR}/SUMMARY.md"

printf "%s─────────────────────────────────────────────────────────────%s\n" "$BOLD" "$RESET"
printf "  captured %s files · %s failed requests\n" "$TOTAL_OK" "$TOTAL_FAIL"
printf "  %s\n  %s\n" "$MANIFEST" "${OUT_DIR}/SUMMARY.md"

if [ "$CRITICAL_FAILURES" -gt 0 ]; then
    printf "\n%s%s✗ %s CRITICAL endpoint(s) failed.%s\n" "$BOLD" "$RED" "$CRITICAL_FAILURES" "$RESET"
    printf "  Do NOT restart the affected seed — its irreplaceable state is not banked.\n\n"
    exit 1
fi

printf "\n%s%s✓ every critical endpoint captured on every seed.%s\n" "$BOLD" "$GREEN" "$RESET"
printf "  The volatile state is banked; a rolling restart no longer loses it.\n\n"
exit 0
