#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Inference Verifier
#
# Take an inference attestation tx_hash, fetch its recorded details
# (input, output, model_id, output_hash), re-run the SAME input through
# the SAME coordinator, and verify the new output_hash matches the
# original. Independent third-party verification of any past inference run.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh \
#     | bash -s -- --latest
#
#   # Or with a specific tx_hash:
#   curl -sSL .../arc-verify.sh | bash -s -- 0x933b8616d6712baff21bc9083705f7715da4a9fd20d1815c6412426d8f071c24
#
#   # Or with a custom coordinator:
#   ARC_COORDINATOR=http://your-node:9090 bash arc-verify.sh <tx_hash>
#
# WHERE THE DATA COMES FROM (2026-08-17)
#   Attestation records live in TWO places and neither is guaranteed populated:
#     /inference/results       node-local, in-memory, lost on restart
#     /inference/attestations  same records, plus on-chain rows, plus - past
#                              the end of the real list - a fallthrough that
#                              emits unrelated transactions tagged tx_type
#                              "Other". Those are filtered out here.
#   On the live network /inference/results is EMPTY on most seeds. This script
#   therefore sweeps every seed and both endpoints rather than trusting one.
#
# Requires curl + python3.
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

SEED_LIST_DEFAULT="http://104.238.171.11:9090 http://136.244.109.1:9090 http://140.82.16.112:9090 http://202.182.107.41:9090 http://149.28.153.31:9090 http://149.28.32.76:9090"

# ── Pick a live coordinator by probing seeds (override with ARC_COORDINATOR)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
PICK="$SCRIPT_DIR/arc-pick-coordinator.sh"
if [ ! -f "$PICK" ]; then
    PICK=$(mktemp)
    curl -fsSL "https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-pick-coordinator.sh" -o "$PICK" 2>/dev/null || true
fi
if [ -z "${ARC_COORDINATOR:-}" ] && [ -s "$PICK" ]; then
    ARC_COORDINATOR=$(bash "$PICK" 2>/dev/null || echo "")
fi
COORDINATOR="${ARC_COORDINATOR:-http://104.238.171.11:9090}"
TX_HASH="${1:-}"

# Seeds to sweep when the chosen coordinator has no record. The coordinator is
# always tried first; the rest follow in order.
ARC_SEEDS_SWEEP="${ARC_SEEDS_SWEEP:-$SEED_LIST_DEFAULT}"

# Colors
if [ -t 1 ]; then
    BOLD=$'\033[1m' DIM=$'\033[2m' RESET=$'\033[0m'
    GREEN=$'\033[32m' CYAN=$'\033[36m' YELLOW=$'\033[33m' RED=$'\033[31m'
else
    BOLD='' DIM='' RESET='' GREEN='' CYAN='' YELLOW='' RED=''
fi

if [ -z "$TX_HASH" ]; then
    cat <<USAGE
${BOLD}arc-verify.sh${RESET} - independently verify a past inference attestation

USAGE:
  arc-verify.sh <tx_hash>           # verify a specific past attestation
  arc-verify.sh --latest            # verify the newest attestation on the network
  arc-verify.sh -l                  # short form

  # Live against the testnet (default):
  curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh \\
    | bash -s -- --latest

  # Or against your own coordinator:
  ARC_COORDINATOR=http://localhost:9944 bash arc-verify.sh --latest

WHAT IT DOES:
  1. Sweeps every seed's /inference/results AND /inference/attestations
  2. Locates the entry for the given tx_hash (or the newest, for --latest)
  3. Pulls the original input text + claimed output_hash + model_hash
  4. Re-runs the SAME input on the seed that holds the record
  5. Compares the new output_hash to the claimed one
  6. Prints VERIFIED, VERIFIED (FROM CACHE), or MISMATCH

CAVEAT - read this before quoting the result:
  A re-run of a prompt the coordinator has already served is answered from its
  content-addressed cache in microseconds. That proves the cache is consistent,
  not that the pipeline recomputed. This script asks for force_recompute and
  reports honestly which of the two it actually got.
USAGE
    exit 1
fi

if ! command -v curl >/dev/null; then
    printf "${RED}[FAIL]${RESET} curl is not installed.\n" >&2
    exit 1
fi
if ! command -v python3 >/dev/null; then
    printf "${RED}[FAIL]${RESET} python3 is not installed.\n" >&2
    exit 1
fi

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

# ── Normalizer: read both endpoint shapes, emit flat records ────────────────
# /inference/results     -> {"results":[{tx_hash, input, output_hash, ...}]}
# /inference/attestations-> {"attestations":[{tx_hash, tx_type, inference:{...}}]}
# Rows with tx_type != "Inference" (the generic-transaction fallthrough) and
# rows with no output_hash are dropped - they are not attestations.
read -r -d '' ARC_NORMALIZE_PY <<'PYEOF' || true
import json, sys

def flatten(doc, source):
    out = []
    for row in doc.get("results", []) or []:
        if row.get("output_hash"):
            r = dict(row); r["_source"] = source
            out.append(r)
    for row in doc.get("attestations", []) or []:
        if row.get("tx_type") not in (None, "Inference"):
            continue
        inf = row.get("inference") or {}
        if not inf.get("output_hash"):
            continue
        r = dict(inf)
        r["tx_hash"] = row.get("tx_hash", "")
        r["block_height"] = row.get("block_height")
        r["_source"] = source
        out.append(r)
    return out

records = []
for raw, source in zip(sys.argv[1::2], sys.argv[2::2]):
    try:
        with open(raw) as fh:
            body = fh.read().strip()
        if not body:
            continue
        records.extend(flatten(json.loads(body), source))
    except Exception:
        continue

seen, uniq = set(), []
for r in records:
    h = (r.get("tx_hash") or "").lower()
    if h and h in seen:
        continue
    seen.add(h)
    uniq.append(r)
print(json.dumps(uniq))
PYEOF

# fetch_records <url> -> writes normalized JSON array to stdout
fetch_records() {
    local url="$1"
    local f_res="$WORK_DIR/res.json" f_att="$WORK_DIR/att.json"
    curl -sf -m 25 "${url}/inference/results" -o "$f_res" 2>/dev/null || : > "$f_res"
    curl -sf -m 25 "${url}/inference/attestations?limit=50" -o "$f_att" 2>/dev/null || : > "$f_att"
    python3 -c "$ARC_NORMALIZE_PY" "$f_res" "${url}/inference/results" \
                                   "$f_att" "${url}/inference/attestations" 2>/dev/null || echo "[]"
}

# ── Locate the attestation across every seed ────────────────────────────────
WANT="${TX_HASH#0x}"
WANT="$(printf '%s' "$WANT" | tr 'A-Z' 'a-z')"
LATEST_MODE=0
if [ "$TX_HASH" = "--latest" ] || [ "$TX_HASH" = "-l" ]; then
    LATEST_MODE=1
fi

printf "${CYAN}[1/3]${RESET} Locating the attestation...\n"

SWEEP="$COORDINATOR"
for s in $ARC_SEEDS_SWEEP; do
    [ "$s" = "$COORDINATOR" ] && continue
    SWEEP="$SWEEP $s"
done

FOUND_JSON=""
FOUND_SEED=""
for seed in $SWEEP; do
    RECORDS=$(fetch_records "$seed")
    N=$(printf '%s' "$RECORDS" | python3 -c "import json,sys; print(len(json.load(sys.stdin)))" 2>/dev/null || echo 0)
    if [ "${N:-0}" -eq 0 ] 2>/dev/null; then
        printf "       %s-%s no attestation records\n" "$DIM" "$RESET" >/dev/null
        printf "       %s%-28s  0 records%s\n" "$DIM" "${seed#http://}" "$RESET"
        continue
    fi
    printf "       %s%-28s  %s records%s\n" "$DIM" "${seed#http://}" "$N" "$RESET"
    SEL=$(ARC_RECORDS="$RECORDS" ARC_WANT="$WANT" ARC_LATEST="$LATEST_MODE" python3 -c '
import json, os, sys
rows = json.loads(os.environ["ARC_RECORDS"])
want = os.environ["ARC_WANT"]
if os.environ["ARC_LATEST"] == "1":
    print(json.dumps(rows[0])); sys.exit(0)
for r in rows:
    if (r.get("tx_hash") or "").lstrip("0x").lower() == want:
        print(json.dumps(r)); sys.exit(0)
sys.exit(1)
' 2>/dev/null)
    if [ -n "$SEL" ]; then
        FOUND_JSON="$SEL"
        FOUND_SEED="$seed"
        break
    fi
done

if [ -z "$FOUND_JSON" ]; then
    printf "\n${RED}[FAIL]${RESET} No matching attestation found on any seed.\n" >&2
    if [ "$LATEST_MODE" = "1" ]; then
        printf "       No seed is holding any inference attestation right now.\n" >&2
        printf "       These records are in-memory and are lost when a node restarts.\n" >&2
        printf "       Seed the network with one run first:\n" >&2
        printf "         curl -X POST %s/inference/run_sharded \\\\\n" "$COORDINATOR" >&2
        printf "           -H 'Content-Type: application/json' \\\\\n" >&2
        printf "           -d '{\"input\":\"What is a blockchain?\",\"max_tokens\":12}'\n" >&2
    else
        printf "       tx_hash %s is not in any seed's results or attestations list.\n" "$TX_HASH" >&2
    fi
    exit 1
fi

echo "$FOUND_JSON" > "$WORK_DIR/original.json"
jget() { python3 -c "import json;d=json.load(open('$WORK_DIR/original.json'));print(d.get('$1',''))"; }

ORIG_TX=$(jget tx_hash)
ORIG_INPUT=$(jget input)
ORIG_OUTPUT_HASH=$(jget output_hash)
ORIG_MODEL_HASH=$(jget model_hash)
ORIG_OUTPUT=$(python3 -c "import json;d=json.load(open('$WORK_DIR/original.json'));print(d.get('output','')[:100])")
ORIG_SHARDED=$(jget sharded)
ORIG_ENGINE=$(jget engine)
ORIG_SOURCE=$(jget _source)
ORIG_BLOCK=$(jget block_height)
ORIG_TOKENS=$(jget tokens_generated)

# The re-run has to hit the seed that actually holds the record.
COORDINATOR="$FOUND_SEED"

cat <<HEADER

${BOLD}${CYAN}
  ╔══════════════════════════════════════════════════════════════╗
  ║   ARC Chain - Inference Verifier                             ║
  ╚══════════════════════════════════════════════════════════════╝${RESET}

  Coordinator: ${COORDINATOR}
  tx_hash:     ${ORIG_TX}

HEADER

printf "${GREEN}[ OK]${RESET} Found attestation\n"
printf "       source:       %s\n" "$ORIG_SOURCE"
printf "       input:        '%s'\n" "$ORIG_INPUT"
printf "       output:       '%s'\n" "$ORIG_OUTPUT"
printf "       output_hash:  %s\n" "$ORIG_OUTPUT_HASH"
printf "       model_hash:   %s\n" "$ORIG_MODEL_HASH"
printf "       engine:       %s\n" "$ORIG_ENGINE"
printf "       sharded:      %s\n" "$ORIG_SHARDED"
if [ -z "$ORIG_BLOCK" ] || [ "$ORIG_BLOCK" = "None" ] || [ "$ORIG_BLOCK" = "null" ]; then
    printf "       on chain:     %sno - this attestation is in the node's memory, not in a block%s\n" "$YELLOW" "$RESET"
else
    printf "       on chain:     block %s\n" "$ORIG_BLOCK"
fi
echo ""

# ── Step 2: Re-run the inference ────────────────────────────────────────────
printf "${CYAN}[2/3]${RESET} Re-running the same input through %s...\n" "${COORDINATOR#http://}"

if [ "$ORIG_SHARDED" = "True" ] || [ "$ORIG_SHARDED" = "true" ]; then
    ENDPOINT="/inference/run_sharded"
else
    ENDPOINT="/inference/run"
fi
RERUN_TOKENS="${ORIG_TOKENS:-15}"
case "$RERUN_TOKENS" in ''|*[!0-9]*) RERUN_TOKENS=15 ;; esac
printf "       endpoint:     %s  (max_tokens %s)\n" "$ENDPOINT" "$RERUN_TOKENS"

# Ask for a genuine recomputation. Coordinators that predate the flag ignore
# unknown JSON fields, so this is safe to send everywhere - we detect what we
# actually got by reading cache.hit off the response, not by trusting the flag.
ARC_INPUT="$ORIG_INPUT" ARC_TOK="$RERUN_TOKENS" python3 -c "
import json, os
print(json.dumps({'input': os.environ['ARC_INPUT'],
                  'max_tokens': int(os.environ['ARC_TOK']),
                  'force_recompute': True}))
" > "$WORK_DIR/body.json"

NEW_RESPONSE=$(curl -sf -m 300 -X POST "${COORDINATOR}${ENDPOINT}" \
    -H 'Content-Type: application/json' \
    --data @"$WORK_DIR/body.json" 2>/dev/null || echo "")

# If the coordinator rejected the extra field outright, retry without it.
FORCED_SUPPORTED=1
if [ -z "$NEW_RESPONSE" ]; then
    FORCED_SUPPORTED=0
    printf "       %sforce_recompute rejected by this coordinator - retrying without it%s\n" "$DIM" "$RESET"
    ARC_INPUT="$ORIG_INPUT" ARC_TOK="$RERUN_TOKENS" python3 -c "
import json, os
print(json.dumps({'input': os.environ['ARC_INPUT'], 'max_tokens': int(os.environ['ARC_TOK'])}))
" > "$WORK_DIR/body.json"
    NEW_RESPONSE=$(curl -sf -m 300 -X POST "${COORDINATOR}${ENDPOINT}" \
        -H 'Content-Type: application/json' \
        --data @"$WORK_DIR/body.json" 2>/dev/null || echo "")
fi

if [ -z "$NEW_RESPONSE" ]; then
    printf "${RED}[FAIL]${RESET} Re-run request failed.\n" >&2
    exit 1
fi

echo "$NEW_RESPONSE" > "$WORK_DIR/rerun.json"
rget() { python3 -c "
import json
d=json.load(open('$WORK_DIR/rerun.json'))
v=d.get('$1', d.get('inference',{}).get('$1',''))
print(v if v is not None else '')
" 2>/dev/null || echo ""; }

NEW_OUTPUT_HASH=$(rget output_hash)
NEW_MODEL_HASH=$(rget model_hash)
NEW_TOTAL_MS=$(rget total_ms)
CACHE_HIT=$(python3 -c "
import json
d=json.load(open('$WORK_DIR/rerun.json'))
c=d.get('cache') or {}
print('yes' if c.get('hit') else 'no')
" 2>/dev/null || echo "unknown")
TRACE_LEN=$(python3 -c "
import json
d=json.load(open('$WORK_DIR/rerun.json'))
print(len(d.get('shard_trace') or []))
" 2>/dev/null || echo 0)

printf "${GREEN}[ OK]${RESET} Re-run complete\n"
printf "       new output_hash: %s\n" "$NEW_OUTPUT_HASH"
printf "       new model_hash:  %s\n" "$NEW_MODEL_HASH"
printf "       wall time:       %s ms across %s traced hops\n" "${NEW_TOTAL_MS:-0}" "$TRACE_LEN"
printf "       served from cache: %s\n" "$CACHE_HIT"
echo ""

# ── Step 3: Compare ────────────────────────────────────────────────────────
printf "${CYAN}[3/3]${RESET} Comparing hashes...\n"
echo ""

# cache.hit is the ONLY reliable signal. Do not also require a non-empty
# shard_trace: on older builds a cache hit returns an empty trace, but the
# newer build carries the ORIGINAL run's trace through the cache, so a
# populated trace no longer implies the pipeline actually ran.
RECOMPUTED=0
if [ "$CACHE_HIT" = "no" ]; then
    RECOMPUTED=1
fi

if [ "$ORIG_OUTPUT_HASH" = "$NEW_OUTPUT_HASH" ] && [ "$ORIG_MODEL_HASH" = "$NEW_MODEL_HASH" ]; then
    if [ "$RECOMPUTED" = "1" ]; then
        printf "  ${BOLD}${GREEN}✓ VERIFIED (recomputed)${RESET} - the pipeline ran again and produced the same hashes.\n\n"
        printf "  The network re-executed %s hops and landed on a bit-identical output_hash.\n" "$TRACE_LEN"
        printf "  That is a real reproduction of the original inference.\n"
    else
        printf "  ${BOLD}${YELLOW}✓ VERIFIED (from cache)${RESET} - hashes match, but this run was served from\n"
        printf "  the coordinator's content-addressed cache, not recomputed.\n\n"
        printf "  What this proves: the coordinator still holds the same output for the same\n"
        printf "  input, and the model identity is unchanged.\n"
        printf "  What it does NOT prove: that re-running the pipeline reproduces the hash.\n"
        if [ "$FORCED_SUPPORTED" = "1" ]; then
            printf "  ${DIM}This coordinator ignored force_recompute (older build). To force a real\n"
            printf "  recomputation, run against a coordinator that supports the flag.${RESET}\n"
        fi
    fi
    printf "\n  Checked:\n"
    printf "    • model_hash identical  (%s)\n" "$ORIG_MODEL_HASH"
    printf "    • output_hash identical (%s)\n" "$ORIG_OUTPUT_HASH"
    printf "  ${DIM}Note: model_hash commits to the model's shape label, not to the weight\n"
    printf "  bytes. It proves the same declared model, not the same tensors.${RESET}\n"
    EXIT_CODE=0
else
    printf "  ${BOLD}${RED}✗ MISMATCH${RESET}\n\n"
    if [ "$ORIG_OUTPUT_HASH" != "$NEW_OUTPUT_HASH" ]; then
        printf "    output_hash differs:\n"
        printf "      original: %s\n" "$ORIG_OUTPUT_HASH"
        printf "      new:      %s\n" "$NEW_OUTPUT_HASH"
    fi
    if [ "$ORIG_MODEL_HASH" != "$NEW_MODEL_HASH" ]; then
        printf "    model_hash differs:\n"
        printf "      original: %s\n" "$ORIG_MODEL_HASH"
        printf "      new:      %s\n" "$NEW_MODEL_HASH"
    fi
    EXIT_CODE=1
fi

exit $EXIT_CODE
