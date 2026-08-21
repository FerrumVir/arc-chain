#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — rolling restart of the STALLED seeds, one at a time
#
# WHAT THIS IS FOR
#
# On 2026-08-18 four seeds (AMS, NRT, SGP, LHR) had not sealed a block in ~8
# days while answering {"status":"ok"} and advancing dag_round normally. The
# leading explanation is validator-set divergence:
#
#   genesis.toml declares  6 validators
#   live /validators, stake > 0:
#       NYC  8   LAX  7        <- still sealing
#       AMS 10   LHR 10        <- stalled
#       NRT 10   SGP 10        <- stalled
#
# The four that believe in 10 are exactly the four that stopped. StateDB's
# seed_genesis_validators() documents this mechanism: dynamic validators added
# in a prior process lifetime survive state.wal replay but diverge between
# peers, so peers disagree on the set size, therefore on the 2/3 quorum
# threshold, and BFT consensus stalls.
#
# That function runs at boot (arc-node/src/main.rs) and CLEARS the set before
# reseeding from genesis. So a restart is the documented remedy -- provided the
# process is relaunched WITH --genesis. Without it the reseed is skipped
# entirely and the divergence returns immediately. This script refuses to
# restart a node whose live argv lacks --genesis.
#
# WHAT A RESTART COSTS, AND WHY THAT IS NOW PAYABLE
#
# Chain state survives: blocks, height and accounts replay from state.wal, and
# the DAG WAL recovers on boot. What does NOT survive is process memory:
# inference_results, the community worker registry, and sharded_runs_total.
# Run scripts/arc-export-volatile-state.sh FIRST -- this script verifies that
# export exists and is complete before it will touch anything.
#
# SAFETY
#
#   - Dry-run is the DEFAULT. Pass --run to actually restart anything.
#   - NYC and LAX are NEVER touched. They are the only seeds sealing; if the
#     roll goes wrong they are the entire remaining network.
#   - One seed at a time. Each must be sealing again before the next is
#     touched. First failure aborts the whole run.
#   - arc-self-heal is stopped on the target for the duration and restarted
#     afterwards. A cold boot reloads shards for ~3 min during which /health
#     is silent, which is exactly self-heal's SILENT trigger -- leaving it
#     running risks a double restart mid-roll.
#   - The relaunch reuses the live argv read from /proc/PID/cmdline, so
#     --shard-range and ARC_PUBLIC_SOCKET survive, the same way self-heal
#     does it.
#
# USAGE
#
#   bash scripts/arc-rolling-restart.sh                  # dry run
#   bash scripts/arc-rolling-restart.sh --run
#   bash scripts/arc-rolling-restart.sh --run --only AMS
#   bash scripts/arc-rolling-restart.sh --run --export-dir arc-state-export/<stamp>
#
# EXIT CODES
#   0  every targeted seed restarted and is sealing again
#   1  aborted -- a precondition failed or a seed did not recover
#   2  bad usage / missing dependency
# ─────────────────────────────────────────────────────────────────────────────

set -u

RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'
BOLD=$'\033[1m'; DIM=$'\033[2m'; RESET=$'\033[0m'

# Stalled seeds, in roll order. LHR is LAST on purpose: it holds the only
# meaningful inference history on the network, so it is the one we most want
# to have already seen the procedure succeed elsewhere.
TARGETS=("AMS:136.244.109.1" "NRT:202.182.107.41" "SGP:149.28.153.31" "LHR:104.238.171.11")

# Never restarted by this script. Used only as a health floor.
KEEP=("NYC:149.28.32.76" "LAX:140.82.16.112")

RPC_PORT="${ARC_RPC_PORT:-9090}"
SSH_USER="${ARC_SSH_USER:-root}"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new)
NODE_SERVICE="${ARC_NODE_SERVICE:-arc-node}"
HEAL_SERVICE="${ARC_HEAL_SERVICE:-arc-self-heal}"

# What counts as "still sealing" for the two seeds we refuse to touch.
# Measured cadence on 2026-08-18/19 over 10.3 h: NYC ~744 s/block, LAX ~344
# s/block. A 600 s floor would have failed NYC while it was perfectly healthy.
# Matches HEALTH_STALL_SECS in crates/arc-node/src/rpc.rs.
SEALING_FLOOR_SECS="${ARC_SEALING_FLOOR:-1800}"

# A cold boot reloads 3x GGUF shards (~3 min). Give it generous room before
# declaring failure, then require the height to actually ADVANCE.
BOOT_WAIT="${ARC_BOOT_WAIT:-420}"
SEAL_WAIT="${ARC_SEAL_WAIT:-300}"
POLL="${ARC_POLL:-15}"

DRY_RUN=1
ONLY=""
EXPORT_DIR=""

while [ $# -gt 0 ]; do
    case "$1" in
        --run)        DRY_RUN=0; shift ;;
        --only)       ONLY="${2:-}"; shift 2 ;;
        --export-dir) EXPORT_DIR="${2:-}"; shift 2 ;;
        -h|--help)    sed -n '2,64p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) printf "%s[FAIL]%s unknown argument: %s\n" "$RED" "$RESET" "$1" >&2; exit 2 ;;
    esac
done

command -v curl >/dev/null || { printf "%sFAIL%s curl not found\n" "$RED" "$RESET" >&2; exit 2; }
command -v ssh  >/dev/null || { printf "%sFAIL%s ssh not found\n"  "$RED" "$RESET" >&2; exit 2; }

say()  { printf "    %s\n" "$*"; }
ok()   { printf "    %s✓%s %s\n" "$GREEN" "$RESET" "$*"; }
warn() { printf "    %s!%s %s\n" "$YELLOW" "$RESET" "$*"; }
bad()  { printf "    %s✗%s %s\n" "$RED" "$RESET" "$*"; }

rpc() { curl -sS -m 20 "http://$1:${RPC_PORT}$2" 2>/dev/null; }

height_of() { rpc "$1" /health | python3 -c 'import json,sys;print(json.load(sys.stdin).get("height",-1))' 2>/dev/null || echo -1; }

# Seconds since this seed's newest block, from the block's own header.
block_age_of() {
    rpc "$1" /block/latest | python3 -c '
import json,sys,time
try:
    ts=json.load(sys.stdin)["header"]["timestamp"]
    print(int(time.time()-ts/1000))
except Exception:
    print(-1)
' 2>/dev/null || echo -1
}

staked_validators_of() {
    rpc "$1" /validators | python3 -c '
import json,sys
try:
    d=json.load(sys.stdin); vs=d.get("validators", d if isinstance(d,list) else [])
    print(sum(1 for x in vs if (x.get("stake",0) or 0)>0))
except Exception:
    print(-1)
' 2>/dev/null || echo -1
}

# ── Preconditions that gate the WHOLE run ────────────────────────────────────
printf "\n%s┌─ ARC rolling restart ───────────────────────────────────────┐%s\n" "$BOLD" "$RESET"
printf "%s│%s  targets: %s\n" "$BOLD" "$RESET" "${TARGETS[*]%%:*}"
printf "%s│%s  never touched: NYC, LAX\n" "$BOLD" "$RESET"
printf "%s└─────────────────────────────────────────────────────────────┘%s\n\n" "$BOLD" "$RESET"

printf "%sPreconditions%s\n" "$BOLD" "$RESET"

# 1. The volatile state must already be banked.
if [ -z "$EXPORT_DIR" ]; then
    EXPORT_DIR="$(ls -1d arc-state-export/*/ 2>/dev/null | tail -1)"
fi
if [ -z "$EXPORT_DIR" ] || [ ! -f "${EXPORT_DIR%/}/manifest.json" ]; then
    bad "no volatile-state export found. Run scripts/arc-export-volatile-state.sh --run first."
    exit 1
fi
crit_fail="$(python3 -c '
import json,sys
print(json.load(open(sys.argv[1])).get("critical_failures", -1))
' "${EXPORT_DIR%/}/manifest.json" 2>/dev/null || echo -1)"
if [ "$crit_fail" != "0" ]; then
    bad "export ${EXPORT_DIR} reports critical_failures=${crit_fail}; refusing to restart."
    exit 1
fi
ok "volatile state banked: ${EXPORT_DIR} (critical_failures=0)"

# 2. The seeds we are NOT touching must be sealing. They are the floor.
for entry in "${KEEP[@]}"; do
    n="${entry%%:*}"; ip="${entry##*:}"
    age="$(block_age_of "$ip")"
    if [ "$age" -lt 0 ] || [ "$age" -gt "$SEALING_FLOOR_SECS" ]; then
        bad "$n is not sealing (block age ${age}s). Aborting: the roll needs a healthy floor."
        exit 1
    fi
    ok "$n sealing (block age ${age}s) — untouched by this script"
done

printf "\n"
[ "$DRY_RUN" -eq 1 ] && printf "%sDRY RUN%s — no seed will be restarted. Pass %s--run%s to execute.\n\n" \
    "$YELLOW$BOLD" "$RESET" "$BOLD" "$RESET"

# ── Roll ─────────────────────────────────────────────────────────────────────
restart_one() {
    local name="$1" ip="$2"
    printf "%s%s%s (%s)\n" "$BOLD" "$name" "$RESET" "$ip"

    local before_h before_age before_v
    before_h="$(height_of "$ip")"; before_age="$(block_age_of "$ip")"; before_v="$(staked_validators_of "$ip")"
    say "${DIM}before: height=${before_h} block_age=${before_age}s staked_validators=${before_v}${RESET}"

    # Read the live argv. This is both the --genesis check and the relaunch
    # source of truth.
    local argv
    argv="$(ssh "${SSH_OPTS[@]}" "${SSH_USER}@${ip}" \
        'pid=$( pgrep -x arc-node | head -1); [ -n "$pid" ] && tr "\0" " " < /proc/$pid/cmdline' 2>&1)"
    if [ -z "$argv" ] || printf '%s' "$argv" | grep -qi 'denied\|refused\|timed out'; then
        bad "cannot read live argv over ssh: ${argv:-no response}"
        return 1
    fi

    if ! printf '%s' "$argv" | grep -q -- '--genesis'; then
        bad "live argv has NO --genesis. Restarting would SKIP seed_genesis_validators()"
        bad "and reproduce the divergence. Fix the unit file first. Argv was:"
        say "${DIM}${argv}${RESET}"
        return 1
    fi
    ok "argv carries --genesis (seed_genesis_validators will run at boot)"

    if [ "$DRY_RUN" -eq 1 ]; then
        say "${DIM}would: systemctl stop ${HEAL_SERVICE}${RESET}"
        say "${DIM}would: pipe scripts/arc-remote-relaunch.sh over ssh — capture argv+env+cwd${RESET}"
        say "${DIM}         from /proc, pkill -9 -x arc-node, re-exec the SAME argv from cwd.${RESET}"
        say "${DIM}         NOT systemctl: arc-node is not under systemd here and the unit${RESET}"
        say "${DIM}         file lacks --genesis, --model and every --shard-range.${RESET}"
        say "${DIM}would: poll /health up to ${BOOT_WAIT}s, then require height to advance within ${SEAL_WAIT}s${RESET}"
        say "${DIM}would: systemctl start ${HEAL_SERVICE}${RESET}"
        printf "\n"
        return 0
    fi

    # Stop self-heal so the ~3 min silent model reload cannot trip its SILENT
    # trigger and stack a second restart on top of ours.
    ssh "${SSH_OPTS[@]}" "${SSH_USER}@${ip}" "systemctl stop ${HEAL_SERVICE}" >/dev/null 2>&1 \
        && ok "self-heal stopped for the duration" || warn "could not stop ${HEAL_SERVICE} (may not be installed)"

    # NOT `systemctl restart` — arc-node is not under systemd on these seeds and
    # the unit on disk differs materially from the live process. See the header
    # of arc-remote-relaunch.sh for the verified evidence.
    local out
    out=$(ssh "${SSH_OPTS[@]}" "${SSH_USER}@${ip}" 'bash -s' < "$(dirname "$0")/arc-remote-relaunch.sh" 2>&1)
    if ! printf '%s' "$out" | grep -q RELAUNCH_OK; then
        bad "relaunch failed:"
        printf '%s\n' "$out" | sed 's/^/        /' | head -8
        ssh "${SSH_OPTS[@]}" "${SSH_USER}@${ip}" "systemctl start ${HEAL_SERVICE}" >/dev/null 2>&1
        return 1
    fi
    printf '%s\n' "$out" | grep -E '^CAPTURED_CWD' | sed "s/^/    ${DIM}/; s/$/${RESET}/"
    ok "relaunched with live argv preserved; waiting for RPC (up to ${BOOT_WAIT}s)"

    local waited=0 back=0
    while [ "$waited" -lt "$BOOT_WAIT" ]; do
        sleep "$POLL"; waited=$((waited + POLL))
        if [ -n "$(rpc "$ip" /health)" ]; then back=1; ok "RPC back after ${waited}s"; break; fi
    done
    if [ "$back" -eq 0 ]; then
        bad "no RPC after ${BOOT_WAIT}s — ABORTING the roll, remaining seeds untouched"
        ssh "${SSH_OPTS[@]}" "${SSH_USER}@${ip}" "systemctl start ${HEAL_SERVICE}" >/dev/null 2>&1
        return 1
    fi

    # The real success criterion is not "answers /health" -- that was true
    # throughout the eight-day stall. It is that the height ADVANCES.
    local h0 waited2=0 advanced=0
    h0="$(height_of "$ip")"
    say "post-boot height=${h0}; requiring it to advance within ${SEAL_WAIT}s"
    while [ "$waited2" -lt "$SEAL_WAIT" ]; do
        sleep "$POLL"; waited2=$((waited2 + POLL))
        local h1; h1="$(height_of "$ip")"
        if [ "$h1" -gt "$h0" ] 2>/dev/null; then
            advanced=1; ok "SEALING — height ${h0} → ${h1} after ${waited2}s"; break
        fi
    done

    ssh "${SSH_OPTS[@]}" "${SSH_USER}@${ip}" "systemctl start ${HEAL_SERVICE}" >/dev/null 2>&1 \
        && ok "self-heal restarted" || warn "could not restart ${HEAL_SERVICE} — do this by hand"

    if [ "$advanced" -eq 0 ]; then
        bad "height did not advance in ${SEAL_WAIT}s. ABORTING; remaining seeds untouched."
        bad "This seed is no worse than before, but the hypothesis did not hold — stop and re-diagnose."
        return 1
    fi

    local after_v; after_v="$(staked_validators_of "$ip")"
    say "staked validators: ${before_v} → ${after_v}   ${DIM}(genesis declares 6)${RESET}"
    [ "$after_v" = "$before_v" ] && warn "validator count unchanged — reseed may not have taken effect"
    printf "\n"
    return 0
}

rc=0
for entry in "${TARGETS[@]}"; do
    name="${entry%%:*}"; ip="${entry##*:}"
    if [ -n "$ONLY" ] && [ "$ONLY" != "$name" ]; then continue; fi
    if ! restart_one "$name" "$ip"; then
        printf "%s%s✗ ABORTED at %s. No further seed was touched.%s\n\n" "$BOLD" "$RED" "$name" "$RESET"
        rc=1; break
    fi
done

if [ "$rc" -eq 0 ]; then
    if [ "$DRY_RUN" -eq 1 ]; then
        printf "%sDry run complete.%s Re-run with --run to execute.\n\n" "$BOLD" "$RESET"
    else
        printf "%s%s✓ roll complete — every targeted seed is sealing again.%s\n" "$BOLD" "$GREEN" "$RESET"
        printf "  Re-run scripts/arc-export-volatile-state.sh to capture the new post-restart state.\n\n"
    fi
fi
exit "$rc"
