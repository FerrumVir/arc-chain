#!/usr/bin/env bash
# Interrupt a TLS download in the middle with iptables, then restore the network. Needs root. THROWAWAY LAB FILE.
#
#   arm QUOTA_BYTES      count every inbound TLS byte (rule tagged w0-count) and ACCEPT only the first QUOTA_BYTES,
#                        then DROP the rest of inbound TCP from port 443: a download that needs more than the quota
#                        stalls at a byte count that no timing race can move. The guest's ssh (port 22) is untouched.
#   watch FILE THRESHOLD TIMEOUT_S
#                        fallback when the quota match is unavailable: poll FILE (the launcher's .partial) and insert
#                        the DROP rule as soon as it holds THRESHOLD bytes.
#   count                count inbound TLS bytes only (no cut), to measure a download.
#   counters             print the inbound TLS bytes counted since arm or count.
#   disarm               remove every rule this script added (idempotent).
#   selftest             arm with a huge quota and disarm; exit 0 only if the quota match works.
set -Eeuo pipefail
chain=W0QUOTA
tag=w0-count

disarm() {
    while iptables -C INPUT -p tcp --sport 443 -j "$chain" 2>/dev/null; do iptables -D INPUT -p tcp --sport 443 -j "$chain"; done
    while iptables -C INPUT -p tcp --sport 443 -m comment --comment "$tag" 2>/dev/null; do iptables -D INPUT -p tcp --sport 443 -m comment --comment "$tag"; done
    while iptables -C INPUT -p tcp --sport 443 -m comment --comment w0-cut -j DROP 2>/dev/null; do iptables -D INPUT -p tcp --sport 443 -m comment --comment w0-cut -j DROP; done
    iptables -F "$chain" 2>/dev/null || true
    iptables -X "$chain" 2>/dev/null || true
}

arm() {
    local quota="$1"
    disarm
    iptables -N "$chain"
    iptables -A "$chain" -m quota --quota "$quota" -j ACCEPT
    iptables -A "$chain" -j DROP
    iptables -I INPUT 1 -p tcp --sport 443 -j "$chain"
    iptables -I INPUT 1 -p tcp --sport 443 -m comment --comment "$tag"
}

case "${1:-}" in
    arm)
        arm "${2:?quota bytes}"
        echo "armed: first ${2} inbound TLS bytes pass, the rest is dropped"
        ;;
    watch)
        file="${2:?partial file}"
        threshold="${3:?threshold bytes}"
        timeout_s="${4:-120}"
        disarm
        iptables -I INPUT 1 -p tcp --sport 443 -m comment --comment "$tag"
        end=$(( $(date +%s) + timeout_s ))
        while [ "$(date +%s)" -lt "$end" ]; do
            if [ -f "$file" ] && [ "$(stat -c %s "$file")" -ge "$threshold" ]; then
                iptables -I INPUT 1 -p tcp --sport 443 -m comment --comment w0-cut -j DROP
                echo "cut at $(stat -c %s "$file") bytes"
                exit 0
            fi
            sleep 0.02
        done
        echo "watch timed out before $file reached $threshold bytes" >&2
        exit 1
        ;;
    count)
        disarm
        iptables -I INPUT 1 -p tcp --sport 443 -m comment --comment "$tag"
        echo "counting inbound TLS bytes"
        ;;
    counters)
        iptables -nvxL INPUT | awk -v tag="$tag" 'index($0, tag) { print $2; found = 1; exit } END { if (!found) print 0 }'
        ;;
    disarm)
        disarm
        echo "disarmed"
        ;;
    selftest)
        arm 4000000000
        iptables -C "$chain" -m quota --quota 4000000000 -j ACCEPT
        disarm
        echo "quota match works"
        ;;
    *)
        echo "usage: interrupt.sh arm QUOTA | watch FILE THRESHOLD [TIMEOUT_S] | count | counters | disarm | selftest" >&2
        exit 64
        ;;
esac
