#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: deploy/monitor.sh cannot establish network or consensus health.' \
    'No endpoint was queried. An HTTP response alone is not a fork, liveness, or finality check.' >&2
exit 78
