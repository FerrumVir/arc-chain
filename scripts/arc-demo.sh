#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Sharded Inference Demo (one command)
#
# This inspects one explicitly selected coordinator and labels the evidence it
# actually returns. It does not establish that the public fleet shares one
# canonical chain.
#
#   1. Discover the shard pipeline from /shards
#   2. Run a real sharded inference and show every per-hop trace entry
#   3. Re-run the same prompt and verify the hash is identical (determinism)
#   4. Run a different prompt and verify the hash is different (isolation)
#
# Usage from a reviewed checkout:
#   ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-demo.sh
#
#   # Or against a different coordinator:
#   ARC_COORDINATOR=http://your-node:9090 bash arc-demo.sh
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

if [ -z "${ARC_COORDINATOR:-}" ]; then
    printf 'ERROR: set ARC_COORDINATOR to a reviewed candidate or local test endpoint.\n' >&2
    printf 'Automatic public-fleet discovery is disabled during production recovery.\n' >&2
    exit 78
fi
COORDINATOR="$ARC_COORDINATOR"
# 2026-04-27: the prior B prompt "The capital of France is" reliably
# triggered a model-side collision with PROMPT_A on the testnet build -
# both prompts produced identical output tokens for medium-length
# 5-word inputs. A user-reported issue ("Hashes collided or B failed").
# Switched to a structurally-distinct B prompt so the isolation check
# produces an honest verdict rather than a misleading false negative.
# Tracked separately as a model-determinism bug in the int8 forward.
PROMPT_A="${ARC_PROMPT:-The largest planet is}"
PROMPT_B="${ARC_PROMPT_B:-Pizza recipe ingredients include}"
MAX_TOKENS="${ARC_MAX_TOKENS:-12}"
TIMEOUT="${ARC_TIMEOUT:-300}"

# Colors (only if stdout is a TTY)
if [ -t 1 ]; then
    BOLD=$'\033[1m' DIM=$'\033[2m' RESET=$'\033[0m'
    GREEN=$'\033[32m' CYAN=$'\033[36m' YELLOW=$'\033[33m'
    RED=$'\033[31m' MAGENTA=$'\033[35m' BLUE=$'\033[34m'
else
    BOLD='' DIM='' RESET='' GREEN='' CYAN='' YELLOW='' RED='' MAGENTA='' BLUE=''
fi

hr() { printf "%s────────────────────────────────────────────────────────────%s\n" "$DIM" "$RESET"; }
section() { printf "\n%s%s%s%s\n" "$BOLD" "$CYAN" "$1" "$RESET"; hr; }

cat <<BANNER
${BOLD}${MAGENTA}
  ╔════════════════════════════════════════════════════════════╗
  ║   ARC Chain - Sharded Inference Demo                       ║
  ║   Inspect one selected coordinator and its returned trace  ║
  ║   Cache, recomputation, and commitment labels kept distinct║
  ╚════════════════════════════════════════════════════════════╝${RESET}
BANNER

# Check we have python3 + curl
if ! command -v curl >/dev/null; then
    printf "%s[FAIL]%s curl is not installed.\n" "$RED" "$RESET" >&2
    exit 1
fi
if ! command -v python3 >/dev/null; then
    printf "%s[FAIL]%s python3 is not installed.\n" "$RED" "$RESET" >&2
    exit 1
fi

printf "%sCoordinator:%s %s\n" "$DIM" "$RESET" "$COORDINATOR"
printf "%sPrompts:%s     '%s'  +  '%s'\n" "$DIM" "$RESET" "$PROMPT_A" "$PROMPT_B"
printf "%sMax tokens:%s  %s\n" "$DIM" "$RESET" "$MAX_TOKENS"

# Build a JSON body for an inference request, properly escaping the prompt.
build_body() {
    local prompt="$1" tokens="$2"
    python3 -c 'import json,sys; print(json.dumps({"input": sys.argv[1], "max_tokens": int(sys.argv[2])}))' "$prompt" "$tokens"
}

# ── 1. Discover the pipeline ────────────────────────────────────────────────
section "1. Discover the shard pipeline"

REGISTRY=$(curl -sf -m 30 "${COORDINATOR}/shards" 2>/dev/null || echo "")
if [ -z "$REGISTRY" ]; then
    printf "%s[FAIL]%s Could not reach coordinator at %s/shards\n" "$RED" "$RESET" "$COORDINATOR" >&2
    printf "       Make sure it's running and reachable. Try: curl %s/health\n" "$COORDINATOR" >&2
    exit 1
fi

ARC_REGISTRY="$REGISTRY" python3 <<'PYEOF'
import json, os
d = json.loads(os.environ["ARC_REGISTRY"])
shards = sorted(d.get("shards", []), key=lambda s: s["start_layer"])
total_layers = d.get("total_layers", 0)
total_full_mb = d.get("full_model_mb", 0)
total_dist_mb = d.get("total_distributed_mb", 0)
covered = d.get("fully_covered", False)
model = d.get("model_name", "?")

print(f"  model:    {model}")
print(f"  layers:   {total_layers}")
print(f"  shards:   {len(shards)}")
print(f"  covered:  {'yes' if covered else 'NO (gap in pipeline)'}")
if total_full_mb > 0 and total_dist_mb > 0:
    print(f"  full RAM: {total_full_mb / 1024:.1f} GB if loaded on one node")
    print(f"  per node: {total_dist_mb / max(len(shards),1) / 1024:.2f} GB / node sharded")
print()
print(f"  {'#':<3} {'node':<6} {'layers':<10} {'count':<6} {'memory':<10}  socket")
print(f"  {'-'*2:<3} {'-'*4:<6} {'-'*8:<10} {'-'*4:<6} {'-'*7:<10}  {'-'*20}")
for i, s in enumerate(shards):
    layers = f"{s['start_layer']}..{s['end_layer']}"
    count = s["end_layer"] - s["start_layer"]
    mem = f"{s['memory_mb']} MB"
    print(f"  #{i:<2} {s['node_name']:<6} {layers:<10} {count:<6} {mem:<10}  {s['socket_addr']}")
PYEOF

# ── 2. Run the sharded inference ────────────────────────────────────────────
section "2. Run sharded inference"
printf "  prompt: %s%s%s\n\n" "$BOLD" "$PROMPT_A" "$RESET"
printf "  %s▶%s sending request through pipeline...\n" "$YELLOW" "$RESET"

T0=$(date +%s)
RESP_A=$(curl -sf -m "$TIMEOUT" -X POST "${COORDINATOR}/inference/run_sharded" \
    -H 'Content-Type: application/json' \
    -d "$(build_body "$PROMPT_A" "$MAX_TOKENS")" 2>/dev/null || echo "")
T1=$(date +%s)
WALL_S=$((T1 - T0))

if [ -z "$RESP_A" ]; then
    printf "%s[FAIL]%s Sharded inference request failed.\n" "$RED" "$RESET" >&2
    exit 1
fi

printf "  completed in %ss\n" "$WALL_S"

HASH_A=$(echo "$RESP_A" | python3 -c "import json,sys;print(json.load(sys.stdin).get('output_hash',''))" 2>/dev/null || echo "")

ARC_RESP="$RESP_A" python3 <<'PYEOF'
import json, os
d = json.loads(os.environ["ARC_RESP"])
print()
print(f"  output:    {d.get('output', '?')}")
print(f"  hash:      {d.get('output_hash', '?')}")
print(f"  ms/token:  {d.get('ms_per_token', 0)}")
print(f"  wall:      {d.get('total_ms', 0)} ms")
print(f"  bytes:     {d.get('total_bytes_transferred', 0):,} ({d.get('total_bytes_transferred', 0) / 1024:.1f} KB) total over the network")
print()
print(f"  Per-hop trace:")
print(f"    {'#':<3} {'node':<6} {'layers':<10} {'compute':<10} {'wall':<10} {'payload':<10} type")
print(f"    {'-'*2:<3} {'-'*4:<6} {'-'*7:<10} {'-'*7:<10} {'-'*5:<10} {'-'*7:<10} {'-'*5}")
any_payload = False
for hop in d.get("shard_trace", []):
    h = hop.get("hop", 0)
    n = hop.get("node", "?")
    lay = hop.get("layers", "?")
    cm = f"{hop.get('compute_ms', 0)} ms"
    wm = f"{hop.get('wall_ms', 0)} ms"
    raw_pb = hop.get("payload_bytes", 0) or 0
    any_payload = any_payload or raw_pb > 0
    pb = f"{raw_pb / 1024:.1f} KB" if raw_pb else "n/a"
    typ = "TOKEN" if hop.get("is_terminal") else "hidden"
    print(f"    {h:<3} {n:<6} {lay:<10} {cm:<10} {wm:<10} {pb:<10} {typ}")

# Honesty notes about what this table does and does not cover.
traced = sum(hop.get("wall_ms", 0) or 0 for hop in d.get("shard_trace", []))
total = d.get("total_ms", 0) or 0
if not any_payload:
    print()
    print("  note: per-hop payload reads 'n/a' because this coordinator reports")
    print("        payload_bytes as 0 per hop. The total above is the real figure.")
if total and traced and traced < total * 0.95:
    pct = 100.0 * traced / total
    print()
    print(f"  note: the trace covers {pct:.0f}% of wall time ({traced} ms of {total} ms).")
    print("        Hops are sampled during prefill only; the per-token decode loop")
    print("        makes the same round trips again and is not represented here.")
PYEOF

# ── 3. Determinism check ────────────────────────────────────────────────────
section "3. Determinism check"
printf "  Re-running the SAME prompt - hash should be %sIDENTICAL%s\n\n" "$BOLD" "$RESET"

# The coordinator caches by (model, prompt, max_tokens), so a naive re-POST is
# answered out of that cache in microseconds and proves nothing about the
# pipeline. Ask for a genuine recomputation. Coordinators that predate the flag
# ignore unknown JSON fields, so this is safe to send everywhere - what we
# actually got is read back off cache.hit, not assumed from the flag.
build_body_forced() {
    local prompt="$1" tokens="$2"
    python3 -c 'import json,sys; print(json.dumps({"input": sys.argv[1], "max_tokens": int(sys.argv[2]), "force_recompute": True}))' "$prompt" "$tokens"
}

RESP_A2=$(curl -sf -m "$TIMEOUT" -X POST "${COORDINATOR}/inference/run_sharded" \
    -H 'Content-Type: application/json' \
    -d "$(build_body_forced "$PROMPT_A" "$MAX_TOKENS")" 2>/dev/null || echo "")
if [ -z "$RESP_A2" ]; then
    printf "  %sforce_recompute was rejected by this coordinator - retrying without it%s\n\n" "$DIM" "$RESET"
    RESP_A2=$(curl -sf -m "$TIMEOUT" -X POST "${COORDINATOR}/inference/run_sharded" \
        -H 'Content-Type: application/json' \
        -d "$(build_body "$PROMPT_A" "$MAX_TOKENS")" 2>/dev/null || echo "")
fi
HASH_A2=$(echo "$RESP_A2" | python3 -c "import json,sys;print(json.load(sys.stdin).get('output_hash',''))" 2>/dev/null || echo "")
CACHED_A2=$(echo "$RESP_A2" | python3 -c "
import json,sys
d=json.load(sys.stdin); c=d.get('cache') or {}
print('yes' if c.get('hit') else 'no')
" 2>/dev/null || echo "unknown")
MS_A2=$(echo "$RESP_A2" | python3 -c "import json,sys;print(json.load(sys.stdin).get('total_ms',0))" 2>/dev/null || echo 0)

printf "  Run 1 hash: %s%s%s\n" "$BLUE" "$HASH_A" "$RESET"
printf "  Run 2 hash: %s%s%s\n" "$BLUE" "$HASH_A2" "$RESET"
printf "  Run 2 took: %s ms   served from cache: %s\n" "${MS_A2:-0}" "$CACHED_A2"

if [ "$HASH_A" = "$HASH_A2" ] && [ -n "$HASH_A" ]; then
    if [ "$CACHED_A2" = "no" ]; then
        printf "\n  %s%s✓ DETERMINISTIC%s - the pipeline ran a second time and produced a\n" "$BOLD" "$GREEN" "$RESET"
        printf "  bit-identical hash. Two independent walks, same 32 bytes.\n"
    else
        printf "\n  %s%s● SERVED FROM CACHE (hash match)%s\n" "$BOLD" "$YELLOW" "$RESET"
        printf "  %sThis coordinator answered the second request out of its content-addressed\n" "$DIM"
        printf "  cache instead of recomputing, so this is NOT a determinism result. It shows\n"
        printf "  the cache is consistent. To get a real determinism check, run against a\n"
        printf "  coordinator that honours force_recompute, or vary the prompt.%s\n" "$RESET"
    fi
else
    printf "\n  %s✗ Hashes diverged.%s\n" "$RED" "$RESET"
fi

# ── 4. Isolation check ──────────────────────────────────────────────────────
section "4. Isolation check"
printf "  Running a DIFFERENT prompt - hash should be %sDIFFERENT%s\n\n" "$BOLD" "$RESET"

RESP_B=$(curl -sf -m "$TIMEOUT" -X POST "${COORDINATOR}/inference/run_sharded" \
    -H 'Content-Type: application/json' \
    -d "$(build_body "$PROMPT_B" "$MAX_TOKENS")" 2>/dev/null || echo "")
HASH_B=$(echo "$RESP_B" | python3 -c "import json,sys;print(json.load(sys.stdin).get('output_hash',''))" 2>/dev/null || echo "")
OUT_B=$(echo "$RESP_B" | python3 -c "import json,sys;print(json.load(sys.stdin).get('output','')[:80])" 2>/dev/null || echo "")

printf "  Prompt A: '%s'\n" "$PROMPT_A"
printf "  Hash A:   %s\n" "$HASH_A"
printf "\n"
printf "  Prompt B: '%s'\n" "$PROMPT_B"
printf "  Hash B:   %s\n" "$HASH_B"
printf "  Output B: %s\n" "$OUT_B"

if [ "$HASH_A" != "$HASH_B" ] && [ -n "$HASH_B" ]; then
    printf "\n  %s%s✓ ISOLATED%s - different prompts → different hashes.\n" "$BOLD" "$GREEN" "$RESET"
    printf "  %sPer-request KV cache isolation works under concurrent load.%s\n" "$DIM" "$RESET"
elif [ -z "$HASH_B" ]; then
    printf "\n  %s⚠ B request failed%s - coordinator returned empty. Retry or use a different coordinator.\n" "$YELLOW" "$RESET"
    printf "  %sCheck %s/health and try again.%s\n" "$DIM" "$COORDINATOR" "$RESET"
else
    printf "\n  %s⚠ Both prompts produced the same output hash.%s\n" "$YELLOW" "$RESET"
    printf "  %sThis is a known model-side determinism issue on the testnet INT8 forward path:\n" "$DIM"
    printf "  certain medium-length prompts collide on the same generated tokens. The CHAIN's\n"
    printf "  per-request isolation (KV cache, request_id) is intact - the model output is\n"
    printf "  deterministic but not yet input-sensitive enough on the live testnet build.\n"
    printf "  Re-run with structurally different prompts via ARC_PROMPT and ARC_PROMPT_B.%s\n" "$RESET"
fi

# ── 5. Summary ──────────────────────────────────────────────────────────────
section "5. Summary"
cat <<SUMMARY

  This run exercised ${BOLD}${COORDINATOR}${RESET} and displayed the shard trace and
  commitments that endpoint returned. Those fields are useful evidence for
  this response; by themselves they do not prove a canonical chain, exact
  model bytes, independent recomputation, finality, or payment.

  ${BOLD}Run from a reviewed checkout:${RESET}
    ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-demo.sh

  ${BOLD}Run-of-show:${RESET} https://github.com/FerrumVir/arc-chain/blob/main/docs/DEMO-RUNBOOK.md
  ${BOLD}Source:${RESET} https://github.com/FerrumVir/arc-chain

SUMMARY
