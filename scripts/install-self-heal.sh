#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ARC Chain — install arc-self-heal on ONE seed host (GH #30)
#
# Deploys scripts/arc-self-heal.sh + arc-self-heal.service to the target,
# enables the systemd unit, and prints status. Safe to re-run.
#
# The daemon is supervisory only — installing/starting it does NOT touch the
# running arc-node process. The daemon will only restart arc-node if it
# detects a silent or drifted state per the issue acceptance criteria.
#
# Usage:
#   ./scripts/install-self-heal.sh <NODE_IP>
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

IP="${1:?usage: $0 <ip> — e.g. 136.244.109.1 (AMS)}"
SSH_KEY="$HOME/.ssh/id_ed25519"
SSH_OPTS="-i $SSH_KEY -o ConnectTimeout=10 -o StrictHostKeyChecking=no -o BatchMode=yes"

HERE="$(cd "$(dirname "$0")" && pwd)"

BOLD="\033[1m" GREEN="\033[32m" CYAN="\033[36m" YELLOW="\033[33m" RED="\033[31m" RESET="\033[0m"
info()  { printf "${CYAN}[INFO]${RESET}  %s\n" "$*"; }
ok()    { printf "${GREEN}[  OK]${RESET}  %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${RESET}  %s\n" "$*"; }
fail()  { printf "${RED}[FAIL]${RESET}  %s\n" "$*" >&2; exit 1; }

# Refuse to install if arc-node is managed by systemd on this host — we'd
# conflict on restart. (Currently seeds run arc-node bare via setsid+nohup.)
info "Preflight: checking $IP..."
ARC_UNIT_STATE=$(ssh $SSH_OPTS "root@${IP}" "systemctl is-active arc-node 2>/dev/null || true")
if [ "$ARC_UNIT_STATE" = "active" ]; then
    fail "arc-node.service is active on $IP — self-heal would conflict. Disable the systemd unit first."
fi

# Confirm arc-node is running as a bare process (gives us a live cmdline to snapshot).
PROC_COUNT=$(ssh $SSH_OPTS "root@${IP}" "pgrep -f 'arc-node --rpc' | wc -l" || echo 0)
if [ "${PROC_COUNT:-0}" = "0" ]; then
    warn "No arc-node process on $IP. Daemon will still install but has no cmdline to snapshot until arc-node starts."
fi

info "Copying scripts/arc-self-heal.sh + arc-self-heal.service to $IP..."
rsync -az -e "ssh $SSH_OPTS" \
    "$HERE/arc-self-heal.sh" "$HERE/arc-self-heal.service" \
    "root@${IP}:/root/arc-chain/scripts/"

info "Installing systemd unit..."
ssh $SSH_OPTS "root@${IP}" bash <<'REMOTE'
set -euo pipefail
chmod +x /root/arc-chain/scripts/arc-self-heal.sh
install -m 0644 /root/arc-chain/scripts/arc-self-heal.service /etc/systemd/system/arc-self-heal.service
systemctl daemon-reload
systemctl enable arc-self-heal >/dev/null 2>&1 || true
systemctl restart arc-self-heal
sleep 2
systemctl is-active arc-self-heal
REMOTE

ok "arc-self-heal installed and running on $IP"

info "Status (first 20 lines):"
ssh $SSH_OPTS "root@${IP}" "systemctl status --no-pager arc-self-heal | head -20; echo; echo '--- tail of self-heal.log ---'; tail -n 10 /root/arc-chain/self-heal.log 2>/dev/null || echo '(no log yet)'"
