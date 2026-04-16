#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — Coordinator Auto-Discovery
#
# Probes the testnet seed list and prints the URL of the first coordinator
# that is actually able to serve inference requests right now. Used by
# arc-demo.sh, arc-verify.sh, arc-bench.sh as a drop-in replacement for
# the old hardcoded `http://149.28.32.76:9090` default.
#
# Selection policy (most to least desirable):
#   1. Node with fully_covered=true (can run sharded inference end-to-end)
#   2. Node whose /shards AND /health both respond with peers >= 1
#      (alive, networked, AND able to serve the shard registry)
#   3. Node whose /health responds with peers >= 1 but /shards is broken
#      (lookup queries work but sharded inference will fail)
#   4. Any node with /health 200 (alive but possibly isolated)
#
# Exits non-zero if NO seed node is reachable.
#
# Usage:
#   URL="$(bash arc-pick-coordinator.sh)"
#   ARC_COORDINATOR="$URL" bash arc-demo.sh
#
#   # Also supports sourcing to get a bash function:
#   source arc-pick-coordinator.sh
#   URL=$(arc_pick_coordinator)
#
#   # Verbose mode prints per-seed probe results to stderr:
#   ARC_PICK_VERBOSE=1 bash arc-pick-coordinator.sh
# ─────────────────────────────────────────────────────────────────────────────

# Default seed list. Can be overridden with ARC_SEEDS="ip1:port1 ip2:port2 ..."
# or with ARC_SEEDS_URL pointing to a plaintext list (one "host:port" per line).
# We keep NYC + LAX in the list — they come back from time to time, and the
# scripts should prefer them when healthy because they've historically held
# layers 0-9 of the production pipeline.
_ARC_DEFAULT_SEEDS="149.28.32.76:9090 140.82.16.112:9090 136.244.109.1:9090 104.238.171.11:9090 202.182.107.41:9090 149.28.153.31:9090 216.238.120.27:9090 139.84.237.49:9090"

arc_pick_coordinator() {
    local seeds="${ARC_SEEDS:-}"
    local timeout="${ARC_PICK_TIMEOUT:-3}"
    local verbose="${ARC_PICK_VERBOSE:-0}"

    # If ARC_SEEDS_URL is set, fetch it (space- or newline-separated host:port list)
    if [ -z "$seeds" ] && [ -n "${ARC_SEEDS_URL:-}" ]; then
        seeds=$(curl -sf -m 5 "$ARC_SEEDS_URL" 2>/dev/null | tr '\n' ' ')
    fi
    seeds="${seeds:-$_ARC_DEFAULT_SEEDS}"

    local best_url=""
    local best_tier=0  # 4=fully_covered, 3=shards+peers, 2=peers_only, 1=alive

    for seed in $seeds; do
        # Strip any scheme/path the user might have included
        seed="${seed#http://}"
        seed="${seed#https://}"
        seed="${seed%%/*}"
        local url="http://${seed}"

        # Probe /health first — fastest signal
        local health_json
        health_json=$(curl -sf -m "$timeout" "${url}/health" 2>/dev/null || echo "")
        if [ -z "$health_json" ]; then
            [ "$verbose" = "1" ] && echo "  [DEAD] $url" >&2
            continue
        fi
        local peers
        peers=$(echo "$health_json" | sed -n 's/.*"peers":\([0-9][0-9]*\).*/\1/p')
        peers=${peers:-0}

        # Probe /shards — required for inference routing
        local shards_json
        shards_json=$(curl -sf -m "$timeout" "${url}/shards" 2>/dev/null || echo "")
        local shards_ok=0
        local fully_covered=0
        if [ -n "$shards_json" ]; then
            shards_ok=1
            echo "$shards_json" | grep -q '"fully_covered":true' && fully_covered=1
        fi

        # Tier 4: perfect — full pipeline ready
        if [ "$fully_covered" = "1" ]; then
            [ "$verbose" = "1" ] && echo "  [tier4 full-pipeline] $url" >&2
            echo "$url"
            return 0
        fi
        # Tier 3: /shards works + peers >= 1 (best available when no one has the full pipeline)
        if [ "$shards_ok" = "1" ] && [ "$peers" -ge 1 ] 2>/dev/null; then
            [ "$verbose" = "1" ] && echo "  [tier3 shards+peers=$peers] $url" >&2
            if [ "$best_tier" -lt 3 ]; then
                best_url="$url"
                best_tier=3
            fi
            continue
        fi
        # Tier 2: /health with peers >= 1, /shards broken (lookup-only)
        if [ "$peers" -ge 1 ] 2>/dev/null; then
            [ "$verbose" = "1" ] && echo "  [tier2 peers=$peers shards=broken] $url" >&2
            if [ "$best_tier" -lt 2 ]; then
                best_url="$url"
                best_tier=2
            fi
            continue
        fi
        # Tier 1: alive but isolated
        [ "$verbose" = "1" ] && echo "  [tier1 alive peers=0] $url" >&2
        if [ "$best_tier" -lt 1 ]; then
            best_url="$url"
            best_tier=1
        fi
    done

    if [ -n "$best_url" ]; then
        echo "$best_url"
        return 0
    fi
    return 1
}

# If executed (not sourced), print the chosen URL and exit.
if [ "${BASH_SOURCE[0]}" = "${0}" ] || [ -z "${BASH_SOURCE[0]:-}" ]; then
    url=$(arc_pick_coordinator)
    rc=$?
    if [ $rc -ne 0 ] || [ -z "$url" ]; then
        echo "ERROR: no reachable ARC seed. All 8 testnet seeds failed their /health probe." >&2
        echo "       Check https://github.com/FerrumVir/arc-chain for testnet status." >&2
        exit 1
    fi
    echo "$url"
fi
