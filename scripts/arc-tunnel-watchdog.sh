#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: arc-tunnel-watchdog.sh cannot safely create a tunnel to the legacy fleet.' \
    'No process was killed and no SSH connection was opened. Wait for the approved v3 operator tool.' >&2
exit 78
