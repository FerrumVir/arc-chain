#!/usr/bin/env bash
# Install the lab units inside the guest. Needs root. THROWAWAY LAB FILE.
#   live-block  arc-w0-live-block.service rejects the live-network addresses (same iptables rule as the repository
#               CI); installed and started BEFORE the v0.7.7 baseline so it never touches the live network. The host
#               disables it before the bridged node first starts when live_network is "allowed", and enables it
#               again before the final stop/rollback test.
#   sampler     arc-w0-sampler.service, enabled observer: one JSON line per interval, across reboots, never starts
#               anything.
# Usage: install-units.sh --units live-block
#        install-units.sh --units sampler --interval SECONDS --arc-dir DIR --user USER
set -Eeuo pipefail
units=""
interval=""
arc_dir=""
user=""
while [ $# -gt 0 ]; do
    case "$1" in
        --units) units="$2"; shift 2 ;;
        --interval) interval="$2"; shift 2 ;;
        --arc-dir) arc_dir="$2"; shift 2 ;;
        --user) user="$2"; shift 2 ;;
        *) echo "unknown argument $1" >&2; exit 64 ;;
    esac
done
: "${units:?--units live-block|sampler}"

install_live_block() {
cat > /etc/systemd/system/arc-w0-live-block.service <<UNIT
[Unit]
Description=ARC Wave 0 lab: reject the live-network addresses (the repository CI isolation rule)
After=network-pre.target
Wants=network-pre.target
Before=arc-node.service network.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/opt/arc-w0/lab/live-block.sh on
ExecStop=/opt/arc-w0/lab/live-block.sh off

[Install]
WantedBy=multi-user.target
UNIT

    systemctl daemon-reload
    systemctl enable --now arc-w0-live-block.service
    /opt/arc-w0/lab/live-block.sh status
}

install_sampler() {
    : "${interval:?--interval}" "${arc_dir:?--arc-dir}" "${user:?--user}"
cat > /etc/systemd/system/arc-w0-sampler.service <<UNIT
[Unit]
Description=ARC Wave 0 lab sampler (observer only; never starts or restarts anything)
After=network.target

[Service]
Type=simple
User=$user
ExecStart=/usr/bin/python3 /opt/arc-w0/lab/sampler.py --out /var/lib/arc-w0/samples.jsonl --arc-dir $arc_dir --interval $interval --before-snapshot /var/lib/arc-w0/before-snapshot.json --compare-every 10
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now arc-w0-sampler.service
systemctl is-active arc-w0-sampler.service
}

case "$units" in
    live-block) install_live_block ;;
    sampler) install_sampler ;;
    *) echo "unknown --units $units" >&2; exit 64 ;;
esac
