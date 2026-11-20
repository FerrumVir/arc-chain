#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' \
    'RETIRED: deploy-testnet.sh cannot safely provision or change a validator fleet.' \
    'No action was taken. Wait for an approved, checksummed v3 fleet manifest and operator tool.' >&2
exit 78
