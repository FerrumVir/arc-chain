#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: deploy/teardown.sh cannot safely select or delete cloud servers.' \
    'No server or local state was deleted. Use an approved v3 manifest and provider-side review.' >&2
exit 78
