#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: deploy-explorer.sh cannot safely install packages or create public node services.' \
    'No host change was made. Wait for approved, checksummed node and explorer deployment manifests.' >&2
exit 78
