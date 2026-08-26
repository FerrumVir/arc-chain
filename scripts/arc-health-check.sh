#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: arc-health-check.sh cannot establish network or consensus health.' \
    'No host was queried. Host-local HTTP and DAG counters do not prove shared-chain finality.' >&2
exit 78
