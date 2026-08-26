#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: this legacy false-health monitor is disabled in the v0.7.12 recovery candidate.' \
    'No approved v3 manifest-based replacement is available yet; this command made no request or change.' >&2
exit 78
