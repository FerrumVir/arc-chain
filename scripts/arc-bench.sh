#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Factual Benchmark
#
# Runs 10 factual prompts through the sharded inference pipeline and
# checks each output for an expected keyword. Captures latency, tx_hash,
# and pass/fail. Emits a markdown report that can be saved or piped to
# a file for sharing.
#
# Usage:
#   ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-bench.sh
#   ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-bench.sh > BENCHMARK.md
#
# Run this only against an explicitly selected coordinator or from a reviewed
# checkout with the local coordinator picker. Output describes that coordinator;
# it is not proof that the public fleet shares one canonical chain.
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

if [ -z "${ARC_COORDINATOR:-}" ]; then
    printf 'ERROR: set ARC_COORDINATOR to a reviewed candidate or local test endpoint.\n' >&2
    printf 'Automatic public-fleet discovery is disabled during production recovery.\n' >&2
    exit 78
fi
COORDINATOR="$ARC_COORDINATOR"
MAX_TOKENS="${ARC_MAX_TOKENS:-12}"

# Each entry: prompt|expected_keyword (case-insensitive substring match)
# Default 5 prompts. Set ARC_BENCH_FULL=1 for the 10-prompt version.
if [ "${ARC_BENCH_FULL:-0}" = "1" ]; then
    PROMPTS=(
        "The capital of France is|paris"
        "The largest planet is|jupiter"
        "The sky is|blue"
        "The fastest land animal is|cheetah"
        "The deepest ocean is|challenger"
        "The tallest mountain is|everest"
        "The currency of Japan is|yen"
        "The longest river is|nile"
        "The hottest planet is|venus"
        "The speed of light in a vacuum is|299"
    )
else
    PROMPTS=(
        "The capital of France is|paris"
        "The largest planet is|jupiter"
        "The fastest land animal is|cheetah"
        "The longest river is|nile"
        "The tallest mountain is|everest"
    )
fi

if ! command -v curl >/dev/null || ! command -v python3 >/dev/null; then
    echo "ERROR: arc-bench.sh requires curl + python3" >&2
    exit 1
fi

# Markdown header
cat <<HEADER
# ARC Chain - Factual Benchmark Report

Runs the configured factual prompts against one explicitly selected coordinator
and checks each response for the expected keyword. This report describes that
endpoint and time; it does not prove that another node, model artifact, or fork
will return the same hashes.

- **Coordinator**: \`$COORDINATOR\`
- **Date**: $(date -u '+%Y-%m-%d %H:%M:%S UTC')
- **Max tokens per response**: $MAX_TOKENS

| # | Prompt | Expected | Output | Pass | ms/tok | tx_hash |
|---|--------|----------|--------|------|--------|---------|
HEADER

PASS=0
FAIL=0
TOTAL=0
TOTAL_MS_PER_TOK=0

for entry in "${PROMPTS[@]}"; do
    PROMPT="${entry%%|*}"
    EXPECTED="${entry##*|}"
    TOTAL=$((TOTAL + 1))

    # Pause between requests so the 7-shard pipeline has time to drain.
    # Without this the coordinator's HTTP server backs up under sequential
    # load and the watchdog eventually restarts it. 60s gives the pipeline
    # time to fully drain (each request takes ~150s wall time through the
    # 7-hop chain, so requests overlap if the sleep is too short).
    if [ $TOTAL -gt 1 ]; then
        sleep 60
    fi

    # POST to /inference/run_sharded with a generous timeout
    BODY=$(python3 -c "import json; print(json.dumps({'input': '$PROMPT', 'max_tokens': $MAX_TOKENS}))")
    RESP=$(curl -sf -m 600 -X POST "${COORDINATOR}/inference/run_sharded" \
        -H 'Content-Type: application/json' \
        -d "$BODY" 2>/dev/null || echo "")

    if [ -z "$RESP" ]; then
        printf "| %d | %s | %s | (request failed) | ✗ | - | - |\n" "$TOTAL" "$PROMPT" "$EXPECTED"
        FAIL=$((FAIL + 1))
        continue
    fi

    OUTPUT=$(echo "$RESP" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('output','').strip()[:80])" 2>/dev/null)
    MS_PER_TOK=$(echo "$RESP" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('ms_per_token',0))" 2>/dev/null)
    TX_HASH=$(echo "$RESP" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('attestation',{}).get('tx_hash',''))" 2>/dev/null)

    # Case-insensitive substring match
    OUTPUT_LOWER=$(echo "$OUTPUT" | tr '[:upper:]' '[:lower:]')
    EXPECTED_LOWER=$(echo "$EXPECTED" | tr '[:upper:]' '[:lower:]')
    if echo "$OUTPUT_LOWER" | grep -q "$EXPECTED_LOWER"; then
        STATUS="✓"
        PASS=$((PASS + 1))
    else
        STATUS="✗"
        FAIL=$((FAIL + 1))
    fi

    TOTAL_MS_PER_TOK=$((TOTAL_MS_PER_TOK + MS_PER_TOK))
    # Escape pipes in output for markdown table
    OUTPUT_ESC=$(echo "$OUTPUT" | sed 's/|/\\|/g')
    TX_SHORT="${TX_HASH:0:14}…"
    printf "| %d | %s | %s | %s | %s | %s | \`%s\` |\n" "$TOTAL" "$PROMPT" "$EXPECTED" "$OUTPUT_ESC" "$STATUS" "$MS_PER_TOK" "$TX_SHORT"
done

AVG_MS=$((TOTAL_MS_PER_TOK / TOTAL))

cat <<FOOTER

## Summary

- **Pass rate**: $PASS / $TOTAL ($(( PASS * 100 / TOTAL ))%)
- **Average ms/token**: $AVG_MS
- **Pipeline topology**: not asserted by this benchmark
- **output_hashes**: coordinator-reported commitments, not cross-node determinism proof

To reproduce:

\`\`\`bash
ARC_COORDINATOR=$COORDINATOR bash scripts/arc-bench.sh
\`\`\`

To verify any individual run, take its tx_hash from the table above and run:

\`\`\`bash
ARC_COORDINATOR=$COORDINATOR bash scripts/arc-verify.sh <tx_hash>
\`\`\`
FOOTER
