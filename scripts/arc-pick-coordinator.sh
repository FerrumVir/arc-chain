#!/usr/bin/env bash

_ARC_DEFAULT_SEEDS="149.28.32.76:9090 140.82.16.112:9090 136.244.109.1:9090 104.238.171.11:9090 202.182.107.41:9090 149.28.153.31:9090"

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

    if [ -z "$seeds" ] && [ -n "${ARC_SEEDS_URL:-}" ]; then
        seeds=$(curl -sf -m 5 "$ARC_SEEDS_URL" 2>/dev/null | tr '\n' ' ')
    fi
    seeds="${seeds:-$_ARC_DEFAULT_SEEDS}"

    local candidates=""
    local heights=""

    local seed url health_json peers version verrank tier
    local results_json has_results latency_ms height score

    for seed in $seeds; do
        seed="${seed#http://}"
        seed="${seed#https://}"
        seed="${seed%%/*}"
        url="http://${seed}"

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

        has_results=0
        results_json=$(curl -sf -m "$timeout" "${url}/inference/results" 2>/dev/null || echo "")
        if [ -n "$results_json" ] && ! echo "$results_json" | grep -q '"count":0'; then
            echo "$results_json" | grep -q '"tx_hash"' && has_results=1
        fi

        score=$(( tier * 10000000 + verrank * 10 + has_results ))

        [ "$verbose" = "1" ] && \
            echo "  [tier$tier v$version results=$has_results ${latency_ms}ms] $url" >&2

        candidates="${candidates}${score} ${latency_ms} ${url}"$'\n'
        heights="${heights}${url} ${height}"$'\n'
    done

    [ -z "$candidates" ] && return 1

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

    printf '%s' "$candidates" | grep -v '^[[:space:]]*$' \
        | sort -k1,1nr -k2,2n | head -n1 | awk '{print $3}'
    return 0
}

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
