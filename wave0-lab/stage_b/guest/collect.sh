#!/usr/bin/env bash
# Pack the guest evidence into /var/lib/arc-w0/evidence.tgz with the v0.7 seed redacted. Read-only for the install.
# THROWAWAY LAB FILE. Runs as the guest user with passwordless sudo.
set -uo pipefail
work=/var/lib/arc-w0
arc_dir="$HOME/.arc"
out="$(mktemp -d "$work/collect.XXXXXX")"
mkdir -p "$out/logs" "$out/units" "$out/state"

{
    echo "== uname"; uname -a
    echo "== systemd"; systemctl --version | head -2
    echo "== python"; python3 --version
    echo "== uptime / boots"; uptime; journalctl --list-boots --no-pager 2>&1
    echo "== df"; df -h /
    echo "== free"; free -m
    echo "== ip"; ip -br addr
} > "$out/state/guest-info.txt" 2>&1
# shellcheck disable=SC2024  # the output files must belong to the invoking user, which is what a plain redirect gives
sudo iptables -S > "$out/state/iptables-rules.txt" 2>&1
# shellcheck disable=SC2024
sudo iptables -nvxL INPUT > "$out/state/iptables-input-counters.txt" 2>&1
systemctl status arc-node arc-updater.service arc-updater.timer arc-w0-sampler arc-w0-heartbeat arc-w0-live-block --no-pager -l > "$out/state/systemctl-status.txt" 2>&1
systemctl show arc-node -p ActiveState -p SubState -p MainPID -p NRestarts -p ActiveEnterTimestamp -p ExecMainStartTimestamp > "$out/state/arc-node-properties.txt" 2>&1
# shellcheck disable=SC2024
sudo journalctl -b 0 --no-pager -o short-iso -u arc-node -u arc-updater.service -u arc-updater.timer -u arc-w0-sampler -u arc-w0-heartbeat -u arc-w0-live-block > "$out/logs/journal-current-boot.txt" 2>&1
# shellcheck disable=SC2024
sudo journalctl -b -1 --no-pager -o short-iso -u arc-node -u arc-updater.service -u arc-updater.timer -u arc-w0-sampler -u arc-w0-heartbeat -u arc-w0-live-block > "$out/logs/journal-previous-boot.txt" 2>&1
tail -n 20000 "$arc_dir/node.log" > "$out/logs/node.log.tail" 2>&1
cp "$arc_dir/auto-update.log" "$out/logs/auto-update.log" 2>&1
cp "$arc_dir/version.txt" "$out/state/version.txt" 2>&1
cp "$arc_dir/legacy-bridge/bridge.log" "$out/logs/bridge.log" 2>&1
find "$arc_dir/legacy-bridge" -maxdepth 4 -printf '%M %u %s %p\n' > "$out/state/legacy-bridge.tree" 2>&1
find "$arc_dir/legacy-bridge" -maxdepth 3 \( -name 'bridge-state.json' -o -name 'compute-consent' -o -name 'v0.7-data-archive-*.json' \) -exec cp --parents {} "$out/state/" \; 2>/dev/null
for f in "$work"/samples.jsonl "$work"/heartbeats.jsonl "$work"/before-snapshot.json "$work"/invariants-*.json "$work"/state-*.json "$work"/journal-*.txt "$work"/node-log-*.txt "$work"/bridge-log-*.txt "$work"/watch.log; do
    [ -f "$f" ] && cp "$f" "$out/state/" 2>/dev/null
done
mkdir -p "$out/snapshots" "$out/baseline"
cp "$work"/snapshots/start-*.json "$out/snapshots/" 2>/dev/null
cp "$work"/baseline/v07-install.log "$work"/baseline/baseline-result.txt "$work"/baseline/real-releases-latest.json "$out/baseline/" 2>/dev/null
for u in /etc/systemd/system/arc-node.service /etc/systemd/system/arc-updater.service /etc/systemd/system/arc-updater.timer /etc/systemd/system/arc-node.service.d/*.conf /etc/systemd/system/arc-w0-*.service; do
    [ -f "$u" ] && cp "$u" "$out/units/" 2>/dev/null
done

# Redact the v0.7 identity seed everywhere (the v0.7 unit passes it on the command line).
python3 - "$out" "$arc_dir/identity.seed" <<'PY'
import os, sys
root, seed_path = sys.argv[1], sys.argv[2]
try:
    seed = open(seed_path, encoding="utf-8").read().strip()
except OSError:
    seed = ""
hits = 0
if len(seed) >= 8:
    for current, _dirs, files in os.walk(root):
        for name in files:
            path = os.path.join(current, name)
            try:
                data = open(path, "rb").read()
            except OSError:
                continue
            if seed.encode() in data:
                hits += data.count(seed.encode())
                open(path, "wb").write(data.replace(seed.encode(), b"<redacted-seed>"))
print(f"redacted {hits} occurrence(s) of the v0.7 seed")
leftover = 0
if len(seed) >= 8:
    for current, _dirs, files in os.walk(root):
        for name in files:
            try:
                if seed.encode() in open(os.path.join(current, name), "rb").read():
                    leftover += 1
            except OSError:
                pass
sys.exit(1 if leftover else 0)
PY
status=$?
tar czf "$work/evidence.tgz" -C "$out" .
echo "collected $(du -h "$work/evidence.tgz" | cut -f1) redaction_status=$status"
exit "$status"
