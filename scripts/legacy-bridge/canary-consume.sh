#!/usr/bin/env bash
# Consume a legacy-bridge canary release on ONE v0.7 headless machine.
#
# Before the bridge release is marked "Latest", no v0.7 updater can see it.
# This script performs exactly the steps the v0.7.11 arc-auto-update.sh
# performs once a release is "Latest", but against an explicit tag, and adds
# the digest check the old updater never had:
#
#   download releases/download/<tag>/arc-node-<platform>, require the SHA-256
#   from the handoff SHA256SUMS, keep bin/arc-node.prev, move the launcher into
#   bin/arc-node, write version.txt, restart the v0.7 service, wait 30 s, and
#   roll back exactly as the v0.7 updater would if /health does not answer.
#
# It prints the plan and changes nothing unless --apply is given.
#
# Usage:
#   bash canary-consume.sh --tag v0.7.12 --expect-sha256 <hex> [--apply] [--arc-dir ~/.arc]
set -Eeuo pipefail

repo="FerrumVir/arc-chain"
tag=""
expected=""
apply=false
arc_dir="${ARC_DIR:-$HOME/.arc}"

while [ $# -gt 0 ]; do
    case "$1" in
        --tag) tag="$2"; shift 2 ;;
        --expect-sha256) expected="$2"; shift 2 ;;
        --apply) apply=true; shift ;;
        --arc-dir) arc_dir="$2"; shift 2 ;;
        -h | --help) sed -n '2,19p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 64 ;;
    esac
done
[[ "$tag" =~ ^v0\.7\.[0-9]+$ ]] || { echo "--tag must be the exact v0.7.x bridge tag" >&2; exit 64; }
[[ "$expected" =~ ^[0-9a-f]{64}$ ]] || { echo "--expect-sha256 must be the launcher digest from the handoff SHA256SUMS" >&2; exit 64; }

case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) asset=arc-node-macos-arm64 ;;
    Darwin/x86_64) asset=arc-node-macos-x86_64 ;;
    Linux/x86_64 | Linux/amd64) asset=arc-node-linux-x86_64 ;;
    Linux/aarch64 | Linux/arm64) asset=arc-node-linux-aarch64 ;;
    *) echo "unsupported platform $(uname -s)/$(uname -m)" >&2; exit 69 ;;
esac

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}

restart_service() {
    if [ "$(uname -s)" = Darwin ]; then
        launchctl kickstart -k "gui/$(id -u)/com.arc.inference"
    else
        sudo systemctl restart arc-node
    fi
}

[ -x "$arc_dir/bin/arc-node" ] || { echo "no v0.7 install at $arc_dir (bin/arc-node missing)" >&2; exit 66; }
[ -f "$arc_dir/version.txt" ] || { echo "$arc_dir is not a v0.7 community-installer layout (no version.txt)" >&2; exit 66; }
if [ "$(uname -s)" = Darwin ]; then
    [ -f "$HOME/Library/LaunchAgents/com.arc.inference.plist" ] || { echo "no com.arc.inference LaunchAgent" >&2; exit 66; }
else
    [ -f /etc/systemd/system/arc-node.service ] || { echo "no arc-node.service unit" >&2; exit 66; }
fi
current_version="$(cat "$arc_dir/version.txt")"
url="https://github.com/$repo/releases/download/$tag/$asset"

cat <<PLAN
Canary plan for $arc_dir (currently v$current_version)
  1. download $url
  2. require SHA-256 $expected
  3. keep $arc_dir/bin/arc-node as arc-node.prev; install the launcher as bin/arc-node
  4. write ${tag#v} to version.txt; restart the v0.7 service
  5. after 30 s require /health on localhost:9944 or :9090, else roll back like the v0.7 updater
The v0.7 data directory ($arc_dir/data) is never written by this script or by the bridge.
PLAN
if [ "$apply" != true ]; then
    echo "Dry run only. Re-run with --apply to perform these steps."
    exit 0
fi

staged="$arc_dir/bin/arc-node.canary"
curl -fL --proto '=https' --tlsv1.2 -o "$staged" "$url"
actual="$(sha256 "$staged")"
if [ "$actual" != "$expected" ]; then
    rm -f "$staged"
    echo "digest mismatch: got $actual, expected $expected; nothing changed" >&2
    exit 65
fi
chmod +x "$staged"
cp "$arc_dir/bin/arc-node" "$arc_dir/bin/arc-node.prev"
mv "$staged" "$arc_dir/bin/arc-node"
printf '%s\n' "${tag#v}" > "$arc_dir/version.txt"
restart_service
echo "service restarted; waiting 30 s like the v0.7 updater"
sleep 30
for port in 9944 9090; do
    if curl -sf -m 5 "http://localhost:$port/health" >/dev/null; then
        echo "healthy on port $port. Bridge status:"
        "$arc_dir/bin/arc-node" --legacy-bridge-status
        curl -sf -m 5 "http://localhost:$port/node/info" && echo
        exit 0
    fi
done
echo "not healthy after 30 s: rolling back exactly like the v0.7 updater" >&2
mv "$arc_dir/bin/arc-node.prev" "$arc_dir/bin/arc-node"
printf '%s\n' "$current_version" > "$arc_dir/version.txt"
restart_service
echo "rolled back to v$current_version; see $arc_dir/node.log and $arc_dir/legacy-bridge/bridge.log" >&2
exit 1
