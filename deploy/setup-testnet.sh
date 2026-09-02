#!/usr/bin/env bash
# ARC_RETIRED_LIVE_TOOL_V3_REQUIRED
set -euo pipefail
printf '%s\n' \
    'RETIRED: deploy/setup-testnet.sh cannot safely provision a validator fleet.' \
    'No server was created. Wait for an approved, checksummed v3 fleet manifest and operator tool.' >&2
exit 78
