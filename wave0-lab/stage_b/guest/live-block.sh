#!/usr/bin/env bash
# Reject (or restore) the live-network addresses with iptables, exactly like the repository CI isolation does
# (`iptables -I OUTPUT -d <ip> -j REJECT`). Needs root. THROWAWAY LAB FILE.
# Usage: live-block.sh on|off|status
set -Eeuo pipefail
ips_file="${ARC_W0_LIVE_IPS:-/opt/arc-w0/live-ips.txt}"
action="${1:?usage: live-block.sh on|off|status}"
while read -r ip; do
    [ -n "$ip" ] || continue
    case "$action" in
        on)
            iptables -C OUTPUT -d "$ip" -j REJECT 2>/dev/null || iptables -I OUTPUT -d "$ip" -j REJECT
            ;;
        off)
            while iptables -C OUTPUT -d "$ip" -j REJECT 2>/dev/null; do
                iptables -D OUTPUT -d "$ip" -j REJECT
            done
            ;;
        status)
            if iptables -C OUTPUT -d "$ip" -j REJECT 2>/dev/null; then echo "blocked $ip"; else echo "open    $ip"; fi
            ;;
        *)
            echo "unknown action $action" >&2
            exit 64
            ;;
    esac
done < "$ips_file"
