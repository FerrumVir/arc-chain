#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain - Coordinator Auto-Discovery
#
# Probes the testnet seed list and prints the URL of the coordinator best able
# to serve a demo right now. Used by arc-demo.sh, arc-verify.sh, arc-bench.sh
# as a drop-in replacement for the old hardcoded `http://149.28.32.76:9090`.
#
# Selection policy. Every reachable seed is scored and the BEST one wins -
# unlike the previous version, no seed short-circuits the loop. Ranked by:
#
#   1. Block liveness  - only when ARC_PICK_BLOCK_WINDOW > 0 (opt-in, costs
#                        that many seconds). Seeds whose /health height moved
#                        during the window outrank seeds whose height is frozen.
#   2. Capability tier - 4 = /shards fully_covered (full pipeline reachable)
#                        3 = /shards responds and peers >= 1
#                        2 = /health responds, peers >= 1, /shards broken
#                        1 = /health responds but peers = 0 (isolated)
#   3. Node version    - higher semver wins. The seeds are NOT all on the same
#                        binary; picking the newest avoids the least-tested
#                        cross-version code paths.
#   4. Attestation data - a seed whose /inference/results is non-empty outranks
#                        an empty one. arc-verify.sh --latest reads that
#                        endpoint, and on the live net most seeds return [].
#   5. Latency         - /health round-trip, lowest wins.
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
#   # Verbose mode prints per-seed scoring to stderr:
#   ARC_PICK_VERBOSE=1 bash arc-pick-coordinator.sh
#
#   # Also require recent block production (adds N seconds of wall time):
#   ARC_PICK_BLOCK_WINDOW=20 bash arc-pick-coordinator.sh
#
# Environment:
#   ARC_SEEDS               space-separated "host:port" list, overrides default
#   ARC_SEEDS_URL           URL of a plaintext host:port list (one per line)
#   ARC_PICK_TIMEOUT        per-request curl timeout, seconds (default 3)
#   ARC_PICK_BLOCK_WINDOW   seconds to wait before re-sampling height (default 0 = skip)
#   ARC_PICK_VERBOSE        1 to print scoring to stderr
# ─────────────────────────────────────────────────────────────────────────────

# Default seed list - the six seeds that are actually deployed.
#
# 2026-08-17: 216.238.120.27 (SAO) and 139.84.237.49 (JNB) were REMOVED. They
# were retired on 2026-04-22 (GH #32) and have not answered a TCP connect
# since; every run of this script was paying two full curl timeouts for them.
# Do not add them back without confirming /health responds.
_ARC_DEFAULT_SEEDS="149.28.32.76:9090 140.82.16.112:9090 136.244.109.1:9090 104.238.171.11:9090 202.182.107.41:9090 149.28.153.31:9090"

# Turn "0.7.11" into a comparable integer (major*10000 + minor*100 + patch).
_arc_version_rank() {
    local v="${1:-0.0.0}" maj min pat
    maj="${v%%.*}"; v="${v#*.}"
    min="${v%%.*}"; v="${v#*.}"
    pat="${v%%[!0-9]*}"
    maj=$(( ${maj:-0} + 0 )) 2>/dev/null || maj=0
    min=$(( ${min:-0} + 0 )) 2>/dev/null || min=0
    pat=$(( ${pat:-0} + 0 )) 2>/dev/null || pat=0
    echo $(( maj * 10000 + min * 100 + pat ))
}

arc_pick_coordinator() {
    local seeds="${ARC_SEEDS:-}"
    local timeout="${ARC_PICK_TIMEOUT:-3}"
    local verbose="${ARC_PICK_VERBOSE:-0}"
    local block_window="${ARC_PICK_BLOCK_WINDOW:-0}"

    # If ARC_SEEDS_URL is set, fetch it (space- or newline-separated host:port list)
    if [ -z "$seeds" ] && [ -n "${ARC_SEEDS_URL:-}" ]; then
        seeds=$(curl -sf -m 5 "$ARC_SEEDS_URL" 2>/dev/null | tr '\n' ' ')
    fi
    seeds="${seeds:-$_ARC_DEFAULT_SEEDS}"

    local candidates=""   # newline-separated "score latency_ms url"
    local heights=""      # newline-separated "url height" for the liveness pass

    local seed url health_json peers version verrank tier
    local results_json has_results latency_ms height score

    for seed in $seeds; do
        # Strip any scheme/path the user might have included
        seed="${seed#http://}"
        seed="${seed#https://}"
        seed="${seed%%/*}"
        url="http://${seed}"

        # Probe /health first - fastest signal, and gives us version + height.
        local probe
        probe=$(curl -sf -m "$timeout" -w '\n%{time_total}' "${url}/health" 2>/dev/null || echo "")
        if [ -z "$probe" ]; then
            [ "$verbose" = "1" ] && echo "  [DEAD] $url" >&2
            continue
        fi
        latency_ms=$(printf '%s' "${probe##*$'\n'}" | awk '{printf "%d", $1 * 1000}')
        health_json="${probe%$'\n'*}"

        peers=$(echo "$health_json" | sed -n 's/.*"peers":\([0-9][0-9]*\).*/\1/p')
        peers=${peers:-0}
        version=$(echo "$health_json" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')
        version=${version:-0.0.0}
        verrank=$(_arc_version_rank "$version")
        height=$(echo "$health_json" | sed -n 's/.*"height":\([0-9][0-9]*\).*/\1/p')
        height=${height:-0}

        # Probe /shards - required for sharded inference routing
        local shards_json shards_ok=0 fully_covered=0
        shards_json=$(curl -sf -m "$timeout" "${url}/shards" 2>/dev/null || echo "")
        if [ -n "$shards_json" ]; then
            shards_ok=1
            echo "$shards_json" | grep -q '"fully_covered":true' && fully_covered=1
        fi

        if [ "$fully_covered" = "1" ]; then
            tier=4
        elif [ "$shards_ok" = "1" ] && [ "$peers" -ge 1 ] 2>/dev/null; then
            tier=3
        elif [ "$peers" -ge 1 ] 2>/dev/null; then
            tier=2
        else
            tier=1
        fi

        # Does this seed actually hold attestation results? arc-verify.sh
        # --latest reads /inference/results, and on the live network that list
        # is empty on most seeds, so an empty seed is a bad demo coordinator.
        has_results=0
        results_json=$(curl -sf -m "$timeout" "${url}/inference/results" 2>/dev/null || echo "")
        if [ -n "$results_json" ] && ! echo "$results_json" | grep -q '"count":0'; then
            echo "$results_json" | grep -q '"tx_hash"' && has_results=1
        fi

        # Composite score. Liveness (added in the second pass) outranks tier,
        # tier outranks version, version outranks attestation data.
        score=$(( tier * 10000000 + verrank * 10 + has_results ))

        [ "$verbose" = "1" ] && \
            echo "  [tier$tier v$version results=$has_results ${latency_ms}ms] $url" >&2

        candidates="${candidates}${score} ${latency_ms} ${url}"$'\n'
        heights="${heights}${url} ${height}"$'\n'
    done

    [ -z "$candidates" ] && return 1

    # ── Optional second pass: did the seed actually commit a block? ──────────
    # Four of the six seeds have gone days without sealing a block while still
    # reporting {"status":"ok"}, so /health alone is not a liveness signal.
    # This costs ARC_PICK_BLOCK_WINDOW seconds, so it is off by default.
    if [ "$block_window" -gt 0 ] 2>/dev/null; then
        [ "$verbose" = "1" ] && echo "  [liveness] waiting ${block_window}s to re-sample block height..." >&2
        sleep "$block_window"
        local rescored="" c_score c_lat c_url before after
        while IFS=' ' read -r c_score c_lat c_url; do
            [ -z "$c_url" ] && continue
            before=$(printf '%s' "$heights" | awk -v u="$c_url" '$1==u{print $2}')
            after=$(curl -sf -m "$timeout" "${c_url}/health" 2>/dev/null \
                    | sed -n 's/.*"height":\([0-9][0-9]*\).*/\1/p')
            after=${after:-0}
            if [ "${after:-0}" -gt "${before:-0}" ] 2>/dev/null; then
                c_score=$(( c_score + 100000000 ))
                [ "$verbose" = "1" ] && echo "  [liveness OK  +$(( after - before )) blocks] $c_url" >&2
            else
                [ "$verbose" = "1" ] && echo "  [liveness STALL height frozen at $before] $c_url" >&2
            fi
            rescored="${rescored}${c_score} ${c_lat} ${c_url}"$'\n'
        done <<< "$candidates"
        candidates="$rescored"
    fi

    # Highest score wins; lowest latency breaks ties.
    printf '%s' "$candidates" | grep -v '^[[:space:]]*$' \
        | sort -k1,1nr -k2,2n | head -n1 | awk '{print $3}'
    return 0
}

# If executed (not sourced), print the chosen URL and exit.
if [ "${BASH_SOURCE[0]}" = "${0}" ] || [ -z "${BASH_SOURCE[0]:-}" ]; then
    url=$(arc_pick_coordinator)
    rc=$?
    if [ $rc -ne 0 ] || [ -z "$url" ]; then
        echo "ERROR: no reachable ARC seed. All testnet seeds failed their /health probe." >&2
        echo "       Check https://github.com/FerrumVir/arc-chain for testnet status." >&2
        exit 1
    fi
    echo "$url"
fi
