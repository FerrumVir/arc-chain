#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: arc-remote-relaunch.sh cannot safely restart a validator process.' \
    'No action was taken. Wait for an approved, checksummed v3 fleet manifest and operator tool.' >&2
exit 78
