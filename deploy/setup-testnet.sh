#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' \
    'RETIRED: deploy/setup-testnet.sh cannot safely provision a validator fleet.' \
    'No server was created. Wait for an approved, checksummed v3 fleet manifest and operator tool.' >&2
exit 78
