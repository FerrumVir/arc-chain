#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' \
    'RETIRED: arc-export-volatile-state.sh cannot safely operate the current validator fleet.' \
    'No action was taken. Use the approved manifest-bound recovery capture workflow.' >&2
exit 78
